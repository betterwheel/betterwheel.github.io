//! One-sided vertical credit spread — a bull put spread (`Right::Put`) or a bear
//! call spread (`Right::Call`). Defined risk; the thread's "go directional with a
//! single spread" alternative to the full condor.

use chrono::NaiveDate;

use super::{build_defined_risk, leg, nearest_by_delta, nearest_by_strike, StructureParams};
use crate::engine::types::{LegSide, OptionQuote, Right, Suggestion};

/// Build the best credit spread on `side` for `params`: a short leg near the
/// target delta, hedged by a long leg `params.wing_points` further OTM.
pub fn select(
    params: &StructureParams,
    symbol: &str,
    spot: f64,
    quotes: &[OptionQuote],
    today: NaiveDate,
    r: f64,
    side: Right,
) -> Option<Suggestion> {
    // Calls use `call_delta` when set; puts always use `short_delta`.
    let target = match side {
        Right::Put => params.short_delta,
        Right::Call => {
            if params.call_delta > 0.0 { params.call_delta } else { params.short_delta }
        }
    };

    let short = nearest_by_delta(quotes, side, spot, target, params.delta_tol, today, r)?;
    let expiry = short.expiry;
    // The protective wing sits further OTM: below a short put, above a short call.
    let long_target = match side {
        Right::Put => short.strike - params.wing_points,
        Right::Call => short.strike + params.wing_points,
    };
    let long = nearest_by_strike(quotes, side, expiry, long_target)?;

    // Reject a wing that didn't land further OTM than the short.
    let valid = match side {
        Right::Put => long.strike < short.strike,
        Right::Call => long.strike > short.strike,
    };
    if !valid {
        return None;
    }

    let short_leg = leg(short, LegSide::Sell);
    let legs = vec![short_leg, leg(long, LegSide::Buy)];
    build_defined_risk(params, symbol, spot, expiry, today, legs, short_leg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::structures::{breakevens, max_loss_per_share, net_credit};
    use crate::engine::types::{ActionKind, StructureKind};

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(n)
    }

    fn q(right: Right, strike: f64, delta: f64, mid: f64) -> OptionQuote {
        OptionQuote {
            right,
            strike,
            expiry: day(1),
            bid: mid - 0.05,
            ask: mid + 0.05,
            delta: Some(delta),
            implied_volatility: Some(0.2),
            open_interest: Some(1000),
        }
    }

    fn params(kind: StructureKind) -> StructureParams {
        StructureParams {
            kind,
            dte: 1,
            short_delta: 0.11,
            call_delta: 0.0,
            wing_points: 10.0,
            delta_tol: 0.05,
            min_credit: 0.20,
            max_risk: 5_000.0,
            ..Default::default()
        }
    }

    #[test]
    fn put_credit_spread_picks_short_and_wing() {
        // Spot 5000; short ~0.11Δ put at 4960, wing 10pt down at 4950.
        let quotes = vec![
            q(Right::Put, 4950.0, -0.07, 1.20),
            q(Right::Put, 4960.0, -0.11, 2.00),
        ];
        let s = super::select(&params(StructureKind::PutCreditSpread), "SPX", 5000.0, &quotes, day(0), 0.04, Right::Put)
            .expect("a put spread");
        let ActionKind::OpenStructure { kind, legs } = &s.kind else {
            panic!("expected OpenStructure");
        };
        assert_eq!(*kind, StructureKind::PutCreditSpread);
        assert_eq!(legs.len(), 2);
        assert_eq!(s.strike, 4960.0);
        // Credit 0.80; width 10 → max loss 9.20; one breakeven at 4959.20.
        assert!((net_credit(legs) - 0.80).abs() < 1e-9);
        assert!((max_loss_per_share(legs) - 9.20).abs() < 1e-9);
        let bes = breakevens(legs);
        assert_eq!(bes.len(), 1);
        assert!((bes[0] - 4959.20).abs() < 1e-2);
    }

    #[test]
    fn call_credit_spread_wing_is_above() {
        // Spot 5000; short ~0.11Δ call at 5040, wing 10pt up at 5050.
        let quotes = vec![
            q(Right::Call, 5040.0, 0.11, 2.10),
            q(Right::Call, 5050.0, 0.07, 1.30),
        ];
        let s = super::select(&params(StructureKind::CallCreditSpread), "SPX", 5000.0, &quotes, day(0), 0.04, Right::Call)
            .expect("a call spread");
        let ActionKind::OpenStructure { legs, .. } = &s.kind else {
            panic!("expected OpenStructure");
        };
        // Long wing strike is above the short.
        let long = legs.iter().find(|l| l.side == LegSide::Buy).unwrap();
        let short = legs.iter().find(|l| l.side == LegSide::Sell).unwrap();
        assert!(long.strike > short.strike);
    }
}
