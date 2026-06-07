//! Management of open short options: take-profit and roll/defend.

use chrono::{Duration, NaiveDate};

use super::math::{dte, resolve_abs_delta, round_cents};
use super::types::{
    ActionKind, EngineConfig, OpenShortOption, OptionQuote, Right, Suggestion, UnderlyingQuote,
};

/// Decide whether an open short option should be closed for profit or defended.
///
/// `current` is the live quote for the short contract we hold.
pub fn manage_short_option(
    symbol: &str,
    pos: &OpenShortOption,
    current: &OptionQuote,
    underlying: UnderlyingQuote,
    cfg: &EngineConfig,
    today: NaiveDate,
) -> Option<Suggestion> {
    let d = dte(today, pos.expiry);
    let price = current.mid();

    // Fraction of the original credit now captured (price has decayed).
    let captured = if pos.entry_credit > 0.0 {
        (pos.entry_credit - price) / pos.entry_credit
    } else {
        0.0
    };

    // 1) Take profit — buy to close.
    if captured >= cfg.take_profit_pct {
        let limit = round_cents(price);
        return Some(Suggestion {
            symbol: symbol.to_string(),
            kind: ActionKind::CloseForProfit,
            right: pos.right,
            strike: pos.strike,
            underlying_price: underlying.last,
            expiry: pos.expiry,
            dte: d,
            quantity: pos.quantity,
            limit_price: limit,
            delta: current.abs_delta(),
            premium_total: limit * 100.0 * pos.quantity as f64,
            capital_required: 0.0,
            annualized_yield: 0.0,
            rationale: format!("Take profit: {:.0}% of max premium captured", captured * 100.0),
        });
    }

    // 2) Defend — roll out when tested (high delta) or in-the-money near expiry.
    let abs_delta = resolve_abs_delta(current, underlying.last, d.max(1), cfg.risk_free_rate);
    let itm = match pos.right {
        Right::Put => underlying.last < pos.strike,
        Right::Call => underlying.last > pos.strike,
    };
    let tested = abs_delta.is_some_and(|ad| ad > cfg.roll_delta);

    if tested || (itm && d <= cfg.roll_dte) {
        let to_expiry = today + Duration::days(cfg.max_dte);
        return Some(Suggestion {
            symbol: symbol.to_string(),
            kind: ActionKind::Roll { to_expiry, to_strike: pos.strike },
            right: pos.right,
            strike: pos.strike,
            underlying_price: underlying.last,
            expiry: pos.expiry,
            dte: d,
            quantity: pos.quantity,
            limit_price: round_cents(price),
            delta: abs_delta,
            premium_total: 0.0,
            capital_required: 0.0,
            annualized_yield: 0.0,
            rationale: format!(
                "Defend: Δ{:.2}{}, {}DTE — roll to ~{}DTE for a net credit",
                abs_delta.unwrap_or(0.0),
                if itm { ", ITM" } else { "" },
                d,
                cfg.max_dte
            ),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + Duration::days(n)
    }

    fn short_put(entry_credit: f64) -> OpenShortOption {
        OpenShortOption {
            right: Right::Put,
            strike: 95.0,
            expiry: day(20),
            entry_credit,
            quantity: 1,
        }
    }

    fn quote(price: f64, delta: f64) -> OptionQuote {
        OptionQuote {
            right: Right::Put,
            strike: 95.0,
            expiry: day(20),
            bid: price - 0.02,
            ask: price + 0.02,
            delta: Some(-delta),
            implied_volatility: Some(0.3),
            open_interest: Some(500),
        }
    }

    #[test]
    fn takes_profit_at_threshold() {
        let pos = short_put(2.00);
        let q = quote(0.90, 0.20); // 55% captured
        let s = manage_short_option(
            "AAPL",
            &pos,
            &q,
            UnderlyingQuote { last: 99.0 },
            &EngineConfig::default(),
            day(0),
        )
        .expect("close suggestion");
        assert_eq!(s.kind, ActionKind::CloseForProfit);
    }

    #[test]
    fn rolls_when_tested_and_itm() {
        let pos = short_put(2.00);
        let q = quote(2.50, 0.62); // lost value, high delta
        let s = manage_short_option(
            "AAPL",
            &pos,
            &q,
            UnderlyingQuote { last: 90.0 }, // below strike → ITM put
            &EngineConfig::default(),
            day(0),
        )
        .expect("roll suggestion");
        assert!(matches!(s.kind, ActionKind::Roll { .. }));
    }

    #[test]
    fn holds_when_calm() {
        let pos = short_put(2.00);
        let q = quote(1.50, 0.30); // 25% captured, modest delta
        let s = manage_short_option(
            "AAPL",
            &pos,
            &q,
            UnderlyingQuote { last: 99.0 },
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_none());
    }
}
