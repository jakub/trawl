// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed storage for scheduled queries and report runs.
//!
//! Concurrency rests on the database, not on a process-wide lock. ONE lock
//! order runs through all of it, `saved_queries` -> `schedules` ->
//! `report_runs`, and a path that does not need a level skips it rather
//! than reordering around it: [`ScheduleStore::claim_due_run`] and
//! [`ScheduleStore::claim_manual_run`] take all three,
//! [`super::SavedQueryStore::delete`] takes all three because its cascade
//! reaches every one of them, [`ScheduleStore::delete_schedule`] and
//! [`ScheduleStore::finish_run`] start at `schedules` and never touch
//! `saved_queries`. A path that took the run rows before the schedule would
//! deadlock against any of the others.
//!
//! Level 1 is not optional for anything that writes a run. Inserting a
//! `report_runs` row checks its `saved_query_id` foreign key, and postgres
//! takes FOR KEY SHARE on the saved query to do it — a lock the insert
//! never spells out. A claim holding only the schedule row would therefore
//! be at level 2 waiting for level 1, and a `set_schedule_checked` walking
//! 1 then 2 beside it closes the cycle: postgres kills one of them with
//! 40P01. Taking the saved query explicitly, first, is what keeps that
//! implicit lock in order.
//!
//! - the no-concurrent-run guard is the partial unique index
//!   `report_runs_one_running`; [`ScheduleStore::claim_run`] maps the named
//!   23505 to [`RunClaim::AlreadyRunning`];
//! - the manual-trigger path's `max_runs` check joins the run claim in one
//!   transaction ([`ScheduleStore::claim_run`], `FOR UPDATE` on the schedule
//!   row);
//! - deletions lock the parent row then every `report_runs` row (`FOR
//!   UPDATE`, no `result_path` filter) before collecting parquet paths, so a
//!   concurrent `finish_run` either lands its path before the lock or blocks
//!   until the cascade removes its row — never orphaning the file;
//! - [`ScheduleStore::finish_run`] reports whether it updated a row so a run
//!   cascade-deleted mid-flight can have its freshly-written file removed. A
//!   successful completion also advances the schedule's `since_last`
//!   watermark in that same transaction, and takes the schedule lock FIRST
//!   for it — same order as the claim and the delete, so the three can never
//!   deadlock against each other.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgPool, Row as _};

use super::error::{PgViolation, StoreError, WindowWriteError, classify_violation};
use super::history::{bind_u64, bind_usize};
use super::saved::{SavedQuery, row_to_saved_query_at};
use super::status::{RunStatus, decode_status};
use crate::report_window::{
    Due, MaterializeError, PlanError, PlanInput, ReportWindow, ScheduleWindow, WindowKind,
    WindowPolicyError, materialize_window, plan_due_run, validate_window_compatibility,
};

const MIN_INTERVAL_SECS: u64 = 60;

/// The longest duration the schedule grammar accepts: ten years.
///
/// Every duration here becomes date arithmetic somewhere — a fire cursor, a
/// window bound, a lag applied to both — in postgres, in chrono, or in the
/// DSL text a run executes. Each of those has its own overflow behaviour,
/// and `10000000w` is a typo, never an intent. One cap up front gives all
/// of them a domain, which is cheaper than proving each is total.
pub const MAX_DURATION_SECS: u64 = 315_360_000;

/// A schedule attached to a saved query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: i64,
    pub saved_query_id: i64,
    pub key_id: i64,
    pub interval_secs: u64,
    pub max_runs: Option<u64>,
    pub enabled: bool,
    /// The window this schedule covers, or `None` for query-text timing,
    /// where the saved DSL is executed verbatim (ADR-0018 ruling 6).
    pub window: Option<ScheduleWindow>,
    /// Late-arrival allowance shifting both window bounds back. Zero unless
    /// a window is set.
    pub lag_secs: u64,
    /// The `since_last` watermark: the end of the newest window a
    /// successful run covered. `None` until the first one lands.
    pub covered_through: Option<DateTime<Utc>>,
    /// The planned next fire instant. The scheduler fires on this rather
    /// than on `last_run + interval`, so fire-time drift never accumulates.
    pub next_fire_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single report run (the result blob is fetched separately via
/// [`ScheduleStore::get_run_result`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRun {
    pub id: i64,
    pub schedule_id: i64,
    pub saved_query_id: i64,
    pub query: String,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub row_count: Option<usize>,
    pub error_message: Option<String>,
    /// Filesystem path to the parquet result file (relative to data dir).
    pub result_path: Option<String>,
    /// Inclusive lower bound of the window this run covered; `None` for a
    /// run whose query owned its own time clause (ADR-0018 ruling 11).
    pub window_start: Option<DateTime<Utc>>,
    /// Exclusive upper bound of the covered window.
    pub window_end: Option<DateTime<Utc>>,
    /// Whether the covered window was clamped forward past an uncovered
    /// catch-up gap. `None` (not `Some(false)`) for a run with no window:
    /// `Some(false)` is the positive claim that the window is complete.
    pub window_truncated: Option<bool>,
    /// The mode the run was CLAIMED under. Read instead of the schedule's
    /// current mode wherever finishing the run has to know, so an edit
    /// racing the run cannot change what the run means.
    pub window_kind: Option<WindowKind>,
}

/// Outcome of a transactional run claim ([`ScheduleStore::claim_run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunClaim {
    /// A run row was created; execute it.
    Started(i64),
    /// A run is already in progress for this schedule.
    AlreadyRunning,
    /// The schedule has reached its `max_runs` cap.
    MaxRunsReached,
}

/// Outcome of an operator-triggered run claim
/// ([`ScheduleStore::claim_manual_run`]).
///
/// [`ManualRunClaim::CoverageMode`] is the one that is not about capacity.
/// A schedule with a window OWNS what its reports cover: `since_last` tiles
/// from a watermark a manual run would either skip past or double, and a
/// fixed window is measured from a planned fire a manual run does not have.
/// Neither has defined bounds outside the schedule, so the run is refused
/// rather than given bounds nobody chose (ADR-0018 ruling 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualRunClaim {
    /// A run row was created; execute it.
    Started(ClaimedManualRun),
    /// The saved query has no schedule, so there is nothing to record a
    /// run under.
    NoSchedule,
    /// The schedule owns a window. The mode travels with the refusal so
    /// the caller can name it.
    CoverageMode(ScheduleWindow),
    /// A run is already in progress for this schedule.
    AlreadyRunning,
    /// The schedule has reached its `max_runs` cap.
    MaxRunsReached,
}

/// The run a manual claim created, with everything executing it needs.
///
/// The text comes from the saved-query row the claim locked, never from a
/// snapshot the handler read a moment earlier, for the reason
/// [`ClaimedRun`] gives about the scheduler's enumeration: what a run
/// executes and what `report_runs.query` stores are one string, and reading
/// it outside the lock lets an edit land in between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedManualRun {
    /// The `report_runs` row id.
    pub run_id: i64,
    /// The saved DSL, executed and stored verbatim.
    pub query: String,
    /// The saved query's display name at claim time.
    pub query_name: String,
}

/// Outcome of a scheduler tick's due-run decision
/// ([`ScheduleStore::claim_due_run`]).
///
/// Only [`DueClaim::Started`] carries work. Everything else is a reason the
/// tick moves on, and each one differs in what it left behind: `Advanced`
/// moved the fire cursor over coverage that already exists, while
/// `AlreadyRunning` and `MaxRunsReached` deliberately leave the cursor
/// where it was, so the boundary they refused is claimed again on the next
/// poll once the run finishes or the cap is raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueClaim {
    /// The fire cursor is in the future, or the schedule is disabled or
    /// gone. Nothing was written.
    NotDue,
    /// The cursor moved past a window already covered by the watermark; no
    /// run was claimed (ADR-0018 [`Due::Advance`]).
    Advanced,
    /// A run row was created; execute it.
    Started(ClaimedRun),
    /// A run for this schedule is still in flight. The cursor stays put:
    /// the boundary is claimed at the next poll after that run finishes,
    /// and the missed ones coalesce into its window (ruling 9).
    AlreadyRunning,
    /// The schedule hit its `max_runs` cap. The cursor stays put, so
    /// raising the cap resumes from the boundary that was refused.
    MaxRunsReached,
}

/// The run a due claim created, with everything executing it needs.
///
/// `resolved_query` is what the run executes AND what `report_runs.query`
/// stores. It is deliberately not the saved DSL the enumeration handed the
/// scheduler: the saved text is read inside the claim transaction, under
/// the saved-query row lock, and a window is spliced onto it there (ADR-0018
/// ruling 11). Executing the enumeration's copy would run text nobody
/// recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedRun {
    /// The `report_runs` row id.
    pub run_id: i64,
    /// The saved query this run belongs to.
    pub saved_query_id: i64,
    /// The saved query's display name at claim time.
    pub query_name: String,
    /// The resolved DSL: the saved text with `earliest=`/`latest=` spliced
    /// on, or the saved text verbatim in query mode.
    pub resolved_query: String,
    /// The window this run covers, or `None` in query mode.
    pub window: Option<ReportWindow>,
}

/// Why a due-run claim failed.
///
/// Four distinct causes, kept apart because they mean different things
/// about the install: the store is down, the schedule's own numbers cannot
/// be planned, or the schedule's window and its saved DSL disagree in one
/// of the two ways write-time validation exists to prevent. Every one of
/// them leaves the fire cursor alone, so the tick fails loudly on every
/// poll until the state is repaired instead of skipping a schedule
/// silently.
///
/// It is deliberately NOT a [`StoreError`] variant. `ServerError` already
/// wraps these three window errors directly, with a considered wire
/// treatment for each (the policy refusal is the operator's own input and
/// keeps its text; a plan or materialize failure is broken stored state and
/// is redacted). Re-wrapping them in `StoreError` would give each leaf a
/// second, differently-mapped route to the same wire.
#[derive(Debug, thiserror::Error)]
pub enum DueClaimError {
    /// The app-state store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The fire cursor or a window bound could not be computed.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// The schedule's window may not be attached to this query text
    /// (ADR-0018 rulings 7 and 12).
    #[error(transparent)]
    Policy(#[from] WindowPolicyError),
    /// The planned window could not be put onto the saved DSL (ruling 11).
    #[error(transparent)]
    Materialize(#[from] MaterializeError),
}

impl From<sqlx::Error> for DueClaimError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
}

impl DueClaimError {
    /// A closed-set class label for log events, for the same reason
    /// [`StoreError::class`] has one: a window failure's Display can quote
    /// the saved DSL and the parser's message, and a scheduler event lands
    /// in the retained `service=trawld` corpus.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Store(e) => e.class(),
            Self::Plan(_) => "window_plan",
            Self::Policy(_) => "window_policy",
            Self::Materialize(_) => "window_materialize",
        }
    }
}

/// Outcome of [`ScheduleStore::finish_run`], naming the file-cleanup obligation
/// the caller inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishOutcome {
    /// The run row was updated; any result file it points at must be kept.
    Persisted,
    /// Zero rows updated — the run was cascade-deleted mid-flight (its saved
    /// query or schedule is gone); the caller must remove any result file it
    /// just wrote.
    RunDeleted,
}

/// Outcome of [`ScheduleStore::fail_run_if_running`]'s guarded flip. Note the
/// file-cleanup obligation is *inverted* relative to [`FinishOutcome`]: here the
/// successful flip (`FlippedToError`) is the one that orphans a file, because it
/// means the earlier success never committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipOutcome {
    /// A still-`running` row was flipped to `error`: the earlier success never
    /// committed, so any parquet the caller wrote is orphaned and should be
    /// removed.
    FlippedToError,
    /// No `running` row matched — either the ambiguous success actually
    /// committed (its result must be preserved) or the run was cascade-deleted.
    /// Either way, leave the file alone.
    NotRunning,
}

/// Parse a duration string (same syntax as the DSL `last=` filter) into
/// seconds, with no lower bound.
///
/// Supported units: `s`, `m`, `h`, `d`, `w`. This is the grammar alone, so
/// `"0s"` and `"30s"` are legal answers: a report window's `lag` is a
/// straggler allowance and zero is its default, while a schedule interval
/// has a floor and goes through [`parse_interval`] instead.
///
/// There is a ceiling either way, [`MAX_DURATION_SECS`], because every
/// duration the grammar parses ends up in date arithmetic.
pub fn parse_duration_secs(s: &str) -> Result<u64, StoreError> {
    let s = s.trim();
    // char_indices, not byte split_at: a multi-byte trailing char would put
    // `s.len() - 1` inside a UTF-8 sequence and panic on user-supplied input.
    let Some((unit_idx, unit)) = s.char_indices().last() else {
        return Err(StoreError::InvalidInterval {
            input: s.to_string(),
        });
    };
    let digits = &s[..unit_idx];
    let value: u64 = digits.parse().map_err(|_| StoreError::InvalidInterval {
        input: s.to_string(),
    })?;

    // Checked: `999999999999w` is client input, and a wrapping multiply
    // would turn a nonsense duration into a plausible small one.
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        'w' => 604_800,
        _ => {
            return Err(StoreError::InvalidInterval {
                input: s.to_string(),
            });
        }
    };
    let secs = value
        .checked_mul(scale)
        .ok_or(StoreError::InvalidInterval {
            input: s.to_string(),
        })?;
    if secs > MAX_DURATION_SECS {
        return Err(StoreError::DurationTooLong { secs });
    }
    Ok(secs)
}

/// Parse a duration string into seconds, refusing anything below the 60s
/// schedule floor. The grammar is [`parse_duration_secs`].
pub fn parse_interval(s: &str) -> Result<u64, StoreError> {
    let secs = parse_duration_secs(s)?;
    if secs < MIN_INTERVAL_SECS {
        return Err(StoreError::IntervalTooShort { secs });
    }
    Ok(secs)
}

/// Reject a sub-minute interval at the store boundary.
///
/// [`parse_interval`] already rejects short strings, but that guards only the
/// one handler path; binding the invariant here means every entry into
/// [`ScheduleStore::create_schedule`]/[`ScheduleStore::update_schedule`]
/// upholds it, matched by the migration's `CHECK (interval_secs >= 60)`.
fn ensure_min_interval(secs: u64) -> Result<(), StoreError> {
    if secs < MIN_INTERVAL_SECS {
        return Err(StoreError::IntervalTooShort { secs });
    }
    Ok(())
}

/// Refuse a lag on a schedule that has no window.
///
/// `lag` shifts a window's bounds back to cover stragglers, so without a
/// window there is nothing for it to shift: the saved DSL owns its own time
/// clause and trawl must execute it verbatim. Accepting the pair would store
/// a number that changes no answer, and the operator would read the schedule
/// back as if the allowance were in force.
fn ensure_lag_has_window(window: Option<ScheduleWindow>, lag_secs: u64) -> Result<(), StoreError> {
    if window.is_none() && lag_secs > 0 {
        return Err(StoreError::LagWithoutWindow { lag_secs });
    }
    Ok(())
}

/// The instant a `since_last` schedule starts owing coverage from.
///
/// A schedule anchored at `next_fire_at` fires there first, and ruling 14
/// makes that first window `[next_fire_at - lag - interval, next_fire_at -
/// lag)`. Writing its lower bound down as the watermark at creation makes
/// the owed interval durable BEFORE any run exists, which is the whole
/// point: if the first run FAILS, an unset watermark sends the planner back
/// to the same ruling-14 fallback and the next run re-covers one interval
/// ending at its own fire, so the failed interval is dropped with nothing
/// recording the loss. Seeded, the failure leaves the origin standing and
/// the next success covers both intervals, exactly as every later failure
/// already behaved.
///
/// The arithmetic is checked. [`parse_duration_secs`] caps both numbers at
/// ten years, so an overflow here needs a caller that bypassed the grammar.
fn seed_covered_through(
    next_fire_at: DateTime<Utc>,
    interval_secs: u64,
    lag_secs: u64,
) -> Result<DateTime<Utc>, StoreError> {
    let owed = interval_secs
        .checked_add(lag_secs)
        .and_then(|secs| i64::try_from(secs).ok())
        .and_then(chrono::TimeDelta::try_seconds)
        .and_then(|delta| next_fire_at.checked_sub_signed(delta));
    owed.ok_or(StoreError::DurationTooLong {
        secs: interval_secs.saturating_add(lag_secs),
    })
}

/// Format seconds into a human-readable duration string (e.g. "5m", "1h").
pub fn format_interval(secs: u64) -> String {
    // Zero first: every modulus divides it, so the ladder below would render
    // "0w" and a `lag = 0` would read back as a week.
    if secs == 0 {
        "0s".to_owned()
    } else if secs.is_multiple_of(604_800) {
        format!("{}w", secs / 604_800)
    } else if secs.is_multiple_of(86400) {
        format!("{}d", secs / 86400)
    } else if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Decode a [`Schedule`] from columns named `{prefix}id`, `{prefix}key_id`, …
pub(crate) fn row_to_schedule_at(row: &PgRow, prefix: &str) -> Result<Schedule, sqlx::Error> {
    let col = |name: &str| format!("{prefix}{name}");
    Ok(Schedule {
        id: row.try_get(col("id").as_str())?,
        saved_query_id: row.try_get(col("saved_query_id").as_str())?,
        key_id: row.try_get(col("key_id").as_str())?,
        interval_secs: u64::try_from(row.try_get::<i64, _>(col("interval_secs").as_str())?)
            .unwrap_or_default(),
        max_runs: row
            .try_get::<Option<i64>, _>(col("max_runs").as_str())?
            .map(|v| u64::try_from(v).unwrap_or_default()),
        enabled: row.try_get(col("enabled").as_str())?,
        window: decode_window(row, prefix)?,
        lag_secs: u64::try_from(row.try_get::<i64, _>(col("lag_secs").as_str())?)
            .unwrap_or_default(),
        covered_through: row.try_get(col("covered_through").as_str())?,
        next_fire_at: row.try_get(col("next_fire_at").as_str())?,
        created_at: row.try_get(col("created_at").as_str())?,
        updated_at: row.try_get(col("updated_at").as_str())?,
    })
}

/// Decode the `window_kind`/`window_secs` pair into a [`ScheduleWindow`].
///
/// The pairing is enforced by `schedules_window_shape`, so an unpaired or
/// unknown value here means the row was written past the constraint (a
/// hand-edit, a future kind this binary predates). That is a decode failure,
/// not a `None` window: silently reading it as "no window" would make the
/// scheduler use query-text timing and execute the DSL verbatim.
pub(crate) fn decode_window(
    row: &PgRow,
    prefix: &str,
) -> Result<Option<ScheduleWindow>, sqlx::Error> {
    let kind: Option<String> = row.try_get(format!("{prefix}window_kind").as_str())?;
    let secs: Option<i64> = row.try_get(format!("{prefix}window_secs").as_str())?;
    let decode_err = |msg: String| sqlx::Error::Decode(msg.into());
    match (kind.as_deref().map(WindowKind::parse), secs) {
        (None, None) => Ok(None),
        (Some(Some(WindowKind::SinceLast)), None) => Ok(Some(ScheduleWindow::SinceLast)),
        (Some(Some(WindowKind::Fixed)), Some(secs)) => Ok(Some(ScheduleWindow::Fixed {
            secs: u64::try_from(secs).unwrap_or_default(),
        })),
        _ => Err(decode_err(format!(
            "schedule window_kind={kind:?} with window_secs={secs:?} is not a valid window"
        ))),
    }
}

/// Decode a run's claim-time `window_kind`.
///
/// Absent is a run with no window. Present but unreadable means the row was
/// written past the `report_runs_window_kind` CHECK, and reading it as
/// "no window" would silently drop a watermark advance, so it fails instead.
fn decode_run_window_kind(row: &PgRow, prefix: &str) -> Result<Option<WindowKind>, sqlx::Error> {
    let kind: Option<String> = row.try_get(format!("{prefix}window_kind").as_str())?;
    match kind {
        None => Ok(None),
        Some(kind) => WindowKind::parse(&kind).map(Some).ok_or_else(|| {
            sqlx::Error::Decode(
                format!("report run window_kind={kind:?} is not a window mode").into(),
            )
        }),
    }
}

/// Decode a [`ReportRun`] from columns named `{prefix}id`, `{prefix}status`, …
pub(crate) fn row_to_report_run_at(row: &PgRow, prefix: &str) -> Result<ReportRun, sqlx::Error> {
    let col = |name: &str| format!("{prefix}{name}");
    Ok(ReportRun {
        id: row.try_get(col("id").as_str())?,
        schedule_id: row.try_get(col("schedule_id").as_str())?,
        saved_query_id: row.try_get(col("saved_query_id").as_str())?,
        query: row.try_get(col("query").as_str())?,
        status: decode_status(row, col("status").as_str())?,
        started_at: row.try_get(col("started_at").as_str())?,
        finished_at: row.try_get(col("finished_at").as_str())?,
        duration_ms: row
            .try_get::<Option<i64>, _>(col("duration_ms").as_str())?
            .map(|v| u64::try_from(v).unwrap_or_default()),
        row_count: row
            .try_get::<Option<i64>, _>(col("row_count").as_str())?
            .map(|v| usize::try_from(v).unwrap_or_default()),
        error_message: row.try_get(col("error_message").as_str())?,
        result_path: row.try_get(col("result_path").as_str())?,
        window_start: row.try_get(col("window_start").as_str())?,
        window_end: row.try_get(col("window_end").as_str())?,
        window_truncated: row.try_get(col("window_truncated").as_str())?,
        window_kind: decode_run_window_kind(row, prefix)?,
    })
}

fn row_to_schedule(row: &PgRow) -> Result<Schedule, sqlx::Error> {
    row_to_schedule_at(row, "")
}

fn row_to_report_run(row: &PgRow) -> Result<ReportRun, sqlx::Error> {
    row_to_report_run_at(row, "")
}

/// SELECT fragment exposing the latest report run (aliased `r_*`) and the total
/// run count (`run_count`) resolved by [`LATEST_RUN_JOINS`]. Decode the pair
/// with [`latest_run_and_count_from_row`]. Requires a schedule aliased `s` and
/// the lateral joins `lr`/`rc`.
pub(crate) const LATEST_RUN_COLS: &str = "lr.id             AS r_id,
     lr.schedule_id    AS r_schedule_id,
     lr.saved_query_id AS r_saved_query_id,
     lr.query          AS r_query,
     lr.status         AS r_status,
     lr.started_at     AS r_started_at,
     lr.finished_at    AS r_finished_at,
     lr.duration_ms    AS r_duration_ms,
     lr.row_count      AS r_row_count,
     lr.error_message  AS r_error_message,
     lr.result_path    AS r_result_path,
     lr.window_start   AS r_window_start,
     lr.window_end     AS r_window_end,
     lr.window_truncated AS r_window_truncated,
     lr.window_kind    AS r_window_kind,
     rc.run_count      AS run_count";

/// LEFT JOIN LATERAL fragment resolving the single latest run (`lr`, tie-broken
/// by `started_at DESC, id DESC`) and the total run count (`rc`) for the
/// schedule aliased `s`. Pairs with [`LATEST_RUN_COLS`].
pub(crate) const LATEST_RUN_JOINS: &str = "LEFT JOIN LATERAL (
         SELECT * FROM report_runs r
         WHERE r.schedule_id = s.id
         ORDER BY r.started_at DESC, r.id DESC
         LIMIT 1
     ) lr ON TRUE
     LEFT JOIN LATERAL (
         SELECT COUNT(*) AS run_count FROM report_runs r2 WHERE r2.schedule_id = s.id
     ) rc ON TRUE";

/// Decode the latest run (`r_` prefix, `None` when the schedule has no runs)
/// and total run count (`run_count`) columns produced by [`LATEST_RUN_COLS`].
pub(crate) fn latest_run_and_count_from_row(
    row: &PgRow,
) -> Result<(Option<ReportRun>, u64), sqlx::Error> {
    let latest_run = if row.try_get::<Option<i64>, _>("r_id")?.is_some() {
        Some(row_to_report_run_at(row, "r_")?)
    } else {
        None
    };
    let total_runs =
        u64::try_from(row.try_get::<Option<i64>, _>("run_count")?.unwrap_or(0)).unwrap_or_default();
    Ok((latest_run, total_runs))
}

const SCHEDULE_COLS: &str = "id, saved_query_id, key_id, interval_secs, max_runs, enabled, \
     window_kind, window_secs, lag_secs, covered_through, next_fire_at, created_at, updated_at";

const RUN_COLS: &str = "id, schedule_id, saved_query_id, query, status, started_at, finished_at, \
     duration_ms, row_count, error_message, result_path, window_start, window_end, \
     window_truncated, window_kind";

/// Postgres-backed storage for schedules and report runs. Cheap to clone.
#[derive(Debug, Clone)]
pub struct ScheduleStore {
    pool: PgPool,
}

impl ScheduleStore {
    /// Wrap the shared app-state pool.
    ///
    /// The pool must point at an already-migrated trawl app-state database
    /// (production goes through `StorageState::connect`, which migrates;
    /// tests use `#[sqlx::test]`-migrated pools directly).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a schedule for a saved query.
    ///
    /// `now` is the caller's instant and becomes the schedule's first
    /// `next_fire_at`, so a schedule created at T is due at T. It is
    /// deliberately not the database's `now()`: the scheduler samples one
    /// application instant per tick and compares fire cursors against it,
    /// and a test driving a fake clock has to be able to create a schedule
    /// that is due at its own instant.
    ///
    /// The schedule is created ENABLED. This door takes no flag because it
    /// has no caller that wants one: the HTTP door is
    /// [`Self::set_schedule_checked`], which passes the request's `enabled`
    /// to the same INSERT.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_schedule(
        &self,
        saved_query_id: i64,
        key_id: i64,
        interval_secs: u64,
        max_runs: Option<u64>,
        window: Option<ScheduleWindow>,
        lag_secs: u64,
        now: DateTime<Utc>,
    ) -> Result<Schedule, StoreError> {
        let mut conn = self.pool.acquire().await?;
        let schedule = create_schedule_in(
            &mut conn,
            saved_query_id,
            key_id,
            interval_secs,
            max_runs,
            true,
            window,
            lag_secs,
            now,
        )
        .await?;
        log_schedule_created(&schedule);
        Ok(schedule)
    }

    /// Update an existing schedule. Returns `NotFound` if not owned by `key_id`.
    ///
    /// Two cursor rules, both about not moving coverage the operator did not
    /// ask to move:
    ///
    /// - An existing `covered_through` is never touched. Editing a schedule
    ///   (or its saved DSL) does not reset the watermark — the per-run
    ///   resolved snapshot is the audit trail (ADR-0018 ruling 14). An
    ///   ABSENT one is seeded when the edit re-anchors a `since_last`
    ///   schedule, for the reason [`seed_covered_through`] gives: the
    ///   re-anchored cursor is a fresh origin of owed coverage, and a first
    ///   run that fails under it must not drop its interval. The SQL is a
    ///   `COALESCE`, so "seed only when absent" is one statement rather than
    ///   a read followed by a decision.
    /// - `next_fire_at` is re-anchored to `now` only when the cadence itself
    ///   changed: a different `interval_secs` or a different window. Editing
    ///   `max_runs` or flipping `enabled` leaves the planned cursor alone, so
    ///   a schedule cannot be kept permanently un-due by repeated edits. The
    ///   comparison reads the current row under `FOR UPDATE`, in the same
    ///   transaction as the write, so a concurrent edit cannot land between
    ///   the read and the decision.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_schedule(
        &self,
        id: i64,
        key_id: i64,
        interval_secs: u64,
        max_runs: Option<u64>,
        enabled: bool,
        window: Option<ScheduleWindow>,
        lag_secs: u64,
        now: DateTime<Utc>,
    ) -> Result<Schedule, StoreError> {
        let mut tx = self.pool.begin().await?;
        let schedule = match update_schedule_in(
            &mut tx,
            id,
            key_id,
            interval_secs,
            max_runs,
            enabled,
            window,
            lag_secs,
            now,
        )
        .await
        {
            Ok(schedule) => schedule,
            Err(e) => {
                tx.rollback().await?;
                return Err(e);
            }
        };
        tx.commit().await?;
        log_schedule_updated(&schedule);
        Ok(schedule)
    }

    /// Create or update the schedule of a saved query, with ADR-0018's
    /// window/query compatibility proved in the same transaction as the
    /// write (rulings 7 and 12).
    ///
    /// This is the HTTP door. The pair it writes is two rows an operator
    /// edits independently, and the rule spans both of them, so a
    /// read-then-write sequence would leave the window a schedule to hold
    /// and a `last=` in the saved DSL one interleaving apart: read the DSL,
    /// have [`super::SavedQueryStore::update_checked`] commit a time clause,
    /// then write the window over a query that now owns its own. Locking
    /// the saved query FOR UPDATE and reading its text under that lock is
    /// what closes it — the other direction takes the same lock first, so
    /// one of the two waits and sees the other's committed text.
    ///
    /// LOCK ORDER: `saved_queries` -> `schedules`, the order every
    /// multi-row path in this module takes.
    ///
    /// Query mode (`window` is `None`) parses nothing:
    /// [`validate_window_compatibility`] returns immediately, and the saved
    /// text stays as unexamined here as `create_saved` leaves it.
    ///
    /// `enabled` reaches both paths: a `PUT` carrying `enabled: false` on a
    /// saved query with no schedule yet creates a disabled one, rather than
    /// creating it enabled and leaving the caller to send a second request.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_schedule_checked(
        &self,
        saved_query_id: i64,
        key_id: i64,
        interval_secs: u64,
        max_runs: Option<u64>,
        enabled: bool,
        window: Option<ScheduleWindow>,
        lag_secs: u64,
        now: DateTime<Utc>,
    ) -> Result<Schedule, WindowWriteError> {
        // Cheap refusals before any lock: a bad interval or a lag with no
        // window is decided by the arguments alone.
        ensure_min_interval(interval_secs)?;
        ensure_lag_has_window(window, lag_secs)?;

        let mut tx = self.pool.begin().await?;

        // Level 1: the saved query. The lock and the DSL read are one
        // statement, and the ownership check rides the same WHERE clause.
        let dsl: Option<String> = sqlx::query_scalar(
            "SELECT query FROM saved_queries WHERE id = $1 AND key_id = $2 FOR UPDATE",
        )
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(dsl) = dsl else {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id: saved_query_id,
                resource: "saved query",
            }
            .into());
        };

        if let Err(e) = validate_window_compatibility(window, &dsl) {
            tx.rollback().await?;
            return Err(e.into());
        }

        // Which of the two writes to make. An unlocked read is enough: the
        // saved-query lock above serializes every writer of this pair, so
        // no schedule can appear or vanish between here and the write.
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM schedules WHERE saved_query_id = $1 AND key_id = $2",
        )
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&mut *tx)
        .await?;

        // Level 2: the schedule.
        let written = match existing {
            Some(id) => {
                update_schedule_in(
                    &mut tx,
                    id,
                    key_id,
                    interval_secs,
                    max_runs,
                    enabled,
                    window,
                    lag_secs,
                    now,
                )
                .await
            }
            None => {
                create_schedule_in(
                    &mut tx,
                    saved_query_id,
                    key_id,
                    interval_secs,
                    max_runs,
                    enabled,
                    window,
                    lag_secs,
                    now,
                )
                .await
            }
        };
        let schedule = match written {
            Ok(schedule) => schedule,
            Err(e) => {
                tx.rollback().await?;
                return Err(e.into());
            }
        };

        tx.commit().await?;

        if existing.is_some() {
            log_schedule_updated(&schedule);
        } else {
            log_schedule_created(&schedule);
        }

        Ok(schedule)
    }

    /// Delete a schedule by its saved query id, collecting the parquet paths
    /// of its runs in the same transaction (cascade wipes the rows).
    ///
    /// Lock the schedule before its runs, as [`Self::claim_run`] does after
    /// locking the saved query. `FOR UPDATE` also blocks a concurrent run
    /// INSERT via its FK `FOR KEY SHARE`. Then lock every one of its
    /// `report_runs`. Locking all run rows, not
    /// just those with a non-null `result_path`, forces a concurrent
    /// `finish_run` to either commit its path before us (collected here) or
    /// block until our cascade deletes its row (it then updates zero rows and
    /// the caller unlinks the file). Filtering on `result_path IS NOT NULL`
    /// would skip still-running rows and reopen the orphan-file race.
    ///
    /// Returns the relative parquet paths for the caller to unlink, or
    /// `NotFound` if no schedule is owned by `key_id` for this saved query.
    pub async fn delete_schedule(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<Vec<String>, StoreError> {
        let mut tx = self.pool.begin().await?;

        let schedule_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM schedules WHERE saved_query_id = $1 AND key_id = $2 FOR UPDATE",
        )
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(schedule_id) = schedule_id else {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id: saved_query_id,
                resource: "schedule",
            });
        };

        let paths: Vec<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT result_path FROM report_runs WHERE schedule_id = $1 FOR UPDATE",
        )
        .bind(schedule_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .flatten()
        .collect();

        sqlx::query("DELETE FROM schedules WHERE id = $1")
            .bind(schedule_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            event_type = "schedule_deleted",
            saved_query_id,
            key_id,
            "Schedule deleted"
        );

        Ok(paths)
    }

    /// Get the schedule for a given saved query, if owned by `key_id`.
    pub async fn get_schedule_for_saved_query(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<Option<Schedule>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {SCHEDULE_COLS} FROM schedules WHERE saved_query_id = $1 AND key_id = $2"
        )))
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_schedule).transpose()?)
    }

    /// Get a schedule with its latest run and total run count in one
    /// statement (feeds `ScheduleResponse` without per-item lookups).
    pub async fn get_schedule_with_stats(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<Option<(Schedule, Option<ReportRun>, u64)>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {cols},
                    {LATEST_RUN_COLS}
             FROM schedules s
             {LATEST_RUN_JOINS}
             WHERE s.saved_query_id = $1 AND s.key_id = $2",
            cols = schedule_cols("s.")
        )))
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };
        let schedule = row_to_schedule(&row)?;
        let (latest_run, total_runs) = latest_run_and_count_from_row(&row)?;
        Ok(Some((schedule, latest_run, total_runs)))
    }

    /// List all enabled schedules with their associated saved queries.
    /// Used by the scheduler — no ownership check (internal/cross-user).
    pub async fn list_enabled_schedules(&self) -> Result<Vec<(Schedule, SavedQuery)>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {cols},
                    sq.id         AS sq_id,
                    sq.key_id     AS sq_key_id,
                    sq.name       AS sq_name,
                    sq.query      AS sq_query,
                    sq.created_at AS sq_created_at,
                    sq.updated_at AS sq_updated_at
             FROM schedules s
             JOIN saved_queries sq ON sq.id = s.saved_query_id
             WHERE s.enabled = TRUE
             ORDER BY s.id",
            cols = schedule_cols("s.")
        )))
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push((row_to_schedule(row)?, row_to_saved_query_at(row, "sq_")?));
        }
        Ok(out)
    }

    /// Count enabled schedules (dashboard/monitor).
    pub async fn count_enabled_schedules(&self) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schedules WHERE enabled = TRUE")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    /// Claim a run in one transaction. Lock the saved query, then the schedule,
    /// enforce `max_runs`, and insert the running row. Concurrent claims cannot
    /// exceed the cap.
    ///
    /// `window` is the interval the run is about to cover, recorded on the
    /// row at claim time because that is when it is decided. `None` writes
    /// all three bound columns NULL: the query owns its own time clause and
    /// trawl claims no coverage for it.
    pub async fn claim_run(
        &self,
        schedule_id: i64,
        saved_query_id: i64,
        query: &str,
        max_runs: Option<u64>,
        window: Option<&ReportWindow>,
    ) -> Result<RunClaim, StoreError> {
        let mut tx = self.pool.begin().await?;

        // The run insert checks its saved-query foreign key. Take that lock
        // before the schedule lock to keep saved_queries -> schedules order.
        let saved: Option<i64> =
            sqlx::query_scalar("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
                .bind(saved_query_id)
                .fetch_optional(&mut *tx)
                .await?;
        if saved.is_none() {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id: saved_query_id,
                resource: "saved query",
            });
        }

        // FOR UPDATE serializes concurrent claims on this schedule; a claim
        // that lost the race observes the winner's committed run count.
        let locked: Option<i64> =
            sqlx::query_scalar("SELECT id FROM schedules WHERE id = $1 FOR UPDATE")
                .bind(schedule_id)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id: schedule_id,
                resource: "schedule",
            });
        }

        if let Some(max) = max_runs {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM report_runs WHERE schedule_id = $1")
                    .bind(schedule_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if u64::try_from(count).unwrap_or_default() >= max {
                tx.rollback().await?;
                return Ok(RunClaim::MaxRunsReached);
            }
        }

        let inserted =
            insert_running_run(&mut tx, schedule_id, saved_query_id, query, window).await;

        match inserted {
            Ok(id) => {
                tx.commit().await?;
                tracing::info!(
                    event_type = "report_run_started",
                    run_id = id,
                    schedule_id,
                    saved_query_id,
                    "Report run started"
                );
                Ok(RunClaim::Started(id))
            }
            Err(e) => {
                tx.rollback().await?;
                match classify_violation(&e) {
                    Some(PgViolation::RunAlreadyRunning) => Ok(RunClaim::AlreadyRunning),
                    _ => Err(e.into()),
                }
            }
        }
    }

    /// Claim an operator-triggered run of a saved query's schedule.
    ///
    /// The whole decision is one transaction, and the coverage-mode test is
    /// asked FIRST, before the cap count and before the insert. A windowed
    /// schedule is refused outright (ADR-0018 ruling 6), and asking under
    /// the schedule lock is what makes the refusal reliable: a
    /// `PUT .../schedule` that adds a window either commits before this
    /// read or waits behind it, so a manual run can never slip through on a
    /// snapshot taken a moment earlier.
    ///
    /// A missing saved query is a `NotFound`, not a [`ManualRunClaim`]
    /// variant: the caller has nothing to run and nothing to own the run,
    /// which is the same answer a request naming a stranger's id gets.
    ///
    /// LOCK ORDER: `saved_queries` -> `schedules` -> `report_runs`, all
    /// three, the same walk [`Self::claim_due_run`] takes. The saved query
    /// is locked EXPLICITLY even though only its text is read, because the
    /// insert at level 3 takes FOR KEY SHARE on that same row for its
    /// foreign key check. Locking the schedule first would leave this
    /// transaction at level 2 waiting for level 1, and
    /// [`Self::set_schedule_checked`] walking 1 then 2 beside it closes the
    /// cycle postgres answers with 40P01.
    ///
    /// Reading the DSL from that locked row rather than from the caller is
    /// the same rule [`ClaimedRun`] states: the text this run executes and
    /// the text `report_runs.query` stores are one string, so a concurrent
    /// edit lands entirely before the claim or waits for it.
    pub async fn claim_manual_run(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<ManualRunClaim, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Level 1. The lock, the ownership check and the DSL read are one
        // statement.
        let saved = sqlx::query(
            "SELECT name, query FROM saved_queries WHERE id = $1 AND key_id = $2 FOR UPDATE",
        )
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(saved) = saved else {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id: saved_query_id,
                resource: "saved query",
            });
        };
        let query_name: String = saved.try_get("name")?;
        let query: String = saved.try_get("query")?;

        // Level 2.
        let locked = sqlx::query(AssertSqlSafe(format!(
            "SELECT {SCHEDULE_COLS} FROM schedules
             WHERE saved_query_id = $1 AND key_id = $2 FOR UPDATE"
        )))
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(locked) = locked else {
            tx.rollback().await?;
            return Ok(ManualRunClaim::NoSchedule);
        };
        let schedule = row_to_schedule(&locked)?;

        if let Some(window) = schedule.window {
            tx.rollback().await?;
            return Ok(ManualRunClaim::CoverageMode(window));
        }

        // The cap is counted from the LOCKED row's `max_runs`, not from a
        // value the caller read earlier: concurrent triggers serialize
        // here, and each one sees the winner's committed run count.
        if let Some(max) = schedule.max_runs {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM report_runs WHERE schedule_id = $1")
                    .bind(schedule.id)
                    .fetch_one(&mut *tx)
                    .await?;
            if u64::try_from(count).unwrap_or_default() >= max {
                tx.rollback().await?;
                return Ok(ManualRunClaim::MaxRunsReached);
            }
        }

        // Level 3. A manual run is query mode by definition, so it records
        // no window and nothing is spliced onto the text.
        match insert_running_run(&mut tx, schedule.id, saved_query_id, &query, None).await {
            Ok(run_id) => {
                tx.commit().await?;
                tracing::info!(
                    event_type = "report_run_started",
                    run_id,
                    schedule_id = schedule.id,
                    saved_query_id,
                    "Report run started"
                );
                Ok(ManualRunClaim::Started(ClaimedManualRun {
                    run_id,
                    query,
                    query_name,
                }))
            }
            Err(e) => {
                tx.rollback().await?;
                match classify_violation(&e) {
                    Some(PgViolation::RunAlreadyRunning) => Ok(ManualRunClaim::AlreadyRunning),
                    _ => Err(e.into()),
                }
            }
        }
    }

    /// Plan, materialize and claim one schedule's due run in a single
    /// transaction (ADR-0018 rulings 6-14).
    ///
    /// This is the scheduler tick's whole decision. It reads the schedule's
    /// cadence, asks [`plan_due_run`] what the instant `now` owes, splices
    /// the planned window onto the saved DSL, and inserts the `running` row
    /// carrying that resolved text — then, and only then, moves the fire
    /// cursor. One transaction is what makes the cursor and the run row one
    /// fact: a crash between them would either lose a boundary forever or
    /// claim it twice.
    ///
    /// `now` is a value, never a clock reading taken here: the tick samples
    /// one instant and every schedule in it is judged against that same
    /// instant, so two schedules cannot land on either side of a boundary
    /// that passed mid-poll.
    ///
    /// LOCK ORDER: `saved_queries` -> `schedules` -> `report_runs`, the
    /// order every multi-row path in this module takes, extended one level
    /// up. The saved-query row is locked FIRST because the DSL read below
    /// is what this run executes and stores: a concurrent edit either lands
    /// entirely before the claim or waits for it, so no run can execute
    /// text that was never recorded. [`Self::delete_schedule`] and
    /// [`Self::finish_run`] start at `schedules` and never reach for
    /// `saved_queries`, which skips a level of the same order rather than
    /// inverting it.
    ///
    /// Three outcomes deliberately leave `next_fire_at` alone:
    /// [`DueClaim::NotDue`], [`DueClaim::AlreadyRunning`] and
    /// [`DueClaim::MaxRunsReached`]. A boundary refused because the
    /// previous run is still going is claimed at the next poll once it
    /// finishes, and the coverage it owed folds into that window; a
    /// boundary refused by the cap is claimed when the cap is raised. An
    /// error leaves it alone for the same reason, and is loud on every poll
    /// until the operator repairs the schedule.
    pub async fn claim_due_run(
        &self,
        schedule_id: i64,
        now: DateTime<Utc>,
        max_catchup_intervals: u32,
    ) -> Result<DueClaim, DueClaimError> {
        let mut tx = self.pool.begin().await?;

        let Some(locked) = lock_for_claim(&mut tx, schedule_id).await? else {
            tx.rollback().await?;
            return Ok(DueClaim::NotDue);
        };
        let LockedSchedule {
            saved_query_id,
            query_name,
            dsl,
            schedule,
        } = locked;

        let plan = match plan_due_run(&PlanInput {
            now,
            next_fire_at: schedule.next_fire_at,
            interval_secs: schedule.interval_secs,
            window: schedule.window,
            lag_secs: schedule.lag_secs,
            covered_through: schedule.covered_through,
            max_catchup_intervals,
        }) {
            Ok(Due::NotYet) => {
                tx.rollback().await?;
                return Ok(DueClaim::NotDue);
            }
            Ok(Due::Advance { next_fire_at }) => {
                set_next_fire_at(&mut tx, schedule_id, next_fire_at).await?;
                tx.commit().await?;
                tracing::info!(
                    event_type = "schedule_cursor_advanced",
                    schedule_id,
                    "fire cursor advanced over a window the watermark already covers"
                );
                return Ok(DueClaim::Advanced);
            }
            Ok(Due::Run(plan)) => plan,
            Err(e) => {
                tx.rollback().await?;
                return Err(e.into());
            }
        };

        // Ruling 11: what the run executes is what it stores. A window
        // failure rolls back with the cursor untouched, so the schedule is
        // refused again on the next poll instead of quietly skipping a
        // boundary.
        let resolved_query = match plan.window {
            Some(window) => match resolve_window_text(schedule.window, &dsl, &window) {
                Ok(text) => text,
                Err(e) => {
                    tx.rollback().await?;
                    return Err(e);
                }
            },
            None => dsl,
        };

        if let Some(max) = schedule.max_runs {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM report_runs WHERE schedule_id = $1")
                    .bind(schedule_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if u64::try_from(count).unwrap_or_default() >= max {
                tx.rollback().await?;
                return Ok(DueClaim::MaxRunsReached);
            }
        }

        // Level 3: the run row.
        let run_id = match insert_running_run(
            &mut tx,
            schedule_id,
            saved_query_id,
            &resolved_query,
            plan.window.as_ref(),
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                tx.rollback().await?;
                return match classify_violation(&e) {
                    Some(PgViolation::RunAlreadyRunning) => Ok(DueClaim::AlreadyRunning),
                    _ => Err(StoreError::from(e).into()),
                };
            }
        };

        set_next_fire_at(&mut tx, schedule_id, plan.next_fire_at).await?;
        tx.commit().await?;

        tracing::info!(
            event_type = "report_run_started",
            run_id,
            schedule_id,
            saved_query_id,
            "Report run started"
        );

        Ok(DueClaim::Started(ClaimedRun {
            run_id,
            saved_query_id,
            query_name,
            resolved_query,
            window: plan.window,
        }))
    }

    /// Finish a run with status, timing, and optional result path or blob.
    ///
    /// Returns [`FinishOutcome::RunDeleted`] when zero rows were updated — the
    /// run was cascade-deleted mid-flight (its saved query or schedule is gone),
    /// and the caller must remove any result file it just wrote — otherwise
    /// [`FinishOutcome::Persisted`].
    ///
    /// A SUCCESS also advances the owning schedule's `since_last` watermark
    /// to the window this run covered, in the same transaction as the row
    /// update (ADR-0018 ruling 9). Whether this run is a `since_last` one is
    /// read off the RUN's own claim-time `window_kind`, so an operator
    /// retyping the schedule's window mid-run cannot make the answer depend
    /// on commit order. One transaction is the whole point: the
    /// run's own record of what it covered and the schedule's record of what
    /// is covered are one fact, and a crash between two statements would
    /// either re-run a covered window or skip an uncovered one forever.
    ///
    /// Error and timeout completions update the run alone. That absence is
    /// how "the watermark advances only on success" is enforced — a failed
    /// run leaves the gap for the next successful one to cover — and it is
    /// why [`Self::fail_run_if_running`] and [`Self::cleanup_stale_runs`]
    /// carry no watermark statement either.
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_run(
        &self,
        run_id: i64,
        status: RunStatus,
        duration_ms: u64,
        row_count: Option<usize>,
        error_message: Option<&str>,
        result_data: Option<&[u8]>,
        result_path: Option<&str>,
    ) -> Result<FinishOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;

        // Schedule before run, the order every multi-row path here takes:
        // `claim_run` and `delete_schedule` both lock the schedule before runs, so
        // updating the run first and reaching for the schedule afterwards
        // would let this transaction deadlock against either of them.
        // `FOR UPDATE OF s` locks the schedule alone — the join reads the
        // run without locking it, which is what keeps the order intact.
        //
        // The lock is taken only when there is an advance to make, and the
        // test is the same one the advance itself uses: a run claimed as
        // `since_last`. Everything it reads is written at claim time and
        // never updated, so the answer cannot change under us. A run without
        // a since_last window does not lock the schedule and never queues
        // behind a schedule someone else is holding. A
        // cascade-deleted run matches nothing and skips the lock; the run
        // UPDATE below then reports RunDeleted as it always has.
        if status == RunStatus::Success {
            sqlx::query_scalar::<_, i64>(
                "SELECT s.id FROM schedules s
                 JOIN report_runs r ON r.schedule_id = s.id
                 WHERE r.id = $1 AND r.window_kind = 'since_last'
                 FOR UPDATE OF s",
            )
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
        }

        let updated = sqlx::query(
            "UPDATE report_runs
             SET status = $1, finished_at = now(), duration_ms = $2, row_count = $3,
                 error_message = $4, result_data = $5, result_path = $6
             WHERE id = $7",
        )
        .bind(status.as_str())
        .bind(bind_u64(duration_ms))
        .bind(row_count.map(bind_usize))
        .bind(error_message)
        .bind(result_data)
        .bind(result_path)
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| match classify_violation(&e) {
            // Backstop: unreachable via the typed API, kept so a future raw
            // path still maps the CHECK to Validation rather than 503.
            Some(PgViolation::Check) => {
                StoreError::Validation(format!("invalid run status {:?}", status.as_str()))
            }
            _ => StoreError::from(e),
        })?
        .rows_affected();

        // The watermark advance, and nothing else: the WHERE clause is the
        // whole policy. It fires only for a run CLAIMED as `since_last`,
        // only when that run recorded a window, and only when the window
        // ends after what is already covered — so an out-of-order finish (a
        // slow run completing after a later one) cannot rewind coverage.
        //
        // The mode is read off the RUN, never off the schedule. Reading the
        // schedule would make a run finishing while an operator retypes the
        // window answer differently depending on which commit landed first.
        if status == RunStatus::Success {
            sqlx::query(
                "UPDATE schedules s SET covered_through = r.window_end
                   FROM report_runs r
                  WHERE r.id = $1 AND s.id = r.schedule_id
                    AND r.window_kind = 'since_last'
                    AND r.window_end IS NOT NULL
                    AND (s.covered_through IS NULL OR r.window_end > s.covered_through)",
            )
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        tracing::info!(
            event_type = "report_run_finished",
            run_id,
            status = status.as_str(),
            duration_ms,
            orphaned = (updated == 0),
            "Report run finished"
        );

        Ok(if updated > 0 {
            FinishOutcome::Persisted
        } else {
            FinishOutcome::RunDeleted
        })
    }

    /// Flip a run to `error`, but only while it is still `running`.
    ///
    /// This is the scheduler's ambiguous-commit recovery path: a prior
    /// `finish_run("success", …)` returned `Err`, which can mean the COMMIT
    /// landed server-side while the client's ack was lost. That is as true of
    /// the finish transaction as it was of the autocommit UPDATE it replaced:
    /// the ambiguity is in the lost ack, not in the statement count, and a
    /// committed finish carries its watermark advance with it. An unconditional overwrite would destroy that committed success —
    /// clearing `row_count`/`result_data`/`result_path` and permanently
    /// orphaning the parquet file the row pointed at. Guarding on
    /// `status = 'running'` makes completion a state transition: the flip lands
    /// only if the success did not commit.
    ///
    /// Returns [`FlipOutcome::FlippedToError`] when a running row was flipped
    /// (the earlier success never committed, so any parquet the caller wrote is
    /// now orphaned and should be removed), [`FlipOutcome::NotRunning`] when no
    /// running row matched — either the ambiguous success actually committed (its
    /// result must be preserved) or the run was cascade-deleted.
    pub async fn fail_run_if_running(
        &self,
        run_id: i64,
        duration_ms: u64,
        error_message: &str,
    ) -> Result<FlipOutcome, StoreError> {
        let updated = sqlx::query(
            "UPDATE report_runs
             SET status = 'error', finished_at = now(), duration_ms = $1,
                 row_count = NULL, error_message = $2, result_data = NULL,
                 result_path = NULL
             WHERE id = $3 AND status = 'running'",
        )
        .bind(bind_u64(duration_ms))
        .bind(error_message)
        .bind(run_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        tracing::info!(
            event_type = "report_run_fail_if_running",
            run_id,
            flipped = (updated > 0),
            "Guarded run failure applied"
        );

        Ok(if updated > 0 {
            FlipOutcome::FlippedToError
        } else {
            FlipOutcome::NotRunning
        })
    }

    /// List runs for a saved query, paginated. Excludes result blobs.
    pub async fn list_runs(
        &self,
        saved_query_id: i64,
        key_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ReportRun>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {cols}
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.saved_query_id = $1 AND s.key_id = $2
             ORDER BY r.started_at DESC, r.id DESC
             LIMIT $3 OFFSET $4",
            cols = run_cols("r.")
        )))
        .bind(saved_query_id)
        .bind(key_id)
        .bind(bind_usize(limit))
        .bind(bind_usize(offset))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(row_to_report_run)
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Get a single run by id (no result blob). Checks ownership via schedule.
    pub async fn get_run(&self, run_id: i64, key_id: i64) -> Result<Option<ReportRun>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {cols}
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.id = $1 AND s.key_id = $2",
            cols = run_cols("r.")
        )))
        .bind(run_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_report_run).transpose()?)
    }

    /// Get the zstd-compressed result blob for a run, non-null only when the
    /// parquet write failed and the scheduler fell back to it. Checks ownership.
    pub async fn get_run_result(
        &self,
        run_id: i64,
        key_id: i64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let row: Option<Option<Vec<u8>>> = sqlx::query_scalar(
            "SELECT r.result_data
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.id = $1 AND s.key_id = $2",
        )
        .bind(run_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.flatten())
    }

    /// Count runs for a schedule.
    pub async fn count_runs(&self, schedule_id: i64) -> Result<u64, StoreError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM report_runs WHERE schedule_id = $1")
                .bind(schedule_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    /// Get the most recent run for a schedule.
    pub async fn latest_run(&self, schedule_id: i64) -> Result<Option<ReportRun>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM report_runs
             WHERE schedule_id = $1
             ORDER BY started_at DESC, id DESC
             LIMIT 1"
        )))
        .bind(schedule_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_report_run).transpose()?)
    }

    /// Get the most recent successful run for a saved query (`run=latest`).
    ///
    /// The NEWEST success, whether or not it wrote a parquet file. A run
    /// with no rows has no file (there is no schema to write one from) and
    /// carries its result as a blob instead, and skipping it here would make
    /// `run=latest` silently answer from an older run: the same report, but
    /// over a window that has already been superseded. The caller decides
    /// what to do with a run that has no path (`from_saved::resolve_latest`
    /// builds an empty typed source from the blob's column names), and its
    /// refusals name THIS run rather than resolving a different one
    /// (ADR-0018 ruling 13).
    pub async fn latest_successful_run(
        &self,
        saved_query_id: i64,
    ) -> Result<Option<ReportRun>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM report_runs
             WHERE saved_query_id = $1 AND status = 'success'
             ORDER BY started_at DESC, id DESC
             LIMIT 1"
        )))
        .bind(saved_query_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_report_run).transpose()?)
    }

    /// List all successful runs with parquet results for a saved query
    /// (`run=all`), oldest first.
    ///
    /// The `result_path IS NOT NULL` filter stays, unlike
    /// [`Self::latest_successful_run`]'s: `run=all` unions the runs' files,
    /// and a zero-row run has none. Adding it as an empty typed source would
    /// contribute no rows to the union while risking a column-type clash
    /// with the real files, so a zero-row run is simply not a member (see
    /// `from_saved::resolve_all`).
    pub async fn list_successful_runs(
        &self,
        saved_query_id: i64,
    ) -> Result<Vec<ReportRun>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM report_runs
             WHERE saved_query_id = $1 AND status = 'success' AND result_path IS NOT NULL
             ORDER BY started_at ASC, id ASC"
        )))
        .bind(saved_query_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(row_to_report_run)
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Mark any runs with status `running` as `error` (crash recovery on
    /// startup — safe because the advisory lock guarantees no live sibling).
    pub async fn cleanup_stale_runs(&self) -> Result<usize, StoreError> {
        let updated = sqlx::query(
            "UPDATE report_runs
             SET status = 'error', finished_at = now(),
                 error_message = 'interrupted by server restart'
             WHERE status = 'running'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated > 0 {
            tracing::warn!(
                event_type = "stale_runs_cleaned",
                count = updated,
                "Marked stale running report runs as error"
            );
        }

        Ok(usize::try_from(updated).unwrap_or_default())
    }

    /// Delete old runs for retention. Keeps at most `max_per_schedule` runs
    /// per schedule and deletes runs older than `max_age_days`; both
    /// deletions and the path collection happen in one transaction.
    ///
    /// Returns `(count_deleted, result_paths)`; the caller unlinks the
    /// parquet files.
    pub async fn delete_old_runs(
        &self,
        max_age_days: u64,
        max_per_schedule: u64,
    ) -> Result<(usize, Vec<String>), StoreError> {
        let cutoff =
            Utc::now() - chrono::Duration::days(i64::try_from(max_age_days).unwrap_or(365));

        let mut tx = self.pool.begin().await?;

        let aged: Vec<Option<String>> = sqlx::query_scalar(
            "DELETE FROM report_runs WHERE started_at < $1 RETURNING result_path",
        )
        .bind(cutoff)
        .fetch_all(&mut *tx)
        .await?;

        let excess: Vec<Option<String>> = sqlx::query_scalar(
            "DELETE FROM report_runs
             WHERE id NOT IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY schedule_id ORDER BY started_at DESC, id DESC
                     ) AS rn
                     FROM report_runs
                 ) ranked
                 WHERE rn <= $1
             )
             RETURNING result_path",
        )
        .bind(bind_u64(max_per_schedule))
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;

        let total = aged.len() + excess.len();
        let paths: HashSet<String> = aged.into_iter().chain(excess).flatten().collect();

        if total > 0 {
            tracing::info!(
                event_type = "old_runs_deleted",
                total,
                parquet_paths = paths.len(),
                "Deleted old report runs"
            );
        }

        Ok((total, paths.into_iter().collect()))
    }

    /// Count total runs for a saved query (for pagination).
    pub async fn count_runs_for_saved_query(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<usize, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.saved_query_id = $1 AND s.key_id = $2",
        )
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    /// List runs across every saved query for a user, paginated. Each run is
    /// paired with its saved query's name, ordered before pagination.
    pub async fn list_all_runs(
        &self,
        key_id: i64,
        limit: usize,
        offset: usize,
        sort: trawl_api::RunsSortKey,
        dir: trawl_api::RunsSortDir,
    ) -> Result<Vec<(ReportRun, String)>, StoreError> {
        use trawl_api::{
            RunsSortDir::{Asc, Desc},
            RunsSortKey::{Duration, Net, Rows, Started, Status},
        };
        let order = match (sort, dir) {
            (Net, Asc) => r#"lower(sq.name) COLLATE "C" ASC, r.started_at DESC, r.id DESC"#,
            (Net, Desc) => r#"lower(sq.name) COLLATE "C" DESC, r.started_at DESC, r.id DESC"#,
            (Status, Asc) => r#"r.status COLLATE "C" ASC, r.started_at DESC, r.id DESC"#,
            (Status, Desc) => r#"r.status COLLATE "C" DESC, r.started_at DESC, r.id DESC"#,
            (Started, Asc) => "r.started_at ASC, r.id ASC",
            (Started, Desc) => "r.started_at DESC, r.id DESC",
            (Duration, Asc) => "r.duration_ms ASC NULLS LAST, r.started_at DESC, r.id DESC",
            (Duration, Desc) => "r.duration_ms DESC NULLS LAST, r.started_at DESC, r.id DESC",
            (Rows, Asc) => "r.row_count ASC NULLS LAST, r.started_at DESC, r.id DESC",
            (Rows, Desc) => "r.row_count DESC NULLS LAST, r.started_at DESC, r.id DESC",
        };
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {cols}, sq.name AS sq_name
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             JOIN saved_queries sq ON sq.id = r.saved_query_id
             WHERE s.key_id = $1
             ORDER BY {order}
             LIMIT $2 OFFSET $3",
            cols = run_cols("r.")
        )))
        .bind(key_id)
        .bind(bind_usize(limit))
        .bind(bind_usize(offset))
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let run = row_to_report_run(row)?;
            let net_name: String = row.try_get("sq_name")?;
            out.push((run, net_name));
        }
        Ok(out)
    }

    /// Count all runs across all saved queries for a user.
    pub async fn count_all_runs(&self, key_id: i64) -> Result<usize, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE s.key_id = $1",
        )
        .bind(key_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    /// Aggregate stats across all runs for a user:
    /// `(total, success, error, timeout, avg_duration_ms)`.
    pub async fn runs_stats(
        &self,
        key_id: i64,
    ) -> Result<(u64, u64, u64, u64, Option<u64>), StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*)                                                        AS total,
                    COALESCE(SUM(CASE WHEN r.status = 'success' THEN 1 ELSE 0 END), 0) AS success,
                    COALESCE(SUM(CASE WHEN r.status = 'error' THEN 1 ELSE 0 END), 0)   AS error,
                    COALESCE(SUM(CASE WHEN r.status = 'timeout' THEN 1 ELSE 0 END), 0) AS timeout,
                    AVG(r.duration_ms) FILTER (WHERE r.duration_ms IS NOT NULL)::DOUBLE PRECISION
                                                                                    AS avg_ms
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE s.key_id = $1",
        )
        .bind(key_id)
        .fetch_one(&self.pool)
        .await?;

        let get_u64 = |name: &str| -> Result<u64, sqlx::Error> {
            Ok(u64::try_from(row.try_get::<i64, _>(name)?).unwrap_or_default())
        };
        let avg: Option<f64> = row.try_get("avg_ms")?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let avg_ms = avg.map(|v| v.max(0.0) as u64);
        Ok((
            get_u64("total")?,
            get_u64("success")?,
            get_u64("error")?,
            get_u64("timeout")?,
            avg_ms,
        ))
    }
}

/// The rows one due-run claim reads before it decides anything.
struct LockedSchedule {
    saved_query_id: i64,
    /// The saved query's display name at claim time.
    query_name: String,
    /// The saved DSL, read under the saved-query row lock.
    dsl: String,
    /// The schedule, re-read under its own lock.
    schedule: Schedule,
}

/// Take the claim's locks in order and read what the plan needs.
///
/// `saved_queries` FOR UPDATE, then `schedules` FOR UPDATE: the order every
/// multi-row path in this module takes, extended one level up. Both rows are
/// re-read here rather than trusted from the caller's enumeration snapshot,
/// because an operator can edit the DSL, disable the schedule or delete
/// either one between the list and the claim.
///
/// `None` means there is nothing to run — the schedule or its saved query
/// was deleted, or the schedule is disabled — and the caller answers
/// [`DueClaim::NotDue`] for all three.
async fn lock_for_claim(
    conn: &mut sqlx::PgConnection,
    schedule_id: i64,
) -> Result<Option<LockedSchedule>, DueClaimError> {
    // Which saved query to lock. Unlocked on purpose: it is the lookup that
    // decides which row the FIRST lock is taken on, and a schedule never
    // changes its saved query (the pair is created together and
    // `schedules_saved_query_unique` keeps it 1:1).
    let saved_query_id: Option<i64> =
        sqlx::query_scalar("SELECT saved_query_id FROM schedules WHERE id = $1")
            .bind(schedule_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(saved_query_id) = saved_query_id else {
        return Ok(None);
    };

    // Level 1: the saved query. The lock and the DSL read are one statement
    // — the text this returns is the text the run executes and stores.
    let saved = sqlx::query("SELECT name, query FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(saved_query_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(saved) = saved else {
        return Ok(None);
    };

    // Level 2: the schedule.
    let locked = sqlx::query(AssertSqlSafe(format!(
        "SELECT {SCHEDULE_COLS} FROM schedules WHERE id = $1 FOR UPDATE"
    )))
    .bind(schedule_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(locked) = locked else {
        return Ok(None);
    };
    let schedule = row_to_schedule(&locked)?;
    if !schedule.enabled {
        return Ok(None);
    }

    Ok(Some(LockedSchedule {
        saved_query_id,
        query_name: saved.try_get("name")?,
        dsl: saved.try_get("query")?,
        schedule,
    }))
}

/// Insert one schedule row, returning it decoded.
///
/// ONE spelling of the statement for the two doors:
/// [`ScheduleStore::create_schedule`], which runs it on a pooled
/// connection, and [`ScheduleStore::set_schedule_checked`], which runs it
/// inside the transaction that holds the saved-query lock. Taking a
/// connection rather than a pool is what lets the second one exist: a
/// second copy of the INSERT would be a second place for a column to be
/// forgotten.
///
/// Logging is the caller's, deliberately. This function can run inside a
/// transaction that later rolls back, and "Schedule created" is not a thing
/// to say about a row nobody can see.
#[allow(clippy::too_many_arguments)]
async fn create_schedule_in(
    conn: &mut sqlx::PgConnection,
    saved_query_id: i64,
    key_id: i64,
    interval_secs: u64,
    max_runs: Option<u64>,
    enabled: bool,
    window: Option<ScheduleWindow>,
    lag_secs: u64,
    now: DateTime<Utc>,
) -> Result<Schedule, StoreError> {
    ensure_min_interval(interval_secs)?;
    ensure_lag_has_window(window, lag_secs)?;

    // A `since_last` schedule owes coverage from its first window's start,
    // so that instant is written down now rather than inferred from an
    // absent watermark after the first run (see [`seed_covered_through`]).
    // Every other mode keeps none.
    let covered_through = match window {
        Some(ScheduleWindow::SinceLast) => {
            Some(seed_covered_through(now, interval_secs, lag_secs)?)
        }
        Some(ScheduleWindow::Fixed { .. }) | None => None,
    };

    let row = sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO schedules
             (saved_query_id, key_id, interval_secs, max_runs, enabled,
              window_kind, window_secs, lag_secs, covered_through, next_fire_at,
              created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now(), now())
         RETURNING {SCHEDULE_COLS}"
    )))
    .bind(saved_query_id)
    .bind(key_id)
    .bind(bind_u64(interval_secs))
    .bind(max_runs.map(bind_u64))
    .bind(enabled)
    .bind(window.map(|w| w.kind().as_str()))
    .bind(window.and_then(ScheduleWindow::secs).map(bind_u64))
    .bind(bind_u64(lag_secs))
    .bind(covered_through)
    .bind(now)
    .fetch_one(conn)
    .await
    .map_err(|e| match classify_violation(&e) {
        Some(PgViolation::ScheduleTaken) => StoreError::ScheduleExists { saved_query_id },
        Some(PgViolation::ForeignKey) => StoreError::NotFound {
            id: saved_query_id,
            resource: "saved query",
        },
        _ => StoreError::from(e),
    })?;

    Ok(row_to_schedule(&row)?)
}

/// Update one schedule row under its own `FOR UPDATE` lock, returning it
/// decoded.
///
/// The twin of [`create_schedule_in`], and the same reason for taking a
/// connection: [`ScheduleStore::update_schedule`] gives it a transaction of
/// its own, [`ScheduleStore::set_schedule_checked`] gives it the one
/// already holding the saved-query lock. Re-locking a schedule the caller
/// has effectively pinned costs nothing and keeps this function correct on
/// its own.
///
/// Two cursor rules, both about not moving coverage the operator did not
/// ask to move:
///
/// - An existing `covered_through` is never touched. Editing a schedule (or
///   its saved DSL) does not reset the watermark — the per-run resolved
///   snapshot is the audit trail (ADR-0018 ruling 14). An ABSENT one is
///   seeded when the edit re-anchors a `since_last` schedule, for the
///   reason [`seed_covered_through`] gives: the re-anchored cursor is a
///   fresh origin of owed coverage, and a first run that fails under it
///   must not drop its interval. The SQL is a `COALESCE`, so "seed only
///   when absent" is one statement rather than a read followed by a
///   decision.
/// - `next_fire_at` is re-anchored to `now` only when the cadence itself
///   changed: a different `interval_secs` or a different window. Editing
///   `max_runs` or flipping `enabled` leaves the planned cursor alone, so a
///   schedule cannot be kept permanently un-due by repeated edits. The
///   comparison reads the current row under `FOR UPDATE`, in the same
///   transaction as the write, so a concurrent edit cannot land between the
///   read and the decision.
#[allow(clippy::too_many_arguments)]
async fn update_schedule_in(
    conn: &mut sqlx::PgConnection,
    id: i64,
    key_id: i64,
    interval_secs: u64,
    max_runs: Option<u64>,
    enabled: bool,
    window: Option<ScheduleWindow>,
    lag_secs: u64,
    now: DateTime<Utc>,
) -> Result<Schedule, StoreError> {
    ensure_min_interval(interval_secs)?;
    ensure_lag_has_window(window, lag_secs)?;

    let current = sqlx::query(AssertSqlSafe(format!(
        "SELECT {SCHEDULE_COLS} FROM schedules
         WHERE id = $1 AND key_id = $2 FOR UPDATE"
    )))
    .bind(id)
    .bind(key_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(current) = current else {
        return Err(StoreError::NotFound {
            id,
            resource: "schedule",
        });
    };
    let current = row_to_schedule(&current)?;
    let reanchor = current.interval_secs != interval_secs || current.window != window;
    let seed = match (reanchor, window) {
        (true, Some(ScheduleWindow::SinceLast)) => {
            Some(seed_covered_through(now, interval_secs, lag_secs)?)
        }
        _ => None,
    };

    let row = sqlx::query(AssertSqlSafe(format!(
        "UPDATE schedules
         SET interval_secs = $1, max_runs = $2, enabled = $3,
             window_kind = $4, window_secs = $5, lag_secs = $6,
             next_fire_at = CASE WHEN $7 THEN $8 ELSE next_fire_at END,
             covered_through = COALESCE(covered_through, $11),
             updated_at = now()
         WHERE id = $9 AND key_id = $10
         RETURNING {SCHEDULE_COLS}"
    )))
    .bind(bind_u64(interval_secs))
    .bind(max_runs.map(bind_u64))
    .bind(enabled)
    .bind(window.map(|w| w.kind().as_str()))
    .bind(window.and_then(ScheduleWindow::secs).map(bind_u64))
    .bind(bind_u64(lag_secs))
    .bind(reanchor)
    .bind(now)
    .bind(id)
    .bind(key_id)
    .bind(seed)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(StoreError::NotFound {
        id,
        resource: "schedule",
    })?;

    Ok(row_to_schedule(&row)?)
}

/// Announce a committed schedule creation. One spelling for both doors.
fn log_schedule_created(schedule: &Schedule) {
    tracing::info!(
        event_type = "schedule_created",
        schedule_id = schedule.id,
        saved_query_id = schedule.saved_query_id,
        key_id = schedule.key_id,
        interval_secs = schedule.interval_secs,
        "Schedule created"
    );
}

/// Announce a committed schedule edit. One spelling for both doors.
fn log_schedule_updated(schedule: &Schedule) {
    tracing::info!(
        event_type = "schedule_updated",
        schedule_id = schedule.id,
        key_id = schedule.key_id,
        interval_secs = schedule.interval_secs,
        enabled = schedule.enabled,
        "Schedule updated"
    );
}

/// Insert one `running` row for a claim, returning its id.
///
/// ONE spelling of the statement for its three claimants:
/// [`ScheduleStore::claim_run`], [`ScheduleStore::claim_manual_run`] and
/// [`ScheduleStore::claim_due_run`]. All three write the same nine columns,
/// and a second copy would be a second place for the window columns to be
/// forgotten. The raw `sqlx::Error` comes back so each caller classifies
/// the `report_runs_one_running` 23505 into its own answer.
///
/// This statement takes a lock it does not name. The `saved_query_id`
/// foreign key makes postgres take FOR KEY SHARE on the saved-query row, so
/// a caller that has not already locked that row is reaching UP a level
/// while holding a lower one. Every caller that can run beside
/// [`ScheduleStore::set_schedule_checked`] therefore takes `saved_queries`
/// first, explicitly.
async fn insert_running_run(
    conn: &mut sqlx::PgConnection,
    schedule_id: i64,
    saved_query_id: i64,
    query: &str,
    window: Option<&ReportWindow>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO report_runs
             (schedule_id, saved_query_id, query, status, started_at,
              window_start, window_end, window_truncated, window_kind)
         VALUES ($1, $2, $3, 'running', now(), $4, $5, $6, $7)
         RETURNING id",
    )
    .bind(schedule_id)
    .bind(saved_query_id)
    .bind(query)
    .bind(window.map(|w| w.start))
    .bind(window.map(|w| w.end))
    .bind(window.map(|w| w.truncated))
    .bind(window.map(|w| w.kind.as_str()))
    .fetch_one(conn)
    .await
}

/// Move a schedule's fire cursor.
///
/// `updated_at` is deliberately not touched: it records operator edits, and
/// a tick moving its own cursor is not one.
async fn set_next_fire_at(
    conn: &mut sqlx::PgConnection,
    schedule_id: i64,
    next_fire_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE schedules SET next_fire_at = $1 WHERE id = $2")
        .bind(next_fire_at)
        .bind(schedule_id)
        .execute(conn)
        .await
        .map(|_| ())
}

/// The text a windowed run executes and stores, or why it has none.
///
/// Both halves of ADR-0018's write-time rule are asked again here, at
/// execution time. The schedule and the saved DSL are two rows an operator
/// can edit independently, and a pair that was written past the gate (a
/// direct UPDATE, an older binary) would otherwise execute a query whose
/// own `last=` silently overrides the window the run row claims to cover.
fn resolve_window_text(
    mode: Option<ScheduleWindow>,
    dsl: &str,
    window: &ReportWindow,
) -> Result<String, DueClaimError> {
    validate_window_compatibility(mode, dsl)?;
    Ok(materialize_window(dsl, window)?)
}

/// Build a prefixed run column list (e.g. `r.id, r.schedule_id, …`).
fn run_cols(prefix: &str) -> String {
    prefixed(RUN_COLS, prefix)
}

/// Build a prefixed schedule column list (e.g. `s.id, s.saved_query_id, …`).
///
/// The twin of [`run_cols`]: every statement that joins schedules spells the
/// same list, so a column added to [`SCHEDULE_COLS`] reaches all of them
/// instead of only the ones someone remembered to edit.
fn schedule_cols(prefix: &str) -> String {
    prefixed(SCHEDULE_COLS, prefix)
}

/// Build a schedule column list aliased for a joined decode, e.g.
/// `s.id AS s_id, s.saved_query_id AS s_saved_query_id, …`.
///
/// The shape [`row_to_schedule_at`] reads when a schedule rides along with
/// another row. Generated rather than spelled out, so a new schedule column
/// cannot reach the struct and miss the join.
pub(crate) fn schedule_cols_as(table: &str, alias: &str) -> String {
    SCHEDULE_COLS
        .split(',')
        .map(|c| {
            let c = c.trim();
            format!("{table}{c} AS {alias}{c}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn prefixed(cols: &str, prefix: &str) -> String {
    cols.split(',')
        .map(|c| format!("{prefix}{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-function coverage for interval parsing/formatting lives here (no
    // database); pg-backed behaviour is covered in tests/store_pg.rs.

    #[test]
    fn parse_interval_valid() {
        assert_eq!(parse_interval("60s").unwrap(), 60);
        assert_eq!(parse_interval("5m").unwrap(), 300);
        assert_eq!(parse_interval("1h").unwrap(), 3600);
        assert_eq!(parse_interval("1d").unwrap(), 86400);
        assert_eq!(parse_interval("1w").unwrap(), 604_800);
    }

    #[test]
    fn parse_interval_rejects_too_short() {
        assert!(matches!(
            parse_interval("30s"),
            Err(StoreError::IntervalTooShort { secs: 30 })
        ));
        assert!(matches!(
            parse_interval("0s"),
            Err(StoreError::IntervalTooShort { secs: 0 })
        ));
    }

    #[test]
    fn parse_interval_rejects_invalid() {
        assert!(parse_interval("").is_err());
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("5x").is_err());
    }

    #[test]
    fn parse_interval_rejects_multibyte_without_panicking() {
        // regression: split_at(len - 1) panicked mid-char on multi-byte input
        assert!(parse_interval("5µ").is_err());
        assert!(parse_interval("µ").is_err());
        assert!(parse_interval("5週").is_err());
    }

    #[test]
    fn parse_duration_secs_has_no_floor() {
        assert_eq!(parse_duration_secs("0s").unwrap(), 0);
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
    }

    #[test]
    fn parse_duration_secs_caps_at_ten_years() {
        assert_eq!(
            parse_duration_secs("315360000s").unwrap(),
            MAX_DURATION_SECS
        );
        assert!(matches!(
            parse_duration_secs("315360001s"),
            Err(StoreError::DurationTooLong { secs: 315_360_001 })
        ));
        // The cap is on the parsed seconds, so every unit reaches it.
        assert!(matches!(
            parse_duration_secs("522w"),
            Err(StoreError::DurationTooLong { .. })
        ));
        assert!(matches!(
            parse_interval("100000d"),
            Err(StoreError::DurationTooLong { .. })
        ));
    }

    /// The ten-year cap lives in two places that cannot see each other:
    /// this constant, which the duration grammar enforces on every write,
    /// and the initial schema's CHECK constraints, which enforce it in the
    /// database. Drift either way is a store that accepts what the grammar
    /// refuses or refuses what it accepts, and neither shows up until a
    /// real row hits it, so the test reads the migration and compares the
    /// literal.
    #[test]
    fn initial_schema_spells_the_same_duration_cap() {
        let sql = std::fs::read_to_string("migrations/20260913000001_initial_schema.sql")
            .expect("the crate's own migration file");

        for name in [
            "schedules_interval_within_cap",
            "schedules_window_secs_within_cap",
            "schedules_lag_within_cap",
        ] {
            assert!(
                sql.contains(name),
                "initial schema no longer declares {name}"
            );
        }

        // Every run of digits long enough to be a second count has to BE
        // the cap. Matching on the number rather than on one spelling of
        // the CHECK means another duration constraint
        // cannot introduce a different literal unnoticed.
        let cap = MAX_DURATION_SECS.to_string();
        let literals: Vec<&str> = sql
            .split(|c: char| !c.is_ascii_digit())
            .filter(|run| run.len() >= 6)
            .collect();
        assert!(
            !literals.is_empty(),
            "initial schema spells no duration cap at all"
        );
        for literal in literals {
            assert_eq!(
                literal, cap,
                "initial schema spells {literal}, MAX_DURATION_SECS is {cap}"
            );
        }
    }

    #[test]
    fn parse_duration_secs_rejects_overflow() {
        // u64 seconds overflow rather than wrapping to a plausible span.
        assert!(matches!(
            parse_duration_secs("99999999999999999w"),
            Err(StoreError::InvalidInterval { .. })
        ));
        assert!(matches!(
            parse_interval("99999999999999999w"),
            Err(StoreError::InvalidInterval { .. })
        ));
    }

    #[test]
    fn format_interval_roundtrip() {
        assert_eq!(format_interval(60), "1m");
        assert_eq!(format_interval(300), "5m");
        assert_eq!(format_interval(3600), "1h");
        assert_eq!(format_interval(86400), "1d");
        assert_eq!(format_interval(604_800), "1w");
        assert_eq!(format_interval(90), "90s");
        // Zero is a lag, not a week: every modulus divides it.
        assert_eq!(format_interval(0), "0s");
    }

    #[test]
    fn run_cols_prefixes_every_column() {
        let cols = run_cols("r.");
        assert!(cols.starts_with("r.id, r.schedule_id"));
        assert!(cols.ends_with("r.window_kind"));
        assert!(!cols.contains(" ,"));
        assert_eq!(cols.split(", ").count(), RUN_COLS.split(',').count());
    }

    #[test]
    fn schedule_cols_as_aliases_every_column() {
        let cols = schedule_cols_as("s.", "s_");
        assert!(cols.starts_with("s.id AS s_id, s.saved_query_id AS s_saved_query_id"));
        assert!(cols.ends_with("s.updated_at AS s_updated_at"));
        assert_eq!(cols.split(", ").count(), SCHEDULE_COLS.split(',').count());
    }

    #[test]
    fn schedule_cols_prefixes_every_column() {
        let cols = schedule_cols("s.");
        assert!(cols.starts_with("s.id, s.saved_query_id"));
        assert!(cols.ends_with("s.updated_at"));
        assert!(cols.contains("s.window_kind, s.window_secs"));
        assert!(!cols.contains(" ,"));
        assert_eq!(cols.split(", ").count(), SCHEDULE_COLS.split(',').count());
    }

    /// Every run column reachable through the embedded `last_run` of a
    /// schedule response. `LATEST_RUN_COLS` is a hand-written alias list, so
    /// a column added to `RUN_COLS` alone would decode as missing there —
    /// silently, for exactly one of the two ways a run is read.
    #[test]
    fn latest_run_cols_carries_every_run_column() {
        for col in RUN_COLS.split(',').map(str::trim) {
            assert!(
                LATEST_RUN_COLS.contains(&format!("lr.{col} ")),
                "LATEST_RUN_COLS is missing lr.{col}"
            );
            assert!(
                LATEST_RUN_COLS.contains(&format!("AS r_{col}")),
                "LATEST_RUN_COLS is missing the r_{col} alias"
            );
        }
    }
}
