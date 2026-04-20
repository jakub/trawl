// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Short relative-time labels for history rows and similar lists.
//!
//! Pure function so native `cargo test` exercises the buckets. Only
//! consumer is the wasm `HistoryPage`, so we silence the dead-code
//! warning on native targets the same way `query_merge.rs` does.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use chrono::{DateTime, Datelike, TimeZone, Utc};

/// Format an ISO8601 UTC timestamp relative to `now_ms` (ms since epoch).
///
/// Buckets:
/// - `< 60s`   → `"just now"`
/// - `< 1h`    → `"Nm ago"`
/// - `< 24h`   → `"Nh ago"`
/// - `< 7d`    → `"Nd ago"`
/// - `>= 7d`   → `"Mon D"` (e.g. `"Apr 11"`)
///
/// Returns the raw input unchanged if it can't be parsed as RFC3339 —
/// better to show something imperfect than a panicky `"?"`.
#[must_use]
pub fn time_ago(iso: &str, now_ms: i64) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(iso) else {
        return iso.to_string();
    };
    let then_ms = parsed.timestamp_millis();
    let delta = now_ms.saturating_sub(then_ms);

    // Future timestamps (clock skew / bad data) — treat as "just now".
    if delta < 60_000 {
        return "just now".to_string();
    }

    let secs = delta / 1_000;
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;

    if mins < 60 {
        return format!("{mins}m ago");
    }
    if hours < 24 {
        return format!("{hours}h ago");
    }
    if days < 7 {
        return format!("{days}d ago");
    }

    // Calendar fallback: "Apr 11". Use the parsed timestamp's own UTC
    // calendar — good enough for a list label.
    let d: DateTime<Utc> = Utc
        .timestamp_millis_opt(then_ms)
        .single()
        .unwrap_or_else(Utc::now);
    format!("{} {}", month_abbrev(d.month()), d.day())
}

/// Compact duration: `"0.482s"` for >=10ms, `"3ms"` for sub-10ms.
#[must_use]
pub fn format_duration(ms: u64) -> String {
    if ms < 10 {
        return format!("{ms}ms");
    }
    let s = ms / 1000;
    let frac = ms % 1000;
    format!("{s}.{frac:03}s")
}

fn month_abbrev(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference moment: 2026-04-18T12:00:00Z.
    fn now() -> i64 {
        DateTime::parse_from_rfc3339("2026-04-18T12:00:00Z")
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn just_now_under_minute() {
        assert_eq!(time_ago("2026-04-18T11:59:30Z", now()), "just now");
    }

    #[test]
    fn future_clock_skew_is_just_now() {
        assert_eq!(time_ago("2026-04-18T12:00:30Z", now()), "just now");
    }

    #[test]
    fn minutes_bucket() {
        assert_eq!(time_ago("2026-04-18T11:55:00Z", now()), "5m ago");
        assert_eq!(time_ago("2026-04-18T11:01:00Z", now()), "59m ago");
    }

    #[test]
    fn hours_bucket() {
        assert_eq!(time_ago("2026-04-18T10:00:00Z", now()), "2h ago");
        assert_eq!(time_ago("2026-04-17T13:00:00Z", now()), "23h ago");
    }

    #[test]
    fn days_bucket() {
        assert_eq!(time_ago("2026-04-16T12:00:00Z", now()), "2d ago");
        assert_eq!(time_ago("2026-04-13T12:00:01Z", now()), "4d ago");
    }

    #[test]
    fn calendar_fallback_beyond_week() {
        // 10 days back → "Apr 8".
        assert_eq!(time_ago("2026-04-08T12:00:00Z", now()), "Apr 8");
    }

    #[test]
    fn malformed_input_passthrough() {
        assert_eq!(time_ago("not a date", now()), "not a date");
    }
}
