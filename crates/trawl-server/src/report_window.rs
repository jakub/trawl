// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-window domain types (ADR-0018 rulings 6-14).

use std::fmt;

use chrono::{DateTime, SecondsFormat, SubsecRound as _, TimeDelta, Utc};

use trawl_core::ast::{PipeStage, Query, TimeClause};
use trawl_core::canonical_form::same_query;

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
    pub fn kind(self) -> WindowKind {
        match self {
            Self::SinceLast => WindowKind::SinceLast,
            Self::Fixed { .. } => WindowKind::Fixed,
        }
    }

    pub fn secs(self) -> Option<u64> {
        match self {
            Self::SinceLast => None,
            Self::Fixed { secs } => Some(secs),
        }
    }

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Whether `start` was clamped forward past an uncovered gap.
    pub truncated: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanInput {
    /// The scheduler's clock reading for this tick, or the instant a
    /// manual claim took its locks.
    pub now: DateTime<Utc>,
    /// The schedule's planned fire instant. Fires are planned from this
    /// cursor rather than from the last run's start, so execution time
    /// never drifts the schedule. A manual plan does not read it.
    pub next_fire_at: DateTime<Utc>,
    /// The schedule's period. Also the first `since_last` window's length
    /// (ruling 14) and the unit `max_catchup_intervals` counts.
    pub interval_secs: u64,
    /// The schedule's window mode, or `None` for query-text timing, where
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
    /// The boundary this run stands for: the LATEST boundary at or before
    /// `now` (ruling 9).
    pub planned_fire: DateTime<Utc>,
    /// The cursor to store: one interval past `planned_fire`.
    pub next_fire_at: DateTime<Utc>,
    /// The window this run covers, or `None` in query mode.
    pub window: Option<ReportWindow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
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
    Run(DueRunPlan),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// A fire cursor or window bound left the representable instant
    /// range.
    #[error("report window arithmetic left the representable instant range")]
    Arithmetic,
    #[error(
        "a schedule needs an interval of at least one second and max_catchup_intervals of at least 1"
    )]
    InvalidConfig,
}

/// Decide what one schedule owes at instant `now` (ADR-0018 rulings 6, 9,
/// 10 and 14).
pub fn plan_due_run(input: &PlanInput) -> Result<Due, PlanError> {
    if input.interval_secs == 0 || input.max_catchup_intervals == 0 {
        return Err(PlanError::InvalidConfig);
    }

    let now = truncate_to_micros(input.now);
    let Some(FireBoundaries {
        planned_fire,
        next_fire_at,
    }) = fire_boundaries(input.next_fire_at, input.interval_secs, now)?
    else {
        return Ok(Due::NotYet);
    };
    let end = lagged(planned_fire, input.lag_secs)?;

    Ok(match window_ending_at(end, input)? {
        Coverage::Window(window) => Due::Run(DueRunPlan {
            planned_fire,
            next_fire_at,
            window,
        }),
        Coverage::Covered { .. } => Due::Advance { next_fire_at },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FireBoundaries {
    /// The latest planned boundary at or before the instant.
    pub planned_fire: DateTime<Utc>,
    /// The first planned boundary strictly after the instant: one interval
    /// past `planned_fire`.
    pub next_fire_at: DateTime<Utc>,
}

/// Place `now` on the cadence that runs through `next_fire_at` every
/// `interval_secs`, or `None` when the cursor is still in the future.
///
/// `now` is compared as given; a caller holding a clock reading truncates
/// it first, as [`plan_due_run`] does.
pub fn fire_boundaries(
    next_fire_at: DateTime<Utc>,
    interval_secs: u64,
    now: DateTime<Utc>,
) -> Result<Option<FireBoundaries>, PlanError> {
    if interval_secs == 0 {
        return Err(PlanError::InvalidConfig);
    }
    if now < next_fire_at {
        return Ok(None);
    }

    let interval_secs = plan_secs(interval_secs)?;
    let interval = plan_delta(interval_secs)?;

    // `now >= next_fire_at`, so the elapsed span is non-negative and
    // integer division is a floor.
    let elapsed = now.signed_duration_since(next_fire_at);
    let missed = elapsed.num_seconds() / interval_secs;
    let skipped = plan_delta(
        missed
            .checked_mul(interval_secs)
            .ok_or(PlanError::Arithmetic)?,
    )?;

    let planned_fire = next_fire_at
        .checked_add_signed(skipped)
        .ok_or(PlanError::Arithmetic)?;
    let next_fire_at = planned_fire
        .checked_add_signed(interval)
        .ok_or(PlanError::Arithmetic)?;
    Ok(Some(FireBoundaries {
        planned_fire,
        next_fire_at,
    }))
}

/// What a manual run of a schedule covers (ADR-0018 amended 2026-09-23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualPlan {
    /// Run, covering this window, or `None` in query mode.
    Run(Option<ReportWindow>),
    /// A `since_last` window ending at `end` would read nothing the
    /// watermark does not already cover. No run is claimed.
    Covered {
        covered_through: DateTime<Utc>,
        /// Where the refused window would have ended: `t - lag`.
        end: DateTime<Utc>,
    },
}

/// Decide what a manual run claimed at `input.now` covers: the schedule's
/// next window, fired early (ADR-0018 amended 2026-09-23).
pub fn plan_manual_run(input: &PlanInput) -> Result<ManualPlan, PlanError> {
    if input.interval_secs == 0 || input.max_catchup_intervals == 0 {
        return Err(PlanError::InvalidConfig);
    }

    let end = lagged(truncate_to_micros(input.now), input.lag_secs)?;
    Ok(match window_ending_at(end, input)? {
        Coverage::Window(window) => ManualPlan::Run(window),
        Coverage::Covered { covered_through } => ManualPlan::Covered {
            covered_through,
            end,
        },
    })
}

/// The window one run ending at `end` covers, before the caller decides
/// what that means for its cursor.
enum Coverage {
    /// A window to run, or `None` in query mode.
    Window(Option<ReportWindow>),
    /// `since_last` coverage already reaches `end`.
    Covered { covered_through: DateTime<Utc> },
}

fn lagged(instant: DateTime<Utc>, lag_secs: u64) -> Result<DateTime<Utc>, PlanError> {
    instant
        .checked_sub_signed(plan_delta(plan_secs(lag_secs)?)?)
        .ok_or(PlanError::Arithmetic)
}

/// The window math every run shares, scheduled or manual: given where the
/// window ends, where does it start (ADR-0018 rulings 6, 9 and 14)?
fn window_ending_at(end: DateTime<Utc>, input: &PlanInput) -> Result<Coverage, PlanError> {
    let Some(window) = input.window else {
        return Ok(Coverage::Window(None));
    };

    let interval_secs = plan_secs(input.interval_secs)?;
    let interval = plan_delta(interval_secs)?;
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
                    return Ok(Coverage::Covered {
                        covered_through: covered,
                    });
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

    Ok(Coverage::Window(Some(ReportWindow {
        start,
        end,
        truncated,
        kind: window.kind(),
    })))
}

fn plan_secs(secs: u64) -> Result<i64, PlanError> {
    i64::try_from(secs).map_err(|_| PlanError::Arithmetic)
}

fn plan_delta(secs: i64) -> Result<TimeDelta, PlanError> {
    TimeDelta::try_seconds(secs).ok_or(PlanError::Arithmetic)
}

/// Why a planned window could not be put onto a saved query.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MaterializeError {
    #[error("the saved query does not parse: {message}")]
    SourceUnparseable { message: String },
    /// The saved text owns its own window. Two spellings of one interval
    /// never coexist (ruling 7).
    #[error(
        "the saved query carries a {}= time clause, which a schedule window cannot be spliced onto",
        .clause.keyword()
    )]
    SourceTimeClause {
        /// The clause the query carries.
        clause: TimeClause,
    },
    /// The saved text reads stored report rows, which no `_time` window
    /// applies to (ruling 12).
    #[error("the saved query reads stored report rows ('from saved'), which take no _time window")]
    SourceFromSaved,
    /// The spliced text does not parse. The prefix is machine-written, so
    /// this means the saved text parses on its own but not behind two
    /// bounds, and executing it anyway would run something nobody wrote.
    #[error("the query with its window spliced on does not parse: {message}")]
    OutputUnparseable {
        /// The parser's first message.
        message: String,
    },
    #[error("the spliced query's bounds did not read back as earliest={start} latest={end}")]
    BoundsNotReadBack { start: String, end: String },
    #[error("splicing the window changed the query itself, not just its bounds")]
    BodyChanged,
}

/// Put a planned window onto a saved query as absolute bounds, returning
/// the text the run executes and stores (ADR-0018 ruling 11).
///
/// The output is a PREFIX SPLICE: `earliest="<start>" latest="<end>" `
/// followed by the saved text byte for byte. The three time keywords are
/// read before anything else in the search stage, so the prefix is valid
/// ahead of a field filter, a bare word, a comment or a leading `|`.
///
/// Check 4 compares the two through [`same_query`] rather than by
/// `PartialEq`: the AST carries source spans and the hoisted clauses'
/// spans, and the prefix moves every byte of the original, so a direct
/// comparison would report a difference for every query. The canonical
/// form leaves both out and compares everything else.
pub fn materialize_window(dsl: &str, window: &ReportWindow) -> Result<String, MaterializeError> {
    let original =
        parse_dsl(dsl).map_err(|message| MaterializeError::SourceUnparseable { message })?;
    if let Some(clause) = original.time_clause() {
        return Err(MaterializeError::SourceTimeClause { clause });
    }
    if carries_from_saved(&original) {
        return Err(MaterializeError::SourceFromSaved);
    }

    let start = format_window_bound(window.start);
    let end = format_window_bound(window.end);
    let spliced = format!("earliest=\"{start}\" latest=\"{end}\" {dsl}");

    let reparsed =
        parse_dsl(&spliced).map_err(|message| MaterializeError::OutputUnparseable { message })?;

    let bounds_read_back = reparsed.search.time_filter.is_none()
        && reparsed.search.earliest.as_ref().map(|b| b.node.text()) == Some(start.as_str())
        && reparsed.search.latest.as_ref().map(|b| b.node.text()) == Some(end.as_str());
    if !bounds_read_back {
        return Err(MaterializeError::BoundsNotReadBack { start, end });
    }

    if !body_unchanged(reparsed, &original) {
        return Err(MaterializeError::BodyChanged);
    }

    Ok(spliced)
}

/// Check 4: `spliced` with the two bounds the splice wrote cleared has
/// the canonical form of `original`. Nothing else is cleared: a `last=`
/// beside the bounds is a change.
fn body_unchanged(mut spliced: Query, original: &Query) -> bool {
    spliced.search.earliest = None;
    spliced.search.latest = None;
    same_query(&spliced, original)
}

/// Window policy reads what the text means and runs nothing, so the parse
/// builds no pattern (ADR-0063): the run's admission check builds them.
///
/// The message quotes the operator's own tokens, so it is fine to show a
/// client and never fine to persist: callers put it in an error the HTTP
/// layer redacts, and telemetry logs the class instead.
fn parse_dsl(dsl: &str) -> Result<Query, String> {
    trawl_core::parser::parse_unbuilt(dsl).map_err(|errors| {
        errors
            .first()
            .map_or_else(|| "parse error".to_owned(), |e| e.message.clone())
    })
}

/// Whether any stage of the pipeline reads stored report rows.
///
/// Deliberately not [`trawl_core::ast::Query::from_saved_stage`], which
/// reports only a FIRST stage. A `from saved` further along parses fine
/// and is refused later by the emitter, and window policy has to see it
/// here: a query it cannot execute must not be handed a window either.
fn carries_from_saved(query: &Query) -> bool {
    query
        .pipeline
        .iter()
        .any(|stage| matches!(stage.node, PipeStage::FromSaved(_)))
}

/// Why a window and a saved query may not be attached to each other.
///
/// Every message names BOTH sides, because the operator has to remove one
/// of them and the server has no basis for choosing which.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindowPolicyError {
    /// A window was attached to text that does not parse. A window is a
    /// claim about what the query covers, and there is no query yet.
    #[error(
        "schedule window \"{window}\" cannot be attached to a query that does not parse: {message}"
    )]
    Unparseable {
        window: ScheduleWindow,
        message: String,
    },
    /// The query already spells its own interval (ruling 7).
    #[error(
        "schedule window \"{window}\" conflicts with the saved query's {}= time clause; remove one side",
        .clause.keyword()
    )]
    TimeClause {
        window: ScheduleWindow,
        clause: TimeClause,
    },
    /// The query reads stored report rows, not ingest events (ruling 12).
    #[error(
        "schedule window \"{window}\" conflicts with the saved query's \"from saved\" source; \
         stored report rows cannot receive a _time window"
    )]
    FromSaved { window: ScheduleWindow },
}

/// Decide whether a window may be attached to this query text (ADR-0018
/// rulings 7 and 12).
///
/// The time-clause check reads the parsed AST rather than scanning the
/// source: a backticked `` `last`=5 `` is an ordinary field filter, while
/// `service=x OR last=1h` carries a clause a per-group text scan would miss.
pub fn validate_window_compatibility(
    window: Option<ScheduleWindow>,
    dsl: &str,
) -> Result<(), WindowPolicyError> {
    let Some(window) = window else {
        return Ok(());
    };

    let query =
        parse_dsl(dsl).map_err(|message| WindowPolicyError::Unparseable { window, message })?;
    if let Some(clause) = query.time_clause() {
        return Err(WindowPolicyError::TimeClause { window, clause });
    }
    if carries_from_saved(&query) {
        return Err(WindowPolicyError::FromSaved { window });
    }
    Ok(())
}

/// [`validate_window_compatibility`] on the blocking pool (ADR-0063).
pub async fn check_window_compatibility(
    window: Option<ScheduleWindow>,
    dsl: &str,
) -> Result<(), WindowPolicyError> {
    if window.is_none() {
        return Ok(());
    }
    let dsl = dsl.to_owned();
    off_reactor(move || validate_window_compatibility(window, &dsl)).await
}

/// Run window policy's `work` on the blocking pool, with no preparation
/// slot (ADR-0063): the parses it makes build no pattern, so its cost is
/// the grammar's, and its callers hold a row lock no slot may delay.
pub(crate) async fn off_reactor<T, F>(work: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|failed| std::panic::resume_unwind(failed.into_panic()))
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

    const HOUR: u64 = 3600;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 14, hour, minute, 0).unwrap()
    }

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

    fn run(input: &PlanInput) -> DueRunPlan {
        match plan_due_run(input).unwrap() {
            Due::Run(plan) => plan,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    fn window_of(input: &PlanInput) -> ReportWindow {
        run(input)
            .window
            .expect("a windowed schedule plans a window")
    }

    #[test]
    fn consecutive_since_last_windows_tile_without_overlap() {
        let first_plan = run(&since_last_input(at(3, 0), None));
        let first = first_plan.window.unwrap();

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

        let boundary = first.end;
        assert!(boundary >= second.start && boundary < second.end);
        assert!(!(boundary >= first.start && boundary < first.end));
    }

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

    #[test]
    fn query_mode_plans_a_run_with_no_window() {
        let mut input = since_last_input(at(3, 0), Some(at(9, 0)));
        input.window = None;
        let plan = run(&input);
        assert_eq!(plan.planned_fire, at(3, 0));
        assert_eq!(plan.next_fire_at, at(4, 0));
        assert_eq!(plan.window, None);
    }

    /// The two operator-chosen numbers the planner multiplies are both
    /// capped, so there is one widest catch-up span any install can ask
    /// for: a million intervals of the longest interval the duration
    /// grammar accepts. It has to PLAN. Were the product unrepresentable,
    /// the answer would be `PlanError::Arithmetic` on this poll and on
    /// every poll after it, with the fire cursor never advancing.
    #[test]
    fn the_widest_configurable_catchup_span_still_plans() {
        let mut widest = since_last_input(at(3, 0), Some(at(2, 0)));
        widest.interval_secs = crate::store::MAX_DURATION_SECS;
        widest.max_catchup_intervals = trawl_config::MAX_SCHEDULER_CATCHUP_INTERVALS;

        // The watermark is an hour back, far inside the ceiling, so the
        // window tiles from it. Reaching that answer at all means the
        // ceiling's span was computed rather than overflowed: the
        // comparison against it happens before the branch is chosen.
        let window = window_of(&widest);
        assert_eq!(window.start, at(2, 0));
        assert_eq!(window.end, at(3, 0));
        assert!(!window.truncated);
    }

    /// trawl-config cannot import the duration cap, because trawl-server
    /// depends on trawl-config and not the other way round, so it carries
    /// a copy. This is the only place that can see both.
    #[test]
    fn config_duration_mirror_matches_the_grammar_cap() {
        assert_eq!(
            trawl_config::MIRRORED_MAX_DURATION_SECS,
            crate::store::MAX_DURATION_SECS,
            "trawl-config's mirrored duration cap drifted from the grammar's"
        );
    }

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

    #[test]
    fn fire_boundaries_name_the_first_boundary_strictly_after_now() {
        let on = fire_boundaries(at(3, 0), HOUR, at(5, 0)).unwrap().unwrap();
        assert_eq!(on.planned_fire, at(5, 0));
        assert_eq!(on.next_fire_at, at(6, 0));

        let between = fire_boundaries(at(3, 0), HOUR, at(5, 59)).unwrap().unwrap();
        assert_eq!(between.planned_fire, at(5, 0));
        assert_eq!(between.next_fire_at, at(6, 0));

        let future = at(3, 0) - TimeDelta::microseconds(1);
        assert_eq!(fire_boundaries(at(3, 0), HOUR, future).unwrap(), None);

        assert_eq!(
            fire_boundaries(at(3, 0), 0, at(5, 0)),
            Err(PlanError::InvalidConfig)
        );
    }

    #[test]
    fn a_manual_since_last_run_covers_from_the_watermark_to_now_minus_lag() {
        let t = at(3, 17) + TimeDelta::nanoseconds(123_456_789);
        let input = PlanInput {
            now: t,
            // A cursor far off in either direction changes nothing.
            next_fire_at: at(9, 0),
            interval_secs: HOUR,
            window: Some(ScheduleWindow::SinceLast),
            lag_secs: 300,
            covered_through: Some(at(2, 55)),
            max_catchup_intervals: 24,
        };
        let expected = ReportWindow {
            start: at(2, 55),
            end: truncate_to_micros(t) - TimeDelta::try_seconds(300).unwrap(),
            truncated: false,
            kind: WindowKind::SinceLast,
        };
        assert_eq!(
            plan_manual_run(&input).unwrap(),
            ManualPlan::Run(Some(expected))
        );
        let behind = PlanInput {
            next_fire_at: at(0, 0),
            ..input
        };
        assert_eq!(
            plan_manual_run(&behind).unwrap(),
            ManualPlan::Run(Some(expected))
        );
    }

    #[test]
    fn a_manual_fixed_run_covers_the_trailing_span_ending_now_minus_lag() {
        let input = PlanInput {
            now: at(6, 10),
            next_fire_at: at(7, 0),
            interval_secs: HOUR,
            window: Some(ScheduleWindow::Fixed { secs: 2 * HOUR }),
            lag_secs: 300,
            covered_through: Some(at(9, 0)),
            max_catchup_intervals: 24,
        };
        assert_eq!(
            plan_manual_run(&input).unwrap(),
            ManualPlan::Run(Some(ReportWindow {
                start: at(4, 5),
                end: at(6, 5),
                truncated: false,
                kind: WindowKind::Fixed,
            }))
        );
    }

    #[test]
    fn a_manual_query_mode_run_plans_no_window() {
        let mut input = since_last_input(at(3, 17), Some(at(9, 0)));
        input.window = None;
        assert_eq!(plan_manual_run(&input).unwrap(), ManualPlan::Run(None));
    }

    #[test]
    fn a_manual_run_past_the_catchup_bound_is_clamped_and_flagged() {
        let mut input = since_last_input(
            at(3, 17),
            Some(at(3, 17) - TimeDelta::try_hours(5).unwrap()),
        );
        input.max_catchup_intervals = 3;
        assert_eq!(
            plan_manual_run(&input).unwrap(),
            ManualPlan::Run(Some(ReportWindow {
                start: at(0, 17),
                end: at(3, 17),
                truncated: true,
                kind: WindowKind::SinceLast,
            }))
        );
    }

    #[test]
    fn a_manual_run_without_a_watermark_covers_one_interval() {
        let mut input = since_last_input(at(3, 17), None);
        input.lag_secs = 60;
        assert_eq!(
            plan_manual_run(&input).unwrap(),
            ManualPlan::Run(Some(ReportWindow {
                start: at(2, 16),
                end: at(3, 16),
                truncated: false,
                kind: WindowKind::SinceLast,
            }))
        );
    }

    #[test]
    fn a_manual_run_with_nothing_new_to_read_is_covered() {
        let t = at(3, 17);
        for covered in [t, t + TimeDelta::try_hours(1).unwrap()] {
            assert_eq!(
                plan_manual_run(&since_last_input(t, Some(covered))).unwrap(),
                ManualPlan::Covered {
                    covered_through: covered,
                    end: t,
                }
            );
        }

        let one_micro_behind = t - TimeDelta::microseconds(1);
        assert_eq!(
            plan_manual_run(&since_last_input(t, Some(one_micro_behind))).unwrap(),
            ManualPlan::Run(Some(ReportWindow {
                start: one_micro_behind,
                end: t,
                truncated: false,
                kind: WindowKind::SinceLast,
            }))
        );
    }

    #[test]
    fn a_manual_plan_refuses_zero_config() {
        let no_catchup = PlanInput {
            max_catchup_intervals: 0,
            ..since_last_input(at(3, 0), None)
        };
        assert_eq!(plan_manual_run(&no_catchup), Err(PlanError::InvalidConfig));
        let no_interval = PlanInput {
            interval_secs: 0,
            ..since_last_input(at(3, 0), None)
        };
        assert_eq!(plan_manual_run(&no_interval), Err(PlanError::InvalidConfig));
    }

    fn spliceable_window() -> ReportWindow {
        ReportWindow {
            start: at(2, 0),
            end: at(3, 0),
            truncated: false,
            kind: WindowKind::SinceLast,
        }
    }

    const SPLICED_PREFIX: &str =
        "earliest=\"2026-03-14T02:00:00.000000Z\" latest=\"2026-03-14T03:00:00.000000Z\" ";

    #[test]
    fn materializing_prefixes_the_saved_text_verbatim() {
        let cases = [
            "service=nginx | stats count() by host",
            "| stats count()",
            "service=nginx # nightly rollup, do not touch",
            "service=nginx OR service=apache | head 5",
        ];
        for dsl in cases {
            let out = materialize_window(dsl, &spliceable_window()).unwrap();
            assert_eq!(out, format!("{SPLICED_PREFIX}{dsl}"), "for {dsl}");
            assert!(out.ends_with(dsl), "saved text not verbatim: {out}");

            let reparsed = trawl_core::parser::parse(&out).unwrap();
            let original = trawl_core::parser::parse(dsl).unwrap();
            assert!(reparsed.search.earliest.is_some() && reparsed.search.latest.is_some());
            assert!(body_unchanged(reparsed, &original), "for {dsl}");
        }

        let commented = materialize_window(
            "service=nginx # nightly rollup, do not touch",
            &spliceable_window(),
        )
        .unwrap();
        assert!(commented.contains("# nightly rollup, do not touch"));
    }

    #[test]
    fn report_window_proof_detects_body_changes() {
        let parse =
            |dsl: &str| trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
        let spliced = |body: &str| parse(&format!("{SPLICED_PREFIX}{body}"));

        for (original, same) in [
            (
                "service=nginx | stats count() by host",
                "service=nginx\n|stats count() by host # nightly",
            ),
            (r#"`OR.a`="""#, r#"`OR.a`=""  "#),
        ] {
            assert!(
                body_unchanged(spliced(same), &parse(original)),
                "{original} against {same}"
            );
        }
        for (original, changed) in [
            (
                "service=nginx | stats count() by host",
                "service=apache | stats count() by host",
            ),
            (
                "service=nginx | stats count() by host",
                "service=nginx | stats count() by service",
            ),
            (r#"`OR.a`="""#, r#"`OR.a`="x""#),
            (r#"`OR.a`="""#, r#"`OR.a`!="""#),
            (r#"`OR.a`="" b"#, r#"`OR.b`="" b"#),
            ("a=*", r#"a="*""#),
            ("x=1", "x=1.0"),
            ("a b", "a NOT b"),
            ("a b", "a OR b"),
        ] {
            assert!(
                !body_unchanged(spliced(changed), &parse(original)),
                "{original} against {changed}"
            );
        }
        assert!(trawl_core::parser::parse(&format!("{SPLICED_PREFIX}last=1h a")).is_err());
    }

    #[test]
    fn spliced_bounds_reparse_to_the_planned_instants() {
        let window = spliceable_window();
        let out = materialize_window("service=nginx", &window).unwrap();
        let parsed = trawl_core::parser::parse(&out).unwrap();

        let read = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(
            read(parsed.search.earliest.unwrap().node.text()),
            window.start
        );
        assert_eq!(read(parsed.search.latest.unwrap().node.text()), window.end);
    }

    #[test]
    fn materializing_refuses_a_query_that_owns_its_own_window() {
        let cases = [
            ("service=nginx last=1h", TimeClause::Last),
            (
                "service=nginx earliest=\"2026-01-01T00:00:00Z\"",
                TimeClause::Earliest,
            ),
            (
                "service=nginx latest=\"2026-01-01T00:00:00Z\"",
                TimeClause::Latest,
            ),
        ];
        for (dsl, clause) in cases {
            assert_eq!(
                materialize_window(dsl, &spliceable_window()),
                Err(MaterializeError::SourceTimeClause { clause }),
                "for {dsl}"
            );
        }
    }

    #[test]
    fn materializing_refuses_a_from_saved_query_anywhere_in_the_pipeline() {
        for dsl in [
            "| from saved daily_rollup",
            "| from saved daily_rollup | head 5",
            "service=nginx | head 5 | from saved daily_rollup",
        ] {
            assert_eq!(
                materialize_window(dsl, &spliceable_window()),
                Err(MaterializeError::SourceFromSaved),
                "for {dsl}"
            );
        }
    }

    #[test]
    fn materializing_refuses_an_unparseable_saved_query() {
        let err = materialize_window("| stats count(", &spliceable_window()).unwrap_err();
        assert!(
            matches!(err, MaterializeError::SourceUnparseable { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("the saved query does not parse")
        );
    }

    #[test]
    fn a_window_and_a_time_clause_refuse_each_other_by_name() {
        let window = ScheduleWindow::SinceLast;
        let cases = [
            ("service=nginx last=1h", TimeClause::Last, "last="),
            (
                "earliest=\"2026-01-01T00:00:00Z\" service=nginx",
                TimeClause::Earliest,
                "earliest=",
            ),
            (
                "latest=\"2026-01-01T00:00:00Z\" service=nginx",
                TimeClause::Latest,
                "latest=",
            ),
        ];
        for (dsl, clause, spelling) in cases {
            let err = validate_window_compatibility(Some(window), dsl).unwrap_err();
            assert_eq!(err, WindowPolicyError::TimeClause { window, clause });
            let message = err.to_string();
            assert!(message.contains("since_last"), "{message}");
            assert!(message.contains(spelling), "{message}");
        }
    }

    #[test]
    fn a_window_and_a_from_saved_source_refuse_each_other_by_name() {
        let window = ScheduleWindow::Fixed { secs: 7200 };
        for dsl in [
            "| from saved daily_rollup",
            "service=nginx | head 5 | from saved daily_rollup",
        ] {
            let err = validate_window_compatibility(Some(window), dsl).unwrap_err();
            assert_eq!(err, WindowPolicyError::FromSaved { window });
            let message = err.to_string();
            assert!(message.contains("\"2h\""), "{message}");
            assert!(message.contains("from saved"), "{message}");
        }
    }

    #[test]
    fn a_backticked_keyword_is_a_field_and_keeps_its_window() {
        assert_eq!(
            validate_window_compatibility(Some(ScheduleWindow::SinceLast), "`last`=5"),
            Ok(())
        );
        assert_eq!(
            validate_window_compatibility(
                Some(ScheduleWindow::SinceLast),
                "service=nginx `earliest`=x | head 5"
            ),
            Ok(())
        );
    }

    #[test]
    fn a_time_clause_hoisted_out_of_an_or_group_is_still_refused() {
        let window = ScheduleWindow::SinceLast;
        assert_eq!(
            validate_window_compatibility(Some(window), "service=x OR last=1h"),
            Err(WindowPolicyError::TimeClause {
                window,
                clause: TimeClause::Last
            })
        );
    }

    #[test]
    fn no_window_accepts_any_text_while_a_window_needs_a_parseable_query() {
        assert_eq!(
            validate_window_compatibility(None, "| stats count( |||"),
            Ok(())
        );
        assert_eq!(validate_window_compatibility(None, "last=1h"), Ok(()));

        let err = validate_window_compatibility(
            Some(ScheduleWindow::Fixed { secs: 7200 }),
            "| stats count( |||",
        )
        .unwrap_err();
        assert!(
            matches!(err, WindowPolicyError::Unparseable { .. }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("\"2h\""), "{err}");
    }

    #[test]
    fn window_parses_build_no_pattern() {
        use trawl_core::parser::{ParseKind, ParseLog};

        let dsl = "zz_cp6b_window_kinds | where message matches /zz_(a|b)+/";
        let log = ParseLog::watch("zz_cp6b_window_kinds");
        validate_window_compatibility(Some(ScheduleWindow::SinceLast), dsl).unwrap();
        materialize_window(dsl, &spliceable_window()).unwrap();
        let kinds: Vec<ParseKind> = log.parses().into_iter().map(|(_, kind)| kind).collect();
        assert_eq!(kinds, [ParseKind::Unbuilt; 3]);
    }
}
