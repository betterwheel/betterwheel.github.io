//! Pure scheduling helpers for 0DTE auto-management: US/Eastern market-time
//! conversion (no `chrono-tz` dependency — the US DST rule is small and fixed)
//! and the entry/time-stop decisions. No I/O, fully unit-testable.
//!
//! Holidays are not modelled: on a market holiday `minutes_since_open` still
//! reports a session, so a scheduled entry would be *attempted* and simply fail
//! at the broker (no fill) — never a wrong trade, just a wasted what-if. Good
//! enough for an MVP; a holiday calendar can be layered on later.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};

/// Minutes from midnight to the regular-session open (09:30 ET) and close (16:00).
const OPEN_MINUTES: i64 = 9 * 60 + 30;
const CLOSE_MINUTES: i64 = 16 * 60;

/// The date of the `n`-th `weekday` (1-based) in `month`/`year`.
fn nth_weekday(year: i32, month: u32, weekday: Weekday, n: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid first-of-month");
    let shift = (7 + weekday.num_days_from_sunday() - first.weekday().num_days_from_sunday()) % 7;
    let day = 1 + shift + (n - 1) * 7;
    NaiveDate::from_ymd_opt(year, month, day).expect("valid nth-weekday date")
}

/// Whether a UTC instant is in US Eastern daylight time (EDT, UTC−4); else EST
/// (UTC−5). US DST runs from 02:00 local on the 2nd Sunday of March (07:00 UTC)
/// to 02:00 local on the 1st Sunday of November (06:00 UTC).
fn is_us_dst(utc: NaiveDateTime) -> bool {
    let y = utc.year();
    let start = nth_weekday(y, 3, Weekday::Sun, 2)
        .and_hms_opt(7, 0, 0)
        .unwrap();
    let end = nth_weekday(y, 11, Weekday::Sun, 1)
        .and_hms_opt(6, 0, 0)
        .unwrap();
    utc >= start && utc < end
}

/// US/Eastern wall-clock time for a UTC instant. Pass `chrono::Local::now()
/// .naive_utc()` from the app.
pub fn eastern_wall(utc: NaiveDateTime) -> NaiveDateTime {
    let offset = if is_us_dst(utc) { 4 } else { 5 };
    utc - Duration::hours(offset)
}

/// Minutes since the 09:30 ET open if `et` is within a weekday regular session
/// (09:30–16:00 ET); otherwise `None`.
pub fn minutes_since_open(et: NaiveDateTime) -> Option<i64> {
    if matches!(et.weekday(), Weekday::Sat | Weekday::Sun) {
        return None;
    }
    let mins = et.time().hour() as i64 * 60 + et.time().minute() as i64;
    (OPEN_MINUTES..=CLOSE_MINUTES).contains(&mins).then_some(mins - OPEN_MINUTES)
}

/// Whether a slot should open a (new) position now: within RTH, at/after its
/// entry offset, and either never entered today (single entry) or — in MEIC mode
/// (`meic_interval > 0`) — at least `meic_interval` minutes since the last entry.
pub fn should_enter(
    now_et: NaiveDateTime,
    entry_minutes: i64,
    meic_interval: i64,
    last_entry_today: Option<NaiveDateTime>,
) -> bool {
    let Some(since_open) = minutes_since_open(now_et) else {
        return false;
    };
    if since_open < entry_minutes {
        return false;
    }
    match last_entry_today {
        None => true,
        Some(last) => meic_interval > 0 && (now_et - last).num_minutes() >= meic_interval,
    }
}

/// Whether `et` has reached the configured `HH:MM` (ET) time-stop. A malformed
/// time never triggers (returns `false`).
pub fn past_time_stop(now_et: NaiveDateTime, hhmm: &str) -> bool {
    match NaiveTime::parse_from_str(hhmm, "%H:%M") {
        Ok(t) => now_et.time() >= t,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn dst_boundaries_2026() {
        // 2026: DST starts Sun Mar 8, ends Sun Nov 1.
        // Jan (EST, −5): 14:30 UTC → 09:30 ET.
        assert_eq!(eastern_wall(dt("2026-01-15 14:30")).time(), NaiveTime::from_hms_opt(9, 30, 0).unwrap());
        // Jul (EDT, −4): 13:30 UTC → 09:30 ET.
        assert_eq!(eastern_wall(dt("2026-07-15 13:30")).time(), NaiveTime::from_hms_opt(9, 30, 0).unwrap());
    }

    #[test]
    fn rth_window_only_on_weekdays() {
        // A summer weekday at 13:45 UTC = 09:45 ET → 15 min after open.
        assert_eq!(minutes_since_open(eastern_wall(dt("2026-07-15 13:45"))), Some(15));
        // Before open (09:00 ET) and after close (16:30 ET) → None.
        assert_eq!(minutes_since_open(eastern_wall(dt("2026-07-15 13:00"))), None);
        assert_eq!(minutes_since_open(eastern_wall(dt("2026-07-15 20:30"))), None);
        // Saturday → None even mid-session.
        assert_eq!(minutes_since_open(eastern_wall(dt("2026-07-18 14:00"))), None);
    }

    #[test]
    fn single_entry_fires_once_after_offset() {
        let et = eastern_wall(dt("2026-07-15 14:20")); // 10:20 ET → 50 min after open
        // entry_minutes 45, single (meic 0), never entered → enter.
        assert!(should_enter(et, 45, 0, None));
        // Already entered today → don't re-enter (single).
        assert!(!should_enter(et, 45, 0, Some(eastern_wall(dt("2026-07-15 14:15")))));
        // Before the offset → no.
        let early = eastern_wall(dt("2026-07-15 14:00")); // 30 min after open
        assert!(!should_enter(early, 45, 0, None));
    }

    #[test]
    fn meic_re_enters_after_interval() {
        let et = eastern_wall(dt("2026-07-15 15:30")); // 11:30 ET
        let last = eastern_wall(dt("2026-07-15 15:00")); // 30 min earlier
        assert!(should_enter(et, 45, 30, Some(last))); // interval elapsed
        let recent = eastern_wall(dt("2026-07-15 15:20")); // 10 min earlier
        assert!(!should_enter(et, 45, 30, Some(recent))); // too soon
    }

    #[test]
    fn time_stop_triggers_at_or_after() {
        let et = eastern_wall(dt("2026-07-15 19:35")); // 15:35 ET
        assert!(past_time_stop(et, "15:30"));
        assert!(!past_time_stop(et, "15:45"));
        assert!(!past_time_stop(et, "garbage"));
    }
}
