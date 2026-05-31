//! Multi-leg, short-dated (0–2 DTE) **structure** selection — iron condors,
//! credit spreads, broken-wing butterflies, iron flies, strangles.
//!
//! This is a sibling of the wheel selectors (`csp`, `covered_call`) but a
//! *separate strategy family*: these are market-neutral premium structures on an
//! index (typically SPX, cash-settled / European — no assignment, no shares),
//! opened and closed within the trade's life rather than walked through the
//! wheel's `WheelState` machine. Pure functions, zero I/O, fully unit-testable.
//!
//! Every structure's P&L at expiry is **piecewise-linear** in the settle price
//! with kinks only at the strikes, so the generic helpers here ([`payoff_at`],
//! [`max_loss_per_share`], [`breakevens`]) derive risk/reward for *any* leg set
//! by sampling the strikes plus both tails — no per-structure math to get wrong.

pub mod broken_wing;
pub mod credit_spread;
pub mod iron_condor;
pub mod iron_fly;
pub mod strangle;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::math::{dte, resolve_abs_delta, round_cents};
use super::types::{
    ActionKind, LegSide, OptionQuote, Right, StructureKind, StructureLeg, Suggestion,
};

/// Per-strategy parameters for a 0DTE/short-dated structure. One of these defines
/// a roster slot; the management fields (entry time, profit target, …) are read
/// by the scheduler, the rest by the selector here. Every field defaults so a
/// partial TOML entry still loads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StructureParams {
    /// Display name for the slot (e.g. "SPX 0DTE Iron Condor").
    pub name: String,
    pub kind: StructureKind,
    /// Underlying symbol (index), e.g. "SPX".
    pub underlying: String,
    /// Days to expiration to target (0 = same-day).
    pub dte: i64,
    /// Target absolute delta for the short put leg(s).
    pub short_delta: f64,
    /// Target absolute delta for the short call leg (iron condor). `0` reuses
    /// `short_delta` for a symmetric condor.
    pub call_delta: f64,
    /// Protective wing width in strike points.
    pub wing_points: f64,
    /// Acceptable +/- band around the target delta when picking a short strike.
    pub delta_tol: f64,
    /// Skip a structure whose net credit (per share) is below this.
    pub min_credit: f64,
    /// Size so the defined max loss per position stays within this budget ($).
    pub max_risk: f64,

    // --- management (consumed by the scheduler in a later phase) ---
    /// Minutes after the regular-session open to place the entry.
    pub entry_minutes_after_open: i64,
    /// Buy-to-close once this fraction of the credit is captured.
    pub profit_target_pct: f64,
    /// Stop at this multiple of the credit (0 = none — "the wings are the stop").
    pub stop_loss_mult: f64,
    /// Force-close at this wall-clock time if still open ("HH:MM", US/Eastern).
    pub time_stop_hhmm: Option<String>,
    /// Repeat entries every N minutes through the session (0 = single entry, the
    /// MEIC mode). Each entry is managed independently.
    pub meic_interval_min: i64,
    /// Whether the scheduler may transmit this slot live (the per-slot opt-in).
    pub automate: bool,
    /// Allow the naked Short Strangle (undefined risk). Ignored for defined-risk
    /// structures.
    pub allow_naked: bool,
}

impl Default for StructureParams {
    fn default() -> Self {
        Self {
            name: "SPX 0DTE Iron Condor".to_string(),
            kind: StructureKind::IronCondor,
            underlying: "SPX".to_string(),
            dte: 0,
            short_delta: 0.13,
            call_delta: 0.13,
            wing_points: 25.0,
            delta_tol: 0.08,
            min_credit: 0.50,
            max_risk: 2500.0,
            entry_minutes_after_open: 45,
            profit_target_pct: 0.40,
            stop_loss_mult: 0.0,
            time_stop_hhmm: None,
            meic_interval_min: 0,
            automate: false,
            allow_naked: false,
        }
    }
}

/// Net credit per share for a leg set: sold premium in, bought premium out.
/// Positive = a credit is received.
pub fn net_credit(legs: &[StructureLeg]) -> f64 {
    legs.iter()
        .map(|l| match l.side {
            LegSide::Sell => l.price,
            LegSide::Buy => -l.price,
        })
        .sum()
}

/// P&L per share at expiry if the underlying settles at `spot`, net of premia.
pub fn payoff_at(legs: &[StructureLeg], spot: f64) -> f64 {
    legs.iter()
        .map(|l| {
            let intrinsic = match l.right {
                Right::Put => (l.strike - spot).max(0.0),
                Right::Call => (spot - l.strike).max(0.0),
            };
            match l.side {
                LegSide::Buy => intrinsic - l.price,
                LegSide::Sell => l.price - intrinsic,
            }
        })
        .sum()
}

/// Sorted, de-duplicated spots at which the piecewise-linear payoff can bend or
/// extremize: zero, every strike, and a far-upside tail (2× the highest strike).
/// The payoff is linear between consecutive entries, so min/max and zero
/// crossings all fall on or between these points.
fn candidate_spots(legs: &[StructureLeg]) -> Vec<f64> {
    let mut spots: Vec<f64> = vec![0.0];
    let mut max_strike = 0.0_f64;
    for l in legs {
        spots.push(l.strike);
        max_strike = max_strike.max(l.strike);
    }
    spots.push(max_strike * 2.0);
    spots.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    spots.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    spots
}

/// Worst-case loss per share (a positive magnitude; 0 if the structure can never
/// lose). For a *defined-risk* structure this is exact; a naked leg's true tail
/// is unbounded, so callers size those by other means (see [`strangle`]).
pub fn max_loss_per_share(legs: &[StructureLeg]) -> f64 {
    let min = candidate_spots(legs)
        .iter()
        .map(|&s| payoff_at(legs, s))
        .fold(f64::INFINITY, f64::min);
    (-min).max(0.0)
}

/// Underlying settle prices where the structure breaks even (zero P&L), found by
/// scanning for sign changes between adjacent candidate spots and interpolating.
pub fn breakevens(legs: &[StructureLeg]) -> Vec<f64> {
    let spots = candidate_spots(legs);
    let mut out = Vec::new();
    for w in spots.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (pa, pb) = (payoff_at(legs, a), payoff_at(legs, b));
        let crosses = (pa <= 0.0 && pb > 0.0) || (pa >= 0.0 && pb < 0.0);
        if crosses && (pb - pa).abs() > 1e-12 {
            out.push(round_cents(a - pa * (b - a) / (pb - pa)));
        }
    }
    out
}

/// Rough probability of profit from the short legs' deltas: 1 − Σ(abs delta) over
/// the *distinct* short strikes. Meaningful for OTM-short structures (condors,
/// credit spreads); near-ATM bodies (iron fly) and butterflies need a richer
/// model, so callers may choose not to surface it there. `None` if no short leg
/// carries a delta.
pub fn estimate_pop(legs: &[StructureLeg]) -> Option<f64> {
    let mut seen: Vec<(f64, Right)> = Vec::new();
    let mut sum = 0.0;
    let mut any = false;
    for l in legs.iter().filter(|l| l.side == LegSide::Sell) {
        if seen
            .iter()
            .any(|(k, r)| (*k - l.strike).abs() < 1e-6 && *r == l.right)
        {
            continue;
        }
        seen.push((l.strike, l.right));
        if let Some(d) = l.delta {
            sum += d.abs();
            any = true;
        }
    }
    any.then(|| (1.0 - sum).clamp(0.0, 1.0))
}

/// Contracts to trade so the defined max loss stays within `max_risk`. Returns 0
/// when a single contract would already exceed the budget (caller skips the
/// suggestion), or when there is no defined risk to size against.
fn size_qty(max_loss_per_share: f64, max_risk: f64) -> i32 {
    let per_contract = max_loss_per_share * 100.0;
    if per_contract <= 0.0 {
        return 0;
    }
    (max_risk / per_contract).floor().max(0.0) as i32
}

/// The OTM quote (for `right`, relative to `spot`) whose absolute delta is
/// closest to `target` and within `tol`. Skips quotes whose delta can't be
/// resolved (no greek and — at 0 DTE — no usable Black-Scholes estimate).
pub(crate) fn nearest_by_delta(
    quotes: &[OptionQuote],
    right: Right,
    spot: f64,
    target: f64,
    tol: f64,
    today: NaiveDate,
    r: f64,
) -> Option<&OptionQuote> {
    quotes
        .iter()
        .filter(|q| q.right == right && is_otm(right, q.strike, spot) && q.mid() > 0.0)
        .filter_map(|q| {
            let d = resolve_abs_delta(q, spot, dte(today, q.expiry).max(0), r)?;
            ((d - target).abs() <= tol).then_some((q, (d - target).abs()))
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(q, _)| q)
}

/// The quote (for `right`, same `expiry`) whose strike is nearest `target`,
/// requiring a positive mid. Used to place protective wings a fixed number of
/// points from the short strike.
pub(crate) fn nearest_by_strike(
    quotes: &[OptionQuote],
    right: Right,
    expiry: NaiveDate,
    target: f64,
) -> Option<&OptionQuote> {
    quotes
        .iter()
        .filter(|q| q.right == right && q.expiry == expiry && q.mid() > 0.0)
        .min_by(|a, b| {
            (a.strike - target)
                .abs()
                .partial_cmp(&(b.strike - target).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Whether `strike` is out-of-the-money for `right` given `spot`.
fn is_otm(right: Right, strike: f64, spot: f64) -> bool {
    match right {
        Right::Put => strike < spot,
        Right::Call => strike > spot,
    }
}

/// Make a [`StructureLeg`] from a chosen quote.
pub(crate) fn leg(q: &OptionQuote, side: LegSide) -> StructureLeg {
    StructureLeg {
        right: q.right,
        strike: q.strike,
        side,
        price: round_cents(q.mid()),
        delta: q.abs_delta(),
    }
}

/// Assemble a [`Suggestion`] for a defined-risk structure from its legs, sizing
/// by `max_risk` and gating on `min_credit`. `primary` is the leg whose
/// strike/right/delta populate the suggestion's scalar fields (the short put for
/// most structures). Returns `None` if the credit is too small or a single
/// contract exceeds the risk budget.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_defined_risk(
    params: &StructureParams,
    symbol: &str,
    spot: f64,
    expiry: NaiveDate,
    today: NaiveDate,
    legs: Vec<StructureLeg>,
    primary: StructureLeg,
) -> Option<Suggestion> {
    // Gate on the cent-rounded credit so a value a hair under the threshold from
    // float error (e.g. 12.00 − 11.90 = 0.0999…) isn't spuriously rejected.
    let net = round_cents(net_credit(&legs));
    if net < params.min_credit {
        return None;
    }
    let credit = net;
    let max_loss_ps = max_loss_per_share(&legs);
    let qty = size_qty(max_loss_ps, params.max_risk);
    if qty < 1 {
        return None;
    }
    let bes = breakevens(&legs);
    let be_lo = bes.first().copied().unwrap_or(0.0);
    let be_hi = bes.last().copied().unwrap_or(0.0);
    let ror = if max_loss_ps > 0.0 { credit / max_loss_ps } else { 0.0 };

    Some(Suggestion {
        symbol: symbol.to_string(),
        kind: ActionKind::OpenStructure { kind: params.kind, legs },
        right: primary.right,
        strike: primary.strike,
        underlying_price: spot,
        expiry,
        dte: dte(today, expiry),
        quantity: qty,
        limit_price: net,
        delta: primary.delta,
        premium_total: net * 100.0 * qty as f64,
        capital_required: round_cents(max_loss_ps) * 100.0 * qty as f64,
        // For 0DTE there is no meaningful annualization; reuse this field as the
        // single-trade return on risk (credit ÷ max loss) so the slot can rank /
        // display it. The 0DTE view labels it accordingly.
        annualized_yield: ror,
        rationale: format!(
            "{} {}DTE: net credit ${:.2}, max loss ${:.0}, breakeven ${:.0}–${:.0} (×{qty})",
            params.kind.label(),
            dte(today, expiry),
            net,
            round_cents(max_loss_ps) * 100.0 * qty as f64,
            be_lo,
            be_hi,
        ),
    })
}

/// Select the best structure for `params` over a single-expiry both-sides chain,
/// or `None` if nothing fits. Dispatches to the per-kind builder.
pub fn select(
    params: &StructureParams,
    symbol: &str,
    spot: f64,
    quotes: &[OptionQuote],
    today: NaiveDate,
    risk_free_rate: f64,
) -> Option<Suggestion> {
    if spot <= 0.0 || quotes.is_empty() {
        return None;
    }
    if params.kind.is_naked() && !params.allow_naked {
        return None;
    }
    match params.kind {
        StructureKind::IronCondor => {
            iron_condor::select(params, symbol, spot, quotes, today, risk_free_rate)
        }
        StructureKind::PutCreditSpread => {
            credit_spread::select(params, symbol, spot, quotes, today, risk_free_rate, Right::Put)
        }
        StructureKind::CallCreditSpread => {
            credit_spread::select(params, symbol, spot, quotes, today, risk_free_rate, Right::Call)
        }
        StructureKind::BrokenWingButterfly => {
            broken_wing::select(params, symbol, spot, quotes, today, risk_free_rate)
        }
        StructureKind::IronFly => {
            iron_fly::select(params, symbol, spot, quotes, today, risk_free_rate)
        }
        StructureKind::ShortStrangle => {
            strangle::select(params, symbol, spot, quotes, today, risk_free_rate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::Right;

    fn l(right: Right, strike: f64, side: LegSide, price: f64) -> StructureLeg {
        StructureLeg { right, strike, side, price, delta: None }
    }

    #[test]
    fn net_credit_sums_signed_premia() {
        // Sell 100p @2.00, buy 95p @0.80 → net credit 1.20.
        let legs = vec![
            l(Right::Put, 100.0, LegSide::Sell, 2.00),
            l(Right::Put, 95.0, LegSide::Buy, 0.80),
        ];
        assert!((net_credit(&legs) - 1.20).abs() < 1e-9);
    }

    #[test]
    fn put_credit_spread_risk_and_breakeven() {
        // 100/95 put spread, 1.20 credit. Width 5 → max loss 3.80; breakeven 98.80.
        let legs = vec![
            l(Right::Put, 100.0, LegSide::Sell, 2.00),
            l(Right::Put, 95.0, LegSide::Buy, 0.80),
        ];
        assert!((max_loss_per_share(&legs) - 3.80).abs() < 1e-9);
        // Above 100 → keep full 1.20; far below 95 → −3.80.
        assert!((payoff_at(&legs, 105.0) - 1.20).abs() < 1e-9);
        assert!((payoff_at(&legs, 90.0) + 3.80).abs() < 1e-9);
        let bes = breakevens(&legs);
        assert_eq!(bes.len(), 1);
        assert!((bes[0] - 98.80).abs() < 1e-2);
    }

    #[test]
    fn iron_condor_caps_both_sides_with_two_breakevens() {
        // Put side 100/95, call side 110/115; total credit 1.20+1.00 = 2.20.
        // Worst loss = wider wing (5) − credit (2.20) = 2.80 per share.
        let legs = vec![
            l(Right::Put, 95.0, LegSide::Buy, 0.80),
            l(Right::Put, 100.0, LegSide::Sell, 2.00),
            l(Right::Call, 110.0, LegSide::Sell, 1.50),
            l(Right::Call, 115.0, LegSide::Buy, 0.50),
        ];
        assert!((net_credit(&legs) - 2.20).abs() < 1e-9);
        assert!((max_loss_per_share(&legs) - 2.80).abs() < 1e-9);
        // Between the shorts → keep the full credit.
        assert!((payoff_at(&legs, 105.0) - 2.20).abs() < 1e-9);
        let bes = breakevens(&legs);
        assert_eq!(bes.len(), 2);
        assert!((bes[0] - 97.80).abs() < 1e-2); // 100 − 2.20
        assert!((bes[1] - 112.20).abs() < 1e-2); // 110 + 2.20
    }

    #[test]
    fn size_qty_respects_budget() {
        // $2.80 max loss/share = $280/contract.
        assert_eq!(size_qty(2.80, 2500.0), 8); // floor(2500/280)
        assert_eq!(size_qty(2.80, 250.0), 0); // one contract exceeds budget → skip
        assert_eq!(size_qty(0.0, 2500.0), 0); // no defined risk
    }

    #[test]
    fn estimate_pop_uses_distinct_short_strikes() {
        let mut legs = vec![
            l(Right::Put, 100.0, LegSide::Sell, 2.0),
            l(Right::Call, 110.0, LegSide::Sell, 1.5),
        ];
        legs[0].delta = Some(-0.16);
        legs[1].delta = Some(0.12);
        // 1 − (0.16 + 0.12) = 0.72.
        assert!((estimate_pop(&legs).unwrap() - 0.72).abs() < 1e-9);
    }
}
