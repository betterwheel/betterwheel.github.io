//! Synthetic market data for **offline mode**, so the TUI is fully usable before
//! a live IBKR connection is configured. Quotes are Black-Scholes-consistent, so
//! the real engine produces realistic suggestions from them.

use chrono::{Duration, NaiveDate};

use crate::engine::csp;
use crate::engine::math::{bs_delta, norm_cdf};
use crate::engine::types::{EngineConfig, OptionQuote, Right, Suggestion, UnderlyingQuote};

const DEMO_IV: f64 = 0.45;
const DEMO_R: f64 = 0.04;

/// Deterministic pseudo-spot per symbol (stable across runs), in ~$50–300.
pub fn demo_spot(symbol: &str) -> f64 {
    let h = symbol
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b)));
    50.0 + f64::from(h % 250)
}

fn bs_put_price(s: f64, k: f64, t: f64, r: f64, sig: f64) -> f64 {
    if t <= 0.0 || sig <= 0.0 {
        return (k - s).max(0.0);
    }
    let d1 = ((s / k).ln() + (r + 0.5 * sig * sig) * t) / (sig * t.sqrt());
    let d2 = d1 - sig * t.sqrt();
    k * (-r * t).exp() * norm_cdf(-d2) - s * norm_cdf(-d1)
}

/// A demo put chain ~`dte` days out around `spot`.
fn demo_puts(spot: f64, today: NaiveDate, dte: i64) -> Vec<OptionQuote> {
    let expiry = today + Duration::days(dte);
    let t = dte as f64 / 365.0;
    let step = (spot * 0.025).max(0.5);
    let mut quotes = Vec::new();
    let mut k = (spot * 0.75 / step).round() * step;
    while k <= spot * 1.05 {
        if k > 0.0 {
            let price = bs_put_price(spot, k, t, DEMO_R, DEMO_IV);
            let delta = bs_delta(Right::Put, spot, k, t, DEMO_R, DEMO_IV);
            quotes.push(OptionQuote {
                right: Right::Put,
                strike: k,
                expiry,
                bid: (price - 0.05).max(0.0),
                ask: price + 0.05,
                delta,
                implied_volatility: Some(DEMO_IV),
                open_interest: Some(800),
                volume: Some(200),
            });
        }
        k += step;
    }
    quotes
}

/// Run the real CSP engine over demo chains for each symbol.
pub fn demo_suggestions(symbols: &[String], cfg: &EngineConfig, today: NaiveDate) -> Vec<Suggestion> {
    let mut out = Vec::new();
    for symbol in symbols {
        let spot = demo_spot(symbol);
        let dte = (cfg.min_dte + cfg.max_dte) / 2;
        let chain = demo_puts(spot, today, dte);
        let budget = spot * 100.0 * 1.2; // enough to size ~1 contract
        if let Some(s) = csp::select_csp(symbol, UnderlyingQuote { last: spot }, &chain, budget, cfg, today)
        {
            out.push(s);
        }
    }
    // Best annualized yield first.
    out.sort_by(|a, b| {
        b.annualized_yield
            .partial_cmp(&a.annualized_yield)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}
