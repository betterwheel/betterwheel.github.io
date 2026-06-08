//! Live broker + engine data pipeline — the UI-agnostic data layer.
//!
//! Sits between the broker/store/engine and the front-ends ([`crate::tui`] and
//! [`crate::web`]): turning IBKR market data and holdings into ranked
//! [`Suggestion`]s, syncing broker positions into the wheel-state store, probing
//! tradability, and resolving roll targets. No UI state — all free functions, so
//! either UI can drive it.

use anyhow::Result;
use chrono::NaiveDate;

use crate::config::{Config, ZeroDteConfig};
use crate::engine::math::{fcmp, round_cents};
use crate::engine::types::{
    ActionKind, EngineConfig, OpenShortOption, OptionQuote, Right, SharePosition, Suggestion,
    UnderlyingQuote, WheelState,
};
use crate::engine::{self, structures, SymbolContext};
use crate::ibkr::{AccountSnapshot, Ibkr, OpenOrderInfo, PositionRow, SnapshotData, Tradability};
use crate::positions;
use crate::store::{JournalRow, Store, WatchlistRow, WheelPositionRow};

/// Keep only OTM strikes within this fraction of spot when building a chain
/// (bounds how far OTM we'll quote for entries / covered calls).
const MAX_OTM_MONEYNESS: f64 = 0.15;
/// Cap on per-symbol option snapshots, to bound market-data requests. Sampled
/// *spread across* the OTM range (see [`sample_spread`]) so the target-delta
/// band is covered, then fetched concurrently.
const MAX_CHAIN_STRIKES: usize = 12;
/// Far-OTM put target (fraction of spot) used by the tradability permission probe.
const PROBE_OTM_FRACTION: f64 = 0.85;
/// Strikes near spot (within this fraction, each side) considered for a 0DTE
/// structure. Kept tight: a same-day short sits ~1% OTM and its wing a little
/// further, so a wide band wastes the strike budget on far strikes that never get
/// picked and leaves the relevant near-ATM grid too coarse to hit the delta
/// target or the exact wing.
const STRUCTURE_OTM_MONEYNESS: f64 = 0.025;
/// Cap on the strike sample for a 0DTE structure (×2 rights → snapshot count).
/// Spread across the tight band above, this gives ~10pt resolution on SPX —
/// fine enough that a points-width wing lands near its target rather than
/// overshooting and inflating max-loss past the risk budget.
const MAX_STRUCTURE_STRIKES: usize = 40;
/// Snapshot concurrency cap for the structure chain. Index 0DTE chains are wide
/// and bursting all legs at once overruns the account's market-data lines (so
/// many silently fail); fetch in bounded chunks instead.
const STRUCTURE_SNAPSHOT_CHUNK: usize = 16;

/// Everything an off-loop reload gathers from the broker + store, to be applied
/// to the `App` back on the event loop by `App::apply_live_data`. This is the
/// single connected-reload pipeline — the heavy broker I/O (chains, snapshots,
/// the tradability probe) never runs on the UI thread, and `App::reload`'s
/// synchronous fallback delegates here too rather than duplicating it.
pub struct LiveData {
    pub account: Option<AccountSnapshot>,
    pub watchlist: Vec<WatchlistRow>,
    pub journal: Vec<JournalRow>,
    pub positions: Vec<WheelPositionRow>,
    pub broker_positions: Vec<PositionRow>,
    pub suggestions: Vec<Suggestion>,
    /// Hedged Wheel suggestions (defined-risk put spreads), from the same fetch.
    pub hedged_suggestions: Vec<Suggestion>,
    /// 0DTE structure suggestions, one per configured quadrant slot.
    pub zerodte_suggestions: Vec<Option<Suggestion>>,
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
pub async fn gather(
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

    let (broker_positions, suggestions, hedged_suggestions, open_orders, positions_ok) = match ibkr.positions().await {
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
            let (suggestions, hedged_suggestions) =
                live_suggestions(ibkr, store, &watchlist, &positions, &working, cfg, today).await;
            (positions, suggestions, hedged_suggestions, open_orders, true)
        }
        Err(e) => {
            // Positions unknown — keep stored state, drop suggestions downstream.
            tracing::warn!("positions fetch failed; suggestions will be cleared, stored state kept: {e}");
            (Vec::new(), Vec::new(), Vec::new(), None, false)
        }
    };

    let journal = store.recent_journal(200).await.unwrap_or_default();
    let positions = store.list_positions().await.unwrap_or_default();

    // 0DTE structures don't depend on existing positions, but gate them on a
    // complete snapshot too so nothing executable shows while broker state is
    // unknown (mirrors how the wheel suggestions are cleared on a failed fetch).
    let zerodte_suggestions = if positions_ok {
        structure_suggestions(ibkr, &cfg.zerodte, cfg.engine.risk_free_rate, today).await
    } else {
        Vec::new()
    };

    LiveData {
        account,
        watchlist,
        journal,
        positions,
        broker_positions,
        suggestions,
        hedged_suggestions,
        zerodte_suggestions,
        open_orders,
        positions_ok,
    }
}

/// One ranked structure per 0DTE-tab slot (`None` where nothing fits), each from
/// a freshly-fetched both-sides index chain at the slot's target DTE. Slots are
/// fetched sequentially (each fans its strike snapshots out concurrently) to keep
/// the simultaneous market-data line count bounded.
pub(crate) async fn structure_suggestions(
    ibkr: &Ibkr,
    zerodte: &ZeroDteConfig,
    risk_free_rate: f64,
    today: NaiveDate,
) -> Vec<Option<Suggestion>> {
    let mut out = Vec::with_capacity(zerodte.slot_count());
    for i in 0..zerodte.slot_count() {
        let Some(p) = zerodte.slot(i) else {
            out.push(None);
            continue;
        };
        match gather_structure_chain(ibkr, &p.underlying, p.dte, today).await {
            Ok(Some((spot, quotes))) => {
                out.push(structures::select(
                    p,
                    &p.underlying,
                    spot,
                    &quotes,
                    today,
                    risk_free_rate,
                ));
            }
            Ok(None) => out.push(None),
            Err(e) => {
                tracing::warn!("0DTE chain for {} ({}): {e}", p.name, p.underlying);
                out.push(None);
            }
        }
    }
    out
}

/// Fetch spot + a both-sides (put & call) near-ATM chain at the target DTE for a
/// 0DTE structure. Unlike the wheel's one-right [`gather_chain_quotes`], this
/// quotes both rights (the structures straddle the money) and allows same-day
/// expiry (`min_dte = 0`).
async fn gather_structure_chain(
    ibkr: &Ibkr,
    symbol: &str,
    dte: i64,
    today: NaiveDate,
) -> Result<Option<(f64, Vec<OptionQuote>)>> {
    let conid = ibkr.underlying_contract_id(symbol).await?;
    let chain = ibkr.option_chain(symbol, conid).await?;
    if chain.expirations.is_empty() || chain.strikes.is_empty() {
        return Ok(None);
    }
    let Some((expiry_str, _)) = pick_expiry(&chain.expirations, today, dte, 0) else {
        return Ok(None); // no listed expiry at/after the target — skip this cycle
    };
    let expiry_date = NaiveDate::parse_from_str(&expiry_str, "%Y%m%d")?;

    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    if spot <= 0.0 {
        return Ok(None);
    }

    // Strikes within a tight band each side of spot, sampled across the range so
    // both shorts and their wings are covered (the selector adapts to whatever is
    // listed). Both rights are quoted at each sampled strike.
    let mut near: Vec<f64> = chain
        .strikes
        .iter()
        .copied()
        .filter(|k| *k > 0.0 && ((spot - *k).abs() / spot) <= STRUCTURE_OTM_MONEYNESS)
        .collect();
    near.sort_by(fcmp);
    let strikes = sample_spread(&near, MAX_STRUCTURE_STRIKES);
    if strikes.is_empty() {
        return Ok(None);
    }

    // (strike, right) request grid, fetched concurrently in bounded chunks so we
    // don't overrun the account's simultaneous market-data lines.
    let reqs: Vec<(f64, Right)> = strikes
        .iter()
        .flat_map(|&k| [Right::Put, Right::Call].into_iter().map(move |r| (k, r)))
        .collect();
    let mut snaps = Vec::with_capacity(reqs.len());
    for chunk in reqs.chunks(STRUCTURE_SNAPSHOT_CHUNK) {
        let part = futures::future::join_all(
            chunk
                .iter()
                .map(|(k, r)| ibkr.option_snapshot(symbol, &expiry_str, *k, r.code())),
        )
        .await;
        snaps.extend(part);
    }

    let mut quotes: Vec<OptionQuote> = Vec::with_capacity(reqs.len());
    for ((k, r), snap) in reqs.iter().zip(snaps) {
        if let Ok(snap) = snap
            && let Some(q) = quote_from_snapshot(&snap, *r, *k, expiry_date)
        {
            quotes.push(q);
        }
    }

    Ok(Some((spot, quotes)))
}

/// Build an [`OptionQuote`] from a snapshot at `(right, strike, expiry)`, or
/// `None` when the contract has no usable price this cycle. The single source for
/// the `model price → last` fallback and the bid==ask==price convention every
/// chain gatherer shares; greeks ride along from the snapshot, while
/// open-interest isn't in the snapshot feed so it's left empty.
fn quote_from_snapshot(
    snap: &SnapshotData,
    right: Right,
    strike: f64,
    expiry: NaiveDate,
) -> Option<OptionQuote> {
    let comp = snap.comp.as_ref()?;
    let price = comp.option_price.or(snap.last)?;
    if price <= 0.0 {
        return None;
    }
    Some(OptionQuote {
        right,
        strike,
        expiry,
        bid: price,
        ask: price,
        delta: comp.delta,
        implied_volatility: comp.implied_volatility,
        open_interest: None,
    })
}

/// One-shot price for an option leg (used to value a roll's new leg): model
/// price if present, else last, rounded to the cent. `None` if unpriced.
pub(crate) async fn price_leg(ibkr: &Ibkr, symbol: &str, expiry: &str, strike: f64, right: &str) -> Option<f64> {
    let snap = ibkr.option_snapshot(symbol, expiry, strike, right).await.ok()?;
    let price = snap.comp.as_ref().and_then(|c| c.option_price).or(snap.last)?;
    (price > 0.0).then(|| round_cents(price))
}

/// Reconcile broker holdings into the local `wheel_positions` store.
///
/// Every symbol that has a current holding *or* an already-tracked row is
/// re-derived (so a closed position falls back to `Idle`), preserving each
/// row's `cumulative_premium` (which the broker can't report).
pub(crate) async fn sync_wheel_state(store: &Store, broker_positions: &[PositionRow]) {
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
pub(crate) async fn probe_unknown_tradability(
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

    let Some((expiry, _)) = pick_expiry(&chain.expirations, today, 35, 1) else {
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
        s.sort_by(fcmp);
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
            fcmp(&(a - target).abs(), &(b - target).abs())
        })
}

/// Resolve the engine's *ideal* roll target (`to_expiry`, `to_strike`) to a
/// real **listed** contract and price it: nearest listed expiry to the ideal
/// DTE, nearest listed strike, and its current credit. `None` if the chain is
/// empty or the leg can't be priced. Without this, the target often falls on a
/// non-trading date and the order would reference a nonexistent contract.
pub(crate) async fn resolve_roll_target(
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
    let Some((expiry, _)) = pick_expiry(&chain.expirations, today, target_dte, 1) else {
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
pub(crate) async fn live_suggestions(
    ibkr: &Ibkr,
    store: &Store,
    watchlist: &[WatchlistRow],
    broker_positions: &[PositionRow],
    working: &[String],
    cfg: &Config,
    today: NaiveDate,
) -> (Vec<Suggestion>, Vec<Suggestion>) {
    let active: Vec<&WatchlistRow> = watchlist.iter().filter(|w| w.is_enabled()).collect();
    if active.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // Size new-entry collateral against the symbols actually eligible to open
    // one (enabled and not blocked); blocked symbols are managed only. Subtract
    // collateral already tied up in open short puts so the cap is a true ceiling
    // on *total* deployment, not just a per-pass budget — successive entries over
    // time can no longer creep past `max_total_deployed`.
    let openable = active.iter().filter(|w| w.tradable != Some(0)).count().max(1);
    let deployed = positions::deployed_put_collateral(broker_positions);
    let remaining = (cfg.guardrails.max_total_deployed - deployed).max(0.0);
    let budget = remaining / openable as f64;

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
        let manages_existing = matches!(
            reconciled.state,
            WheelState::ShortPut | WheelState::HedgedShortPut | WheelState::ShortCall
        );
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

    // A blocked symbol can be closed but not (re)opened, so drop rolls (which open
    // a new leg) for it; its buy-to-close take-profit still stands. Applied to both
    // the Classic and Hedged lists, which share these contexts.
    let drop_blocked_rolls = |list: &mut Vec<Suggestion>| {
        list.retain(|s| {
            !(matches!(s.kind, ActionKind::Roll { .. })
                && watchlist
                    .iter()
                    .any(|w| w.symbol == s.symbol && w.tradable == Some(0)))
        });
    };

    let mut classic = engine::plan(&contexts, &cfg.engine, today).suggestions;
    let mut hedged = engine::plan_hedged(&contexts, &cfg.engine, today).suggestions;
    drop_blocked_rolls(&mut classic);
    drop_blocked_rolls(&mut hedged);
    (classic, hedged)
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
        WheelState::ShortPut | WheelState::HedgedShortPut | WheelState::ShortCall => {
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
    let Some((expiry_str, _)) = pick_expiry(&chain.expirations, today, target_dte, 1) else {
        return Ok(None);
    };
    let expiry_date = NaiveDate::parse_from_str(&expiry_str, "%Y%m%d")?;

    let spot = ibkr.underlying_snapshot(symbol).await?.last.unwrap_or(0.0);
    if spot <= 0.0 {
        return Ok(None);
    }

    // Sample OTM strikes (within 15% of spot) spread *across* the range, not the
    // nearest-to-spot ones — those are all near-ATM/high-delta and miss the
    // wheel's target-delta band entirely. The engine then filters by delta.
    let mut otm: Vec<f64> = chain
        .strikes
        .iter()
        .copied()
        .filter(|k| {
            *k > 0.0 && is_otm(right, *k, spot) && moneyness(right, *k, spot) <= MAX_OTM_MONEYNESS
        })
        .collect();
    otm.sort_by(fcmp);
    let strikes = sample_spread(&otm, MAX_CHAIN_STRIKES);
    if strikes.is_empty() {
        return Ok(None);
    }

    // Fetch the strikes' snapshots concurrently: delayed data is slow, so a
    // serial loop would either drop quotes (short timeout) or take N × timeout.
    let right_code = right.code();
    let snaps = futures::future::join_all(
        strikes
            .iter()
            .map(|&k| ibkr.option_snapshot(symbol, &expiry_str, k, right_code)),
    )
    .await;

    let mut quotes: Vec<OptionQuote> = Vec::with_capacity(strikes.len());
    for (&k, snap) in strikes.iter().zip(snaps) {
        if let Ok(snap) = snap
            && let Some(q) = quote_from_snapshot(&snap, right, k, expiry_date)
        {
            quotes.push(q);
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
        .option_snapshot(symbol, &expiry_str, short.strike, short.right.code())
        .await?;
    let Some(quote) = quote_from_snapshot(&snap, short.right, short.strike, short.expiry) else {
        tracing::info!("{symbol}: open short unpriced this cycle; skipping management");
        return Ok(None);
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


/// Whether broker `positions` still hold a *short* option matching this leg.
/// `right` is an IBKR code (`"P"`/`"C"`); IBKR may report `right` as `PUT`/`CALL`
/// too, so we match on the leading letter.
pub(crate) fn position_has_short(
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

/// Pick the expiration closest to `target_dte` days out, considering only
/// expiries at least `min_dte` days away. The wheel passes `min_dte = 1` (never
/// same-day); the 0DTE path passes `0` so same-day expiries qualify.
fn pick_expiry(
    expirations: &[String],
    today: NaiveDate,
    target_dte: i64,
    min_dte: i64,
) -> Option<(String, i64)> {
    expirations
        .iter()
        .filter_map(|e| {
            NaiveDate::parse_from_str(e, "%Y%m%d")
                .ok()
                .map(|d| (e.clone(), (d - today).num_days()))
        })
        .filter(|(_, dte)| *dte >= min_dte)
        .min_by_key(|(_, dte)| (dte - target_dte).abs())
}

/// Sample up to `n` values spread evenly across a sorted slice. Taking the `n`
/// values nearest one end (e.g. strikes nearest spot) clusters the sample there;
/// an even spread keeps the whole range — and so the target-delta band — covered
/// regardless of strike increment. Returns the slice unchanged when it has `n`
/// or fewer elements.
fn sample_spread(sorted: &[f64], n: usize) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    if sorted.len() <= n {
        return sorted.to_vec();
    }
    if n == 1 {
        return vec![sorted[0]];
    }
    let last = sorted.len() - 1;
    let mut out: Vec<f64> = (0..n).map(|i| sorted[i * last / (n - 1)]).collect();
    out.dedup();
    out
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
        let (chosen, dte) = pick_expiry(&exps, today, 35, 1).expect("an expiry");
        assert_eq!(chosen, "20260703");
        assert_eq!(dte, 32);
    }

    #[test]
    fn pick_expiry_skips_past_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let exps = vec!["20260101".to_string(), "20260630".to_string()];
        let (chosen, _) = pick_expiry(&exps, today, 35, 1).expect("a future expiry");
        assert_eq!(chosen, "20260630");
    }

    #[test]
    fn pick_expiry_min_dte_zero_allows_today() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let exps = vec!["20260601".to_string(), "20260603".to_string()];
        // 0DTE path (min_dte = 0, target 0) selects today's expiry...
        let (same_day, dte) = pick_expiry(&exps, today, 0, 0).expect("a 0DTE expiry");
        assert_eq!(same_day, "20260601");
        assert_eq!(dte, 0);
        // ...while the wheel path (min_dte = 1) skips it for the next listed date.
        let (next, _) = pick_expiry(&exps, today, 0, 1).expect("a future expiry");
        assert_eq!(next, "20260603");
    }

    #[test]
    fn sample_spread_covers_the_whole_range() {
        let xs: Vec<f64> = (0..20).map(|i| i as f64).collect();
        let s = sample_spread(&xs, 5);
        assert_eq!(s.len(), 5);
        assert_eq!(s.first(), Some(&0.0));
        assert_eq!(s.last(), Some(&19.0)); // reaches the deep-OTM end
        assert!(s.iter().any(|&x| (8.0..=11.0).contains(&x))); // and the middle
        // Small inputs pass through; edge counts don't panic.
        assert_eq!(sample_spread(&[1.0, 2.0], 5), vec![1.0, 2.0]);
        assert_eq!(sample_spread(&[], 5), Vec::<f64>::new());
        assert_eq!(sample_spread(&[1.0, 2.0, 3.0], 1), vec![1.0]);
    }

    /// Read-only live smoke test of the full 0DTE path: connect → resolve the
    /// index → fetch a both-sides near-term chain (incl. SPXW dailies) → run the
    /// real selectors. Never transmits. Ignored by default; run against a live
    /// paper Gateway with:
    ///   cargo test --lib live_zerodte_smoke -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a running IB Gateway (read-only)"]
    async fn live_zerodte_smoke() {
        let cfg = Config::load(std::path::Path::new("config.toml")).expect("load config.toml");
        let ibkr = crate::ibkr::Ibkr::connect(&cfg.connection)
            .await
            .expect("connect to IB Gateway");
        let today = chrono::Local::now().date_naive();

        let out = structure_suggestions(&ibkr, &cfg.zerodte, cfg.engine.risk_free_rate, today).await;
        assert_eq!(out.len(), cfg.zerodte.slot_count());
        for (i, slot) in out.iter().enumerate() {
            let name = cfg.zerodte.slot(i).map(|q| q.name.as_str()).unwrap_or("?");
            match slot {
                Some(s) => eprintln!(
                    "slot {i} [{name}] {}DTE: credit ${:.0}  max loss ${:.0}  x{}",
                    s.dte, s.premium_total, s.capital_required, s.quantity
                ),
                None => eprintln!("slot {i} [{name}]: no structure fit"),
            }
        }
    }

    /// Read-only what-if of the first live structure as an N-leg combo: resolves
    /// every leg's contract and runs `.analyze()` (never transmits), confirming
    /// IBKR accepts the BAG and returns a margin ≈ the structure's max loss.
    ///   cargo test --lib live_combo_preview -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a running IB Gateway (read-only)"]
    async fn live_combo_preview() {
        use crate::engine::types::{ActionKind, LegSide};
        use crate::ibkr::{ComboLeg, ComboOrder, OrderOutcome, Side};

        let cfg = Config::load(std::path::Path::new("config.toml")).expect("config");
        let ibkr = crate::ibkr::Ibkr::connect(&cfg.connection).await.expect("connect");
        let today = chrono::Local::now().date_naive();
        let out = structure_suggestions(&ibkr, &cfg.zerodte, cfg.engine.risk_free_rate, today).await;
        let sug = out.iter().flatten().next().expect("at least one structure built");
        let ActionKind::OpenStructure { kind, legs } = &sug.kind else { panic!("not a structure") };
        eprintln!("previewing {} ×{}: {} legs, net credit {:.2}", kind.label(), sug.quantity, legs.len(), sug.limit_price);

        // Merge legs → combo legs (mirrors app::combo_legs_from).
        let mut combo: Vec<ComboLeg> = Vec::new();
        for l in legs {
            let right = l.right.code();
            let action = if l.side == LegSide::Sell { Side::Sell } else { Side::Buy };
            if let Some(c) = combo.iter_mut().find(|c| (c.strike - l.strike).abs() < 1e-6 && c.right == right && c.action == action) {
                c.ratio += 1;
            } else {
                combo.push(ComboLeg { strike: l.strike, right, action, ratio: 1 });
            }
        }
        for c in &combo {
            eprintln!("  {:?} {:.0}{} x{}", c.action, c.strike, c.right, c.ratio);
        }
        let expiry = sug.expiry.format("%Y%m%d").to_string();
        let order = ComboOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &expiry,
            legs: &combo,
            quantity: sug.quantity,
            net_credit: sug.limit_price,
        };
        match ibkr.submit_or_preview_combo(&order, true).await {
            Ok(OrderOutcome::Preview(state)) => eprintln!(
                "ENTRY PREVIEW OK — status={} init_margin={:?} commission={:?} (struct max loss ${:.0})",
                state.status, state.initial_margin_after, state.commission, sug.capital_required
            ),
            Ok(OrderOutcome::Submitted(_)) => panic!("preview unexpectedly transmitted!"),
            Err(e) => panic!("combo preview failed: {e}"),
        }

        // Also verify the profit-close: reverse every leg and buy the package back
        // at the 40%-profit debit (the exact construction the scheduler uses on an
        // entry fill). Read-only — confirms IBKR accepts the reversed BAG + sign.
        let close_legs: Vec<ComboLeg> = combo
            .iter()
            .map(|l| ComboLeg {
                action: if l.action == Side::Buy { Side::Sell } else { Side::Buy },
                ..*l
            })
            .collect();
        let close_debit = sug.limit_price * 0.60;
        let close = ComboOrder {
            symbol: &sug.symbol,
            expiry_yyyymmdd: &expiry,
            legs: &close_legs,
            quantity: sug.quantity,
            net_credit: -close_debit, // a debit close: negated into a +debit limit
        };
        match ibkr.submit_or_preview_combo(&close, true).await {
            Ok(OrderOutcome::Preview(state)) => eprintln!(
                "CLOSE PREVIEW OK — status={} (buy-to-close debit {:.2}, keeps {:.2} of {:.2})",
                state.status, close_debit, sug.limit_price - close_debit, sug.limit_price
            ),
            Ok(OrderOutcome::Submitted(_)) => panic!("close preview unexpectedly transmitted!"),
            Err(e) => panic!("close combo preview failed: {e}"),
        }

        // price_combo drives the stop-loss / time-stop checks: the cost to close a
        // just-built structure should be ≈ the credit just received.
        match ibkr.price_combo(&sug.symbol, &expiry, &combo).await {
            Ok(cost) => eprintln!(
                "PRICE_COMBO OK — cost-to-close {cost:.2} vs entry credit {:.2}",
                sug.limit_price
            ),
            Err(e) => panic!("price_combo failed: {e}"),
        }
    }
}
