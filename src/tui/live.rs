//! Live broker + engine data pipeline.
//!
//! This is where the TUI's data-gathering lives, kept out of the [`super::app`]
//! state machine: turning IBKR market data and holdings into ranked
//! [`Suggestion`]s, syncing broker positions into the wheel-state store, probing
//! tradability, and resolving roll targets. No UI state — all free functions.

use anyhow::Result;
use chrono::NaiveDate;

use crate::config::Config;
use crate::engine::math::round_cents;
use crate::engine::types::{
    ActionKind, EngineConfig, OpenShortOption, OptionQuote, Right, SharePosition, Suggestion,
    UnderlyingQuote, WheelState,
};
use crate::engine::{self, SymbolContext};
use crate::ibkr::{AccountSnapshot, Ibkr, OpenOrderInfo, PositionRow, Tradability};
use crate::positions;
use crate::store::{JournalRow, Store, WatchlistRow, WheelPositionRow};

/// Keep only OTM strikes within this fraction of spot when building a chain
/// (bounds how far OTM we'll quote for entries / covered calls).
const MAX_OTM_MONEYNESS: f64 = 0.15;
/// Cap on per-symbol option snapshots, to bound market-data requests.
const MAX_CHAIN_STRIKES: usize = 5;
/// Far-OTM put target (fraction of spot) used by the tradability permission probe.
const PROBE_OTM_FRACTION: f64 = 0.85;

/// Everything an off-loop reload gathers from the broker + store, to be applied
/// to the `App` back on the event loop. Mirrors the *connected* branch of
/// `App::reload` (which stays inline, for startup only) so the heavy broker I/O
/// — chains, snapshots, the tradability probe — never runs on the UI thread.
pub(super) struct LiveData {
    pub account: Option<AccountSnapshot>,
    pub watchlist: Vec<WatchlistRow>,
    pub journal: Vec<JournalRow>,
    pub positions: Vec<WheelPositionRow>,
    pub broker_positions: Vec<PositionRow>,
    pub suggestions: Vec<Suggestion>,
    /// Authoritative open orders, or `None` if that snapshot failed.
    pub open_orders: Option<Vec<OpenOrderInfo>>,
    /// `false` when the positions snapshot was incomplete: the caller must NOT
    /// treat `broker_positions`/`suggestions` as authoritative (safety-critical —
    /// a failed fetch must never look like "the account is empty").
    pub positions_ok: bool,
}

/// Gather live broker + engine data off the UI thread (see [`LiveData`]).
///
/// `pending_roll_symbols` are folded into the "skip" set so a symbol with an
/// in-flight roll never gets a stacked suggestion. Pending-roll *reconciliation*
/// (stateful, can transmit) stays on the event loop in `App::apply_live_data`.
pub(super) async fn gather(
    ibkr: &Ibkr,
    store: &Store,
    cfg: &Config,
    pending_roll_symbols: &[String],
    today: NaiveDate,
) -> LiveData {
    let account = ibkr.account_summary().await.ok();

    // Probe tradability for still-unknown symbols *before* planning, then refresh
    // the watchlist so a symbol the probe blocks (PRIIPs / permissions) is left
    // out of this pass rather than surfacing an order we can't actually place.
    let mut watchlist = store.list_watchlist().await.unwrap_or_default();
    probe_unknown_tradability(ibkr, store, &watchlist, today).await;
    watchlist = store.list_watchlist().await.unwrap_or_default();

    let (broker_positions, suggestions, open_orders, positions_ok) = match ibkr.positions().await {
        Ok(positions) => {
            sync_wheel_state(store, &positions).await;
            // Authoritative open orders pick which symbols to skip; on a failed
            // snapshot, fall back to the journal's "submitted" rows.
            let (open_orders, working) = match ibkr.open_orders_snapshot().await {
                Ok(open) => {
                    let mut w: Vec<String> = open.iter().map(|o| o.symbol.clone()).collect();
                    w.extend(pending_roll_symbols.iter().cloned());
                    (Some(open), w)
                }
                Err(e) => {
                    tracing::warn!("open orders unavailable ({e}); using journal fallback");
                    let w = store.symbols_with_working_orders().await.unwrap_or_default();
                    (None, w)
                }
            };
            let suggestions =
                live_suggestions(ibkr, store, &watchlist, &positions, &working, cfg, today).await;
            (positions, suggestions, open_orders, true)
        }
        Err(e) => {
            // Positions unknown — keep stored state, drop suggestions downstream.
            tracing::warn!("positions fetch failed; suggestions will be cleared, stored state kept: {e}");
            (Vec::new(), Vec::new(), None, false)
        }
    };

    let journal = store.recent_journal(200).await.unwrap_or_default();
    let positions = store.list_positions().await.unwrap_or_default();

    LiveData {
        account,
        watchlist,
        journal,
        positions,
        broker_positions,
        suggestions,
        open_orders,
        positions_ok,
    }
}

/// One-shot price for an option leg (used to value a roll's new leg): model
/// price if present, else last, rounded to the cent. `None` if unpriced.
pub(super) async fn price_leg(ibkr: &Ibkr, symbol: &str, expiry: &str, strike: f64, right: &str) -> Option<f64> {
    let snap = ibkr.option_snapshot(symbol, expiry, strike, right).await.ok()?;
    let price = snap.comp.as_ref().and_then(|c| c.option_price).or(snap.last)?;
    (price > 0.0).then(|| round_cents(price))
}

/// Reconcile broker holdings into the local `wheel_positions` store.
///
/// Every symbol that has a current holding *or* an already-tracked row is
/// re-derived (so a closed position falls back to `Idle`), preserving each
/// row's `cumulative_premium` (which the broker can't report).
pub(super) async fn sync_wheel_state(store: &Store, broker_positions: &[PositionRow]) {
    use std::collections::BTreeSet;
    let mut symbols: BTreeSet<String> =
        broker_positions.iter().map(|p| p.symbol.clone()).collect();
    if let Ok(existing) = store.list_positions().await {
        symbols.extend(existing.into_iter().map(|p| p.symbol));
    }
    for symbol in symbols {
        let r = positions::reconcile(&symbol, broker_positions);
        let (shares, cost_basis) = r.shares.map_or((0, 0.0), |s| (s.shares, s.cost_basis));
        if let Err(e) = store
            .upsert_wheel_state(&symbol, r.state.as_str(), shares, cost_basis)
            .await
        {
            tracing::warn!("sync wheel state for {symbol}: {e}");
        }
    }
}

/// Probe tradability (EU/PRIIPs permission) for any enabled watchlist symbol
/// whose status is still unknown, persisting Allowed/Blocked. One-shot per
/// symbol: once set it isn't re-probed, so the cost is paid once. Uses a far-OTM
/// put what-if (never transmitted), mirroring the spike's probe.
pub(super) async fn probe_unknown_tradability(
    ibkr: &Ibkr,
    store: &Store,
    watchlist: &[WatchlistRow],
    today: NaiveDate,
) {
    for w in watchlist
        .iter()
        .filter(|w| w.is_enabled() && w.tradable.is_none())
    {
        if let Err(e) = probe_one_tradability(ibkr, store, w, today).await {
            tracing::warn!("tradability probe for {}: {e}", w.symbol);
        }
    }
}

/// Probe and persist one symbol's tradability. A definitively optionless symbol
/// is marked blocked; transient failures (no expiry / no spot) leave it unknown
/// so a later refresh retries.
async fn probe_one_tradability(
    ibkr: &Ibkr,
    store: &Store,
    w: &WatchlistRow,
    today: NaiveDate,
) -> Result<()> {
    let symbol = w.symbol.as_str();
    let conid = resolve_conid(ibkr, store, w).await?;

    let chain = ibkr.option_chain(symbol, conid).await?;
    // An empty chain is more likely a transient/timed-out fetch than a stock
    // with no options at all, so leave it unknown and retry rather than sticking
    // a permanent "blocked".
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(());
    }

    let Some((expiry, _)) = pick_expiry(&chain.expirations, today, 35) else {
        return Ok(()); // no future expiry right now — retry next refresh
    };
    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    let Some(strike) = far_otm_put_strike(&chain.strikes, spot) else {
        return Ok(());
    };

    match ibkr.tradability(symbol, &expiry, strike).await {
        Tradability::Allowed { .. } => store.set_tradable(symbol, true, None).await?,
        // Only persist "blocked" for a recognized permission/PRIIPs rejection.
        // A transient what-if failure (timeout, connection, no market data) is
        // left unknown so a later refresh retries instead of blocking forever.
        Tradability::Blocked(reason) if is_permission_block(&reason) => {
            store.set_tradable(symbol, false, Some(&reason)).await?;
        }
        Tradability::Blocked(reason) => {
            tracing::info!("{symbol}: tradability probe inconclusive ({reason}); will retry");
        }
    }
    Ok(())
}

/// Whether a what-if rejection reads like a *trading-permission* block (PRIIPs /
/// missing entitlement) — a durable "no" — as opposed to a transient failure we
/// should retry. Deliberately conservative: unmatched reasons stay "unknown".
///
/// Market-data permission errors are explicitly *not* trade blocks: a user can
/// be entitled to trade an instrument while lacking its data subscription.
fn is_permission_block(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    if r.contains("market data") || r.contains("market-data") {
        return false;
    }
    [
        "priips",
        "kid",
        "prohibited",
        "trading permission",
        "not allowed",
        "not permitted",
        "professional",
    ]
    .iter()
    .any(|kw| r.contains(kw))
}

/// A clearly-OTM listed put strike for a permission probe: the strike nearest
/// 85% of spot, or — with no spot quote — the median listed strike.
fn far_otm_put_strike(strikes: &[f64], spot: f64) -> Option<f64> {
    let target = if spot > 0.0 {
        spot * PROBE_OTM_FRACTION
    } else {
        let mut s: Vec<f64> = strikes.iter().copied().filter(|k| *k > 0.0).collect();
        if s.is_empty() {
            return None;
        }
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        s[s.len() / 2]
    };
    nearest_strike(strikes, target)
}

/// The listed strike nearest `target` (positive strikes only).
fn nearest_strike(strikes: &[f64], target: f64) -> Option<f64> {
    strikes
        .iter()
        .copied()
        .filter(|k| *k > 0.0)
        .min_by(|a, b| {
            (a - target)
                .abs()
                .partial_cmp(&(b - target).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Resolve the engine's *ideal* roll target (`to_expiry`, `to_strike`) to a
/// real **listed** contract and price it: nearest listed expiry to the ideal
/// DTE, nearest listed strike, and its current credit. `None` if the chain is
/// empty or the leg can't be priced. Without this, the target often falls on a
/// non-trading date and the order would reference a nonexistent contract.
pub(super) async fn resolve_roll_target(
    ibkr: &Ibkr,
    symbol: &str,
    right: &str,
    to_expiry: NaiveDate,
    to_strike: f64,
    today: NaiveDate,
) -> Result<Option<(String, f64, f64)>> {
    let conid = ibkr.underlying_contract_id(symbol).await?;
    let chain = ibkr.option_chain(symbol, conid).await?;
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(None);
    }
    let target_dte = (to_expiry - today).num_days().max(1);
    let Some((expiry, _)) = pick_expiry(&chain.expirations, today, target_dte) else {
        return Ok(None);
    };
    let Some(strike) = nearest_strike(&chain.strikes, to_strike) else {
        return Ok(None);
    };
    let Some(credit) = price_leg(ibkr, symbol, &expiry, strike, right).await else {
        return Ok(None);
    };
    Ok(Some((expiry, strike, credit)))
}

/// Owned per-symbol inputs for one planning pass; [`SymbolContext`] borrows the
/// quote vec from this, so instances are kept alive across [`engine::plan`].
struct SymbolInputs {
    symbol: String,
    state: WheelState,
    underlying: UnderlyingQuote,
    quotes: Vec<OptionQuote>,
    open_short: Option<(OpenShortOption, OptionQuote)>,
    shares: Option<SharePosition>,
    committed_call_contracts: i32,
    max_collateral: f64,
}

/// Compute suggestions across the enabled watchlist using live data, with each
/// symbol advised in the wheel leg its holdings put it in.
pub(super) async fn live_suggestions(
    ibkr: &Ibkr,
    store: &Store,
    watchlist: &[WatchlistRow],
    broker_positions: &[PositionRow],
    working: &[String],
    cfg: &Config,
    today: NaiveDate,
) -> Vec<Suggestion> {
    let active: Vec<&WatchlistRow> = watchlist.iter().filter(|w| w.is_enabled()).collect();
    if active.is_empty() {
        return Vec::new();
    }
    // Size new-entry collateral against the symbols actually eligible to open
    // one (enabled and not blocked); blocked symbols are managed only.
    let openable = active.iter().filter(|w| w.tradable != Some(0)).count().max(1);
    let budget = (cfg.guardrails.max_total_deployed / openable as f64).max(1000.0);

    // `working` = symbols with a live broker order; skip them entirely this pass
    // so we never stack a second action (e.g. a fresh entry while a roll-open is
    // still working, or a duplicate CSP) on a symbol with an in-flight order.
    let mut inputs: Vec<SymbolInputs> = Vec::with_capacity(active.len());
    for &w in &active {
        if working.iter().any(|s| s == &w.symbol) {
            continue;
        }
        let reconciled = positions::reconcile(&w.symbol, broker_positions);
        // A blocked symbol (PRIIPs / no permission) may still hold an open short
        // that we must be able to close, so keep managing existing positions;
        // only suppress *new opening* legs (entry / covered call). Rolls — which
        // open a new leg the account can't take — are filtered out below.
        let manages_existing =
            matches!(reconciled.state, WheelState::ShortPut | WheelState::ShortCall);
        if w.tradable == Some(0) && !manages_existing {
            continue;
        }
        match gather_inputs(ibkr, store, w, &reconciled, &cfg.engine, today, budget).await {
            Ok(Some(si)) => inputs.push(si),
            Ok(None) => {}
            Err(e) => tracing::warn!("live inputs for {}: {e}", w.symbol),
        }
    }

    let contexts: Vec<SymbolContext> = inputs
        .iter()
        .map(|si| SymbolContext {
            symbol: si.symbol.clone(),
            state: si.state,
            underlying: si.underlying,
            option_quotes: &si.quotes,
            open_short: si.open_short.clone(),
            shares: si.shares,
            committed_call_contracts: si.committed_call_contracts,
            max_collateral: si.max_collateral,
        })
        .collect();

    let mut suggestions = engine::plan(&contexts, &cfg.engine, today).suggestions;
    // A blocked symbol can be closed but not (re)opened, so drop rolls (which
    // open a new leg) for it; its buy-to-close take-profit action still stands.
    suggestions.retain(|s| {
        !(matches!(s.kind, ActionKind::Roll { .. })
            && watchlist
                .iter()
                .any(|w| w.symbol == s.symbol && w.tradable == Some(0)))
    });
    suggestions
}

/// Fetch the market data the engine needs for one symbol, dispatched on the
/// wheel leg its holdings put it in:
/// - `Idle` → an OTM put chain for a new cash-secured put
/// - `LongShares` → an OTM call chain for a covered call
/// - `ShortPut` / `ShortCall` → a fresh quote for the open short, to manage it
async fn gather_inputs(
    ibkr: &Ibkr,
    store: &Store,
    w: &WatchlistRow,
    reconciled: &positions::ReconciledPosition,
    cfg: &EngineConfig,
    today: NaiveDate,
    budget: f64,
) -> Result<Option<SymbolInputs>> {
    match reconciled.state {
        WheelState::Idle => {
            let Some((spot, quotes)) =
                gather_chain_quotes(ibkr, store, w, Right::Put, cfg, today).await?
            else {
                return Ok(None);
            };
            Ok(Some(SymbolInputs {
                symbol: w.symbol.clone(),
                state: WheelState::Idle,
                underlying: UnderlyingQuote { last: spot },
                quotes,
                open_short: None,
                shares: None,
                committed_call_contracts: 0,
                max_collateral: budget,
            }))
        }
        WheelState::LongShares => {
            let Some((spot, quotes)) =
                gather_chain_quotes(ibkr, store, w, Right::Call, cfg, today).await?
            else {
                return Ok(None);
            };
            Ok(Some(SymbolInputs {
                symbol: w.symbol.clone(),
                state: WheelState::LongShares,
                underlying: UnderlyingQuote { last: spot },
                quotes,
                open_short: None,
                shares: reconciled.shares,
                committed_call_contracts: reconciled.committed_call_contracts,
                max_collateral: 0.0,
            }))
        }
        WheelState::ShortPut | WheelState::ShortCall => {
            gather_manage_inputs(ibkr, w, reconciled).await
        }
    }
}

/// Resolve (and cache in the watchlist) the underlying's IBKR contract id.
async fn resolve_conid(ibkr: &Ibkr, store: &Store, w: &WatchlistRow) -> Result<i32> {
    match w.conid {
        Some(c) => Ok(c as i32),
        None => {
            let c = ibkr.underlying_contract_id(&w.symbol).await?;
            let _ = store.set_conid(&w.symbol, i64::from(c)).await;
            Ok(c)
        }
    }
}

/// Fetch spot plus a bounded set of OTM option quotes (one right) ~target DTE
/// out: the chain → nearest-to-spot OTM strike pre-filter → per-contract greek
/// snapshot pipeline shared by the entry and covered-call legs.
async fn gather_chain_quotes(
    ibkr: &Ibkr,
    store: &Store,
    w: &WatchlistRow,
    right: Right,
    cfg: &EngineConfig,
    today: NaiveDate,
) -> Result<Option<(f64, Vec<OptionQuote>)>> {
    let symbol = w.symbol.as_str();
    let conid = resolve_conid(ibkr, store, w).await?;

    let chain = ibkr.option_chain(symbol, conid).await?;
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(None);
    }

    let target_dte = (cfg.min_dte + cfg.max_dte) / 2;
    let Some((expiry_str, _)) = pick_expiry(&chain.expirations, today, target_dte) else {
        return Ok(None);
    };
    let expiry_date = NaiveDate::parse_from_str(&expiry_str, "%Y%m%d")?;

    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    if spot <= 0.0 {
        return Ok(None);
    }

    // Keep only OTM strikes within 15% of spot, nearest-to-spot first, capped at
    // 5 so per-contract market-data requests stay bounded.
    let mut strikes: Vec<f64> = chain
        .strikes
        .iter()
        .copied()
        .filter(|k| {
            *k > 0.0 && is_otm(right, *k, spot) && moneyness(right, *k, spot) <= MAX_OTM_MONEYNESS
        })
        .collect();
    strikes.sort_by(|a, b| {
        (a - spot)
            .abs()
            .partial_cmp(&(b - spot).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    strikes.truncate(MAX_CHAIN_STRIKES);
    if strikes.is_empty() {
        return Ok(None);
    }

    let right_char = right_char(right);
    let mut quotes: Vec<OptionQuote> = Vec::with_capacity(strikes.len());
    for k in strikes {
        if let Ok(snap) = ibkr.option_snapshot(symbol, &expiry_str, k, right_char).await
            && let Some(comp) = snap.comp
        {
            let price = comp.option_price.or(snap.last).unwrap_or(0.0);
            if price > 0.0 {
                quotes.push(OptionQuote {
                    right,
                    strike: k,
                    expiry: expiry_date,
                    bid: price,
                    ask: price,
                    delta: comp.delta,
                    implied_volatility: comp.implied_volatility,
                    open_interest: None,
                    volume: None,
                });
            }
        }
    }

    Ok(Some((spot, quotes)))
}

/// Fetch the inputs to manage an open short: spot + a fresh quote for the exact
/// short contract. Returns `None` (no suggestion) when the short can't be priced
/// this cycle — better to stay quiet than risk a bogus take-profit at $0.
async fn gather_manage_inputs(
    ibkr: &Ibkr,
    w: &WatchlistRow,
    reconciled: &positions::ReconciledPosition,
) -> Result<Option<SymbolInputs>> {
    let Some(short) = reconciled.open_short.clone() else {
        return Ok(None);
    };
    let symbol = w.symbol.as_str();

    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    if spot <= 0.0 {
        return Ok(None);
    }

    let expiry_str = short.expiry.format("%Y%m%d").to_string();
    let snap = ibkr
        .option_snapshot(symbol, &expiry_str, short.strike, right_char(short.right))
        .await?;
    let comp = snap.comp.as_ref();
    let price = comp.and_then(|c| c.option_price).or(snap.last).unwrap_or(0.0);
    if price <= 0.0 {
        tracing::info!("{symbol}: open short unpriced this cycle; skipping management");
        return Ok(None);
    }

    let quote = OptionQuote {
        right: short.right,
        strike: short.strike,
        expiry: short.expiry,
        bid: price,
        ask: price,
        delta: comp.and_then(|c| c.delta),
        implied_volatility: comp.and_then(|c| c.implied_volatility),
        open_interest: None,
        volume: None,
    };

    Ok(Some(SymbolInputs {
        symbol: symbol.to_string(),
        state: reconciled.state,
        underlying: UnderlyingQuote { last: spot },
        quotes: Vec::new(),
        open_short: Some((short, quote)),
        shares: reconciled.shares,
        committed_call_contracts: reconciled.committed_call_contracts,
        max_collateral: 0.0,
    }))
}

/// IBKR right code for a snapshot/order request.
pub(super) fn right_char(right: Right) -> &'static str {
    match right {
        Right::Put => "P",
        Right::Call => "C",
    }
}

/// Whether broker `positions` still hold a *short* option matching this leg.
/// `right` is an IBKR code (`"P"`/`"C"`); IBKR may report `right` as `PUT`/`CALL`
/// too, so we match on the leading letter.
pub(super) fn position_has_short(
    positions: &[PositionRow],
    symbol: &str,
    right: &str,
    strike: f64,
    expiry: &str,
) -> bool {
    positions.iter().any(|p| {
        p.symbol == symbol
            && p.security_type == "Option"
            && p.position < 0.0
            && (p.strike - strike).abs() < 1e-6
            && p.expiry == expiry
            && p.right.to_ascii_uppercase().starts_with(right)
    })
}

/// Whether `strike` is out-of-the-money for `right` given `spot`.
fn is_otm(right: Right, strike: f64, spot: f64) -> bool {
    match right {
        Right::Put => strike < spot,
        Right::Call => strike > spot,
    }
}

/// OTM moneyness as a positive fraction of spot (0 at-the-money).
fn moneyness(right: Right, strike: f64, spot: f64) -> f64 {
    if spot <= 0.0 {
        return f64::INFINITY;
    }
    match right {
        Right::Put => (spot - strike) / spot,
        Right::Call => (strike - spot) / spot,
    }
}

/// Pick the expiration closest to `target_dte` days out.
fn pick_expiry(expirations: &[String], today: NaiveDate, target_dte: i64) -> Option<(String, i64)> {
    expirations
        .iter()
        .filter_map(|e| {
            NaiveDate::parse_from_str(e, "%Y%m%d")
                .ok()
                .map(|d| (e.clone(), (d - today).num_days()))
        })
        .filter(|(_, dte)| *dte >= 1)
        .min_by_key(|(_, dte)| (dte - target_dte).abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn opt_pos(symbol: &str, right: &str, strike: f64, expiry: &str, position: f64) -> PositionRow {
        PositionRow {
            account: "DU1".into(),
            symbol: symbol.into(),
            security_type: "Option".into(),
            right: right.into(),
            strike,
            expiry: expiry.into(),
            position,
            average_cost: 100.0,
            multiplier: "100".into(),
        }
    }

    #[test]
    fn position_has_short_matches_leg() {
        let positions = vec![
            opt_pos("AAPL", "P", 100.0, "20260619", -1.0),
            opt_pos("AAPL", "PUT", 90.0, "20260619", 1.0), // long → not a short
        ];
        assert!(position_has_short(&positions, "AAPL", "P", 100.0, "20260619"));
        assert!(!position_has_short(&positions, "AAPL", "P", 90.0, "20260619"));
        assert!(!position_has_short(&positions, "AAPL", "P", 95.0, "20260619"));
        assert!(!position_has_short(&positions, "AAPL", "C", 100.0, "20260619"));
        assert!(!position_has_short(&positions, "MSFT", "P", 100.0, "20260619"));
    }

    #[test]
    fn nearest_strike_picks_closest_listed() {
        let strikes = vec![80.0, 90.0, 95.0, 100.0];
        assert_eq!(nearest_strike(&strikes, 93.0), Some(95.0));
        assert_eq!(nearest_strike(&strikes, 81.0), Some(80.0));
        assert_eq!(nearest_strike(&[], 90.0), None);
    }

    #[test]
    fn permission_block_only_for_recognized_reasons() {
        assert!(is_permission_block("Order rejected: PRIIPs KID required"));
        assert!(is_permission_block("No trading permission for this product"));
        assert!(is_permission_block("Product not allowed for retail"));
        assert!(!is_permission_block("request timed out"));
        assert!(!is_permission_block("connection reset"));
        assert!(!is_permission_block("no market data subscription"));
        assert!(!is_permission_block("No market data permissions for ISLAND"));
    }

    #[test]
    fn far_otm_put_strike_targets_85pct_of_spot() {
        let strikes = vec![70.0, 80.0, 85.0, 90.0, 95.0, 100.0, 110.0];
        assert_eq!(far_otm_put_strike(&strikes, 100.0), Some(85.0));
    }

    #[test]
    fn far_otm_put_strike_uses_median_without_spot() {
        let strikes = vec![70.0, 80.0, 90.0, 100.0, 110.0];
        assert_eq!(far_otm_put_strike(&strikes, 0.0), Some(90.0));
        assert_eq!(far_otm_put_strike(&[], 0.0), None);
    }

    #[test]
    fn otm_and_moneyness_by_right() {
        assert!(is_otm(Right::Put, 95.0, 100.0));
        assert!(!is_otm(Right::Put, 105.0, 100.0));
        assert!(is_otm(Right::Call, 105.0, 100.0));
        assert!(!is_otm(Right::Call, 95.0, 100.0));
        assert!((moneyness(Right::Put, 90.0, 100.0) - 0.10).abs() < 1e-9);
        assert!((moneyness(Right::Call, 110.0, 100.0) - 0.10).abs() < 1e-9);
    }

    #[test]
    fn pick_expiry_chooses_nearest_to_target() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let exps = vec![
            "20260605".to_string(),
            "20260703".to_string(),
            "20260919".to_string(),
        ];
        let (chosen, dte) = pick_expiry(&exps, today, 35).expect("an expiry");
        assert_eq!(chosen, "20260703");
        assert_eq!(dte, 32);
    }

    #[test]
    fn pick_expiry_skips_past_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let exps = vec!["20260101".to_string(), "20260630".to_string()];
        let (chosen, _) = pick_expiry(&exps, today, 35).expect("a future expiry");
        assert_eq!(chosen, "20260630");
    }
}
