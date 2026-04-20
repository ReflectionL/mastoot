//! Relative-time formatting for timeline headers.
//!
//! Matches the Phanpy / Ice Cubes convention:
//! - `<1m`  → `now`
//! - `<1h`  → `42m`
//! - `<1d`  → `3h`
//! - `<7d`  → `2d`
//! - else   → `Jan 15` (or `Jan 15, 2024` if the year differs)
//!
//! Keep this logic pure — no locale formatting, no timezone conversion. The
//! Mastodon API always returns UTC; UI code feeds `Utc::now()` as `now`.

use chrono::{DateTime, Datelike, Utc};

/// Format a past timestamp relative to `now`. Future timestamps (clock
/// skew, edits) collapse to `now`.
#[must_use]
pub fn relative(now: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(ts);
    let secs = delta.num_seconds();

    if secs < 60 {
        return "now".to_string();
    }
    let mins = delta.num_minutes();
    if mins < 60 {
        return format!("{mins}m");
    }
    let hrs = delta.num_hours();
    if hrs < 24 {
        return format!("{hrs}h");
    }
    let days = delta.num_days();
    if days < 7 {
        return format!("{days}d");
    }

    if ts.year() == now.year() {
        ts.format("%b %e").to_string().trim().to_string()
    } else {
        ts.format("%b %e, %Y").to_string().trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn under_a_minute_is_now() {
        // 30s delta rounds to "now"; exactly 60s already tips into "1m".
        let n = utc(2026, 4, 17, 12, 0);
        let ts = n - chrono::Duration::seconds(30);
        assert_eq!(relative(n, ts), "now");
    }

    #[test]
    fn minutes() {
        let n = utc(2026, 4, 17, 12, 30);
        assert_eq!(relative(n, utc(2026, 4, 17, 12, 0)), "30m");
    }

    #[test]
    fn hours() {
        let n = utc(2026, 4, 17, 12, 0);
        assert_eq!(relative(n, utc(2026, 4, 17, 8, 0)), "4h");
    }

    #[test]
    fn days() {
        let n = utc(2026, 4, 17, 12, 0);
        assert_eq!(relative(n, utc(2026, 4, 14, 12, 0)), "3d");
    }

    #[test]
    fn week_falls_back_to_date_same_year() {
        let n = utc(2026, 4, 17, 12, 0);
        // `%e` is space-padded day-of-month on BSD/macOS; trim removes the pad.
        assert_eq!(relative(n, utc(2026, 1, 15, 12, 0)), "Jan 15");
    }

    #[test]
    fn different_year_includes_year() {
        let n = utc(2026, 4, 17, 12, 0);
        assert_eq!(relative(n, utc(2024, 1, 15, 12, 0)), "Jan 15, 2024");
    }

    #[test]
    fn future_ts_collapses_to_now() {
        let n = utc(2026, 4, 17, 12, 0);
        assert_eq!(relative(n, utc(2026, 4, 17, 13, 0)), "now");
    }
}
