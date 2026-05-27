//! Broker-agnostic output types + lenient parsing of the Web API's JSON.
//!
//! IBKR's Web API responses are sparsely documented and shapes vary between
//! versions, so we parse defensively (numbers may arrive as strings, sometimes
//! prefixed; balances may be nested under `value`/`amount`). Helpers here favour
//! robustness over strictness.

use serde::Deserialize;
use serde_json::Value;

/// OAuth2 token response.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// Parsed account balances (each `None` until reported).
#[derive(Debug, Default, Clone, Copy)]
pub struct AccountSnapshot {
    pub net_liquidation: Option<f64>,
    pub total_cash: Option<f64>,
    pub buying_power: Option<f64>,
    pub available_funds: Option<f64>,
}

/// A flattened portfolio position.
#[derive(Debug, Clone)]
pub struct PositionRow {
    pub conid: i64,
    pub symbol: String,
    pub asset_class: String,
    pub position: f64,
    pub avg_price: f64,
    pub mkt_price: f64,
    pub mkt_value: f64,
    pub unrealized_pnl: f64,
}

/// A contract-search match.
#[derive(Debug, Clone)]
pub struct ContractMatch {
    pub conid: i64,
    pub symbol: String,
    pub description: String,
    pub sec_type: String,
}

/// One option's market snapshot (quote + greeks). All optional — greeks can lag.
#[derive(Debug, Default, Clone)]
pub struct OptionQuoteSnap {
    pub conid: i64,
    pub last: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub implied_volatility: Option<f64>,
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub theta: Option<f64>,
    pub vega: Option<f64>,
    pub open_interest: Option<i64>,
    pub volume: Option<i64>,
}

/// Result of a what-if order preview (margin/commission impact). Field names in
/// the response vary, so the salient bits are surfaced plus the raw JSON.
#[derive(Debug, Clone)]
pub struct OrderPreview {
    pub amount: Option<String>,
    pub equity_change: Option<String>,
    pub init_margin_change: Option<String>,
    pub maint_margin_change: Option<String>,
    pub commission: Option<String>,
    pub warning: Option<String>,
    pub raw: Value,
}

/// Whether the account may trade a given underlying's options (EU/PRIIPs probe).
#[derive(Debug)]
pub enum Tradability {
    Allowed(OrderPreview),
    Blocked(String),
}

/// Parse a value that may be a JSON number or a string, possibly prefixed
/// (e.g. `"C123.45"`) or with thousands separators (`"1,234.50"`).
pub fn lenient_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_loose_number(s),
        _ => None,
    }
}

/// Extract a float from a possibly-messy string. Keeps digits, sign and a single
/// decimal point; drops currency/letter prefixes, commas, `%`, etc.
pub fn parse_loose_number(s: &str) -> Option<f64> {
    let mut out = String::with_capacity(s.len());
    let mut seen_dot = false;
    for c in s.trim().chars() {
        match c {
            '0'..='9' => out.push(c),
            '-' if out.is_empty() => out.push(c),
            '.' if !seen_dot => {
                seen_dot = true;
                out.push(c);
            }
            _ => {}
        }
    }
    if out.is_empty() || out == "-" {
        None
    } else {
        out.parse::<f64>().ok()
    }
}

/// Recursively find the first object entry whose key contains `needle`
/// (case-insensitive) and resolve a number from its value — handling values
/// that are bare numbers/strings or nested as `{ "value": .. }` / `{ "amount": .. }`.
pub fn dig_money(v: &Value, needle: &str) -> Option<f64> {
    let needle = needle.to_ascii_lowercase();
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k.to_ascii_lowercase().contains(&needle) {
                    if let Some(n) = resolve_number(val) {
                        return Some(n);
                    }
                }
            }
            // Recurse into children if not found at this level.
            map.values().find_map(|val| dig_money(val, &needle))
        }
        Value::Array(items) => items.iter().find_map(|val| dig_money(val, &needle)),
        _ => None,
    }
}

fn resolve_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(_) | Value::String(_) => lenient_f64(v),
        Value::Object(map) => map
            .get("value")
            .or_else(|| map.get("amount"))
            .and_then(lenient_f64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loose_numbers() {
        assert_eq!(parse_loose_number("123.45"), Some(123.45));
        assert_eq!(parse_loose_number("C123.45"), Some(123.45));
        assert_eq!(parse_loose_number("1,234.50"), Some(1234.50));
        assert_eq!(parse_loose_number("-12.3%"), Some(-12.3));
        assert_eq!(parse_loose_number("n/a"), None);
        assert_eq!(parse_loose_number(""), None);
    }

    #[test]
    fn digs_nested_money() {
        let v = json!({
            "accountSummary": {
                "DU123": {
                    "netLiquidation": { "value": 250000.0, "currency": "USD" },
                    "totalCashValue": { "amount": "50,000.00" }
                }
            }
        });
        assert_eq!(dig_money(&v, "netliquidation"), Some(250000.0));
        assert_eq!(dig_money(&v, "totalcash"), Some(50000.0));
        assert_eq!(dig_money(&v, "missing"), None);
    }
}
