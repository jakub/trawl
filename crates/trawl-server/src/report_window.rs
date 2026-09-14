// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-window domain types (ADR-0018 rulings 6-14).
//!
//! The window is a property of the SCHEDULE, not of the saved query text: a
//! schedule either uses query-text timing (no scheduler-owned window, so the DSL
//! is executed verbatim), tiles `[previous window_end, fire - lag)` under
//! `window = "since_last"`, or takes a fixed trailing span under
//! `window = "<duration>"`. This module owns the types, the two spellings
//! of an instant, and the pure policy over them: [`plan_due_run`] turns a
//! schedule plus a clock reading into the window a run covers, and
//! [`materialize_window`] puts that window onto the saved DSL as absolute
//! bounds. [`validate_window_compatibility`] is the write-time half: the
//! one rule both write directions ask before a window and a query text are
//! attached to each other. The scheduler wiring that persists those
//! answers lives elsewhere.

use std::fmt;

use chrono::{DateTime, SecondsFormat, SubsecRound as _, TimeDelta, Utc};

use trawl_core::ast::{PipeStage, Query, TimeClause};
use trawl_core::format::format_query;

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
///   (ruling 14). The store seeds that same instant as the watermark when
///   it creates or re-anchors a `since_last` schedule, so this branch is
///   the fallback for a row that predates the seed, never the path a fresh
///   schedule takes. It stays because a schedule with no watermark still
///   has to answer something, and one interval is what ruling 14 says.
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

// ---------------------------------------------------------------------------
// The window materializer (ADR-0018 ruling 11)
// ---------------------------------------------------------------------------

/// Why a planned window could not be put onto a saved query.
///
/// The variants name the check that refused, because the caller's only
/// sensible move is to fail the run and say which one: every one of these
/// means the schedule and the query text disagree in a way
/// [`validate_window_compatibility`] is supposed to have made impossible.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MaterializeError {
    /// The saved text does not parse, so there is nothing to splice onto.
    #[error("the saved query does not parse: {message}")]
    SourceUnparseable {
        /// The parser's first message.
        message: String,
    },
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
    /// The spliced text parses, but its bounds are not the planned ones.
    #[error("the spliced query's bounds did not read back as earliest={start} latest={end}")]
    BoundsNotReadBack {
        /// The lower bound the splice wrote.
        start: String,
        /// The upper bound the splice wrote.
        end: String,
    },
    /// The spliced text parses with the right bounds, but the rest of it
    /// is no longer the saved query.
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
/// Rebuilding the text from the AST instead was rejected: the formatter
/// drops the operator's `#` comments and normalizes spelling, and
/// `report_runs.query` is an audit artefact a human is meant to paste back
/// and get the same report from. A prefix keeps the saved text intact.
///
/// A splice on text is only as good as its guard, so every result is
/// checked before it is returned:
///
/// 1. the saved text parses, owns no time clause, and carries no
///    `from saved` stage ANYWHERE in its pipeline (the emitter refuses one
///    that is not the first stage, but it parses, so a first-stage-only
///    check would let it through here);
/// 2. the spliced text parses;
/// 3. the spliced text's `earliest`/`latest` read back as exactly the two
///    rendered bounds, with no `last=` beside them;
/// 4. everything else about the spliced query is the saved query.
///
/// Check 4 compares the two through [`trawl_core::format::format_query`]
/// rather than by `PartialEq`: the AST carries source spans, and the
/// prefix moves every byte of the original, so a direct comparison would
/// report a difference for every query. Formatting both sides is a
/// projection that drops spans by construction.
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
        && reparsed.search.earliest.as_ref().map(|b| b.node.as_str()) == Some(start.as_str())
        && reparsed.search.latest.as_ref().map(|b| b.node.as_str()) == Some(end.as_str());
    if !bounds_read_back {
        return Err(MaterializeError::BoundsNotReadBack { start, end });
    }

    let mut body = reparsed;
    body.search.earliest = None;
    body.search.latest = None;
    if format_query(&body) != format_query(&original) {
        return Err(MaterializeError::BodyChanged);
    }

    Ok(spliced)
}

/// Parse DSL, reducing a parse failure to its first message.
///
/// The message quotes the operator's own tokens, so it is fine to show a
/// client and never fine to persist: callers put it in an error the HTTP
/// layer redacts, and telemetry logs the class instead.
fn parse_dsl(dsl: &str) -> Result<Query, String> {
    trawl_core::parser::parse(dsl).map_err(|errors| {
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

// ---------------------------------------------------------------------------
// The write-time compatibility rule (ADR-0018 rulings 7 and 12)
// ---------------------------------------------------------------------------

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
        /// The window that was being attached.
        window: ScheduleWindow,
        /// The parser's first message.
        message: String,
    },
    /// The query already spells its own interval (ruling 7).
    #[error(
        "schedule window \"{window}\" conflicts with the saved query's {}= time clause; remove one side",
        .clause.keyword()
    )]
    TimeClause {
        /// The window that was being attached.
        window: ScheduleWindow,
        /// The clause the query carries.
        clause: TimeClause,
    },
    /// The query reads stored report rows, not ingest events (ruling 12).
    #[error(
        "schedule window \"{window}\" conflicts with the saved query's \"from saved\" source; \
         stored report rows cannot receive a _time window"
    )]
    FromSaved {
        /// The window that was being attached.
        window: ScheduleWindow,
    },
}

/// Decide whether a window may be attached to this query text (ADR-0018
/// rulings 7 and 12).
///
/// This is ONE function for both write directions: putting a window on a
/// schedule, and editing the DSL of a saved query that already has one.
/// Two rules that agreed today would drift, and the drift would be a saved
/// query whose schedule window and whose `last=` both claim to say what
/// the report covers.
///
/// No window is always fine, whatever the text. Query mode executes the
/// DSL verbatim and never reaches this check, and saved-query creation has
/// never validated DSL at all, so refusing unparseable text here would be
/// a new rejection wearing a window's name.
///
/// With a window, the text must parse (a window over text nothing can run
/// is a claim about nothing), must own no `last=`/`earliest=`/`latest=`,
/// and must carry no `from saved` stage. The time-clause check reads the
/// parsed AST through [`trawl_core::ast::Query::time_clause`] rather than
/// scanning the source: a backticked `` `last`=5 `` is an ordinary field
/// filter and keeps its schedule window, while `service=x OR last=1h`
/// carries a clause that a per-group text scan would miss.
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

    // -- The window materializer (ADR-0018 ruling 11) ----------------------

    /// The window every materializer test splices: 02:00 to 03:00 UTC.
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

    /// The saved text survives the splice byte for byte, including a
    /// comment the formatter would have dropped, and the result is the
    /// same query with bounds on it.
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
            let mut body = reparsed;
            body.search.earliest = None;
            body.search.latest = None;
            assert_eq!(format_query(&body), format_query(&original), "for {dsl}");
        }

        let commented = materialize_window(
            "service=nginx # nightly rollup, do not touch",
            &spliceable_window(),
        )
        .unwrap();
        assert!(commented.contains("# nightly rollup, do not touch"));
    }

    /// The bounds the splice writes read back as the instants that were
    /// planned, not as text that merely looks like them.
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
        assert_eq!(read(&parsed.search.earliest.unwrap().node), window.start);
        assert_eq!(read(&parsed.search.latest.unwrap().node), window.end);
    }

    /// Ruling 7: a query that owns its own window is refused rather than
    /// given a second one. Write-time policy should have stopped this, so
    /// the refusal names the clause it found.
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

    /// Ruling 12: stored report rows take no `_time` window, wherever the
    /// `from saved` stage sits in the pipeline.
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

    /// Text that does not parse is refused before anything is spliced onto
    /// it, and the parser's own message travels with the refusal.
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

    // -- Window/query compatibility (ADR-0018 rulings 7 and 12) ------------

    /// Ruling 7: two spellings of one interval never coexist, and the
    /// refusal names both so the operator can drop one.
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

    /// Ruling 12: a `from saved` query reads stored report rows, so it can
    /// never be given a `_time` window. The refusal names the window's own
    /// spelling and the source it clashes with.
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

    /// A backticked keyword is an ordinary field name, so a query filtering
    /// a column called `last` keeps its schedule window. This is why the
    /// check reads the AST instead of scanning the text.
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

    /// A hoisted clause is still a clause: `service=x OR last=1h` applies
    /// its window to both groups, and a per-group text scan would miss it.
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

    /// No window means no claim about coverage, so the text is not this
    /// check's business: saved-query creation has never validated DSL and
    /// this rule does not start.
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
}
