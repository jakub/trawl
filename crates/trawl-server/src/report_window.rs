// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-window domain types (ADR-0018 rulings 6-14).
//!
//! The window is a property of the SCHEDULE, not of the saved query text: a
//! schedule either has no window (the legacy shape, where the DSL is executed
//! verbatim), tiles `[previous window_end, fire - lag)` under
//! `window = "since_last"`, or takes a fixed trailing span under
//! `window = "<duration>"`. This module owns the types, the two spellings
//! of an instant, and the pure policy over them: [`plan_due_run`] turns a
//! schedule plus a clock reading into the window a run covers. The
//! scheduler wiring that persists those answers lives elsewhere.

use std::fmt;

use chrono::{DateTime, SecondsFormat, SubsecRound as _, TimeDelta, Utc};

use crate::store::{StoreError, format_interval, parse_interval};

/// The window mode, without the span a fixed window carries.
///
/// This is what a RUN records. A schedule's mode can be edited while a run
/// is in flight, so the mode a run was claimed under has to travel with the
/// run: it decides whether finishing that run advances the watermark, and
/// reading it off the schedule at finish time would make the answer depend
/// on which of the two commits landed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// Tiling: the run's end becomes the schedule's watermark.
    SinceLast,
    /// A fixed trailing span, re-measured from every fire time.
    Fixed,
}

impl WindowKind {
    /// The persisted spelling, matched by the `window_kind` CHECKs on both
    /// `schedules` and `report_runs`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SinceLast => "since_last",
            Self::Fixed => "fixed",
        }
    }

    /// Read a persisted spelling back. `None` for anything else, so the
    /// caller decides what an unreadable value means where it is read.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "since_last" => Some(Self::SinceLast),
            "fixed" => Some(Self::Fixed),
            _ => None,
        }
    }
}

impl fmt::Display for WindowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

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
    /// The mode alone, which is what `schedules.window_kind` stores and
    /// what a claimed run records.
    pub fn kind(self) -> WindowKind {
        match self {
            Self::SinceLast => WindowKind::SinceLast,
            Self::Fixed { .. } => WindowKind::Fixed,
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
    /// The mode this window was planned under, recorded on the run so a
    /// mid-flight edit to the schedule cannot change what finishing it means.
    pub kind: WindowKind,
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

// ---------------------------------------------------------------------------
// The due-run planner (ADR-0018 rulings 6, 9, 10, 14)
// ---------------------------------------------------------------------------

/// Everything one due-run decision reads.
///
/// The planner never samples a clock and never touches the store: `now`
/// arrives as a value, and the decision comes back as a value the caller
/// persists. A tick's answer is therefore reproducible from a schedule row
/// plus an instant, which is what lets the tiling rules be tested without a
/// database and keeps the rules in one readable place instead of spread
/// through the scheduler loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanInput {
    /// The scheduler's clock reading for this tick. Truncated to
    /// microseconds by [`plan_due_run`] itself, so a caller that already
    /// truncated loses nothing and one that did not cannot leak
    /// nanoseconds into a stored bound.
    pub now: DateTime<Utc>,
    /// The schedule's planned fire instant. Fires are planned from this
    /// cursor rather than from the last run's start, so execution time
    /// never drifts the schedule.
    pub next_fire_at: DateTime<Utc>,
    /// The schedule's period. Also the first `since_last` window's length
    /// (ruling 14) and the unit `max_catchup_intervals` counts.
    pub interval_secs: u64,
    /// The schedule's window mode, or `None` for the legacy shape where
    /// the saved DSL is executed verbatim (ruling 6).
    pub window: Option<ScheduleWindow>,
    /// Late-arrival allowance. Shifts BOTH window bounds back, so it
    /// delays coverage rather than widening it.
    pub lag_secs: u64,
    /// The `since_last` watermark: the end of the newest window a
    /// successful run covered, or `None` before the first one.
    pub covered_through: Option<DateTime<Utc>>,
    /// How many whole intervals a `since_last` catch-up may span before
    /// the start is clamped forward and the run flagged (ruling 9).
    pub max_catchup_intervals: u32,
}

/// A run the scheduler should claim, with the two values it has to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueRunPlan {
    /// The boundary this run stands for. When fires were missed this is
    /// the LATEST boundary at or before `now`, never the oldest one:
    /// missed fires coalesce into one run and are never backfilled
    /// (ruling 9).
    pub planned_fire: DateTime<Utc>,
    /// The cursor to store: one interval past `planned_fire`.
    pub next_fire_at: DateTime<Utc>,
    /// The window this run covers, or `None` in query mode.
    pub window: Option<ReportWindow>,
}

/// What the scheduler should do with one schedule on one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Not due yet. Leave the row alone.
    NotYet,
    /// Due, but the window it would cover is already covered. Move the
    /// cursor and run nothing.
    ///
    /// Only an edit can produce this: a watermark at or past the window
    /// end means someone moved `covered_through` forward or shortened the
    /// interval. Advancing the cursor is what stops the tick from asking
    /// the same question every poll forever.
    Advance {
        /// The cursor to store.
        next_fire_at: DateTime<Utc>,
    },
    /// Due. Claim a run for this plan.
    Run(DueRunPlan),
}

/// Why a schedule could not be planned.
///
/// Both variants mean the inputs are wrong, not the operator's request:
/// the caller fails the tick and leaves the schedule alone rather than
/// storing a bound it computed from a broken number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// A fire cursor or window bound left the representable instant
    /// range. Every add and subtract in the planner is checked, so this
    /// is what an out-of-range value produces instead of a panic or a
    /// wrapped instant.
    #[error("report window arithmetic left the representable instant range")]
    Arithmetic,
    /// The interval or the catch-up bound is zero. Config refuses both at
    /// load; the planner refuses them again because this is the one place
    /// where the numbers actually mean something.
    #[error(
        "a schedule needs an interval of at least one second and max_catchup_intervals of at least 1"
    )]
    InvalidConfig,
}

/// Decide what one schedule owes at instant `now` (ADR-0018 rulings 6, 9,
/// 10 and 14).
///
/// The rules, in the order they apply:
///
/// - `now < next_fire_at` is [`Due::NotYet`].
/// - `planned_fire` is the latest planned boundary at or before `now`
///   (`next_fire_at + floor((now - next_fire_at) / interval) * interval`).
///   Boundaries missed while the daemon was down are skipped, and the
///   coverage they would have had is folded into this one window instead
///   of replayed as N runs (ruling 9).
/// - `end = planned_fire - lag`. The lag shifts both bounds back so a
///   straggler event that landed after its own boundary is still inside
///   the window that covers it (ruling 6).
/// - No window is query mode: the saved DSL runs verbatim and the run
///   records no bounds (ruling 6).
/// - A fixed window is `[end - span, end)`, re-measured from every fire.
///   It never reads `covered_through`, so a fixed schedule cannot heal a
///   gap and cannot be truncated either.
/// - `since_last` with no watermark covers `[end - interval, end)`
///   (ruling 14).
/// - `since_last` with a watermark starts AT the watermark, so
///   consecutive windows tile: one run's `end` is the next run's `start`,
///   and the half-open shape (ruling 10) keeps the shared instant in
///   exactly one of them. A gap wider than `max_catchup_intervals *
///   interval` clamps the start forward and sets `truncated`, which is
///   the run's own admission that it does not cover everything since the
///   watermark (ruling 9).
///
/// Returns [`Due::Advance`] when the window would end at or before the
/// watermark, which is coverage that already exists.
pub fn plan_due_run(input: &PlanInput) -> Result<Due, PlanError> {
    if input.interval_secs == 0 || input.max_catchup_intervals == 0 {
        return Err(PlanError::InvalidConfig);
    }

    let now = truncate_to_micros(input.now);
    if now < input.next_fire_at {
        return Ok(Due::NotYet);
    }

    let interval_secs = plan_secs(input.interval_secs)?;
    let interval = plan_delta(interval_secs)?;

    // `now >= next_fire_at`, so the elapsed span is non-negative and
    // integer division is a floor.
    let elapsed = now.signed_duration_since(input.next_fire_at);
    let missed = elapsed.num_seconds() / interval_secs;
    let skipped = plan_delta(
        missed
            .checked_mul(interval_secs)
            .ok_or(PlanError::Arithmetic)?,
    )?;

    let planned_fire = input
        .next_fire_at
        .checked_add_signed(skipped)
        .ok_or(PlanError::Arithmetic)?;
    let next_fire_at = planned_fire
        .checked_add_signed(interval)
        .ok_or(PlanError::Arithmetic)?;
    let end = planned_fire
        .checked_sub_signed(plan_delta(plan_secs(input.lag_secs)?)?)
        .ok_or(PlanError::Arithmetic)?;

    let Some(window) = input.window else {
        return Ok(Due::Run(DueRunPlan {
            planned_fire,
            next_fire_at,
            window: None,
        }));
    };

    let (start, truncated) = match window {
        ScheduleWindow::Fixed { secs } => (
            end.checked_sub_signed(plan_delta(plan_secs(secs)?)?)
                .ok_or(PlanError::Arithmetic)?,
            false,
        ),
        ScheduleWindow::SinceLast => match input.covered_through {
            None => (
                end.checked_sub_signed(interval)
                    .ok_or(PlanError::Arithmetic)?,
                false,
            ),
            Some(covered) => {
                if end <= covered {
                    return Ok(Due::Advance { next_fire_at });
                }
                let max_span = plan_delta(
                    interval_secs
                        .checked_mul(i64::from(input.max_catchup_intervals))
                        .ok_or(PlanError::Arithmetic)?,
                )?;
                if end.signed_duration_since(covered) > max_span {
                    (
                        end.checked_sub_signed(max_span)
                            .ok_or(PlanError::Arithmetic)?,
                        true,
                    )
                } else {
                    (covered, false)
                }
            }
        },
    };

    Ok(Due::Run(DueRunPlan {
        planned_fire,
        next_fire_at,
        window: Some(ReportWindow {
            start,
            end,
            truncated,
            kind: window.kind(),
        }),
    }))
}

/// A configured second count as the signed number instant arithmetic uses.
fn plan_secs(secs: u64) -> Result<i64, PlanError> {
    i64::try_from(secs).map_err(|_| PlanError::Arithmetic)
}

/// A whole-second span, or [`PlanError::Arithmetic`] for a count no span
/// can hold.
fn plan_delta(secs: i64) -> Result<TimeDelta, PlanError> {
    TimeDelta::try_seconds(secs).ok_or(PlanError::Arithmetic)
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
    fn window_kind_spellings_round_trip() {
        for kind in [WindowKind::SinceLast, WindowKind::Fixed] {
            assert_eq!(WindowKind::parse(kind.as_str()), Some(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(WindowKind::parse("SinceLast"), None);
        assert_eq!(WindowKind::parse(""), None);
    }

    #[test]
    fn kind_secs_and_display_round_trip() {
        let fixed = ScheduleWindow::Fixed { secs: 7200 };
        assert_eq!(fixed.kind(), WindowKind::Fixed);
        assert_eq!(fixed.secs(), Some(7200));
        assert_eq!(fixed.to_string(), "2h");
        assert_eq!(ScheduleWindow::parse(&fixed.to_string()).unwrap(), fixed);

        let since = ScheduleWindow::SinceLast;
        assert_eq!(since.kind(), WindowKind::SinceLast);
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

    // -- The due-run planner (ADR-0018 rulings 6, 9, 10, 14) ---------------

    const HOUR: u64 = 3600;

    /// 2026-03-14 at the given hour and minute, the fixed clock every
    /// planner test reads.
    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 14, hour, minute, 0).unwrap()
    }

    /// An hourly `since_last` schedule with no lag and the default
    /// catch-up bound, fired exactly on its cursor.
    fn since_last_input(now: DateTime<Utc>, covered: Option<DateTime<Utc>>) -> PlanInput {
        PlanInput {
            now,
            next_fire_at: now,
            interval_secs: HOUR,
            window: Some(ScheduleWindow::SinceLast),
            lag_secs: 0,
            covered_through: covered,
            max_catchup_intervals: 24,
        }
    }

    /// The plan of a schedule that must be due, or a panic naming what
    /// came back instead.
    fn run(input: &PlanInput) -> DueRunPlan {
        match plan_due_run(input).unwrap() {
            Due::Run(plan) => plan,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    /// The window of a schedule that must be due AND windowed.
    fn window_of(input: &PlanInput) -> ReportWindow {
        run(input)
            .window
            .expect("a windowed schedule plans a window")
    }

    /// Ruling 10: consecutive `since_last` windows tile. One run's end is
    /// the next run's start, and because the interval is half-open the
    /// shared instant belongs to the later window alone.
    #[test]
    fn consecutive_since_last_windows_tile_without_overlap() {
        let first_plan = run(&since_last_input(at(3, 0), None));
        let first = first_plan.window.unwrap();

        // Each fire feeds the next: its window end becomes the watermark,
        // its next_fire_at becomes the cursor.
        let mut input = since_last_input(at(4, 0), Some(first.end));
        input.next_fire_at = first_plan.next_fire_at;
        let second_plan = run(&input);
        let second = second_plan.window.unwrap();

        input = since_last_input(at(5, 0), Some(second.end));
        input.next_fire_at = second_plan.next_fire_at;
        let third = window_of(&input);

        assert_eq!(first.end, second.start);
        assert_eq!(second.end, third.start);
        assert!(first.start < first.end && second.start < second.end);
        assert!(!first.truncated && !second.truncated && !third.truncated);

        // The boundary instant is in the later window and nowhere else.
        let boundary = first.end;
        assert!(boundary >= second.start && boundary < second.end);
        assert!(!(boundary >= first.start && boundary < first.end));
    }

    /// Ruling 9: a failed run leaves the watermark, so the next success
    /// covers its own interval and the failed one in a single window.
    /// Nothing is truncated at two intervals of gap.
    #[test]
    fn a_withheld_success_heals_on_the_next_run() {
        let first = window_of(&since_last_input(at(3, 0), None));

        // Fire 2 ran and failed: the cursor moved, the watermark did not.
        let mut input = since_last_input(at(5, 0), Some(first.end));
        input.next_fire_at = at(5, 0);
        let healed = window_of(&input);

        assert_eq!(healed.start, first.end);
        assert_eq!(healed.end, at(5, 0));
        assert_eq!(
            healed.end.signed_duration_since(healed.start),
            TimeDelta::try_hours(2).unwrap()
        );
        assert!(!healed.truncated);
    }

    /// Ruling 9: a gap beyond the bound coalesces into ONE run whose start
    /// is clamped forward and flagged, never N backfilled runs and never a
    /// silent loss. The bound is exclusive: exactly at it, nothing is cut.
    #[test]
    fn a_long_gap_coalesces_into_one_clamped_and_flagged_run() {
        let now = at(3, 0);
        let thirty_behind = now - TimeDelta::try_hours(30).unwrap();

        let plan = run(&since_last_input(now, Some(thirty_behind)));
        let window = plan.window.unwrap();
        assert_eq!(window.end, now);
        assert_eq!(window.start, now - TimeDelta::try_hours(24).unwrap());
        assert!(window.truncated);
        assert_eq!(plan.next_fire_at, at(4, 0));

        let exactly_at_bound = window_of(&since_last_input(
            now,
            Some(now - TimeDelta::try_hours(24).unwrap()),
        ));
        assert!(!exactly_at_bound.truncated);

        let one_over = window_of(&since_last_input(
            now,
            Some(now - TimeDelta::try_hours(25).unwrap()),
        ));
        assert!(one_over.truncated);
        assert_eq!(one_over.start, now - TimeDelta::try_hours(24).unwrap());
    }

    /// Ruling 6: a fixed window is re-measured from every fire, so the
    /// watermark is not read at all, and missed fires are missing runs
    /// rather than a backlog.
    #[test]
    fn a_fixed_window_ignores_the_watermark_and_skips_missed_fires() {
        let base = PlanInput {
            now: at(6, 0),
            next_fire_at: at(3, 0),
            interval_secs: HOUR,
            window: Some(ScheduleWindow::Fixed { secs: 2 * HOUR }),
            lag_secs: 300,
            covered_through: None,
            max_catchup_intervals: 24,
        };

        let plan = run(&base);
        assert_eq!(plan.planned_fire, at(6, 0));
        assert_eq!(plan.next_fire_at, at(7, 0));
        let window = plan.window.unwrap();
        assert_eq!(window.kind, WindowKind::Fixed);
        assert_eq!(window.end, at(5, 55));
        assert_eq!(window.start, at(3, 55));
        assert!(!window.truncated);

        let with_watermark = PlanInput {
            covered_through: Some(at(1, 0)),
            ..base
        };
        assert_eq!(run(&with_watermark), plan);

        let watermark_past_the_end = PlanInput {
            covered_through: Some(at(6, 0)),
            ..base
        };
        assert_eq!(run(&watermark_past_the_end), plan);
    }

    /// Ruling 14: the first `since_last` run covers
    /// `[fire - lag - interval, fire - lag)`.
    #[test]
    fn the_first_since_last_run_covers_one_interval_back() {
        let no_lag = window_of(&since_last_input(at(3, 0), None));
        assert_eq!(no_lag.start, at(2, 0));
        assert_eq!(no_lag.end, at(3, 0));
        assert_eq!(no_lag.kind, WindowKind::SinceLast);
        assert!(!no_lag.truncated);

        let mut lagged = since_last_input(at(3, 0), None);
        lagged.lag_secs = 300;
        let lagged = window_of(&lagged);
        assert_eq!(lagged.start, at(1, 55));
        assert_eq!(lagged.end, at(2, 55));
    }

    /// The cursor is the due test, and it is inclusive: a tick exactly on
    /// it runs, one microsecond before it does not.
    #[test]
    fn a_schedule_is_due_at_its_cursor_and_not_before() {
        let mut early = since_last_input(at(3, 0), None);
        early.now = at(3, 0) - TimeDelta::microseconds(1);
        assert_eq!(plan_due_run(&early).unwrap(), Due::NotYet);

        let exact = since_last_input(at(3, 0), None);
        let plan = run(&exact);
        assert_eq!(plan.planned_fire, at(3, 0));
        assert_eq!(plan.next_fire_at, at(4, 0));
    }

    /// A window already covered moves the cursor and runs nothing, so an
    /// edit that pushed the watermark forward cannot make every poll ask
    /// the same question again.
    #[test]
    fn an_already_covered_window_advances_the_cursor_instead_of_running() {
        for covered in [at(3, 0), at(9, 0)] {
            assert_eq!(
                plan_due_run(&since_last_input(at(3, 0), Some(covered))).unwrap(),
                Due::Advance {
                    next_fire_at: at(4, 0)
                }
            );
        }
    }

    /// Ruling 6: no window is query mode. The run happens and records no
    /// bounds.
    #[test]
    fn query_mode_plans_a_run_with_no_window() {
        let mut input = since_last_input(at(3, 0), Some(at(9, 0)));
        input.window = None;
        let plan = run(&input);
        assert_eq!(plan.planned_fire, at(3, 0));
        assert_eq!(plan.next_fire_at, at(4, 0));
        assert_eq!(plan.window, None);
    }

    /// Every add and subtract is checked, so an instant near the end of
    /// the representable range is an error rather than a panic or a
    /// wrapped bound. A zero interval or catch-up bound is a config fault
    /// the planner refuses on its own.
    #[test]
    fn out_of_range_arithmetic_and_zero_config_are_errors_not_panics() {
        let edge = truncate_to_micros(DateTime::<Utc>::MAX_UTC);
        let at_the_edge = PlanInput {
            now: DateTime::<Utc>::MAX_UTC,
            next_fire_at: edge,
            interval_secs: HOUR,
            window: Some(ScheduleWindow::SinceLast),
            lag_secs: 0,
            covered_through: None,
            max_catchup_intervals: 24,
        };
        assert_eq!(plan_due_run(&at_the_edge), Err(PlanError::Arithmetic));

        let huge_interval = PlanInput {
            interval_secs: u64::MAX,
            ..since_last_input(at(3, 0), None)
        };
        assert_eq!(plan_due_run(&huge_interval), Err(PlanError::Arithmetic));

        let no_catchup = PlanInput {
            max_catchup_intervals: 0,
            ..since_last_input(at(3, 0), None)
        };
        assert_eq!(plan_due_run(&no_catchup), Err(PlanError::InvalidConfig));

        let no_interval = PlanInput {
            interval_secs: 0,
            ..since_last_input(at(3, 0), None)
        };
        assert_eq!(plan_due_run(&no_interval), Err(PlanError::InvalidConfig));
    }

    /// A clock reading finer than a microsecond cannot reach a stored
    /// bound: the planner truncates `now` before it decides anything.
    #[test]
    fn a_nanosecond_clock_reading_yields_microsecond_bounds() {
        let mut nanos = since_last_input(at(3, 0), None);
        nanos.now = at(3, 0) + TimeDelta::nanoseconds(123_456_789);
        let dirty = run(&nanos);
        let clean = run(&since_last_input(at(3, 0), None));

        assert_eq!(dirty, clean);
        let window = dirty.window.unwrap();
        for bound in [
            window.start,
            window.end,
            dirty.planned_fire,
            dirty.next_fire_at,
        ] {
            assert_eq!(bound.timestamp_subsec_nanos() % 1000, 0, "{bound}");
        }
    }
}
