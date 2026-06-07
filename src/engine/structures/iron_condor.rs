//! Iron condor: a short put spread and a short call spread sharing an expiry —
//! defined risk on both sides, maximally profitable while the underlying stays
//! between the shorts. The 0DTE thread's core structure.

use chrono::NaiveDate;

use super::{build_defined_risk, leg, nearest_by_delta, nearest_by_strike, StructureParams};
use crate::engine::types::{LegSide, OptionQuote, Right, Suggestion};

/// Build the best iron condor for `params`: short put and short call near their
/// target deltas, each hedged by a wing `params.wing_points` further OTM.
pub fn select(
    params: &StructureParams,
    symbol: &str,
    spot: f64,
    quotes: &[OptionQuote],
    today: NaiveDate,
    r: f64,
) -> Option<Suggestion> {
    let call_target = if params.call_delta > 0.0 { params.call_delta } else { params.short_delta };

    let short_put =
        nearest_by_delta(quotes, Right::Put, spot, params.short_delta, params.delta_tol, today, r)?;
    let short_call =
        nearest_by_delta(quotes, Right::Call, spot, call_target, params.delta_tol, today, r)?;
    let expiry = short_put.expiry;

    // Wings a fixed number of points outside each short, same expiry.
    let long_put = nearest_by_strike(quotes, Right::Put, expiry, short_put.strike - params.wing_points)?;
    let long_call =
        nearest_by_strike(quotes, Right::Call, expiry, short_call.strike + params.wing_points)?;

    // A degenerate chain can collapse a wing onto its short; reject those.
    if long_put.strike >= short_put.strike || long_call.strike <= short_call.strike {
        return None;
    }

    let sp = leg(short_put, LegSide::Sell);
    let legs = vec![
        leg(long_put, LegSide::Buy),
        sp,
        leg(short_call, LegSide::Sell),
        leg(long_call, LegSide::Buy),
    ];
    build_defined_risk(params, symbol, spot, expiry, today, legs, sp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::structures::{breakevens, net_credit};
    use crate::engine::types::{ActionKind, StructureKind};

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(n)
    }

    fn q(right: Right, strike: f64, delta: f64, mid: f64) -> OptionQuote {
        OptionQuote {
            right,
            strike,
            expiry: day(0), // 0DTE
            bid: mid - 0.05,
            ask: mid + 0.05,
            delta: Some(delta),
            implied_volatility: Some(0.2),
            open_interest: Some(1000),
        }
    }

    fn ic_params() -> StructureParams {
        StructureParams {
            kind: StructureKind::IronCondor,
            short_delta: 0.15,
            call_delta: 0.15,
            wing_points: 25.0,
            delta_tol: 0.06,
            min_credit: 0.50,
            max_risk: 10_000.0,
            ..Default::default()
        }
    }

    #[test]
    fn builds_a_four_leg_condor_with_capped_risk() {
        // Spot 5000. Shorts ~0.15Δ at 4950P / 5050C; wings 25pt out at 4925P / 5075C.
        let quotes = vec![
            q(Right::Put, 4925.0, -0.08, 3.0),
            q(Right::Put, 4950.0, -0.15, 6.0),
            q(Right::Call, 5050.0, 0.15, 6.5),
            q(Right::Call, 5075.0, 0.08, 3.5),
        ];
        let s = super::select(&ic_params(), "SPX", 5000.0, &quotes, day(0), 0.04)
            .expect("a condor");
        let ActionKind::OpenStructure { kind, legs } = &s.kind else {
            panic!("expected OpenStructure, got {:?}", s.kind);
        };
        assert_eq!(*kind, StructureKind::IronCondor);
        assert_eq!(legs.len(), 4);
        // Net credit = (6.0 + 6.5) − (3.0 + 3.5) = 6.0; max loss = 25 − 6 = 19/sh.
        assert!((net_credit(legs) - 6.0).abs() < 1e-9);
        assert!((s.limit_price - 6.0).abs() < 1e-9);
        assert_eq!(s.strike, 4950.0); // primary = short put
        // Two breakevens straddling spot.
        let bes = breakevens(legs);
        assert_eq!(bes.len(), 2);
        assert!(bes[0] < 5000.0 && bes[1] > 5000.0);
        // Sized to budget: max loss/contract = 19×100 = 1900; floor(10000/1900)=5.
        assert_eq!(s.quantity, 5);
    }

    #[test]
    fn rejects_when_no_wing_available() {
        // Shorts present but no further-OTM wings in the chain → no condor.
        let quotes = vec![
            q(Right::Put, 4950.0, -0.15, 6.0),
            q(Right::Call, 5050.0, 0.15, 6.5),
        ];
        assert!(super::select(&ic_params(), "SPX", 5000.0, &quotes, day(0), 0.04).is_none());
    }
}
