//! Reconcile broker holdings into the wheel state machine.
//!
//! Pure, broker-agnostic mapping from flattened [`PositionRow`]s to the
//! [`WheelState`] a symbol is in, plus the share lot / open short the engine
//! needs to advise on it. No I/O lives here, so it is exhaustively unit-tested
//! — the safety net for an otherwise connection-only path.
//!
//! Premium recovery assumes IBKR reports an option's `average_cost` *including*
//! the contract multiplier (the documented TWS behaviour), so per-share entry
//! credit is `average_cost / multiplier`.

use crate::engine::types::{OpenShortOption, Right, SharePosition, WheelState};
use crate::ibkr::PositionRow;
use chrono::NaiveDate;

/// A symbol's wheel position derived purely from broker holdings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconciledPosition {
    pub state: WheelState,
    /// Shares held (for `LongShares` / `ShortCall`).
    pub shares: Option<SharePosition>,
    /// The open short option to manage (for `ShortPut` / `HedgedShortPut` /
    /// `ShortCall`).
    pub open_short: Option<OpenShortOption>,
    /// The protective long put under the short (only for `HedgedShortPut`). Same
    /// expiry, strike below the short. Reuses [`OpenShortOption`] for its
    /// strike/expiry/quantity; its `entry_credit` is the per-share debit *paid*
    /// for the protection (sign isn't meaningful here — it's a long, not a short).
    pub protective_long: Option<OpenShortOption>,
    /// Short calls already written against held shares.
    pub committed_call_contracts: i32,
}

/// Group a symbol's holdings into its current wheel leg.
///
/// An open short is a live obligation, so it's managed before a share lot is
/// treated as coverable — and a short put (assignment risk) outranks a short
/// call. Classification (wheel-centric, holdings-only):
/// - any open short put → `ShortPut` (manage it; shares, if any, ride along)
/// - else any open short call → `ShortCall` (covered or naked — manage it)
/// - else ≥100 shares → `LongShares` (eligible to write calls)
/// - otherwise → `Idle`
///
/// When several short legs of the same right are open, the earliest expiry wins
/// (most urgent to manage). Long option positions are ignored — the wheel only
/// ever holds *short* options.
pub fn reconcile(symbol: &str, rows: &[PositionRow]) -> ReconciledPosition {
    let mine: Vec<&PositionRow> = rows.iter().filter(|r| r.symbol == symbol).collect();

    // --- long stock lot (share-weighted cost basis over long rows) ---
    let mut net_shares = 0.0_f64;
    let mut long_shares = 0.0_f64;
    let mut cost_weight = 0.0_f64;
    for r in mine.iter().filter(|r| is_stock(r)) {
        net_shares += r.position;
        if r.position > 0.0 {
            long_shares += r.position;
            cost_weight += r.position * r.average_cost;
        }
    }
    // Floor, never round: only *whole* shares can back a covered call, so 99.5
    // shares must not be promoted to a 100-share lot.
    let whole_shares = net_shares.floor() as i64;
    let has_lot = whole_shares >= 100;
    let share_lot = has_lot.then(|| SharePosition {
        shares: whole_shares,
        cost_basis: if long_shares > 0.0 { cost_weight / long_shares } else { 0.0 },
    });

    // --- short option legs, earliest expiry first ---
    let mut short_puts = collect_shorts(&mine, Right::Put);
    let mut short_calls = collect_shorts(&mine, Right::Call);
    short_puts.sort_by_key(|o| o.expiry);
    short_calls.sort_by_key(|o| o.expiry);
    let committed_call_contracts: i32 = short_calls.iter().map(|o| o.quantity).sum();
    // Long puts — only used to detect the protective leg of a hedged short put.
    let long_puts = collect_longs(&mine, Right::Put);

    // The wheel models a single open short per symbol. A symbol holding BOTH a
    // short put and a short call (a strangle / manual combo) is out of scope:
    // only the put is surfaced for management below, so flag it rather than
    // silently leaving the other leg unmanaged.
    if !short_puts.is_empty() && !short_calls.is_empty() {
        tracing::warn!(
            "{symbol}: holds both a short put and short call; only the put is managed (multi-leg combos are out of scope)"
        );
    }

    // An open short is a live obligation and must never be dropped, so manage it
    // before treating a lot as coverable. A short put outranks a short call
    // (assignment risk first); writing covered calls on a bare lot is last. The
    // share lot is still carried for the dashboard even while a short is managed.
    if !short_puts.is_empty() {
        let managed = short_puts.into_iter().next().expect("non-empty");
        // A protective long put = SAME expiry, strike strictly BELOW the short
        // (a put credit spread). A long above the short, a different expiry, or a
        // bare long with no short is NOT protective and is ignored — preserving
        // "the wheel only ever holds short options" everywhere but this hedge.
        let protective_long = long_puts
            .into_iter()
            .filter(|l| l.expiry == managed.expiry && l.strike < managed.strike)
            // Tightest protection (highest strike below the short) = the paired leg.
            .max_by(|a, b| a.strike.partial_cmp(&b.strike).unwrap_or(std::cmp::Ordering::Equal));
        let state = if protective_long.is_some() {
            WheelState::HedgedShortPut
        } else {
            WheelState::ShortPut
        };
        ReconciledPosition {
            state,
            shares: share_lot,
            open_short: Some(managed),
            protective_long,
            committed_call_contracts,
        }
    } else if !short_calls.is_empty() {
        ReconciledPosition {
            state: WheelState::ShortCall,
            shares: share_lot,
            open_short: short_calls.into_iter().next(),
            protective_long: None,
            committed_call_contracts,
        }
    } else if has_lot {
        ReconciledPosition {
            state: WheelState::LongShares,
            shares: share_lot,
            open_short: None,
            protective_long: None,
            committed_call_contracts: 0,
        }
    } else {
        ReconciledPosition::default()
    }
}

/// Short option legs of `rows` matching `want`, as [`OpenShortOption`]s. Legs
/// whose expiry can't be parsed are dropped (can't manage what we can't date).
fn collect_shorts(rows: &[&PositionRow], want: Right) -> Vec<OpenShortOption> {
    rows.iter()
        .filter(|r| is_option(r) && r.position < 0.0 && right_of(r) == Some(want))
        .filter_map(|r| {
            let expiry = parse_expiry(&r.expiry)?;
            let mult = multiplier_of(r);
            Some(OpenShortOption {
                right: want,
                strike: r.strike,
                expiry,
                entry_credit: (r.average_cost / mult).abs(),
                quantity: r.position.abs().round() as i32,
            })
        })
        .collect()
}

/// Long option legs of `rows` matching `want` (`position > 0`), reusing
/// [`OpenShortOption`] for strike/expiry/quantity; `entry_credit` carries the
/// per-share debit paid. Mirror of [`collect_shorts`] — used only to find the
/// protective leg of a hedged short put. Undateable expiries are dropped.
fn collect_longs(rows: &[&PositionRow], want: Right) -> Vec<OpenShortOption> {
    rows.iter()
        .filter(|r| is_option(r) && r.position > 0.0 && right_of(r) == Some(want))
        .filter_map(|r| {
            let expiry = parse_expiry(&r.expiry)?;
            let mult = multiplier_of(r);
            Some(OpenShortOption {
                right: want,
                strike: r.strike,
                expiry,
                entry_credit: (r.average_cost / mult).abs(),
                quantity: r.position.abs().round() as i32,
            })
        })
        .collect()
}

fn is_stock(r: &PositionRow) -> bool {
    r.security_type == "Stock"
}

fn is_option(r: &PositionRow) -> bool {
    r.security_type == "Option"
}

/// IBKR `right` strings are any of `P`/`PUT`/`C`/`CALL` (per the crate docs).
fn right_of(r: &PositionRow) -> Option<Right> {
    match r.right.chars().next()?.to_ascii_uppercase() {
        'P' => Some(Right::Put),
        'C' => Some(Right::Call),
        _ => None,
    }
}

/// Contract multiplier, defaulting to 100 (standard equity option) when absent
/// or unparseable, and never zero (avoids a divide-by-zero in credit recovery).
fn multiplier_of(r: &PositionRow) -> f64 {
    match r.multiplier.parse::<f64>() {
        Ok(m) if m > 0.0 => m,
        _ => 100.0,
    }
}

fn parse_expiry(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y%m%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stock(symbol: &str, position: f64, average_cost: f64) -> PositionRow {
        PositionRow {
            account: "DU1".into(),
            symbol: symbol.into(),
            security_type: "Stock".into(),
            right: String::new(),
            strike: 0.0,
            expiry: String::new(),
            position,
            average_cost,
            multiplier: String::new(),
        }
    }

    fn option(
        symbol: &str,
        right: &str,
        strike: f64,
        expiry: &str,
        position: f64,
        average_cost: f64,
    ) -> PositionRow {
        PositionRow {
            account: "DU1".into(),
            symbol: symbol.into(),
            security_type: "Option".into(),
            right: right.into(),
            strike,
            expiry: expiry.into(),
            position,
            average_cost,
            multiplier: "100".into(),
        }
    }

    #[test]
    fn empty_is_idle() {
        let r = reconcile("AAPL", &[]);
        assert_eq!(r.state, WheelState::Idle);
        assert!(r.shares.is_none() && r.open_short.is_none());
    }

    #[test]
    fn hundred_shares_is_long_shares() {
        let rows = vec![stock("AAPL", 100.0, 93.25)];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::LongShares);
        let lot = r.shares.expect("share lot");
        assert_eq!(lot.shares, 100);
        assert!((lot.cost_basis - 93.25).abs() < 1e-9);
        assert_eq!(r.committed_call_contracts, 0);
    }

    #[test]
    fn short_put_recovers_per_share_credit() {
        // average_cost 215.0 / multiplier 100 = $2.15 per-share credit.
        let rows = vec![option("MSFT", "P", 400.0, "20260116", -2.0, 215.0)];
        let r = reconcile("MSFT", &rows);
        assert_eq!(r.state, WheelState::ShortPut);
        let s = r.open_short.expect("open short");
        assert_eq!(s.right, Right::Put);
        assert_eq!(s.quantity, 2);
        assert!((s.entry_credit - 2.15).abs() < 1e-9);
    }

    #[test]
    fn shares_plus_short_call_is_covered_call() {
        let rows = vec![
            stock("AAPL", 100.0, 90.0),
            option("AAPL", "C", 100.0, "20260220", -1.0, 180.0),
        ];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::ShortCall);
        assert!(r.shares.is_some());
        assert_eq!(r.committed_call_contracts, 1);
        assert_eq!(r.open_short.expect("call").right, Right::Call);
    }

    #[test]
    fn open_short_put_outranks_share_lot() {
        // Partial assignment: holding a 100-share lot AND a still-open short put.
        // The put must keep being managed, not be dropped for a covered call.
        let rows = vec![
            stock("AAPL", 100.0, 90.0),
            option("AAPL", "P", 85.0, "20260116", -1.0, 120.0),
        ];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::ShortPut);
        assert_eq!(r.open_short.expect("put").strike, 85.0);
        assert!(r.shares.is_some(), "the lot still rides along for the dashboard");
    }

    #[test]
    fn naked_short_call_still_managed() {
        let rows = vec![option("NVDA", "CALL", 1000.0, "20260116", -1.0, 500.0)];
        let r = reconcile("NVDA", &rows);
        assert_eq!(r.state, WheelState::ShortCall);
        assert!(r.shares.is_none());
    }

    #[test]
    fn partial_lot_with_short_put_is_short_put() {
        // 50 shares is not a coverable lot, so an open short put dominates.
        let rows = vec![
            stock("AAPL", 50.0, 90.0),
            option("AAPL", "P", 85.0, "20260116", -1.0, 120.0),
        ];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::ShortPut);
        assert!(r.shares.is_none());
    }

    #[test]
    fn earliest_expiry_short_leg_wins() {
        let rows = vec![
            option("AAPL", "P", 90.0, "20260220", -1.0, 100.0),
            option("AAPL", "P", 88.0, "20260116", -1.0, 90.0),
        ];
        let r = reconcile("AAPL", &rows);
        let s = r.open_short.expect("short");
        assert_eq!(s.expiry, NaiveDate::from_ymd_opt(2026, 1, 16).unwrap());
        assert_eq!(s.strike, 88.0);
    }

    #[test]
    fn ignores_other_symbols_and_long_options() {
        let rows = vec![
            stock("MSFT", 100.0, 400.0),                       // different symbol
            option("AAPL", "P", 90.0, "20260116", 1.0, 100.0), // long option (position > 0)
        ];
        let r = reconcile("AAPL", &rows);
        // A *bare* long put (no short to protect) is still not a wheel position.
        assert_eq!(r.state, WheelState::Idle);
        assert!(r.protective_long.is_none());
    }

    #[test]
    fn short_put_with_long_below_is_a_hedged_spread() {
        // Sold the $100 put, bought the $95 put (same expiry) = put credit spread.
        let rows = vec![
            option("AAPL", "P", 100.0, "20260116", -1.0, 250.0), // short
            option("AAPL", "P", 95.0, "20260116", 1.0, 100.0),   // protective long
        ];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::HedgedShortPut);
        assert_eq!(r.open_short.expect("short").strike, 100.0);
        let long = r.protective_long.expect("protective long");
        assert_eq!(long.strike, 95.0);
        assert!((long.entry_credit - 1.00).abs() < 1e-9); // 100/100 debit paid
    }

    #[test]
    fn long_put_above_the_short_is_not_protective() {
        // A long ABOVE the short doesn't cap downside → plain ShortPut, not hedged.
        let rows = vec![
            option("AAPL", "P", 100.0, "20260116", -1.0, 250.0),
            option("AAPL", "P", 105.0, "20260116", 1.0, 600.0),
        ];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::ShortPut);
        assert!(r.protective_long.is_none());
    }

    #[test]
    fn long_put_at_a_different_expiry_is_not_protective() {
        // A diagonal isn't the defined-risk spread we model → not hedged.
        let rows = vec![
            option("AAPL", "P", 100.0, "20260116", -1.0, 250.0),
            option("AAPL", "P", 95.0, "20260220", 1.0, 120.0), // later expiry
        ];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::ShortPut);
        assert!(r.protective_long.is_none());
    }

    #[test]
    fn tightest_long_is_chosen_as_the_protective_leg() {
        // Two longs below the short → the highest (tightest) is the paired leg.
        let rows = vec![
            option("AAPL", "P", 100.0, "20260116", -1.0, 250.0),
            option("AAPL", "P", 95.0, "20260116", 1.0, 100.0),
            option("AAPL", "P", 90.0, "20260116", 1.0, 50.0),
        ];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::HedgedShortPut);
        assert_eq!(r.protective_long.expect("long").strike, 95.0);
    }

    #[test]
    fn fractional_holding_under_a_lot_is_not_covered() {
        // 99.5 shares must not be rounded up into a coverable 100-share lot.
        let rows = vec![stock("AAPL", 99.5, 90.0)];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::Idle);
        assert!(r.shares.is_none());
    }

    #[test]
    fn fractional_above_a_lot_keeps_only_whole_shares() {
        // 100.9 shares backs exactly one lot; the fractional remainder is dropped.
        let rows = vec![stock("AAPL", 100.9, 90.0)];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::LongShares);
        assert_eq!(r.shares.expect("lot").shares, 100);
    }

    #[test]
    fn unparseable_option_expiry_is_dropped() {
        // A contract-month-only expiry (YYYYMM) can't be dated → no short leg.
        let rows = vec![option("AAPL", "P", 90.0, "202601", -1.0, 100.0)];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::Idle);
    }

    #[test]
    fn multiple_long_lots_use_share_weighted_cost_basis() {
        // 100 @ $90 + 100 @ $110 → 200 shares, weighted basis $100.
        let rows = vec![stock("AAPL", 100.0, 90.0), stock("AAPL", 100.0, 110.0)];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::LongShares);
        let lot = r.shares.expect("lot");
        assert_eq!(lot.shares, 200);
        assert!((lot.cost_basis - 100.0).abs() < 1e-9, "basis {}", lot.cost_basis);
    }

    #[test]
    fn short_stock_is_not_a_coverable_lot() {
        // The wheel never shorts stock; a negative net must not become a lot.
        let rows = vec![stock("AAPL", -100.0, 90.0)];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.state, WheelState::Idle);
        assert!(r.shares.is_none());
    }

    #[test]
    fn missing_or_zero_multiplier_defaults_to_100() {
        // average_cost 250 with no multiplier → per-share credit 250/100 = 2.50.
        let mut empty = option("MSFT", "P", 400.0, "20260116", -1.0, 250.0);
        empty.multiplier = String::new();
        assert!((reconcile("MSFT", &[empty]).open_short.unwrap().entry_credit - 2.50).abs() < 1e-9);

        let mut zero = option("MSFT", "P", 400.0, "20260116", -1.0, 250.0);
        zero.multiplier = "0".into(); // never divide by zero
        assert!((reconcile("MSFT", &[zero]).open_short.unwrap().entry_credit - 2.50).abs() < 1e-9);
    }

    #[test]
    fn multi_contract_short_put_reports_quantity() {
        let rows = vec![option("AAPL", "P", 90.0, "20260116", -3.0, 90.0)];
        let r = reconcile("AAPL", &rows);
        assert_eq!(r.open_short.expect("short").quantity, 3);
    }
}
