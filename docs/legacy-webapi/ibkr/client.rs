//! Authenticated IBKR Web API client (first-party OAuth 2.0).
//!
//! Talks directly to IBKR's hosted REST host — there is no local gateway/port.
//! Holds the access token + brokerage session and exposes the queries the wheel
//! needs. Response shapes are parsed leniently (see [`super::models`]).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use super::auth;
use super::models::{
    AccountSnapshot, ContractMatch, OptionQuoteSnap, OrderPreview, PositionRow, Tradability,
    dig_money, lenient_f64,
};
use crate::config::{ConnectionConfig, FieldCodes, OAuthConfig};

/// A connected, authenticated Web API session.
pub struct WebApi {
    http: reqwest::Client,
    base_url: String,
    token_url: String,
    oauth: OAuthConfig,
    fields: FieldCodes,
    access_token: String,
    token_expires_at: Instant,
    account_id: Option<String>,
}

impl WebApi {
    /// Authenticate and open a brokerage session.
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        if !cfg.oauth.is_configured() {
            return Err(anyhow!(
                "OAuth is not configured — set [connection.oauth] client_id/kid/credential and the private key (see SETUP.md)"
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("thewheel/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?;

        let mut api = Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            token_url: cfg.token_url(),
            oauth: cfg.oauth.clone(),
            fields: cfg.fields,
            access_token: String::new(),
            token_expires_at: Instant::now(),
            account_id: cfg.account.clone(),
        };

        api.refresh_token().await?;
        api.init_session().await?;
        if api.account_id.is_none() {
            api.account_id = api.first_account().await?;
        }
        Ok(api)
    }

    /// The resolved account id, or an error if none is known yet.
    pub fn account(&self) -> Result<&str> {
        self.account_id
            .as_deref()
            .ok_or_else(|| anyhow!("no account id resolved"))
    }

    // --- session management ---

    async fn refresh_token(&mut self) -> Result<()> {
        let tok = auth::fetch_access_token(&self.http, &self.oauth, &self.token_url).await?;
        let ttl = tok.expires_in.unwrap_or(3600).saturating_sub(60).max(30);
        self.access_token = tok.access_token;
        self.token_expires_at = Instant::now() + Duration::from_secs(ttl);
        Ok(())
    }

    async fn ensure_token(&mut self) -> Result<()> {
        if Instant::now() >= self.token_expires_at {
            self.refresh_token().await?;
        }
        Ok(())
    }

    /// Open the brokerage session needed for `/iserver` endpoints.
    pub async fn init_session(&mut self) -> Result<()> {
        self.post("/iserver/auth/ssodh/init", &json!({"publish": true, "compete": true}))
            .await
            .context("ssodh/init (opening brokerage session)")?;
        // A tickle confirms the session and primes it.
        let _ = self.tickle().await;
        Ok(())
    }

    /// Keep-alive ping; should be called at least every ~60s by a background task.
    pub async fn tickle(&mut self) -> Result<Value> {
        self.get("/tickle", &[]).await
    }

    /// Current authentication status.
    pub async fn auth_status(&mut self) -> Result<Value> {
        self.post("/iserver/auth/status", &json!({})).await
    }

    // --- low-level authed requests ---

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get(&mut self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.ensure_token().await?;
        let resp = self
            .http
            .get(self.url(path))
            .query(query)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        Self::parse(path, resp).await
    }

    async fn post(&mut self, path: &str, body: &Value) -> Result<Value> {
        self.ensure_token().await?;
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.access_token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        Self::parse(path, resp).await
    }

    async fn delete(&mut self, path: &str) -> Result<Value> {
        self.ensure_token().await?;
        let resp = self
            .http
            .delete(self.url(path))
            .bearer_auth(&self.access_token)
            .send()
            .await
            .with_context(|| format!("DELETE {path}"))?;
        Self::parse(path, resp).await
    }

    async fn parse(path: &str, resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("{path} returned {status}: {body}"));
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str::<Value>(&body).with_context(|| format!("parsing {path} response: {body}"))
    }

    // --- accounts & portfolio ---

    /// First brokerage account id reported by the session.
    pub async fn first_account(&mut self) -> Result<Option<String>> {
        let v = self.get("/iserver/accounts", &[]).await?;
        // Shape: { "accounts": ["DU123", ...], "selectedAccount": "DU123" }
        if let Some(sel) = v.get("selectedAccount").and_then(Value::as_str) {
            return Ok(Some(sel.to_string()));
        }
        let first = v
            .get("accounts")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(first)
    }

    /// Net liquidation / cash / buying power / available funds.
    pub async fn account_summary(&mut self) -> Result<AccountSnapshot> {
        let acct = self.account()?.to_string();
        let v = self
            .get(&format!("/iserver/account/{acct}/summary"), &[])
            .await?;
        Ok(AccountSnapshot {
            net_liquidation: dig_money(&v, "netliquidation"),
            total_cash: dig_money(&v, "totalcash"),
            buying_power: dig_money(&v, "buyingpower"),
            available_funds: dig_money(&v, "availablefunds"),
        })
    }

    /// Open positions (first page).
    pub async fn positions(&mut self) -> Result<Vec<PositionRow>> {
        let acct = self.account()?.to_string();
        let v = self
            .get(&format!("/portfolio/{acct}/positions/0"), &[])
            .await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        let rows = arr
            .iter()
            .filter_map(|p| {
                let conid = p.get("conid").and_then(Value::as_i64)?;
                Some(PositionRow {
                    conid,
                    symbol: p
                        .get("ticker")
                        .or_else(|| p.get("contractDesc"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    asset_class: p
                        .get("assetClass")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    position: p.get("position").and_then(lenient_f64).unwrap_or(0.0),
                    avg_price: p
                        .get("avgPrice")
                        .or_else(|| p.get("avgCost"))
                        .and_then(lenient_f64)
                        .unwrap_or(0.0),
                    mkt_price: p.get("mktPrice").and_then(lenient_f64).unwrap_or(0.0),
                    mkt_value: p.get("mktValue").and_then(lenient_f64).unwrap_or(0.0),
                    unrealized_pnl: p.get("unrealizedPnl").and_then(lenient_f64).unwrap_or(0.0),
                })
            })
            .collect();
        Ok(rows)
    }

    // --- contracts & option chain ---

    /// Search for a symbol's contracts (stocks/ETFs).
    pub async fn search_contract(&mut self, symbol: &str) -> Result<Vec<ContractMatch>> {
        let v = self
            .get(
                "/iserver/secdef/search",
                &[("symbol", symbol.to_string()), ("secType", "STK".to_string())],
            )
            .await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .filter_map(|m| {
                Some(ContractMatch {
                    conid: m.get("conid").and_then(value_as_i64)?,
                    symbol: m.get("symbol").and_then(Value::as_str).unwrap_or(symbol).to_string(),
                    description: m
                        .get("description")
                        .or_else(|| m.get("companyHeader"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    sec_type: m.get("secType").and_then(Value::as_str).unwrap_or("STK").to_string(),
                })
            })
            .collect())
    }

    /// Underlying conid for a stock/ETF symbol (first match).
    pub async fn underlying_conid(&mut self, symbol: &str) -> Result<i64> {
        self.search_contract(symbol)
            .await?
            .into_iter()
            .next()
            .map(|m| m.conid)
            .ok_or_else(|| anyhow!("no contract found for {symbol}"))
    }

    /// Available (call, put) strike lists for an underlying in a given month (YYYYMM).
    pub async fn strikes(&mut self, conid: i64, month: &str) -> Result<(Vec<f64>, Vec<f64>)> {
        let v = self
            .get(
                "/iserver/secdef/strikes",
                &[
                    ("conid", conid.to_string()),
                    ("sectype", "OPT".to_string()),
                    ("month", month.to_string()),
                ],
            )
            .await?;
        let calls = number_array(v.get("call"));
        let puts = number_array(v.get("put"));
        Ok((calls, puts))
    }

    /// Resolve a specific option's conid.
    pub async fn option_conid(
        &mut self,
        underlying_conid: i64,
        month: &str,
        strike: f64,
        right: &str,
    ) -> Result<i64> {
        let v = self
            .get(
                "/iserver/secdef/info",
                &[
                    ("conid", underlying_conid.to_string()),
                    ("sectype", "OPT".to_string()),
                    ("month", month.to_string()),
                    ("strike", trim_float(strike)),
                    ("right", right.to_string()),
                ],
            )
            .await?;
        v.as_array()
            .and_then(|a| a.first())
            .and_then(|o| o.get("conid"))
            .and_then(value_as_i64)
            .ok_or_else(|| anyhow!("no option conid for {underlying_conid} {month} {strike}{right}"))
    }

    // --- market data ---

    /// Snapshot greeks + quote for one option conid. Calls twice because IBKR's
    /// first snapshot is often sparse.
    pub async fn option_snapshot(&mut self, conid: i64) -> Result<OptionQuoteSnap> {
        let fields = self.fields;
        let csv = fields.csv();
        let query = [("conids", conid.to_string()), ("fields", csv)];
        let _ = self.get("/iserver/marketdata/snapshot", &query).await; // prime
        let v = self.get("/iserver/marketdata/snapshot", &query).await?;

        let entry = v
            .as_array()
            .and_then(|a| a.iter().find(|e| e.get("conid").and_then(value_as_i64) == Some(conid)))
            .or_else(|| v.as_array().and_then(|a| a.first()))
            .cloned()
            .unwrap_or(Value::Null);

        let f = |code: u32| entry.get(code.to_string()).and_then(lenient_f64);
        Ok(OptionQuoteSnap {
            conid,
            last: f(fields.last),
            bid: f(fields.bid),
            ask: f(fields.ask),
            implied_volatility: f(fields.implied_volatility),
            delta: f(fields.delta),
            gamma: f(fields.gamma),
            theta: f(fields.theta),
            vega: f(fields.vega),
            open_interest: f(fields.open_interest).map(|x| x as i64),
            volume: f(fields.volume).map(|x| x as i64),
        })
    }

    // --- orders ---

    /// What-if preview (margin/commission impact; never transmits).
    pub async fn preview_order(
        &mut self,
        conid: i64,
        side: &str,
        quantity: i32,
        limit: f64,
    ) -> Result<OrderPreview> {
        let acct = self.account()?.to_string();
        let body = json!({ "orders": [ order_payload(conid, side, quantity, limit) ] });
        let v = self
            .post(&format!("/iserver/account/{acct}/orders/whatif"), &body)
            .await?;
        Ok(OrderPreview {
            amount: dig_str(&v, "amount"),
            equity_change: dig_str(&v, "equity"),
            init_margin_change: dig_str(&v, "initial"),
            maint_margin_change: dig_str(&v, "maintenance"),
            commission: dig_str(&v, "commission"),
            warning: dig_str(&v, "warn"),
            raw: v,
        })
    }

    /// EU/PRIIPs tradability probe: a 1-lot far-OTM what-if sell put.
    pub async fn tradability(&mut self, option_conid: i64) -> Tradability {
        match self.preview_order(option_conid, "SELL", 1, 0.01).await {
            Ok(p) => Tradability::Allowed(p),
            Err(e) => Tradability::Blocked(e.to_string()),
        }
    }

    /// Place a live order. Returns the raw reply (may require a confirmation
    /// follow-up via [`Self::confirm_reply`]). Caller must enforce guardrails.
    pub async fn place_order(
        &mut self,
        conid: i64,
        side: &str,
        quantity: i32,
        limit: f64,
    ) -> Result<Value> {
        let acct = self.account()?.to_string();
        let body = json!({ "orders": [ order_payload(conid, side, quantity, limit) ] });
        self.post(&format!("/iserver/account/{acct}/orders"), &body).await
    }

    /// Confirm an order reply prompt (precautionary messages).
    pub async fn confirm_reply(&mut self, reply_id: &str) -> Result<Value> {
        self.post(&format!("/iserver/reply/{reply_id}"), &json!({"confirmed": true}))
            .await
    }

    /// Cancel an open order.
    pub async fn cancel_order(&mut self, order_id: &str) -> Result<Value> {
        let acct = self.account()?.to_string();
        self.delete(&format!("/iserver/account/{acct}/order/{order_id}")).await
    }
}

fn order_payload(conid: i64, side: &str, quantity: i32, limit: f64) -> Value {
    json!({
        "conid": conid,
        "orderType": "LMT",
        "side": side,
        "quantity": quantity,
        "price": limit,
        "tif": "DAY",
    })
}

/// Extract an i64 from a value that may be a number or numeric string.
fn value_as_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

/// Parse an array of (possibly stringy) numbers.
fn number_array(v: Option<&Value>) -> Vec<f64> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(lenient_f64).collect())
        .unwrap_or_default()
}

/// Format a strike without a trailing `.0` (the API wants e.g. `180` or `182.5`).
fn trim_float(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// Recursively find the first string-ish value whose key contains `needle`.
fn dig_str(v: &Value, needle: &str) -> Option<String> {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k.to_ascii_lowercase().contains(needle) {
                    match val {
                        Value::String(s) => return Some(s.clone()),
                        Value::Number(n) => return Some(n.to_string()),
                        _ => {}
                    }
                }
            }
            map.values().find_map(|val| dig_str(val, needle))
        }
        Value::Array(items) => items.iter().find_map(|val| dig_str(val, needle)),
        _ => None,
    }
}
