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
}
