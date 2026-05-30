//! Synthetic market data for **offline mode**, so the TUI is fully usable before
//! a live IBKR connection is configured. Quotes are Black-Scholes-consistent, so
//! the real engine produces realistic suggestions from them.

use chrono::{Duration, NaiveDate};

use crate::config::ZeroDteConfig;
use crate::engine::math::{bs_delta, norm_cdf};
use crate::engine::types::{EngineConfig, OptionQuote, Right, Suggestion, UnderlyingQuote};
use crate::engine::{csp, structures};

const DEMO_IV: f64 = 0.45;
const DEMO_R: f64 = 0.04;
/// A calmer IV for the index 0DTE demo (SPX trades far below the wheel's stock IV).
const DEMO_INDEX_IV: f64 = 0.15;

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

fn bs_call_price(s: f64, k: f64, t: f64, r: f64, sig: f64) -> f64 {
    if t <= 0.0 || sig <= 0.0 {
        return (s - k).max(0.0);
    }
    let d1 = ((s / k).ln() + (r + 0.5 * sig * sig) * t) / (sig * t.sqrt());
    let d2 = d1 - sig * t.sqrt();
    s * norm_cdf(d1) - k * (-r * t).exp() * norm_cdf(d2)
}

/// Index-scale spot for the 0DTE demo (the per-symbol hash spot is stock-scale,
/// so index structures need realistic levels for their point-width wings).
fn demo_index_spot(symbol: &str) -> f64 {
    match symbol {
        "SPX" | "SPXW" => 7600.0,
        "NDX" => 25_000.0,
        "RUT" => 2_400.0,
        "DJI" => 44_000.0,
        _ => demo_spot(symbol).max(100.0),
    }
}

/// Per-strike implied vol with an equity-index **skew**: out-of-the-money puts
/// (lower strikes) carry richer vol. Without this the demo is flat-vol, where a
/// broken-wing butterfly prices to a debit — the credit BWB exists precisely
/// because of put skew, so the demo needs to model it. Same IV per strike for
/// both rights (vol is a property of strike/expiry, by put-call parity).
fn demo_skew_iv(spot: f64, k: f64) -> f64 {
    let moneyness = (spot - k) / spot; // > 0 below spot (OTM puts)
    (DEMO_INDEX_IV + 0.6 * moneyness).clamp(0.08, 0.60)
}

/// A both-sides (put + call) chain ~`dte` days out for an index 0DTE structure.
/// 0DTE is priced with a few-hours floor on time so the synthetic greeks aren't
/// degenerate (a true `t = 0` gives no delta to select strikes by).
fn demo_structure_chain(spot: f64, today: NaiveDate, dte: i64) -> Vec<OptionQuote> {
    let expiry = today + Duration::days(dte);
    let t = ((dte as f64) + 0.3) / 365.0; // 0DTE ⇒ ~7h
    // Listed-strike granularity: index points near ATM (e.g. 10pt for SPX).
    let step = if spot >= 1000.0 {
        10.0
    } else if spot >= 100.0 {
        1.0
    } else {
        0.5
    };
    let mut quotes = Vec::new();
    let mut k = (spot * 0.92 / step).round() * step;
    while k <= spot * 1.08 {
        if k > 0.0 {
            let iv = demo_skew_iv(spot, k);
            for right in [Right::Put, Right::Call] {
                let price = match right {
                    Right::Put => bs_put_price(spot, k, t, DEMO_R, iv),
                    Right::Call => bs_call_price(spot, k, t, DEMO_R, iv),
                };
                quotes.push(OptionQuote {
                    right,
                    strike: k,
                    expiry,
                    bid: (price - 0.10).max(0.0),
                    ask: price + 0.10,
                    delta: bs_delta(right, spot, k, t, DEMO_R, iv),
                    implied_volatility: Some(iv),
                    open_interest: Some(2000),
                    volume: Some(800),
                });
            }
        }
        k += step;
    }
    quotes
}

/// Run the real structure selectors over synthetic index chains, one entry per
/// 0DTE-tab slot (`None` where nothing fits). Lets the 2×2 grid populate offline.
pub fn demo_zerodte(
    zerodte: &ZeroDteConfig,
    engine: &EngineConfig,
    today: NaiveDate,
) -> Vec<Option<Suggestion>> {
    (0..zerodte.slot_count())
        .map(|i| {
            let p = zerodte.slot(i)?;
            let spot = demo_index_spot(&p.underlying);
            let chain = demo_structure_chain(spot, today, p.dte);
            structures::select(p, &p.underlying, spot, &chain, today, engine.risk_free_rate)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ZeroDteConfig;
    use crate::engine::types::{ActionKind, StructureKind};

    #[test]
    fn demo_zerodte_populates_every_default_slot() {
        // The four seeded slots must all produce a structure off the synthetic SPX
        // chain, so the 0DTE grid is populated in offline mode.
        let today = NaiveDate::from_ymd_opt(2026, 5, 30).unwrap();
        let z = ZeroDteConfig::default();
        let eng = EngineConfig::default();
        let out = demo_zerodte(&z, &eng, today);
        assert_eq!(out.len(), 4);
        for (i, slot) in out.iter().enumerate() {
            let s = slot
                .as_ref()
                .unwrap_or_else(|| panic!("slot {i} ({}) produced no structure", z.slot(i).unwrap().kind.label()));
            // Each is a multi-leg structure with a positive credit and capped risk.
            assert!(matches!(s.kind, ActionKind::OpenStructure { .. }));
            assert!(s.limit_price > 0.0, "slot {i} has no credit");
            assert!(s.capital_required > 0.0, "slot {i} has no defined risk");
        }
        // Slot 0 is the flagship iron condor (4 legs).
        let ActionKind::OpenStructure { kind, legs } = &out[0].as_ref().unwrap().kind else {
            unreachable!()
        };
        assert_eq!(*kind, StructureKind::IronCondor);
        assert_eq!(legs.len(), 4);
    }
}
