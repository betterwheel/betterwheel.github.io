//! Iron fly: a short ATM straddle hedged by protective wings. Collects the most
//! premium of any of these structures but with the narrowest profit zone (the
//! underlying must finish near the body). The thread's "early ATM fly, exit fast"
//! variant.

use chrono::NaiveDate;

use super::{build_defined_risk, leg, nearest_by_strike, StructureParams};
use crate::engine::types::{LegSide, OptionQuote, Right, Suggestion};

/// Build the best iron fly for `params`: short put and short call at the listed
/// strike nearest spot (the body), each hedged by a wing `params.wing_points`
/// away.
pub fn select(
    params: &StructureParams,
    symbol: &str,
    spot: f64,
    quotes: &[OptionQuote],
    today: NaiveDate,
    _r: f64,
) -> Option<Suggestion> {
    // Body = the put strike nearest spot; its expiry anchors the rest.
    let body_put = quotes
        .iter()
        .filter(|q| q.right == Right::Put && q.mid() > 0.0)
        .min_by(|a, b| {
            (a.strike - spot)
                .abs()
                .partial_cmp(&(b.strike - spot).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
    let expiry = body_put.expiry;
    let body = body_put.strike;

    let body_call = nearest_by_strike(quotes, Right::Call, expiry, body)?;
    // Require the call body to sit at the same strike as the put body.
    if (body_call.strike - body).abs() > 1e-6 {
        return None;
    }
    let long_put = nearest_by_strike(quotes, Right::Put, expiry, body - params.wing_points)?;
    let long_call = nearest_by_strike(quotes, Right::Call, expiry, body + params.wing_points)?;
    if long_put.strike >= body || long_call.strike <= body {
        return None;
    }

    let short_put = leg(body_put, LegSide::Sell);
    let legs = vec![
        leg(long_put, LegSide::Buy),
        short_put,
        leg(body_call, LegSide::Sell),
        leg(long_call, LegSide::Buy),
    ];
    build_defined_risk(params, symbol, spot, expiry, today, legs, short_put)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::structures::{breakevens, max_loss_per_share, net_credit};
    use crate::engine::types::{ActionKind, StructureKind};

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(n)
    }

    fn q(right: Right, strike: f64, mid: f64) -> OptionQuote {
        OptionQuote {
            right,
            strike,
            expiry: day(0),
            bid: mid - 0.05,
            ask: mid + 0.05,
            delta: Some(if right == Right::Put { -0.5 } else { 0.5 }),
            implied_volatility: Some(0.2),
            open_interest: Some(1000),
            volume: Some(500),
        }
    }

    fn fly_params() -> StructureParams {
        StructureParams {
            kind: StructureKind::IronFly,
            wing_points: 30.0,
            min_credit: 1.0,
            max_risk: 10_000.0,
            ..Default::default()
        }
    }

    #[test]
    fn builds_atm_fly_with_narrow_zone() {
        // Spot 5000; body at 5000 (ATM straddle ~10+10), wings 30pt out at 4970/5030.
        let quotes = vec![
            q(Right::Put, 4970.0, 3.0),
            q(Right::Put, 5000.0, 10.0),
            q(Right::Call, 5000.0, 10.5),
            q(Right::Call, 5030.0, 3.5),
        ];
        let s = super::select(&fly_params(), "SPX", 5000.0, &quotes, day(0), 0.04)
            .expect("a fly");
        let ActionKind::OpenStructure { kind, legs } = &s.kind else {
            panic!("expected OpenStructure");
        };
        assert_eq!(*kind, StructureKind::IronFly);
        assert_eq!(legs.len(), 4);
        assert_eq!(s.strike, 5000.0);
        // Net credit = (10 + 10.5) − (3 + 3.5) = 14.0; max loss = 30 − 14 = 16/sh.
        assert!((net_credit(legs) - 14.0).abs() < 1e-9);
        assert!((max_loss_per_share(legs) - 16.0).abs() < 1e-9);
        // Narrow zone: breakevens body ± credit = 4986 / 5014.
        let bes = breakevens(legs);
        assert_eq!(bes.len(), 2);
        assert!((bes[0] - 4986.0).abs() < 1e-2);
        assert!((bes[1] - 5014.0).abs() < 1e-2);
    }
}
