//! Short strangle: a naked short put + short call, no wings. **Undefined risk** —
//! gated behind `allow_naked` and an IBKR naked-option permission tier. The
//! thread's "just sell strangles if you're going to micro-manage anyway"
//! alternative, with the loud caveat that a gap can be ruinous.

use chrono::NaiveDate;

use super::{leg, nearest_by_delta, net_credit, StructureParams};
use crate::engine::math::{dte, round_cents};
use crate::engine::types::{ActionKind, LegSide, OptionQuote, Right, Suggestion};

/// Build a short strangle for `params`: short put and short call near their
/// target deltas. Sized to a single contract — there is no defined max loss to
/// size against, so the gate (`allow_naked`) plus the order guardrails are the
/// only brakes. `capital_required` reports the put-side cash-secured notional as
/// a (lower-bound) risk proxy for display.
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

    let sp = leg(short_put, LegSide::Sell);
    let legs = vec![sp, leg(short_call, LegSide::Sell)];
    let net = round_cents(net_credit(&legs));
    if net < params.min_credit {
        return None;
    }
    let qty = 1; // naked: never auto-scale — one lot, the gate is the brake

    Some(Suggestion {
        symbol: symbol.to_string(),
        kind: ActionKind::OpenStructure { kind: params.kind, legs },
        right: Right::Put,
        strike: short_put.strike,
        underlying_price: spot,
        expiry,
        dte: dte(today, expiry),
        quantity: qty,
        limit_price: net,
        delta: short_put.abs_delta(),
        premium_total: net * 100.0 * qty as f64,
        // No capped loss; report the put-side notional as a coarse risk proxy.
        capital_required: short_put.strike * 100.0 * qty as f64,
        annualized_yield: 0.0,
        rationale: format!(
            "Short Strangle (NAKED) {dte}DTE: short {:.0}P / {:.0}C, net credit ${net:.2} — undefined risk",
            short_put.strike,
            short_call.strike,
            dte = dte(today, expiry),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::StructureKind;

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(n)
    }

    fn q(right: Right, strike: f64, delta: f64, mid: f64) -> OptionQuote {
        OptionQuote {
            right,
            strike,
            expiry: day(0),
            bid: mid - 0.05,
            ask: mid + 0.05,
            delta: Some(delta),
            implied_volatility: Some(0.2),
            open_interest: Some(1000),
        }
    }

    fn naked_params() -> StructureParams {
        StructureParams {
            kind: StructureKind::ShortStrangle,
            short_delta: 0.15,
            call_delta: 0.15,
            delta_tol: 0.06,
            min_credit: 0.50,
            allow_naked: true,
            ..Default::default()
        }
    }

    #[test]
    fn naked_strangle_gated_off_by_default() {
        let quotes = vec![q(Right::Put, 4950.0, -0.15, 6.0), q(Right::Call, 5050.0, 0.15, 6.5)];
        let mut p = naked_params();
        p.allow_naked = false;
        // The dispatcher enforces the gate; selecting directly still builds, but
        // the public `structures::select` refuses without `allow_naked`.
        assert!(crate::engine::structures::select(&p, "SPX", 5000.0, &quotes, day(0), 0.04).is_none());
    }

    #[test]
    fn builds_one_lot_with_notional_proxy() {
        let quotes = vec![q(Right::Put, 4950.0, -0.15, 6.0), q(Right::Call, 5050.0, 0.15, 6.5)];
        let s = super::select(&naked_params(), "SPX", 5000.0, &quotes, day(0), 0.04)
            .expect("a strangle");
        assert_eq!(s.quantity, 1);
        assert!((s.limit_price - 12.5).abs() < 1e-9); // 6.0 + 6.5
        assert!((s.capital_required - 495_000.0).abs() < 1e-6); // 4950 × 100
    }
}
