// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-window domain types (ADR-0018 rulings 6-14).
//!
//! The window is a property of the SCHEDULE, not of the saved query text: a
//! schedule either has no window (the legacy shape, where the DSL is executed
//! verbatim), tiles `[previous window_end, fire - lag)` under
//! `window = "since_last"`, or takes a fixed trailing span under
//! `window = "<duration>"`. This module owns only the types and the two
//! spellings of an instant; the planner that turns a schedule plus a fire
//! time into a [`ReportWindow`] arrives with the scheduler wiring.

use std::fmt;

use chrono::{DateTime, SecondsFormat, SubsecRound as _, Utc};

use crate::store::{StoreError, format_interval, parse_interval};

/// The window mode configured on a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleWindow {
    /// Tile forward from the last successful run's `window_end`.
    SinceLast,
    /// A fixed trailing span, re-measured from every fire time.
    Fixed {
        /// Span length in seconds (at least 60, like the schedule interval).
        secs: u64,
    },
}

impl ScheduleWindow {
    /// The persisted `schedules.window_kind` spelling, matched by the
    /// migration's `schedules_window_kind` CHECK.
    pub fn kind(self) -> &'static str {
        match self {
            Self::SinceLast => "since_last",
            Self::Fixed { .. } => "fixed",
        }
    }

    /// The persisted `schedules.window_secs` value: a fixed window carries
    /// its span, `since_last` carries nothing.
    pub fn secs(self) -> Option<u64> {
        match self {
            Self::SinceLast => None,
            Self::Fixed { secs } => Some(secs),
        }
    }

    /// Parse the wire spelling: the literal `since_last`, or a duration in
    /// the schedule's own grammar.
    ///
    /// A fixed window takes the interval's 60s floor. A window shorter than
    /// the minimum interval would describe a report nothing can be scheduled
    /// to produce, and the two numbers are the same kind of span.
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        let s = s.trim();
        if s == "since_last" {
            return Ok(Self::SinceLast);
        }
        Ok(Self::Fixed {
            secs: parse_interval(s)?,
        })
    }
}

impl fmt::Display for ScheduleWindow {
    /// Renders back to what [`ScheduleWindow::parse`] accepts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SinceLast => f.write_str("since_last"),
            Self::Fixed { secs } => f.write_str(&format_interval(*secs)),
        }
    }
}

/// The half-open interval `[start, end)` one run covers (ADR-0018 ruling 10).
///
/// `truncated` records that a `since_last` catch-up gap exceeded
/// `max_catchup_intervals` and the start was clamped forward, so the run
/// reports the coverage it actually has instead of silently losing the gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportWindow {
    /// Inclusive lower bound.
    pub start: DateTime<Utc>,
    /// Exclusive upper bound.
    pub end: DateTime<Utc>,
    /// Whether `start` was clamped forward past an uncovered gap.
    pub truncated: bool,
}

/// Render a window bound as the DSL/wire text: RFC 3339, UTC, microseconds.
///
/// One rendering for both bounds and every consumer. `earliest=`/`latest=`
/// text, the `report_runs` row and `DuckDB`'s own parameter all have to name
/// the same instant, and a rendering that dropped digits would move a bound
/// by up to a second per hop.
pub fn format_window_bound(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Truncate an instant to microseconds, the resolution every hop shares.
///
/// Postgres `TIMESTAMPTZ` and `DuckDB` `TIMESTAMP` both store microseconds,
/// and [`format_window_bound`] prints six digits. A `chrono` instant carries
/// nanoseconds, so truncating at ONE point is what keeps the stored
/// watermark, the executed DSL and the next window's start naming one
/// instant rather than three that differ below the microsecond.
pub fn truncate_to_micros(t: DateTime<Utc>) -> DateTime<Utc> {
    t.trunc_subsecs(6)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    #[test]
    fn parse_since_last_and_fixed() {
        assert_eq!(
            ScheduleWindow::parse("since_last").unwrap(),
            ScheduleWindow::SinceLast
        );
        assert_eq!(
            ScheduleWindow::parse(" since_last ").unwrap(),
            ScheduleWindow::SinceLast
        );
        assert_eq!(
            ScheduleWindow::parse("2h").unwrap(),
            ScheduleWindow::Fixed { secs: 7200 }
        );
    }

    #[test]
    fn parse_rejects_short_and_malformed() {
        assert!(matches!(
            ScheduleWindow::parse("30s"),
            Err(StoreError::IntervalTooShort { secs: 30 })
        ));
        assert!(matches!(
            ScheduleWindow::parse("last"),
            Err(StoreError::InvalidInterval { .. })
        ));
        assert!(ScheduleWindow::parse("").is_err());
    }

    #[test]
    fn kind_secs_and_display_round_trip() {
        let fixed = ScheduleWindow::Fixed { secs: 7200 };
        assert_eq!(fixed.kind(), "fixed");
        assert_eq!(fixed.secs(), Some(7200));
        assert_eq!(fixed.to_string(), "2h");
        assert_eq!(ScheduleWindow::parse(&fixed.to_string()).unwrap(), fixed);

        let since = ScheduleWindow::SinceLast;
        assert_eq!(since.kind(), "since_last");
        assert_eq!(since.secs(), None);
        assert_eq!(since.to_string(), "since_last");
        assert_eq!(ScheduleWindow::parse(&since.to_string()).unwrap(), since);
    }

    #[test]
    fn window_bound_is_rfc3339_utc_micros() {
        let t = Utc.with_ymd_and_hms(2026, 8, 18, 4, 5, 6).unwrap()
            + chrono::Duration::nanoseconds(123_456_789);
        assert_eq!(format_window_bound(t), "2026-08-18T04:05:06.123456Z");
    }

    #[test]
    fn truncation_drops_sub_microseconds_only() {
        let t = Utc.with_ymd_and_hms(2026, 8, 18, 4, 5, 6).unwrap()
            + chrono::Duration::nanoseconds(123_456_789);
        let truncated = truncate_to_micros(t);
        assert_eq!(
            format_window_bound(truncated),
            "2026-08-18T04:05:06.123456Z"
        );
        assert_eq!(truncate_to_micros(truncated), truncated);
    }
}
