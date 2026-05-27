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
use tokio::time::timeout;

use ibapi::accounts::types::AccountGroup;
use ibapi::contracts::{OptionChain, OptionComputation};
use ibapi::orders::OrderState;
use ibapi::prelude::*;

use crate::config::{ConnectionConfig, MarketDataPref};

/// Connected handle to IB Gateway / TWS.
#[derive(Clone)]
pub struct Ibkr {
    client: Arc<Client>,
}

impl Ibkr {
    /// Connect to the configured Gateway and apply the market-data preference.
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
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
    pub async fn positions(&self) -> Result<Vec<PositionRow>> {
        let mut sub = self
            .client
            .positions()
            .await
            .map_err(|e| anyhow!("positions: {e}"))?;

        let mut rows = Vec::new();
        let _ = timeout(Duration::from_secs(8), async {
            while let Some(item) = sub.next().await {
                match item {
                    Ok(PositionUpdate::Position(p)) => rows.push(PositionRow::from(&p)),
                    Ok(PositionUpdate::PositionEnd) => break,
                    Err(_) => break,
                }
            }
        })
        .await;
        Ok(rows)
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
        self.collect_snapshot(&contract, &["100", "101", "106"], Duration::from_secs(10))
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
        self.client
            .order(&contract)
            .sell(quantity)
            .limit(limit)
            .day_order()
            .analyze()
            .await
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

    /// Collect a one-shot market-data snapshot for a contract.
    async fn collect_snapshot(
        &self,
        contract: &Contract,
        generic_ticks: &[&str],
        wait: Duration,
    ) -> Result<SnapshotData> {
        let mut sub = self
            .client
            .market_data(contract)
            .generic_ticks(generic_ticks)
            .snapshot()
            .subscribe()
            .await
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
            }
        })
        .await;
        Ok(data)
    }
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
    pub security_type: String,
    pub right: String,
    pub strike: f64,
    pub expiry: String,
    pub position: f64,
    pub average_cost: f64,
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
