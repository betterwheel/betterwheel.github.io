//! Covered-call selection — the income leg after assignment.

use chrono::NaiveDate;

use super::math::{annualized_yield, dte, resolve_abs_delta, round_cents};
use super::types::{
    ActionKind, EngineConfig, OptionQuote, Right, SharePosition, Suggestion, UnderlyingQuote,
};

/// Choose the best covered call to write against held shares, or `None`.
///
/// Never suggests a strike below cost basis (would lock in a loss), and only
/// writes against shares not already committed to existing calls.
pub fn select_covered_call(
    symbol: &str,
    underlying: UnderlyingQuote,
    shares: SharePosition,
    committed_call_contracts: i32,
    quotes: &[OptionQuote],
    cfg: &EngineConfig,
    today: NaiveDate,
) -> Option<Suggestion> {
    let spot = underlying.last;
    if spot <= 0.0 {
        return None;
    }

    // Whole lots available to write calls against.
    let available = (shares.shares / 100) as i32 - committed_call_contracts;
    if available < 1 {
        return None;
    }

    let min_strike = shares.cost_basis * (1.0 + cfg.cc_min_pct_above_basis);

    let mut best: Option<(f64, Suggestion)> = None;

    for q in quotes.iter().filter(|q| q.right == Right::Call) {
        // Never cap below cost basis.
        if q.strike < min_strike {
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

        // Yield measured against the market value of the collateral (shares).
        let ann = annualized_yield(premium, spot, d);
        if ann < cfg.min_annualized_yield {
            continue;
        }

        let limit = round_cents(premium);
        let suggestion = Suggestion {
            symbol: symbol.to_string(),
            kind: ActionKind::SellCall,
            right: Right::Call,
            strike: q.strike,
            expiry: q.expiry,
            dte: d,
            quantity: available,
            limit_price: limit,
            delta: Some(abs_delta),
            premium_total: limit * 100.0 * available as f64,
            capital_required: 0.0, // shares already held
            annualized_yield: ann,
            rationale: format!(
                "CC {:.2}Δ, {}DTE, strike ${:.2} (basis ${:.2}), ${:.2} credit → {:.1}% annualized",
                abs_delta,
                d,
                q.strike,
                shares.cost_basis,
                premium,
                ann * 100.0
            ),
        };

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

    fn call(strike: f64, delta: f64, premium: f64) -> OptionQuote {
        OptionQuote {
            right: Right::Call,
            strike,
            expiry: day(30),
            bid: premium - 0.05,
            ask: premium + 0.05,
            delta: Some(delta),
            implied_volatility: Some(0.3),
            open_interest: Some(500),
            volume: Some(100),
        }
    }

    #[test]
    fn picks_best_call_above_basis() {
        let quotes = vec![
            call(105.0, 0.30, 1.80), // yield ~0.219 — best
            call(110.0, 0.18, 0.80), // yield ~0.097 — too low
        ];
        let s = select_covered_call(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            SharePosition { shares: 100, cost_basis: 95.0 },
            0,
            &quotes,
            &EngineConfig::default(),
            day(0),
        )
        .expect("a suggestion");
        assert_eq!(s.strike, 105.0);
        assert_eq!(s.quantity, 1);
        assert_eq!(s.capital_required, 0.0);
        assert_eq!(s.kind, ActionKind::SellCall);
    }

    #[test]
    fn never_writes_below_cost_basis() {
        // Attractive premium but strike under basis → rejected.
        let quotes = vec![call(105.0, 0.30, 3.00)];
        let s = select_covered_call(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            SharePosition { shares: 100, cost_basis: 106.0 },
            0,
            &quotes,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_none());
    }

    #[test]
    fn no_uncommitted_lots() {
        let quotes = vec![call(105.0, 0.30, 1.80)];
        let s = select_covered_call(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            SharePosition { shares: 100, cost_basis: 95.0 },
            1, // the only lot is already committed
            &quotes,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_none());
    }
}
