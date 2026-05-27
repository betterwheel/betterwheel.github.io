//! Cash-secured put selection — the entry leg of the wheel.

use chrono::NaiveDate;

use super::math::{annualized_yield, dte, resolve_abs_delta, round_cents};
use super::types::{ActionKind, EngineConfig, OptionQuote, Right, Suggestion, UnderlyingQuote};

/// Choose the best cash-secured put for `symbol`, or `None` if nothing fits.
///
/// `max_collateral` is the most cash the caller permits to be tied up in this
/// position (already reconciled against available funds and deployment caps),
/// which also sizes the contract quantity.
pub fn select_csp(
    symbol: &str,
    underlying: UnderlyingQuote,
    quotes: &[OptionQuote],
    max_collateral: f64,
    cfg: &EngineConfig,
    today: NaiveDate,
) -> Option<Suggestion> {
    let spot = underlying.last;
    if spot <= 0.0 {
        return None;
    }

    let mut best: Option<(f64, Suggestion)> = None;

    for q in quotes.iter().filter(|q| q.right == Right::Put) {
        // Out-of-the-money puts only.
        if q.strike >= spot {
            continue;
        }
        let d = dte(today, q.expiry);
        if d < cfg.min_dte || d > cfg.max_dte {
            continue;
        }

        let premium = q.mid();
        if premium < cfg.min_premium {
            continue;
        }

        if let Some(oi) = q.open_interest {
            if oi < cfg.min_open_interest {
                continue;
            }
        }

        let Some(abs_delta) = resolve_abs_delta(q, spot, d, cfg.risk_free_rate) else {
            continue;
        };
        if abs_delta < cfg.min_delta || abs_delta > cfg.max_delta {
            continue;
        }

        // Cash-secured sizing.
        let collateral_per_contract = q.strike * 100.0;
        let qty = (max_collateral / collateral_per_contract).floor() as i32;
        if qty < 1 {
            continue;
        }

        let ann = annualized_yield(premium, q.strike, d);
        if ann < cfg.min_annualized_yield {
            continue;
        }

        let limit = round_cents(premium);
        let suggestion = Suggestion {
            symbol: symbol.to_string(),
            kind: ActionKind::SellPut,
            right: Right::Put,
            strike: q.strike,
            expiry: q.expiry,
            dte: d,
            quantity: qty,
            limit_price: limit,
            delta: Some(abs_delta),
            premium_total: limit * 100.0 * qty as f64,
            capital_required: collateral_per_contract * qty as f64,
            annualized_yield: ann,
            rationale: format!(
                "CSP {:.2}Δ, {}DTE, ${:.2} credit → {:.1}% annualized",
                abs_delta,
                d,
                premium,
                ann * 100.0
            ),
        };

        // Maximize yield, then prefer the strike whose delta is nearest target.
        let score = ann - 0.001 * (abs_delta - cfg.target_delta).abs();
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, suggestion));
        }
    }

    best.map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(n)
    }

    fn put(strike: f64, delta: f64, premium: f64) -> OptionQuote {
        OptionQuote {
            right: Right::Put,
            strike,
            expiry: day(30),
            bid: premium - 0.05,
            ask: premium + 0.05,
            delta: Some(-delta), // puts report negative delta
            implied_volatility: Some(0.3),
            open_interest: Some(500),
            volume: Some(100),
        }
    }

    #[test]
    fn picks_best_yield_within_delta_band() {
        let quotes = vec![
            put(90.0, 0.20, 1.00), // yield ~0.135 — too low
            put(92.0, 0.25, 1.60), // yield ~0.211 — ok
            put(95.0, 0.32, 1.80), // yield ~0.230 — best & closest to 0.30
            put(98.0, 0.45, 2.50), // delta out of band
        ];
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &quotes,
            10_000.0,
            &EngineConfig::default(),
            day(0),
        )
        .expect("a suggestion");
        assert_eq!(s.strike, 95.0);
        assert_eq!(s.quantity, 1); // floor(10000 / 9500)
        assert!((s.capital_required - 9500.0).abs() < 1e-6);
        assert_eq!(s.kind, ActionKind::SellPut);
    }

    #[test]
    fn rejects_when_not_cash_secured() {
        let quotes = vec![put(95.0, 0.32, 1.80)];
        // Budget can't cover even one contract (95 * 100 = 9500).
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &quotes,
            5_000.0,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_none());
    }

    #[test]
    fn rejects_low_liquidity() {
        let mut q = put(95.0, 0.32, 1.80);
        q.open_interest = Some(10); // below default floor of 100
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &[q],
            10_000.0,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_none());
    }

    #[test]
    fn falls_back_to_bs_delta_when_greek_missing() {
        // No reported delta, but IV present → BS estimate keeps it in the band.
        let mut q = put(95.0, 0.0, 1.80);
        q.delta = None;
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &[q],
            10_000.0,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_some(), "BS fallback should resolve a delta");
    }
}
