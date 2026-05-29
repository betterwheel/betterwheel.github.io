//! Thin async wrapper over the `ibapi` client.
//!
//! This is the *only* place that talks to IB Gateway. It owns an
//! [`ibapi::Client`] and exposes the handful of high-level queries the wheel
//! app needs, mapping `ibapi` types into plain structs (and, later, into
//! [`crate::engine`] inputs). Streaming requests are bounded by timeouts so a
//! missing end-marker can never hang the caller.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use tokio::time::timeout;

use ibapi::accounts::types::AccountGroup;
use ibapi::contracts::{LegAction, OptionChain, OptionComputation};
use ibapi::orders::{OrderState, Orders};
use ibapi::prelude::*;

use crate::config::{ConnectionConfig, MarketDataPref};

/// Connected handle to IB Gateway / TWS.
#[derive(Clone)]
pub struct Ibkr {
    client: Arc<Client>,
}

/// Turn a [`Ibkr::connect`] failure into a short, actionable hint for the UI.
///
/// The disclaimer case is a heuristic: on a paper account, Gateway *resets the
/// API socket* (os error 54 / "reset by peer") immediately after sending error
/// 10141 when the simulated-trading disclaimer hasn't been accepted — so the
/// error that reaches us is the reset, not the code itself.
pub fn connect_failure_hint(err: &anyhow::Error) -> String {
    let low = err.to_string().to_ascii_lowercase();
    if low.contains("reset by peer") || low.contains("os error 54") {
        "IB Gateway reset the API session — on a paper account, accept the \
         simulated-trading disclaimer in Gateway/TWS (error 10141)"
            .into()
    } else if low.contains("refused") || low.contains("os error 61") {
        "IB Gateway not reachable — is it running with the API socket port open?".into()
    } else if low.contains("timed out") || low.contains("timeout") {
        "IB Gateway connection timed out — check host/port and that the API is enabled".into()
    } else {
        format!("connect failed: {err}")
    }
}

impl Ibkr {
    /// Connect to the configured Gateway and apply the market-data preference.
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        register_locale_timezone_aliases();
        let addr = cfg.address();
        let client = Client::connect(&addr, cfg.client_id)
            .await
            .map_err(|e| anyhow!("connect to {addr}: {e}"))?;
        let ibkr = Self { client: Arc::new(client) };
        ibkr.set_market_data(cfg.market_data).await?;
        Ok(ibkr)
    }

    /// Switch the session's market-data type (realtime vs delayed).
    pub async fn set_market_data(&self, pref: MarketDataPref) -> Result<()> {
        let t = match pref {
            MarketDataPref::Realtime => MarketDataType::Realtime,
            MarketDataPref::Frozen => MarketDataType::Frozen,
            MarketDataPref::Delayed => MarketDataType::Delayed,
            MarketDataPref::DelayedFrozen => MarketDataType::DelayedFrozen,
        };
        self.client
            .switch_market_data_type(t)
            .await
            .map_err(|e| anyhow!("switch_market_data_type: {e}"))
    }

    /// Snapshot of key account balances.
    pub async fn account_summary(&self) -> Result<AccountSnapshot> {
        let group = AccountGroup::from("All");
        let tags = [
            AccountSummaryTags::NET_LIQUIDATION,
            AccountSummaryTags::TOTAL_CASH_VALUE,
            AccountSummaryTags::BUYING_POWER,
            AccountSummaryTags::AVAILABLE_FUNDS,
        ];
        let mut sub = self
            .client
            .account_summary(&group, &tags)
            .await
            .map_err(|e| anyhow!("account_summary: {e}"))?;

        let mut snap = AccountSnapshot::default();
        let _ = timeout(Duration::from_secs(8), async {
            while let Some(item) = sub.next().await {
                match item {
                    Ok(AccountSummaryResult::Summary(s)) => snap.apply(&s.tag, &s.value),
                    Ok(AccountSummaryResult::End) => break,
                    Err(_) => break,
                }
            }
        })
        .await;
        Ok(snap)
    }

    /// All open positions in the account.
    ///
    /// Returns `Err` if the snapshot is *incomplete* (stream error or timeout
    /// before the `PositionEnd` marker) so callers can tell "the account holds
    /// nothing" (`Ok(vec![])`) apart from "we failed to read positions". This
    /// distinction is safety-critical: the wheel-state sync must never treat a
    /// failed fetch as "all positions closed".
    pub async fn positions(&self) -> Result<Vec<PositionRow>> {
        let mut sub = self
            .client
            .positions()
            .await
            .map_err(|e| anyhow!("positions: {e}"))?;

        let mut rows = Vec::new();
        let completed = timeout(Duration::from_secs(8), async {
            while let Some(item) = sub.next().await {
                match item {
                    Ok(PositionUpdate::Position(p)) => rows.push(PositionRow::from(&p)),
                    Ok(PositionUpdate::PositionEnd) => return true,
                    Err(_) => return false,
                }
            }
            false // stream closed before PositionEnd
        })
        .await;
        match completed {
            Ok(true) => Ok(rows),
            Ok(false) => Err(anyhow!("positions stream ended before completion")),
            Err(_) => Err(anyhow!("positions request timed out")),
        }
    }

    /// Snapshot of this client's currently open orders (id + underlying symbol).
    ///
    /// Authoritative "what's working right now": used to suppress stacking a
    /// second action on a symbol with a live order, and to reconcile pending
    /// rolls after a restart. `Err` on a timed-out snapshot so callers can fall
    /// back rather than treat "unknown" as "nothing open".
    pub async fn open_orders_snapshot(&self) -> Result<Vec<OpenOrderInfo>> {
        let mut sub = self
            .client
            .open_orders()
            .await
            .map_err(|e| anyhow!("open_orders: {e}"))?;

        let mut out = Vec::new();
        // The crate signals `OpenOrderEnd` by ending the stream (`next()` →
        // `None`); a `Some(Err(..))` is therefore a genuine mid-snapshot error.
        // Return `Err` for any incomplete result (error or timeout) so callers
        // fall back rather than treat a partial list as authoritative.
        let outcome = timeout(Duration::from_secs(8), async {
            loop {
                match sub.next().await {
                    Some(Ok(Orders::OrderData(d))) => out.push(OpenOrderInfo {
                        order_id: d.order_id.to_string(),
                        symbol: d.contract.symbol.to_string(),
                    }),
                    Some(Ok(_)) => {} // OrderStatus / Notice — not needed here
                    Some(Err(e)) => return Err(anyhow!("open_orders stream error: {e}")),
                    None => return Ok(()), // clean end (OpenOrderEnd)
                }
            }
        })
        .await;
        match outcome {
            Ok(Ok(())) => Ok(out),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("open_orders request timed out")),
        }
    }

    /// Resolve the underlying's IBKR contract id (needed for the option chain).
    pub async fn underlying_contract_id(&self, symbol: &str) -> Result<i32> {
        let contract = Contract::stock(symbol).build();
        let details = self
            .client
            .contract_details(&contract)
            .await
            .map_err(|e| anyhow!("contract_details {symbol}: {e}"))?;
        let cd = details
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no contract details for {symbol}"))?;
        Ok(cd.contract.contract_id)
    }

    /// Resolve a single option leg's IBKR contract id (`conid`). Combo legs are
    /// keyed by conid, so a spread order must resolve each leg first. Mirrors
    /// [`Self::underlying_contract_id`].
    pub async fn option_contract_id(
        &self,
        symbol: &str,
        expiry_yyyymmdd: &str,
        strike: f64,
        right: &str,
    ) -> Result<i32> {
        let contract = Contract::option(symbol, expiry_yyyymmdd, strike, right);
        let details = self
            .client
            .contract_details(&contract)
            .await
            .map_err(|e| anyhow!("contract_details {symbol} {strike}{right} {expiry_yyyymmdd}: {e}"))?;
        let cd = details
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no contract details for {symbol} {strike}{right} {expiry_yyyymmdd}"))?;
        Ok(cd.contract.contract_id)
    }

    /// Option-chain metadata (expirations + strikes) for an underlying.
    pub async fn option_chain(&self, symbol: &str, underlying_conid: i32) -> Result<ChainMeta> {
        let mut sub = self
            .client
            .option_chain(symbol, "", SecurityType::Stock, underlying_conid)
            .await
            .map_err(|e| anyhow!("option_chain {symbol}: {e}"))?;

        // The first result (typically the SMART aggregate) is enough here.
        let mut meta: Option<ChainMeta> = None;
        let _ = timeout(Duration::from_secs(15), async {
            while let Some(item) = sub.next().await {
                if let Ok(chain) = item {
                    meta = Some(ChainMeta::from(chain));
                    break;
                }
            }
        })
        .await;
        meta.ok_or_else(|| anyhow!("no option chain returned for {symbol}"))
    }

    /// Snapshot greeks + price for one option contract.
    pub async fn option_snapshot(
        &self,
        symbol: &str,
        expiry_yyyymmdd: &str,
        strike: f64,
        right: &str,
    ) -> Result<SnapshotData> {
        let contract = Contract::option(symbol, expiry_yyyymmdd, strike, right);
        // 100=option volume, 101=option open interest, 106=implied vol/greeks.
        // 18s (not 10): free *delayed* greeks can take well over 10s to compute,
        // and dropping them leaves the engine with no in-band quote to rank.
        self.collect_snapshot(&contract, &["100", "101", "106"], Duration::from_secs(18))
            .await
    }

    /// Snapshot the underlying's price.
    pub async fn underlying_snapshot(&self, symbol: &str) -> Result<SnapshotData> {
        let contract = Contract::stock(symbol).build();
        self.collect_snapshot(&contract, &[], Duration::from_secs(8))
            .await
    }

    /// Run a *what-if* preview of selling a put — returns margin/commission
    /// impact without transmitting. Doubles as the EU/PRIIPs tradability probe:
    /// a permission/PRIIPs rejection surfaces as an `Err`.
    pub async fn preview_sell_put(
        &self,
        symbol: &str,
        expiry_yyyymmdd: &str,
        strike: f64,
        quantity: i32,
        limit: f64,
    ) -> Result<OrderState> {
        let contract = Contract::option(symbol, expiry_yyyymmdd, strike, "P");
        let analyze = self
            .client
            .order(&contract)
            .sell(quantity)
            .limit(limit)
            .day_order()
            .analyze();
        // Bound the what-if (never transmits): a read-only Gateway can otherwise
        // leave it pending forever, so a timeout surfaces as a clear Blocked.
        timeout(Duration::from_secs(10), analyze)
            .await
            .map_err(|_| anyhow!("what-if timed out (Gateway API may be read-only)"))?
            .map_err(|e| anyhow!("what-if analyze: {e}"))
    }

    /// Classify whether a far-OTM put on `symbol` can be sold by this account.
    pub async fn tradability(&self, symbol: &str, expiry_yyyymmdd: &str, strike: f64) -> Tradability {
        match self.preview_sell_put(symbol, expiry_yyyymmdd, strike, 1, 0.01).await {
            Ok(state) => Tradability::Allowed {
                init_margin: state.initial_margin_after,
                commission: state.commission,
            },
            Err(e) => Tradability::Blocked(e.to_string()),
        }
    }

    /// Submit (transmit) or preview (what-if) a single-leg option order. A
    /// single entry point so the TUI's preview/execute paths can't drift apart.
    pub async fn submit_or_preview(
        &self,
        order: &OptionOrder<'_>,
        preview: bool,
    ) -> Result<OrderOutcome> {
        let contract =
            Contract::option(order.symbol, order.expiry_yyyymmdd, order.strike, order.right);
        let builder = self.client.order(&contract);
        let sided = match order.side {
            Side::Buy => builder.buy(order.quantity),
            Side::Sell => builder.sell(order.quantity),
        };
        let ready = sided.limit(order.limit).day_order();
        if preview {
            // Bound the what-if: it never transmits, and a read-only Gateway can
            // otherwise leave `analyze()` pending indefinitely.
            let state = timeout(Duration::from_secs(10), ready.analyze())
                .await
                .map_err(|_| anyhow!("preview {order}: timed out (is the Gateway API read-only?)"))?
                .map_err(|e| anyhow!("preview {order}: {e}"))?;
            Ok(OrderOutcome::Preview(Box::new(state)))
        } else {
            let id = ready
                .submit()
                .await
                .map_err(|e| anyhow!("submit {order}: {e}"))?;
            // Store the bare numeric id so it matches `OrderStatus.order_id`
            // from the update stream (see `stream_order_events`).
            Ok(OrderOutcome::Submitted(id.0.to_string()))
        }
    }

    /// Submit (transmit) or preview (what-if) a **defined-risk vertical put
    /// spread** as a single atomic combo (BAG) order — the Hedged Wheel's entry.
    /// Single entry point, mirroring [`Self::submit_or_preview`], so the TUI's
    /// preview/execute paths can't drift apart.
    ///
    /// The combo is `[Buy long, Sell short]`; **buying** that package opens a put
    /// credit spread (buy the cheaper protective put, sell the nearer put). IBKR's
    /// combo limit is a net *debit*, so a credit is submitted as a **negative**
    /// price (`-net_credit`). Always preview against a live Gateway and sanity-
    /// check the returned margin (≈ the spread width) before any live submit.
    pub async fn submit_or_preview_spread(
        &self,
        order: &SpreadOrder<'_>,
        preview: bool,
    ) -> Result<OrderOutcome> {
        // Combo legs are keyed by contract id, so resolve both legs first.
        let long_id = self
            .option_contract_id(order.symbol, order.expiry_yyyymmdd, order.long_strike, "P")
            .await?;
        let short_id = self
            .option_contract_id(order.symbol, order.expiry_yyyymmdd, order.short_strike, "P")
            .await?;
        if long_id == short_id {
            return Err(anyhow!(
                "spread {} {:.1}/{:.1}: both legs resolved to the same contract",
                order.symbol,
                order.short_strike,
                order.long_strike
            ));
        }

        // BAG contract. `vertical()` would leave each leg's exchange empty; set it
        // to SMART explicitly. `build()` leaves the symbol blank — IBKR needs the
        // underlying symbol on the bag, so set it after.
        let mut contract = Contract::spread()
            .add_leg(long_id, LegAction::Buy)
            .on_exchange("SMART")
            .done()
            .add_leg(short_id, LegAction::Sell)
            .on_exchange("SMART")
            .done()
            .build()
            .map_err(|e| anyhow!("build spread {}: {e}", order.symbol))?;
        contract.symbol = order.symbol.into();

        // Credit convention: buy the package at a negative net price = receive credit.
        let combo_limit = -order.net_credit;
        let ready = self
            .client
            .order(&contract)
            .buy(order.quantity)
            .limit(combo_limit)
            .day_order();
        if preview {
            let state = timeout(Duration::from_secs(10), ready.analyze())
                .await
                .map_err(|_| anyhow!("preview {order}: timed out (is the Gateway API read-only?)"))?
                .map_err(|e| anyhow!("preview {order}: {e}"))?;
            Ok(OrderOutcome::Preview(Box::new(state)))
        } else {
            let id = ready
                .submit()
                .await
                .map_err(|e| anyhow!("submit {order}: {e}"))?;
            Ok(OrderOutcome::Submitted(id.0.to_string()))
        }
    }

    /// Consume the account-wide order-activity stream, forwarding each update as
    /// a plain [`OrderEvent`] over `tx`. Runs until the receiver is dropped.
    ///
    /// The crate auto-reconnects the socket but does *not* auto-resubscribe, so
    /// on a stream end we drop the subscription and resubscribe after a short
    /// pause. Returns when `tx` is closed (UI gone) or a fresh subscription
    /// cannot be established.
    pub async fn stream_order_events(&self, tx: mpsc::UnboundedSender<OrderEvent>) {
        loop {
            match self.client.order_update_stream().await {
                Ok(mut sub) => {
                    while let Some(item) = sub.next().await {
                        match item {
                            Ok(update) => {
                                if let Some(ev) = map_order_update(update)
                                    && tx.send(ev).is_err()
                                {
                                    return; // receiver dropped — UI is gone
                                }
                            }
                            Err(e) => {
                                tracing::warn!("order stream error: {e}");
                                break;
                            }
                        }
                    }
                    // Stream ended; `sub` drops here, releasing the subscription
                    // before we resubscribe below.
                }
                Err(e) => {
                    // A (re)subscribe failure must NOT permanently kill event
                    // delivery — the Gateway may be briefly restarting. Notify,
                    // back off, and retry rather than returning.
                    tracing::warn!("order stream subscribe failed: {e}");
                    let _ = tx.send(OrderEvent::Notice(format!("order stream reconnecting: {e}")));
                }
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            if tx.is_closed() {
                return;
            }
        }
    }

    /// Collect a market-data snapshot for a contract.
    ///
    /// IBKR rejects a *snapshot* that also requests generic ticks (e.g. option
    /// greeks via tick 106) — "snapshot not applicable to generic ticks" — so for
    /// greeks we open a brief *streaming* subscription and stop as soon as the
    /// first option computation arrives. Price-only requests (no generic ticks,
    /// e.g. the underlying) use a cheaper one-shot snapshot.
    async fn collect_snapshot(
        &self,
        contract: &Contract,
        generic_ticks: &[&str],
        wait: Duration,
    ) -> Result<SnapshotData> {
        let wants_greeks = !generic_ticks.is_empty();
        let builder = self.client.market_data(contract).generic_ticks(generic_ticks);
        let mut sub = if wants_greeks {
            builder.subscribe().await
        } else {
            builder.snapshot().subscribe().await
        }
        .map_err(|e| anyhow!("market_data subscribe: {e}"))?;

        let mut data = SnapshotData::default();
        let _ = timeout(wait, async {
            while let Some(item) = sub.next().await {
                match item {
                    // We match generically on price so this works under both
                    // realtime and delayed tick types.
                    Ok(TickTypes::Price(p)) if p.price > 0.0 => data.last = Some(p.price),
                    Ok(TickTypes::PriceSize(ps)) if ps.price > 0.0 => data.last = Some(ps.price),
                    Ok(TickTypes::OptionComputation(c)) => {
                        if c.delta.is_some() || c.implied_volatility.is_some() {
                            data.comp = Some(c);
                        }
                    }
                    Ok(TickTypes::SnapshotEnd) => break,
                    Ok(TickTypes::Notice(n)) => data.notices.push(format!("{n:?}")),
                    Ok(_) => {}
                    Err(e) => {
                        data.notices.push(format!("error: {e}"));
                        break;
                    }
                }
                // Streaming has no end marker — stop as soon as we have what we
                // came for (a computation for greeks, else a price) instead of
                // waiting out the full timeout on every contract.
                let enough = if wants_greeks { data.comp.is_some() } else { data.last.is_some() };
                if enough {
                    break;
                }
            }
        })
        .await;
        // Dropping `sub` cancels any streaming subscription.
        Ok(data)
    }
}

/// IB Gateway reports its timezone using the OS locale's *display name*, but
/// `ibapi` only recognizes English/IANA names — so a non-English Gateway fails
/// the handshake with "unrecognized IB Gateway timezone". Register the localized
/// Central-European names we've encountered (Italian) so connecting works
/// regardless of Gateway language. Registration is process-wide and idempotent.
///
/// `Europe/Rome` is CET/CEST (UTC+1 / +2 with EU DST) — identical offsets to the
/// crate's built-in `Central European Standard Time` mapping. Alternatives:
/// set `IBAPI_TIMEZONE_ALIASES`, or switch the Gateway UI to English.
fn register_locale_timezone_aliases() {
    // Italian: "Central European {Standard,Summer} Time". The apostrophe is a
    // curly U+2019 (’), exactly as the Gateway sends it.
    ibapi::register_timezone_alias("Ora standard dell’Europa centrale", "Europe/Rome");
    ibapi::register_timezone_alias("Ora legale dell’Europa centrale", "Europe/Rome");
}

/// Parsed account balances (each `None` until reported).
#[derive(Debug, Default, Clone, Copy)]
pub struct AccountSnapshot {
    pub net_liquidation: Option<f64>,
    pub total_cash: Option<f64>,
    pub buying_power: Option<f64>,
    pub available_funds: Option<f64>,
}

impl AccountSnapshot {
    fn apply(&mut self, tag: &str, value: &str) {
        let v = value.parse::<f64>().ok();
        if tag == AccountSummaryTags::NET_LIQUIDATION {
            self.net_liquidation = v;
        } else if tag == AccountSummaryTags::TOTAL_CASH_VALUE {
            self.total_cash = v;
        } else if tag == AccountSummaryTags::BUYING_POWER {
            self.buying_power = v;
        } else if tag == AccountSummaryTags::AVAILABLE_FUNDS {
            self.available_funds = v;
        }
    }
}

/// A flattened open position.
#[derive(Debug, Clone)]
pub struct PositionRow {
    pub account: String,
    pub symbol: String,
    /// Debug spelling of the `ibapi` security type, e.g. `"Stock"` / `"Option"`.
    pub security_type: String,
    pub right: String,
    pub strike: f64,
    pub expiry: String,
    pub position: f64,
    pub average_cost: f64,
    /// Contract multiplier as IBKR reports it (e.g. `"100"` for an equity
    /// option); empty for stock. Needed to recover per-share option premium.
    pub multiplier: String,
}

impl From<&ibapi::accounts::Position> for PositionRow {
    fn from(p: &ibapi::accounts::Position) -> Self {
        Self {
            account: p.account.clone(),
            symbol: p.contract.symbol.to_string(),
            security_type: format!("{:?}", p.contract.security_type),
            right: p.contract.right.clone(),
            strike: p.contract.strike,
            expiry: p.contract.last_trade_date_or_contract_month.clone(),
            position: p.position,
            average_cost: p.average_cost,
            multiplier: p.contract.multiplier.clone(),
        }
    }
}

/// Option-chain metadata for one underlying/exchange.
#[derive(Debug, Default, Clone)]
pub struct ChainMeta {
    pub underlying_contract_id: i32,
    pub exchange: String,
    pub multiplier: String,
    pub trading_class: String,
    pub expirations: Vec<String>,
    pub strikes: Vec<f64>,
}

impl From<OptionChain> for ChainMeta {
    fn from(c: OptionChain) -> Self {
        Self {
            underlying_contract_id: c.underlying_contract_id,
            exchange: c.exchange,
            multiplier: c.multiplier,
            trading_class: c.trading_class,
            expirations: c.expirations,
            strikes: c.strikes,
        }
    }
}

/// Result of a market-data snapshot. (Not `Clone`: `OptionComputation` isn't.)
#[derive(Debug, Default)]
pub struct SnapshotData {
    pub last: Option<f64>,
    pub comp: Option<OptionComputation>,
    pub notices: Vec<String>,
}

/// Whether the account may trade a given underlying's options.
#[derive(Debug)]
pub enum Tradability {
    Allowed {
        init_margin: Option<f64>,
        commission: Option<f64>,
    },
    Blocked(String),
}

/// Order side for [`Ibkr::submit_or_preview`].
#[derive(Debug, Clone, Copy)]
pub enum Side {
    Buy,
    Sell,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        })
    }
}

/// A single-leg option order request (preview or live). Borrows its strings so
/// callers can build one cheaply from a [`crate::engine::Suggestion`].
#[derive(Debug, Clone, Copy)]
pub struct OptionOrder<'a> {
    pub symbol: &'a str,
    /// Expiry as `YYYYMMDD`.
    pub expiry_yyyymmdd: &'a str,
    pub strike: f64,
    /// IBKR right code: `"P"` or `"C"`.
    pub right: &'a str,
    pub side: Side,
    pub quantity: i32,
    /// Limit price (premium per share).
    pub limit: f64,
}

impl std::fmt::Display for OptionOrder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}x {} {:.1}{} {} @{:.2}",
            self.side, self.quantity, self.symbol, self.strike, self.right, self.expiry_yyyymmdd, self.limit
        )
    }
}

/// A defined-risk vertical **put credit spread** order (the Hedged Wheel's
/// entry): sell `short_strike`, buy `long_strike` (further OTM) for protection,
/// submitted as one combo. Borrows its strings like [`OptionOrder`].
#[derive(Debug, Clone, Copy)]
pub struct SpreadOrder<'a> {
    pub symbol: &'a str,
    /// Expiry as `YYYYMMDD` (both legs share it).
    pub expiry_yyyymmdd: &'a str,
    /// The nearer put we sell.
    pub short_strike: f64,
    /// The further-OTM put we buy as protection (`< short_strike`).
    pub long_strike: f64,
    pub quantity: i32,
    /// Net credit per share to receive (positive); submitted as a negative combo
    /// limit per IBKR's credit convention.
    pub net_credit: f64,
}

impl std::fmt::Display for SpreadOrder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SELL {}x {} {:.1}/{:.1}P {} @{:.2}cr",
            self.quantity,
            self.symbol,
            self.short_strike,
            self.long_strike,
            self.expiry_yyyymmdd,
            self.net_credit
        )
    }
}

/// One of the account's currently open orders (id + underlying symbol).
#[derive(Debug, Clone)]
pub struct OpenOrderInfo {
    pub order_id: String,
    pub symbol: String,
}

/// A plain, broker-agnostic order-activity event mapped from `ibapi`'s
/// `OrderUpdate`. Only the variants the wheel app acts on are carried.
#[derive(Debug, Clone)]
pub enum OrderEvent {
    /// A status transition for an order, keyed by its numeric id (matches the
    /// id stored on submission). `status` is IBKR's raw status string.
    Status {
        order_id: i32,
        status: String,
        filled: f64,
        remaining: f64,
        avg_fill_price: f64,
    },
    /// A notice or error message from the order subsystem.
    Notice(String),
}

/// Map an `ibapi` order update into an [`OrderEvent`], or `None` for updates the
/// app doesn't act on (open-order snapshots, executions, commission reports —
/// fills are already conveyed by `OrderStatus`).
fn map_order_update(update: ibapi::orders::OrderUpdate) -> Option<OrderEvent> {
    use ibapi::orders::OrderUpdate;
    match update {
        OrderUpdate::OrderStatus(s) => Some(OrderEvent::Status {
            order_id: s.order_id,
            status: s.status,
            filled: s.filled,
            remaining: s.remaining,
            avg_fill_price: s.average_fill_price,
        }),
        OrderUpdate::Message(n) => {
            // Connectivity/system warnings, cancel confirmations, and contract /
            // market-data lookup misses are non-actionable — log them but keep
            // them off the status line. Surface the rest as a clean "[code] text".
            if notice_is_noise(n.code) {
                tracing::debug!("broker notice [{}]: {}", n.code, n.message);
                None
            } else {
                tracing::info!("broker notice [{}]: {}", n.code, n.message);
                Some(OrderEvent::Notice(format!("[{}] {}", n.code, n.message)))
            }
        }
        OrderUpdate::OpenOrder(_)
        | OrderUpdate::ExecutionData(_)
        | OrderUpdate::CommissionReport(_) => None,
    }
}

/// Whether a broker notice is non-actionable for the wheel app, so it's logged
/// but kept off the status line. Covers ibapi's connectivity/system + warning
/// tiers, order-cancel confirmations (the cancel itself arrives via `OrderStatus`),
/// and the lookup/market-data codes expected while probing chains and snapshots.
fn notice_is_noise(code: i32) -> bool {
    use ibapi::messages::{ORDER_CANCELLED_CODE, SYSTEM_MESSAGE_CODES, WARNING_CODE_RANGE};
    WARNING_CODE_RANGE.contains(&code)
        || SYSTEM_MESSAGE_CODES.contains(&code)
        || code == ORDER_CANCELLED_CODE
        // 200: no security definition found; 10091/10167: delayed / unsubscribed
        // market data — all expected noise during chain & snapshot requests.
        || matches!(code, 200 | 10091 | 10167)
}

/// Result of [`Ibkr::submit_or_preview`].
#[derive(Debug)]
pub enum OrderOutcome {
    /// What-if margin / commission impact. Boxed: `OrderState` is large.
    Preview(Box<OrderState>),
    /// Live submission succeeded; carries the IBKR order id (formatted).
    Submitted(String),
}

#[cfg(test)]
mod tests {
    use super::{connect_failure_hint, notice_is_noise};
    use anyhow::anyhow;

    #[test]
    fn notice_noise_filters_only_benign_codes() {
        // Connectivity/system + warning tiers, cancel confirmation, and the
        // probe-time lookup / market-data codes are noise.
        for code in [1100, 1102, 1300, 2100, 2104, 2158, 2169, 202, 200, 10091, 10167] {
            assert!(notice_is_noise(code), "expected {code} to be filtered");
        }
        // Real, actionable errors (e.g. order rejections) must still surface.
        for code in [201, 203, 321, 2099, 2170, 10147] {
            assert!(!notice_is_noise(code), "expected {code} to surface");
        }
    }

    #[test]
    fn connect_hint_flags_paper_disclaimer_on_reset() {
        // Gateway resets the socket right after error 10141 when the paper
        // disclaimer hasn't been accepted, so what reaches us is the reset.
        let hint = connect_failure_hint(&anyhow!(
            "connect to 127.0.0.1:4002: Connection reset by peer (os error 54)"
        ));
        assert!(hint.contains("10141"), "got: {hint}");
        assert!(
            hint.to_ascii_lowercase().contains("disclaimer"),
            "got: {hint}"
        );
    }

    #[test]
    fn connect_hint_flags_refused_as_not_running() {
        let hint = connect_failure_hint(&anyhow!(
            "connect to 127.0.0.1:4002: Connection refused (os error 61)"
        ));
        assert!(
            hint.to_ascii_lowercase().contains("not reachable"),
            "got: {hint}"
        );
    }

    #[test]
    fn connect_hint_falls_back_to_raw_error() {
        let hint = connect_failure_hint(&anyhow!("some weird handshake failure"));
        assert!(hint.contains("some weird handshake failure"), "got: {hint}");
    }
}
