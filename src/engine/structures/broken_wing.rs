//! Broken-wing put butterfly (skip-strike), structured for a **net credit** so
//! there is no risk to the upside — only a capped loss on a sharp drop. Bullish-
//! to-neutral theta: Buy 1 upper put, Sell 2 body puts, Buy 1 lower put, with the
//! lower (downside) wing wider than the upper, which is what produces the credit.

use chrono::NaiveDate;

use super::{build_defined_risk, leg, nearest_by_delta, nearest_by_strike, StructureParams};
use crate::engine::types::{LegSide, OptionQuote, Right, Suggestion};

/// Build the best credit put BWB for `params`: the body (2 shorts) near the
/// target delta, a narrow upper wing `wing_points` above it, and a wide lower
/// wing `3 × wing_points` below it (the skip-strike that finances the credit). A
/// 1-2-1 put butterfly only prices to a credit when the upper long sits close to
/// a near-the-money body — so this favours a small upper wing on a ~0.30Δ body.
pub fn select(
    params: &StructureParams,
    symbol: &str,
    spot: f64,
    quotes: &[OptionQuote],
    today: NaiveDate,
    r: f64,
) -> Option<Suggestion> {
    let body = nearest_by_delta(quotes, Right::Put, spot, params.short_delta, params.delta_tol, today, r)?;
    let expiry = body.expiry;

    // Narrow wing above the body, wide (skip-strike) wing below.
    let upper = nearest_by_strike(quotes, Right::Put, expiry, body.strike + params.wing_points)?;
    let lower = nearest_by_strike(quotes, Right::Put, expiry, body.strike - 3.0 * params.wing_points)?;

    // Require the broken (asymmetric) shape: lower wing strictly wider than upper.
    let w_upper = upper.strike - body.strike;
    let w_lower = body.strike - lower.strike;
    if w_upper <= 0.0 || w_lower <= w_upper {
        return None;
    }

    let body_leg = leg(body, LegSide::Sell);
    // Body short appears twice (ratio 2): the suggestion's quantity multiplies the
    // whole unit, so two 1-lot Sell legs give the correct 1-2-1 per butterfly.
    let legs = vec![
        leg(lower, LegSide::Buy),
        body_leg,
        body_leg,
        leg(upper, LegSide::Buy),
    ];
    build_defined_risk(params, symbol, spot, expiry, today, legs, body_leg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::structures::{max_loss_per_share, net_credit, payoff_at};
    use crate::engine::types::{ActionKind, StructureKind};

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(n)
    }

    fn put(strike: f64, delta: f64, mid: f64) -> OptionQuote {
        OptionQuote {
            right: Right::Put,
            strike,
            expiry: day(0),
            bid: mid - 0.05,
            ask: mid + 0.05,
            delta: Some(delta),
            implied_volatility: Some(0.2),
            open_interest: Some(1000),
        }
    }

    fn bwb_params() -> StructureParams {
        StructureParams {
            kind: StructureKind::BrokenWingButterfly,
            short_delta: 0.20,
            wing_points: 20.0,
            delta_tol: 0.06,
            min_credit: 0.10,
            max_risk: 10_000.0,
            ..Default::default()
        }
    }

    #[test]
    fn credit_bwb_has_no_upside_risk_and_capped_downside() {
        // Spot 5000. Body ~0.20Δ at 4960; upper wing 4980 (20 up), lower 4920 (40 down).
        // Prices: convexity + the wide lower wing produce a small net credit.
        let quotes = vec![
            put(4920.0, -0.10, 2.50), // lower long (wide wing)
            put(4960.0, -0.20, 6.00), // body (×2 short)
            put(4980.0, -0.30, 9.40), // upper long (narrow wing)
        ];
        let s = super::select(&bwb_params(), "SPX", 5000.0, &quotes, day(0), 0.04)
            .expect("a BWB");
        let ActionKind::OpenStructure { kind, legs } = &s.kind else {
            panic!("expected OpenStructure");
        };
        assert_eq!(*kind, StructureKind::BrokenWingButterfly);
        assert_eq!(legs.len(), 4); // lower, body, body, upper
        // Net credit = 2×6.00 − 9.40 − 2.50 = 0.10.
        assert!((net_credit(legs) - 0.10).abs() < 1e-9);
        // No upside risk: well above all strikes the P&L is the kept credit.
        assert!((payoff_at(legs, 5200.0) - 0.10).abs() < 1e-9);
        // Downside loss is capped: (wide − narrow) − credit = (40 − 20) − 0.10 = 19.90/sh.
        assert!((max_loss_per_share(legs) - 19.90).abs() < 1e-9);
    }

    #[test]
    fn rejects_symmetric_wings() {
        // Equal wings (no skip-strike) → not a broken-wing structure.
        let quotes = vec![
            put(4940.0, -0.12, 3.0),
            put(4960.0, -0.20, 6.0),
            put(4980.0, -0.30, 9.4), // 20 up and (with 4940) 20 down — symmetric
        ];
        let mut p = bwb_params();
        p.wing_points = 20.0; // lower target = 4920 (absent) → nearest is 4940 → symmetric
        assert!(super::select(&p, "SPX", 5000.0, &quotes, day(0), 0.04).is_none());
    }
}
