//! Pricing / yield helpers and a Black-Scholes greeks fallback.
//!
//! The fallback exists because paper accounts (and illiquid strikes) often lack
//! a model delta; given an implied volatility we can still estimate one well
//! enough to filter strikes by moneyness.

use chrono::NaiveDate;

use super::types::{OptionQuote, Right};

/// Calendar days to expiration. Negative once expiry has passed.
pub fn dte(today: NaiveDate, expiry: NaiveDate) -> i64 {
    (expiry - today).num_days()
}

/// Annualized return on collateral, as a fraction (0.30 == 30%/yr).
pub fn annualized_yield(premium_per_share: f64, collateral_per_share: f64, dte: i64) -> f64 {
    if collateral_per_share <= 0.0 || dte <= 0 {
        return 0.0;
    }
    (premium_per_share / collateral_per_share) * (365.0 / dte as f64)
}

/// Round a price to the nearest cent.
pub fn round_cents(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// P&L at expiry of a short put, optionally hedged into a put credit spread by a
/// long put at `long_strike`, when the underlying settles at `spot`.
///
/// `credit` is the per-share premium kept (the spread's *net* credit when hedged)
/// and the result scales by `shares`. Without a hedge the loss grows unbounded as
/// `spot → 0`; with one it is capped at `(width − credit) × shares`. Used to show
/// concrete "what if the stock drops X%" outcomes.
pub fn short_put_pnl_at(
    spot: f64,
    short_strike: f64,
    credit: f64,
    long_strike: Option<f64>,
    shares: f64,
) -> f64 {
    let short_intrinsic = (short_strike - spot).max(0.0);
    let long_intrinsic = long_strike.map_or(0.0, |lk| (lk - spot).max(0.0));
    (credit - (short_intrinsic - long_intrinsic)) * shares
}

/// Standard normal CDF (Zelen & Severo approximation, |err| < 7.5e-8).
pub fn norm_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.231_641_9 * x.abs());
    let d = 0.398_942_280_401_432_7 * (-x * x / 2.0).exp();
    let p = d
        * t
        * (0.319_381_530
            + t * (-0.356_563_782
                + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    if x >= 0.0 { 1.0 - p } else { p }
}

/// Black-Scholes delta for a European option. Signed: calls in (0,1], puts in
/// [-1,0). Returns `None` for degenerate inputs.
pub fn bs_delta(right: Right, spot: f64, strike: f64, t_years: f64, r: f64, sigma: f64) -> Option<f64> {
    if spot <= 0.0 || strike <= 0.0 || t_years <= 0.0 || sigma <= 0.0 {
        return None;
    }
    let d1 = ((spot / strike).ln() + (r + 0.5 * sigma * sigma) * t_years) / (sigma * t_years.sqrt());
    let nd1 = norm_cdf(d1);
    Some(match right {
        Right::Call => nd1,
        Right::Put => nd1 - 1.0,
    })
}

/// Absolute delta for a quote: use the reported greek when present, else fall
/// back to a Black-Scholes estimate from the implied volatility.
pub fn resolve_abs_delta(q: &OptionQuote, spot: f64, dte_days: i64, r: f64) -> Option<f64> {
    if let Some(ad) = q.abs_delta() {
        return Some(ad);
    }
    let iv = q.implied_volatility?;
    let t = dte_days as f64 / 365.0;
    bs_delta(q.right, spot, q.strike, t, r, iv).map(f64::abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yield_basic() {
        // $1.50 credit on $100 collateral over 30 days.
        let y = annualized_yield(1.50, 100.0, 30);
        assert!((y - 0.1825).abs() < 0.001, "got {y}");
    }

    #[test]
    fn yield_guards_zero() {
        assert_eq!(annualized_yield(1.0, 0.0, 30), 0.0);
        assert_eq!(annualized_yield(1.0, 100.0, 0), 0.0);
        assert_eq!(annualized_yield(1.0, 100.0, -5), 0.0);
    }

    #[test]
    fn norm_cdf_known_points() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-9);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-3);
        assert!((norm_cdf(-1.96) - 0.025).abs() < 1e-3);
    }

    #[test]
    fn bs_delta_signs_and_range() {
        // ~30 DTE, 30% IV, ATM.
        let call = bs_delta(Right::Call, 100.0, 100.0, 30.0 / 365.0, 0.04, 0.30).unwrap();
        let put = bs_delta(Right::Put, 100.0, 100.0, 30.0 / 365.0, 0.04, 0.30).unwrap();
        assert!(call > 0.45 && call < 0.60, "call {call}");
        assert!(put < 0.0 && put > -0.55, "put {put}");
        // Put/call delta parity: call - put ≈ 1.
        assert!((call - put - 1.0).abs() < 1e-9);
    }

    #[test]
    fn bs_delta_degenerate() {
        assert!(bs_delta(Right::Put, 0.0, 100.0, 0.1, 0.04, 0.3).is_none());
        assert!(bs_delta(Right::Put, 100.0, 100.0, 0.0, 0.04, 0.3).is_none());
        assert!(bs_delta(Right::Put, 100.0, 100.0, 0.1, 0.04, 0.0).is_none());
    }

    #[test]
    fn short_put_pnl_unhedged() {
        // Sold a $100 put for $2, one contract (100 shares).
        // Above strike → keep the full credit.
        assert!((short_put_pnl_at(105.0, 100.0, 2.0, None, 100.0) - 200.0).abs() < 1e-9);
        // At breakeven ($98) → ~flat.
        assert!(short_put_pnl_at(98.0, 100.0, 2.0, None, 100.0).abs() < 1e-9);
        // Down to $80 → (2 − 20) × 100 = −$1,800.
        assert!((short_put_pnl_at(80.0, 100.0, 2.0, None, 100.0) + 1800.0).abs() < 1e-9);
        // To zero → loss within $200 of the full strike notional (credit kept).
        assert!((short_put_pnl_at(0.0, 100.0, 2.0, None, 100.0) + 9800.0).abs() < 1e-9);
    }

    #[test]
    fn short_put_pnl_spread_caps_the_loss() {
        // $100/$95 put spread, $1.50 net credit, one contract. Width $5.
        let cap = -(5.0 - 1.50) * 100.0; // −$350 max loss
        // At/above the long strike the loss keeps growing until $95...
        assert!((short_put_pnl_at(96.0, 100.0, 1.50, Some(95.0), 100.0) + 250.0).abs() < 1e-9);
        // ...then it plateaus: $90, $50, $0 all hit the same capped loss.
        for spot in [95.0, 90.0, 50.0, 0.0] {
            let pnl = short_put_pnl_at(spot, 100.0, 1.50, Some(95.0), 100.0);
            assert!((pnl - cap).abs() < 1e-9, "spot {spot}: {pnl} != {cap}");
        }
    }
}
