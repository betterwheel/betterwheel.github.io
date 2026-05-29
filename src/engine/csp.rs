//! Cash-secured put selection — the entry leg of the wheel.

use chrono::NaiveDate;

use super::math::{annualized_yield, dte, resolve_abs_delta, round_cents};
use super::types::{ActionKind, EngineConfig, OptionQuote, Right, Suggestion, UnderlyingQuote};

/// Choose the best cash-secured put for `symbol`, or `None` if nothing fits.
///
/// `max_collateral` is the most cash the caller permits to be tied up in this
/// position (already reconciled against available funds and deployment caps),
/// which also sizes the contract quantity.
pub fn select_csp(
    symbol: &str,
    underlying: UnderlyingQuote,
    quotes: &[OptionQuote],
    max_collateral: f64,
    cfg: &EngineConfig,
    today: NaiveDate,
) -> Option<Suggestion> {
    let spot = underlying.last;
    if spot <= 0.0 {
        return None;
    }

    let mut best: Option<(f64, Suggestion)> = None;

    for q in quotes.iter().filter(|q| q.right == Right::Put) {
        // Out-of-the-money puts only.
        if q.strike >= spot {
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

        if let Some(oi) = q.open_interest
            && oi < cfg.min_open_interest {
                continue;
            }

        let Some(abs_delta) = resolve_abs_delta(q, spot, d, cfg.risk_free_rate) else {
            continue;
        };
        if abs_delta < cfg.min_delta || abs_delta > cfg.max_delta {
            continue;
        }

        // Cash-secured sizing.
        let collateral_per_contract = q.strike * 100.0;
        let qty = (max_collateral / collateral_per_contract).floor() as i32;
        if qty < 1 {
            continue;
        }

        let ann = annualized_yield(premium, q.strike, d);
        if ann < cfg.min_annualized_yield {
            continue;
        }

        let limit = round_cents(premium);
        let suggestion = Suggestion {
            symbol: symbol.to_string(),
            kind: ActionKind::SellPut,
            right: Right::Put,
            strike: q.strike,
            underlying_price: spot,
            expiry: q.expiry,
            dte: d,
            quantity: qty,
            limit_price: limit,
            delta: Some(abs_delta),
            premium_total: limit * 100.0 * qty as f64,
            capital_required: collateral_per_contract * qty as f64,
            annualized_yield: ann,
            rationale: format!(
                "CSP {:.2}Δ, {}DTE, ${:.2} credit → {:.1}% annualized",
                abs_delta,
                d,
                premium,
                ann * 100.0
            ),
        };

        // Maximize yield, then prefer the strike whose delta is nearest target.
        let score = ann - 0.001 * (abs_delta - cfg.target_delta).abs();
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, suggestion));
        }
    }

    best.map(|(_, s)| s)
}

/// Choose the best defined-risk **put credit spread** for `symbol` (the Hedged
/// Wheel's entry): a short put picked with the same gates as [`select_csp`],
/// hedged by a cheaper long put further OTM drawn from the SAME `quotes`. Max loss
/// is capped to the spread width minus the net credit. `None` if nothing fits.
///
/// `max_collateral` sizes the position against the short strike's notional —
/// exactly as [`select_csp`] does — so the Hedged Wheel opens the SAME size the
/// Classic Wheel would, just with a protective long wing. The capital actually
/// held is only the spread width (reported as `capital_required`).
pub fn select_put_spread(
    symbol: &str,
    underlying: UnderlyingQuote,
    quotes: &[OptionQuote],
    max_collateral: f64,
    cfg: &EngineConfig,
    today: NaiveDate,
) -> Option<Suggestion> {
    let spot = underlying.last;
    if spot <= 0.0 {
        return None;
    }

    let mut best: Option<(f64, Suggestion)> = None;

    for short in quotes.iter().filter(|q| q.right == Right::Put) {
        // Short leg: the same filters a cash-secured put would pass.
        if short.strike >= spot {
            continue;
        }
        let d = dte(today, short.expiry);
        if d < cfg.min_dte || d > cfg.max_dte {
            continue;
        }
        let short_premium = short.mid();
        if short_premium < cfg.min_premium {
            continue;
        }
        if let Some(oi) = short.open_interest
            && oi < cfg.min_open_interest
        {
            continue;
        }
        let Some(abs_delta) = resolve_abs_delta(short, spot, d, cfg.risk_free_rate) else {
            continue;
        };
        if abs_delta < cfg.min_delta || abs_delta > cfg.max_delta {
            continue;
        }

        // Long leg: a cheaper put no more than `hedge_pct_below` under the short —
        // a *tight* hedge, never a far-away one that barely caps risk. Among those,
        // take the widest (lowest strike) within the cap for the most protection
        // and credit. Same expiry.
        let floor = short.strike * (1.0 - cfg.hedge_pct_below);
        let Some(long) = quotes
            .iter()
            .filter(|q| {
                q.right == Right::Put
                    && q.expiry == short.expiry
                    && q.strike < short.strike
                    && q.strike >= floor
                    && q.mid() > 0.0
                    && q.mid() < short_premium
            })
            .min_by(|a, b| a.strike.partial_cmp(&b.strike).unwrap_or(std::cmp::Ordering::Equal))
        else {
            continue; // no protective leg within the hedge cap from the sampled chain
        };

        let width = short.strike - long.strike;
        if width <= 0.0 {
            continue;
        }
        let net_credit = short_premium - long.mid();
        if net_credit < cfg.hedge_min_credit {
            continue;
        }

        // Size the position exactly as the equivalent cash-secured put would —
        // against the short strike's notional — so the Hedged Wheel takes the
        // SAME position the Classic Wheel would, just with a protective long wing
        // capping the tail. (Sizing by the tiny spread width instead would let
        // the freed-up capital balloon the contract count into a big leveraged
        // bet, which defeats the whole point of hedging.)
        let qty = (max_collateral / (short.strike * 100.0)).floor() as i32;
        if qty < 1 {
            continue;
        }

        // Defined risk / margin actually held ≈ width × 100 × qty.
        let capital_required = width * 100.0 * qty as f64;

        // Yield is on the at-risk capital (the width), not the full strike.
        let ann = annualized_yield(net_credit, width, d);
        if ann < cfg.min_annualized_yield {
            continue;
        }

        let net = round_cents(net_credit);
        let long_price = round_cents(long.mid());
        let suggestion = Suggestion {
            symbol: symbol.to_string(),
            kind: ActionKind::SellPutSpread { long_strike: long.strike, long_price },
            right: Right::Put,
            strike: short.strike,
            underlying_price: spot,
            expiry: short.expiry,
            dte: d,
            quantity: qty,
            limit_price: net, // net credit per share for the whole spread
            delta: Some(abs_delta),
            premium_total: net * 100.0 * qty as f64,
            capital_required,
            annualized_yield: ann,
            rationale: format!(
                "Put spread {:.1}/{:.1} ({abs_delta:.2}Δ short), {d}DTE, ${net:.2} credit, ${:.0} width → {:.1}% annualized, max loss ${:.0}",
                short.strike,
                long.strike,
                width * 100.0,
                ann * 100.0,
                (width - net) * 100.0 * qty as f64,
            ),
        };
        // Prefer yield, then the short delta nearest target (same as CSP scoring).
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

    fn put(strike: f64, delta: f64, premium: f64) -> OptionQuote {
        OptionQuote {
            right: Right::Put,
            strike,
            expiry: day(30),
            bid: premium - 0.05,
            ask: premium + 0.05,
            delta: Some(-delta), // puts report negative delta
            implied_volatility: Some(0.3),
            open_interest: Some(500),
            volume: Some(100),
        }
    }

    #[test]
    fn picks_best_yield_within_delta_band() {
        let quotes = vec![
            put(90.0, 0.20, 1.00), // yield ~0.135 — too low
            put(92.0, 0.25, 1.60), // yield ~0.211 — ok
            put(95.0, 0.32, 1.80), // yield ~0.230 — best & closest to 0.30
            put(98.0, 0.45, 2.50), // delta out of band
        ];
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &quotes,
            10_000.0,
            &EngineConfig::default(),
            day(0),
        )
        .expect("a suggestion");
        assert_eq!(s.strike, 95.0);
        assert_eq!(s.quantity, 1); // floor(10000 / 9500)
        assert!((s.capital_required - 9500.0).abs() < 1e-6);
        assert_eq!(s.kind, ActionKind::SellPut);
    }

    #[test]
    fn rejects_when_not_cash_secured() {
        let quotes = vec![put(95.0, 0.32, 1.80)];
        // Budget can't cover even one contract (95 * 100 = 9500).
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &quotes,
            5_000.0,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_none());
    }

    #[test]
    fn rejects_low_liquidity() {
        let mut q = put(95.0, 0.32, 1.80);
        q.open_interest = Some(10); // below default floor of 100
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &[q],
            10_000.0,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_none());
    }

    #[test]
    fn falls_back_to_bs_delta_when_greek_missing() {
        // No reported delta, but IV present → BS estimate keeps it in the band.
        let mut q = put(95.0, 0.0, 1.80);
        q.delta = None;
        let s = select_csp(
            "AAPL",
            UnderlyingQuote { last: 100.0 },
            &[q],
            10_000.0,
            &EngineConfig::default(),
            day(0),
        );
        assert!(s.is_some(), "BS fallback should resolve a delta");
    }

    #[test]
    fn put_spread_picks_a_tight_protective_leg_within_the_cap() {
        let quotes = vec![
            put(100.0, 0.30, 2.50), // short, in the delta band (spot 105)
            put(96.0, 0.22, 1.00),  // long: 4% below → within the 5% hedge cap
            put(90.0, 0.12, 0.40),  // far hedge (10% below) → must NOT be chosen
        ];
        let s = select_put_spread(
            "AAPL",
            UnderlyingQuote { last: 105.0 },
            &quotes,
            100_000.0,
            &EngineConfig::default(),
            day(0),
        )
        .expect("a spread");
        assert_eq!(
            s.kind,
            ActionKind::SellPutSpread { long_strike: 96.0, long_price: 1.00 }
        );
        assert_eq!(s.strike, 100.0);
        // Sized like the equivalent CSP — against the short strike's notional, not
        // the width — so qty = floor(100000 / (100 × 100)) = 10. The defined risk
        // (margin actually held) is then width × 100 × qty = 4 × 100 × 10 = $4,000.
        assert_eq!(s.quantity, 10);
        assert!((s.capital_required - 4000.0).abs() < 1e-6);
        assert!((s.limit_price - 1.50).abs() < 1e-9); // net credit = 2.50 − 1.00
    }

    #[test]
    fn put_spread_needs_a_cheaper_protective_leg() {
        // Only the short put in the chain — no long leg to buy → no spread.
        let quotes = vec![put(95.0, 0.30, 1.80)];
        assert!(
            select_put_spread(
                "AAPL",
                UnderlyingQuote { last: 100.0 },
                &quotes,
                10_000.0,
                &EngineConfig::default(),
                day(0),
            )
            .is_none()
        );
    }
}
