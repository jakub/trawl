// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The captured-results capability (ADR-0057), transport-free.
//!
//! [`capture`] runs one query and keeps its answer as an immutable,
//! typed, expiring result; the correlation reads of ADR-0058 run through
//! the same core, `run`. [`list`] names the caller's own live captures,
//! [`page`] reads a capture's rows and [`cell`] reads one cell by byte
//! range. Each takes the verified key and answers a wire type or a
//! [`ServerError`]. `routes` adapts them to HTTP; an in-process
//! host calls them as they are.
//!
//! # A capture
//!
//! Every capture, of every kind, runs through one core, `run`, which takes
//! a `Spec`: the kind it records, the text it runs as sent, the request's
//! window, a shape imposed instead of the one `bounds::classify` decides,
//! and a `Selection` the lane applies around the cut. [`capture`], the
//! search, is the spec of kind `search` with the caller's text and window,
//! no imposed shape and a selection that selects nothing. The correlation
//! reads (`context`, `sightings`, `compare`) differ only in their spec:
//! the same guards in the same order, the same lane and the same commit.
//!
//! `run` applies `/query`'s guards in `/query`'s order and calls the same
//! functions, with two capture steps before the query is tracked:
//!
//! 1. The trawl `query` permission, else 403. Then request-shape
//!    validation: a `query` holding U+0000 is a 400, since the store's TEXT
//!    columns refuse it. It opens no timing account.
//! 2. One deadline for the whole capture: `[capture] deadline_secs`,
//!    capped by the server's query timeout.
//! 3. The query id and the timing account of the spec's kind (`capture`
//!    for a search, else `context`, `sightings` or `compare`), opened
//!    before `dsl_check` as `/query` opens its own (ADR-0046): every
//!    capture that reaches the check writes one `query_timing` event,
//!    refusals included.
//! 4. **Bounds**, booked as `dsl_check`. The text as sent passes
//!    `admission::check_dsl`, so a parse error points into the text the
//!    caller wrote. Then `bounds::resolve_as` fixes the window at the
//!    results clock's instant, splices it in and decides the shape, or
//!    takes the one the spec imposes, and the executed text passes the
//!    same check; a failure there is the server's own splice, so a 500. An
//!    event-shaped query leaves one stage of the cap for the order the
//!    server appends, so one at the cap is the bounds' 400. A
//!    `from saved` capture resolves its stored run here, booked as
//!    `saved_lookup`, refuses a run whose file is gone, and measures the
//!    run's files against `source_max_bytes`, as a window's selection is
//!    measured inside the lane. A refusal ends the account with
//!    `outcome=error`. A deadline that passes during the resolution or the
//!    lookup refuses the capture before admission, 503 with
//!    `outcome=capacity_refused`, as `/query` refuses work its deadline
//!    never let start.
//! 5. **Admission** ([`super::charge::CaptureGate::admit`]), booked as
//!    `pool_wait`: the key's lane, the installation's slots and the quota
//!    reservation, else 429 with `Retry-After` and the account ended with
//!    `outcome=capacity_refused` and `work_started=false`.
//! 6. The tracker and `query_start`, over the executed text. A capture
//!    refused in steps 1, 4 or 5 took no permit and no publication guard,
//!    and appears in its timing event, if it opened one, but in neither
//!    `/queries` nor history. Every refusal of the input itself, a read's
//!    own validation included, comes before step 5, so it takes nothing.
//! 7. The capture lane ([`crate::pool::ExecutorPool::capture`]) executes
//!    the text under `RowCap::Refuse(max_result_rows)` and UTC, and inside
//!    the slot applies the spec's selection around the cut, counts the
//!    column facts, writes the parquet file and publishes it under the
//!    result's id. The admission rides the slot, so the lane slot, the
//!    installation slot and the reservation stay charged until the work
//!    physically stops. A window whose selection is over `source_max_bytes`
//!    is refused inside the lane, before the engine scans: the permit is
//!    held for the measurement only and returned before the work starts.
//! 8. A `from saved` capture's vanished-run post-check, then the timing
//!    event.
//! 9. **Commit.** The row, with the spec's kind and the facts its
//!    selection computed, is inserted in a task of its own, which the
//!    request awaits, and only then are the tracker, history and the
//!    response told the capture succeeded. The same task records the
//!    outcome, so a client that goes away cuts neither the commit nor its
//!    record short. A capture dropped at an earlier await closes its tracker
//!    entry as abandoned ([`crate::tracker::Tracked`]).
//!
//! The file reaches its final name before its row commits, ADR-0057's
//! order. From before the publish until the insert is decided the capture
//! is in flight ([`super::InFlight`]), and the sweep leaves its file
//! alone however long the insert takes. A capture that ends before its
//! first insert unlinks the file it published (`Uncommitted`). From that
//! insert on, a row may commit whatever the capture learns. A taken id on
//! the first insert committed nothing, so the file goes. A grace that
//! finds no row removes it too, and a row that commits after that grace
//! reads as `file_missing` (see [`COMMIT_GRACE`]). A commit task cut
//! short, as when trawld shuts down while an insert is pending, leaves the
//! file to boot, which keeps it if its row has committed by then and
//! removes it otherwise ([`super::sweep::reconcile_at_boot`]).
//!
//! An insert that answers an error may still commit. The connection can
//! die after the statement reached the backend, which finishes it anyway,
//! and a read on another connection can run before that commit is
//! visible. So an insert's error is settled by a fence: the same insert
//! again, on another connection. The id is the table's primary key, so at
//! most one of the two commits a row. A first insert whose transaction is
//! still open makes the fence wait until it ends. One that committed
//! leaves the fence the taken id, and one that never arrived leaves the
//! fence to insert the row. In each case the row is committed, and the
//! capture keeps its file and answers 200.
//!
//! A fence that fails too leaves the commit in doubt. The capture answers
//! the first insert's error, and a task of its own keeps the file in
//! flight and the reservation charged for [`COMMIT_GRACE`], then reads the
//! row. A row found then keeps its file. A row still absent loses it. A
//! read that fails leaves the file to the sweep, which removes it once no
//! live row names it ([`super::sweep`]).
//!
//! # The cut
//!
//! A capture keeps at most `max_rows` rows and `max_wire_bytes` of cells,
//! each cell measured as the #214 wire encodes it
//! ([`trawl_api::value::wire_cell_len`]), with no framing. An event-shaped
//! result, already ordered `_time DESC, _ingested DESC` by the executed
//! text, keeps the rows before the first one that does not fit; a first
//! row too large keeps none. `total_rows` is every row the cut saw. A
//! search's selection passes every row the execution produced, so for a
//! search it is every row the execution produced.
//!
//! The spec's selection runs on both sides of the cut, under the `post`
//! phase with it: before, over every executed row, where a read may
//! reorder or drop rows but never change the columns; after, over the kept
//! rows, where it computes the facts the row records as `correlation`. A
//! read's facts name their own denominators. A whole-shaped result that does not fit is refused with
//! `result_too_large`: a prefix of an aggregate answers a different
//! question. A result that the bounds call event-shaped but that lacks
//! `_time` or `_ingested` is whole-shaped (ADR-0057, D9). Any file over
//! `max_file_bytes`, the size its reservation holds, is refused too.
//!
//! # Column facts
//!
//! Counted in the lane over the kept rows: nulls, distinct non-null values
//! up to [`DISTINCT_CAP`], and, for a column of at most
//! [`LISTED_VALUES_MAX`] distinct values, each value with its count, most
//! frequent first. Values are listed only when every one is at most
//! [`VALUE_TEXT_MAX`] bytes as its self-tagged cell, so a list answer
//! stays bounded at 64 results × columns × five values; a longer value is
//! never cut, because a cut value would pass for a real one. `_time` also
//! gets its earliest and latest instant.
//!
//! # Reads
//!
//! Every read needs the `query` permission and ownership, and checks, in
//! order: the id's shape, the caller's row (else 404), a pruned or expired
//! row (410 `result_gone`; a temporary row past its deadline is marked
//! expired and its file unlinked on the spot), a tombstone past
//! `tombstone_secs` (404), a row whose file is missing (409
//! `result_unavailable`), the file's blake3 digest (500 `result_corrupt`),
//! then the read itself on a connection outside the executor permits.
//! A capture with no columns has no file and answers its rows, all empty,
//! without touching disk.
//!
//! A read can lose its file between those checks and the engine's open:
//! the deadline passes, and another read, the sweep or a capture's
//! admission expires the row and unlinks the file. The read path relies on
//! one rule. The server unlinks a committed capture's file only after the
//! store marked its row expired or pruned: `expire_now` marks, then
//! unlinks, and the sweep, boot and the gate unlink only the ids the
//! store's expiry or prune returned, or files no live row names. A
//! capture's own commit unlinks its file for a taken id on the first
//! insert, which committed nothing, and after a grace that found no row;
//! a row that commits after that grace reads as `file_missing` (see
//! [`COMMIT_GRACE`]). A task cut short preserves the file. A gone row
//! never lives again. So a read that finds its file missing, or whose
//! engine fails on the file, reads the row again under the caller's key
//! and answers from the store. A row gone by then answers 410 or 404 as
//! above. Only a row still live lost its file out of band: it is marked
//! `file_missing` and answers 409. For a live row whose file is there, the
//! digest decides: a changed file answers 500 `result_corrupt`, an intact
//! one the engine's own failure.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use fleet_auth::VerifiedKey;
use trawl_api::results::{
    CaptureKind, CaptureRequest, CaptureResponse, ColumnFacts, Correlation, ResultCell,
    ResultColumn, ResultList, ResultPage, ResultShape, ResultSource, ResultState, StoredAs,
    ValueCount,
};
use trawl_api::value::{Column, ColumnType, QueryResult, Value, wire_cell_len, wire_cell_text};
use trawl_api::{PaginationMeta, QueryExecution};
use trawl_engine::error::EngineError;
use trawl_engine::executor::{RowCap, StoredAs as FileStoredAs};
use trawl_engine::timing::{PhaseClock, QueryPhase};

use super::bounds::{self, Limits, Shape, Source, WindowInput};
use super::charge::{Refusal, Reservation};
use super::root::{self, Published, ResultsRoot, Verified};
use super::{Entered, InFlight, ResultsState};
use crate::deadline::{Deadline, Expired};
use crate::error::{CAPACITY_NOT_STARTED, ServerError};
use crate::from_saved::RunFile;
use crate::policy::{Permission, TrawlAuthz as _};
use crate::pool::{CaptureFinish, CaptureInput, CaptureSource, WorkContext, WorkKind};
use crate::query_timing::{Outcome, QueryTiming, TimingGuard, TimingKind};
use crate::state::AppState;
use crate::store::{CaptureInsert, CaptureRow, HistoryStore, NewCapture, RunStatus};
use crate::tracker::Tracked;

/// The layout of a captured result's file that this server writes and
/// reads: [`trawl_engine::executor::Executor::write_capture_parquet`]'s,
/// columns named by position and each stored natively or as wire text.
pub const CAPTURE_FORMAT_VERSION: u16 = 1;

/// Distinct values a column's facts count before they stop: one more than
/// [`LISTED_VALUES_MAX`], so "more than five" is known exactly.
pub const DISTINCT_CAP: u64 = 6;

/// The most distinct values a column's facts list.
pub const LISTED_VALUES_MAX: usize = 5;

/// The longest value a column's facts list, in bytes of its self-tagged
/// cell (`{"type":"string","value":"..."}`).
pub const VALUE_TEXT_MAX: usize = 256;

/// The shortest byte range a cell read takes: the longest UTF-8 character,
/// so every range holds at least one character and moves forward.
pub const CELL_LENGTH_MIN: u64 = trawl_config::CAPTURE_MAX_WIRE_BYTES_MIN;

/// Why a captured result can no longer be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoneReason {
    /// It passed its deadline.
    Expired,
    /// Boot pruned it to bring its key within a lowered quota.
    Pruned,
}

impl GoneReason {
    /// The `result_gone` answer for this reason.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Expired => "the captured result expired; capture the query again",
            Self::Pruned => {
                "the captured result was removed to bring its key within the capture quota; \
                 capture the query again"
            }
        }
    }
}

/// What made a capture impossible to keep (`result_too_large`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CaptureOverflow {
    /// A whole-shaped result with more rows than a capture keeps.
    #[error(
        "the result has {rows} rows, above the capture limit of {max_rows}; a cut would change \
         its answer, so aggregate further or narrow the window"
    )]
    Rows {
        /// Rows the execution produced.
        rows: u64,
        /// `[capture] max_rows`.
        max_rows: u64,
    },
    /// A whole-shaped result with more cell data than a capture keeps.
    #[error(
        "the result holds more than {max_wire_bytes} bytes of cell data, above the capture \
         limit; a cut would change its answer, so aggregate further or narrow the window"
    )]
    WireBytes {
        /// `[capture] max_wire_bytes`.
        max_wire_bytes: u64,
    },
    /// A file larger than the reservation that admitted it.
    #[error(
        "the captured file would be {file_bytes} bytes, above the capture file limit of \
         {max_file_bytes} bytes; aggregate further or narrow the window"
    )]
    FileBytes {
        /// The written file's size.
        file_bytes: u64,
        /// `[capture] max_file_bytes`.
        max_file_bytes: u64,
    },
}

// -- capture -----------------------------------------------------------------

/// Capture one query's result (module docs, "A capture"): a search,
/// recorded with the kind `search` and no correlation facts.
///
/// `request_id` is the request's ULID, the timing event's correlation.
///
/// # Errors
/// Every refusal `/query` makes, the bounds and admission refusals, the
/// capture's own `result_too_large`, and store failures at commit.
pub async fn capture(
    state: &AppState,
    verified: &VerifiedKey,
    request_id: String,
    req: CaptureRequest,
) -> Result<CaptureResponse, ServerError> {
    let window = req.window.map(|window| WindowInput {
        earliest: window.earliest,
        latest: window.latest,
        last: window.last,
    });
    let spec = Spec {
        kind: CaptureKind::Search,
        query: req.query,
        window,
        shape: None,
        selection: Box::new(NoSelection),
    };
    run(state, verified, request_id, spec).await
}

/// What one capture runs and records: everything by which the four kinds
/// differ (module docs, "A capture").
pub(crate) struct Spec {
    /// The read that makes the capture: recorded on its row, and its
    /// ADR-0046 timing kind.
    pub(crate) kind: CaptureKind,
    /// The text as sent: checked, resolved, recorded as `query`, and
    /// shown back. A correlation read sends the text its builder composed.
    pub(crate) query: String,
    /// The request's structured window, if it carries one.
    pub(crate) window: Option<WindowInput>,
    /// A shape imposed instead of the one [`bounds::classify`] decides
    /// ([`bounds::resolve_as`]).
    pub(crate) shape: Option<Shape>,
    /// What the lane selects from the executed rows around the cut.
    pub(crate) selection: Box<dyn Selection>,
}

/// A read's own step inside the capture lane, around the cut (module docs,
/// "The cut"). Both methods run on the worker, inside the slot, while the
/// query's [`QueryPhase::Post`] is active on its clock.
pub(crate) trait Selection: Send {
    /// Sees every row the executed pipeline produced, uncut. It may reorder
    /// or drop rows and must leave the column list as it is; a selection
    /// that changes it fails the capture as the server's fault. It enters
    /// no phase of its own: `clock` already books `post`.
    ///
    /// # Errors
    /// The capture fails with the error, before anything is written.
    fn before_cut(
        &mut self,
        result: &mut QueryResult,
        clock: &PhaseClock,
    ) -> Result<(), ServerError>;

    /// Sees the rows the cut kept, before their column facts are counted,
    /// and answers the facts the capture records beside them.
    ///
    /// # Errors
    /// The capture fails with the error, before anything is written.
    fn after_cut(&mut self, kept: &QueryResult) -> Result<Option<Correlation>, ServerError>;
}

/// A search's selection: every row goes to the cut, and the capture has
/// no correlation facts.
pub(crate) struct NoSelection;

impl Selection for NoSelection {
    fn before_cut(&mut self, _: &mut QueryResult, _: &PhaseClock) -> Result<(), ServerError> {
        Ok(())
    }

    fn after_cut(&mut self, _: &QueryResult) -> Result<Option<Correlation>, ServerError> {
        Ok(None)
    }
}

/// The ADR-0046 timing kind of a capture made by `kind`. The lane is the
/// capture lane for every kind ([`WorkKind::Capture`]).
const fn timing_kind(kind: CaptureKind) -> TimingKind {
    match kind {
        CaptureKind::Search => TimingKind::Capture,
        CaptureKind::Context => TimingKind::Context,
        CaptureKind::Sightings => TimingKind::Sightings,
        CaptureKind::Compare => TimingKind::Compare,
    }
}

/// Run one capture of any kind (module docs, "A capture").
///
/// Everything that can refuse the capture for its input runs before the
/// admission, so a refused capture takes no permit, no lane slot and no
/// reservation: the request-shape check, then [`plan`]'s bounds. A read
/// that validates more of its input does so before calling this, or in
/// [`plan`], never after the admission.
///
/// # Errors
/// As [`capture`].
#[allow(clippy::too_many_lines)]
pub(crate) async fn run(
    state: &AppState,
    verified: &VerifiedKey,
    request_id: String,
    spec: Spec,
) -> Result<CaptureResponse, ServerError> {
    require_query(verified)?;
    let Spec {
        kind,
        query,
        window,
        shape,
        selection,
    } = spec;
    // Request-shape validation, like the JSON extractor's: before the
    // timing account opens. The store's TEXT columns refuse U+0000 and a
    // capture's text is stored and shown back, so the insert would fail
    // as a 503 for an input the caller controls.
    if query.contains('\0') {
        return Err(ServerError::BadRequest(
            "the query text contains a NUL character".into(),
        ));
    }
    let results = Arc::clone(&state.results);
    let limits = &results.capture;
    let pool = &state.query.pool;

    // One absolute budget for resolution, admission, execution and the
    // file write together (ADR-0057), stamped before anything waits.
    let deadline = Deadline::after(Duration::from_secs(
        limits.deadline_secs.min(state.query.timeout_secs),
    ));

    state
        .total_queries
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // The timing account opens before `dsl_check`, with the query id
    // (ADR-0046), and its window starts where the execution record's
    // `duration_ms` does. Dropped unwritten, because the caller walked
    // away before the pool took over, it writes `outcome=abandoned`.
    let query_id = pool.allocate_query_id();
    crate::transport::failure::record_query_id(query_id);
    let started_at = Utc::now();
    let start = Instant::now();
    let timing = TimingGuard::for_request(query_id, request_id, timing_kind(kind), start);

    // Every refusal of the input happens here, before the admission below.
    let planned = plan(
        state,
        verified,
        &query,
        window.as_ref(),
        shape,
        deadline,
        timing.timing(),
    )
    .await;
    let plan = match planned {
        Ok(plan) => plan,
        Err(e) => {
            timing.emit_complete(Outcome::of_error(&e), Instant::now());
            return Err(e);
        }
    };
    let admitted = {
        let _wait = timing.clock().guard(QueryPhase::PoolWait);
        results.gate.admit(verified.id, deadline).await
    };
    let admission = match admitted {
        Ok(admission) => admission,
        Err(refusal) => {
            let (outcome, error) = refused(refusal);
            timing.emit_complete(outcome, Instant::now());
            return Err(error);
        }
    };
    // A handle on the charge for the commit; the admission itself rides
    // the executor slot.
    let reservation = admission.reservation().clone();

    // The entry closes with `tracked`: through the outcome the capture
    // reaches, or as abandoned if the transport drops this future at an
    // await before the commit task takes it over.
    let executed = plan.resolved.executed.as_str();
    let tracked = state.query.tracker.start(query_id, verified, executed);
    tracing::info!(
        event_type = "query_start",
        user = %verified.name,
        roles = %verified.roles_display(),
        query_id,
        query_len = executed.len(),
        kind = WorkKind::Capture.as_str(),
        "executing capture"
    );
    tracing::debug!(
        event_type = "query_text",
        query_id,
        query = %executed,
        "raw query text (DEBUG-only: never stored under the default filter)"
    );

    let id = new_result_id();
    let keep = Keep {
        root: Arc::clone(&results.root),
        in_flight: Arc::clone(&results.in_flight),
        id: id.clone(),
        shape: plan.resolved.shape,
        selection,
        max_rows: limits.max_rows,
        max_wire_bytes: limits.max_wire_bytes,
        max_file_bytes: limits.max_file_bytes,
    };
    let finish: CaptureFinish<Captured> = Box::new(
        move |input: CaptureInput<'_>| -> Result<Captured, ServerError> { keep.run(input) },
    );
    let work = WorkContext::key(WorkKind::Capture, verified.id)
        .with_user(&verified.name)
        .with_timing(timing.timing().clone())
        .holding(admission.into_holds());
    let outcome = pool
        .capture(
            query_id,
            &plan.lane_dsl,
            plan.lane_source,
            RowCap::Refuse(pool.max_result_rows()),
            deadline,
            0,
            work,
            finish,
        )
        .await;

    // A `from saved` read whose run files went during execution reads as
    // an empty relation; ask the files again, as `/query` does. Booked as
    // `post` unless the account is already written.
    let outcome = if plan.run_files.is_empty() || timing.emitted() {
        guard_vanished_run(outcome, &plan.run_files)
    } else {
        timing.clock().time(QueryPhase::Post, || {
            guard_vanished_run(outcome, &plan.run_files)
        })
    };

    // One instant ends the execution record's duration and the timing
    // window. The timing event describes the execution; the commit below
    // is not a query phase.
    let end = Instant::now();
    let elapsed = end.saturating_duration_since(start);
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let duration_secs = elapsed.as_secs_f64();
    timing.emit_complete(
        outcome
            .as_ref()
            .map_or_else(Outcome::of_error, |_| Outcome::Success),
        end,
    );

    let record = Record {
        tracked,
        history: state.storage.history.clone(),
        key_id: verified.id,
        user: verified.name.clone(),
        executed: executed.to_owned(),
        query_id,
        deadline,
        timeout_secs: deadline_secs(state),
        started_at,
        duration_ms,
        duration_secs,
    };
    match outcome.and_then(|captured| {
        let new = new_capture(
            &results,
            verified.id,
            id,
            kind,
            query,
            &plan.resolved,
            &captured,
        )?;
        Ok((new, captured.file.map(|(file, _)| file)))
    }) {
        // The commit and the record of how it ended run in a task the
        // request awaits, so a client that goes away cuts neither short.
        // The commit has a task of its own inside it, so one that panics is
        // recorded as the failure it is.
        Ok((new, file)) => tokio::spawn(async move {
            let committed = tokio::spawn(commit(results, new, file, reservation))
                .await
                .map_err(|e| ServerError::from_join("capture commit", e))
                .and_then(|committed| committed);
            record.finish(committed).await
        })
        .await
        .map_err(|e| ServerError::from_join("capture record", e))
        .and_then(|answered| answered),
        Err(e) => record.finish(Err(e)).await,
    }
}

/// Step 9's record of how a capture ended: its tracker entry, its history
/// row, its log line and its metrics. Owned, so the commit task carries it
/// and a capture whose client went away during the commit is recorded as
/// the capture it is.
struct Record {
    tracked: Tracked,
    history: HistoryStore,
    key_id: i64,
    user: String,
    executed: String,
    query_id: u64,
    deadline: Deadline,
    /// The deadline the capture ran under, in whole seconds.
    timeout_secs: u64,
    started_at: DateTime<Utc>,
    duration_ms: u64,
    duration_secs: f64,
}

impl Record {
    /// Record `committed` and answer it.
    #[allow(clippy::too_many_lines)]
    async fn finish(
        self,
        committed: Result<CaptureRow, ServerError>,
    ) -> Result<CaptureResponse, ServerError> {
        let Self {
            tracked,
            history,
            key_id,
            user,
            executed,
            query_id,
            deadline,
            timeout_secs,
            started_at,
            duration_ms,
            duration_secs,
        } = self;
        match committed {
            Ok(row) => {
                let total = usize::try_from(row.total_rows).unwrap_or(usize::MAX);
                tracked.complete(total);

                // Best effort under the capture's deadline, as `/query`
                // records history: the capture is committed whatever
                // happens here.
                let recorded = deadline
                    .run(history.record_query(
                        key_id,
                        &executed,
                        duration_ms,
                        total,
                        RunStatus::Success,
                    ))
                    .await;
                match recorded {
                    Ok(Err(e)) => tracing::warn!(
                        event_type = "history_error",
                        key_id,
                        error = %e,
                        "failed to record capture in history store"
                    ),
                    Err(Expired) => tracing::warn!(
                        event_type = "history_error",
                        key_id,
                        "abandoned the history write: the capture's deadline passed with the \
                         result already committed"
                    ),
                    Ok(Ok(_)) => {}
                }

                tracing::info!(
                    event_type = "query_complete",
                    user = %user,
                    query_id,
                    kind = WorkKind::Capture.as_str(),
                    query_len = executed.len(),
                    total_rows = row.total_rows,
                    returned_rows = row.captured_rows,
                    duration_ms,
                    "capture complete"
                );
                metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "success")
                    .increment(1);
                metrics::histogram!(crate::metrics::QUERY_DURATION).record(duration_secs);

                Ok(CaptureResponse {
                    result: row.to_wire(),
                    execution: Some(QueryExecution {
                        started_at: started_at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                        duration_ms,
                    }),
                })
            }
            Err(ServerError::Timeout) => {
                tracked.timeout();
                metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "timeout")
                    .increment(1);
                metrics::histogram!(crate::metrics::QUERY_DURATION).record(duration_secs);
                tracing::warn!(
                    event_type = "query_timeout",
                    user = %user,
                    query_id,
                    kind = WorkKind::Capture.as_str(),
                    query_len = executed.len(),
                    duration_ms,
                    timeout_secs,
                    "capture timed out"
                );
                Err(ServerError::Timeout)
            }
            Err(e) => {
                let safe_msg = e.safe_message();
                tracked.fail(&safe_msg);
                metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "error").increment(1);
                metrics::histogram!(crate::metrics::QUERY_DURATION).record(duration_secs);
                // The class only, as `/query` logs a failure: messages can
                // quote the caller's tokens and database errors their SQL.
                let error_class = e.error_class();
                if is_callers_mistake(&e) {
                    tracing::warn!(
                        event_type = "query_failed",
                        error_type = "parse",
                        error_class,
                        user = %user,
                        query_id,
                        kind = WorkKind::Capture.as_str(),
                        query_len = executed.len(),
                        duration_ms,
                        "capture failed: bad request"
                    );
                } else {
                    tracing::error!(
                        event_type = "query_failed",
                        error_type = "engine",
                        error_class,
                        user = %user,
                        query_id,
                        kind = WorkKind::Capture.as_str(),
                        query_len = executed.len(),
                        duration_ms,
                        "capture failed: engine error"
                    );
                }
                tracing::debug!(
                    event_type = "query_error_text",
                    query_id,
                    error_class,
                    error = %e,
                    safe_error = %safe_msg,
                    "capture failure detail (DEBUG-only: never stored under the default filter)"
                );
                Err(e)
            }
        }
    }
}

/// The deadline a capture runs under, in whole seconds: `[capture]
/// deadline_secs`, capped by `[server] timeout_secs`.
pub(crate) fn deadline_secs(state: &AppState) -> u64 {
    state
        .results
        .capture
        .deadline_secs
        .min(state.query.timeout_secs)
}

/// Whether `/query` logs this failure as the caller's (WARN), not the
/// server's (ERROR). The capture's own refusals join the engine's.
fn is_callers_mistake(error: &ServerError) -> bool {
    matches!(
        error,
        ServerError::Engine(
            EngineError::Parse(_) | EngineError::Emit(_) | EngineError::Refused { .. }
        ) | ServerError::CaptureTooLarge(_)
            | ServerError::CaptureSourceTooLarge { .. }
    )
}

/// What step 3 decided: the text to record and run, and where the lane
/// reads from.
struct Plan {
    resolved: bounds::Resolved,
    /// The text the lane executes: the executed text, or for `from saved`
    /// the stages after the `from saved` stage.
    lane_dsl: String,
    lane_source: CaptureSource,
    /// A `from saved` capture's run files, for the post-check.
    run_files: Vec<RunFile>,
}

/// Step 4: check `query`, resolve the bounds from it and `window` under
/// the imposed `shape`, if any, and, for `from saved`, the stored run and
/// its budget, booking the work on `timing`. Takes nothing a refusal must
/// give back.
///
/// Writes the account itself only when the deadline passed during the
/// resolution or the lookup, which is a capacity refusal; every other
/// refusal is the caller's to write.
#[allow(clippy::too_many_lines)]
async fn plan(
    state: &AppState,
    verified: &VerifiedKey,
    query: &str,
    window: Option<&WindowInput>,
    shape: Option<Shape>,
    deadline: Deadline,
    timing: &QueryTiming,
) -> Result<Plan, ServerError> {
    let capture = &state.results.capture;
    let resolved = timing.clock().time(QueryPhase::DslCheck, || {
        crate::admission::check_dsl(query)?;
        let limits = Limits {
            default_window: Duration::from_secs(capture.default_window_secs),
            max_span: Duration::from_secs(capture.max_span_secs),
        };
        let resolved = bounds::resolve_as(query, window, state.results.clock.now(), &limits, shape)
            .map_err(ServerError::CaptureBounds)?;
        // The server made this text, so a text that fails the check is the
        // server's fault, never the caller's.
        if let Err(e) = crate::admission::check_dsl(&resolved.executed) {
            tracing::debug!(
                event_type = "capture_splice_refused",
                error = %e,
                "the executed text of a capture failed the query check"
            );
            return Err(ServerError::Internal(
                "the capture's executed text failed the query check".into(),
            ));
        }
        Ok(resolved)
    })?;
    // Resolution is work on this worker that no wait interrupts. However
    // short, it may outlast what was left of the deadline, and a capture
    // the deadline never let start is refused here, before it takes an
    // admission, as an expired lookup is below.
    if deadline.expired() {
        timing.emit_complete(Outcome::CapacityRefused, Instant::now());
        return Err(ServerError::ServiceUnavailable(
            CAPACITY_NOT_STARTED.to_owned(),
        ));
    }

    match resolved.source {
        Source::Live { earliest, latest } => Ok(Plan {
            lane_dsl: resolved.executed.clone(),
            lane_source: CaptureSource::Window {
                start: earliest,
                end: latest,
                max_bytes: capture.source_max_bytes,
            },
            run_files: Vec::new(),
            resolved,
        }),
        Source::Stored => {
            let looked_up = {
                let _lookup = timing.clock().guard(QueryPhase::SavedLookup);
                deadline
                    .run(crate::handlers::try_resolve_from_saved(
                        state,
                        verified,
                        &resolved.executed,
                    ))
                    .await
            };
            let from_saved = match looked_up {
                Ok(Ok(Some(from_saved))) => from_saved,
                Ok(Ok(None)) => {
                    return Err(ServerError::Internal(
                        "a stored capture did not resolve as `from saved`".into(),
                    ));
                }
                Ok(Err(refusal)) => return Err(refusal),
                // Refused before any work, as `/query` refuses an expired
                // saved lookup, and named where it is made.
                Err(Expired) => {
                    timing.emit_complete(Outcome::CapacityRefused, Instant::now());
                    return Err(ServerError::ServiceUnavailable(
                        CAPACITY_NOT_STARTED.to_owned(),
                    ));
                }
            };
            if let Some(conflict) =
                crate::from_saved::unavailable_run_conflict(&from_saved.run_files)
            {
                return Err(conflict);
            }
            // The lane measures a window's selection; a stored run's files
            // are measured here, against the same budget.
            let selected_bytes = stored_bytes(&from_saved.run_files).map_err(EngineError::Io)?;
            if selected_bytes > capture.source_max_bytes {
                tracing::warn!(
                    event_type = "capture_refused",
                    reason = "source_too_large",
                    selected_bytes,
                    max_bytes = capture.source_max_bytes,
                    "capture refused before the engine ran: its stored run is larger than the \
                     capture source budget"
                );
                return Err(ServerError::CaptureSourceTooLarge {
                    selected_bytes,
                    max_bytes: capture.source_max_bytes,
                });
            }
            Ok(Plan {
                lane_dsl: from_saved.remaining_dsl,
                lane_source: CaptureSource::Resolved {
                    source: from_saved.source,
                },
                run_files: from_saved.run_files,
                resolved,
            })
        }
    }
}

/// The bytes of a stored run's files, as the engine reads them: through a
/// symlink, unlike a window's selection, because a run file is one named
/// path and the engine opens it by name. A file gone since the
/// vanished-run check counts as zero; the post-check names it.
fn stored_bytes(files: &[RunFile]) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    for file in files {
        match std::fs::metadata(&file.absolute_path) {
            Ok(meta) if meta.is_file() => total = total.saturating_add(meta.len()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// An admission refusal as the request's answer, with the outcome its
/// timing account ends with: a full lane, installation or quota is a
/// capacity refusal, a store failure an error.
fn refused(refusal: Refusal) -> (Outcome, ServerError) {
    let (reason, retry_after_secs, error) = match refusal {
        Refusal::Busy { retry_after_secs } => (
            "busy",
            retry_after_secs,
            ServerError::CaptureBusy { retry_after_secs },
        ),
        Refusal::QuotaExceeded { retry_after_secs } => (
            "quota_exceeded",
            retry_after_secs,
            ServerError::CaptureQuotaExceeded { retry_after_secs },
        ),
        Refusal::Store(e) => {
            let error = ServerError::Store(e);
            return (Outcome::of_error(&error), error);
        }
    };
    tracing::warn!(
        event_type = "capture_refused",
        reason,
        retry_after_secs,
        "capture refused before it took an executor permit"
    );
    (Outcome::CapacityRefused, error)
}

/// `/query`'s vanished-run post-check ([`crate::handlers`]'s
/// `guard_vanished_run_result`) over a capture's outcome: a `from saved`
/// read that came back with no columns, or with an engine error, asks its
/// run files once more, and a file now gone is the named 409. A capture
/// dropped here unlinks the file it published.
fn guard_vanished_run(
    outcome: Result<Captured, ServerError>,
    run_files: &[RunFile],
) -> Result<Captured, ServerError> {
    if run_files.is_empty() {
        return outcome;
    }
    let suspect = match &outcome {
        Ok(captured) => captured.columns.is_empty(),
        Err(ServerError::Engine(_)) => true,
        Err(_) => false,
    };
    if suspect && let Some(conflict) = crate::from_saved::unavailable_run_conflict(run_files) {
        return Err(conflict);
    }
    outcome
}

/// A fresh result id: 128 random bits as 32 lowercase hex digits.
fn new_result_id() -> String {
    use std::fmt::Write as _;
    let bytes: [u8; 16] = rand::random();
    let mut id = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// What the lane's `finish` needs: where the file goes, the read's
/// selection and the cap.
struct Keep {
    root: Arc<ResultsRoot>,
    in_flight: Arc<InFlight>,
    id: String,
    shape: Shape,
    selection: Box<dyn Selection>,
    max_rows: u64,
    max_wire_bytes: u64,
    max_file_bytes: u64,
}

/// What a capture kept, out of the slot.
struct Captured {
    columns: Vec<ResultColumn>,
    facts: Vec<ColumnFacts>,
    shape: ResultShape,
    kept_rows: u64,
    total_rows: u64,
    wire_bytes: u64,
    /// The facts the read's selection computed; `None` for a search.
    correlation: Option<Correlation>,
    /// The published file and what publishing it measured; `None` for a
    /// result with no columns.
    file: Option<(Uncommitted, Published)>,
}

impl Keep {
    /// The lane's `finish`: select, cut, select again, count the facts,
    /// write and publish, all on the worker inside the slot.
    fn run(self, input: CaptureInput<'_>) -> Result<Captured, ServerError> {
        #[cfg(any(test, feature = "test-support"))]
        let seams = input.seams;
        let CaptureInput {
            mut result,
            executor,
            clock,
            cancel,
            ..
        } = input;
        let mut selection = self.selection;
        let shape = match self.shape {
            Shape::Events if has_event_columns(&result) => ResultShape::Events,
            Shape::Events | Shape::Whole => ResultShape::Whole,
        };
        let (total_rows, kept, wire_bytes, facts, correlation) =
            clock.time(QueryPhase::Post, || {
                let columns = result.columns.clone();
                selection.before_cut(&mut result, clock)?;
                if result.columns != columns {
                    return Err(ServerError::Internal(
                        "a capture's selection changed the result's columns".into(),
                    ));
                }
                let total_rows = u64::try_from(result.rows.len()).unwrap_or(u64::MAX);
                let (kept, wire_bytes) = cut(&result, shape, self.max_rows, self.max_wire_bytes)?;
                result.rows.truncate(kept);
                let correlation = selection.after_cut(&result)?;
                let facts = column_facts(&result)?;
                Ok::<_, ServerError>((total_rows, kept, wire_bytes, facts, correlation))
            })?;
        let kept_rows = u64::try_from(kept).unwrap_or(u64::MAX);

        if result.columns.is_empty() {
            return Ok(Captured {
                columns: Vec::new(),
                facts,
                shape,
                kept_rows,
                total_rows,
                wire_bytes,
                correlation,
                file: None,
            });
        }
        // The write takes no cancellation latch of its own.
        if cancel.latched() {
            return Err(ServerError::Engine(EngineError::Cancelled));
        }
        // In flight before the file can reach its final name, so no sweep
        // sees it published and unregistered.
        let entered = self.in_flight.enter(&self.id);
        let (stored_as, published) = clock.time(QueryPhase::Write, || {
            let staged = self.root.stage().map_err(EngineError::Io)?;
            #[cfg(any(test, feature = "test-support"))]
            seams.reach(crate::pool::seam::Seam::Writing);
            let written = executor.write_capture_parquet(&result, staged.path())?;
            if written.bytes > self.max_file_bytes {
                return Err(ServerError::CaptureTooLarge(CaptureOverflow::FileBytes {
                    file_bytes: written.bytes,
                    max_file_bytes: self.max_file_bytes,
                }));
            }
            let published = self
                .root
                .publish(staged, &self.id)
                .map_err(EngineError::Io)?;
            Ok((written.stored_as, published))
        })?;
        let file = Uncommitted {
            root: self.root,
            id: self.id,
            armed: true,
            _in_flight: entered,
        };
        let columns = result
            .columns
            .into_iter()
            .zip(stored_as)
            .map(|(column, stored_as)| ResultColumn {
                name: column.name,
                ty: column.ty,
                stored_as: wire_stored_as(stored_as),
            })
            .collect();
        Ok(Captured {
            columns,
            facts,
            shape,
            kept_rows,
            total_rows,
            wire_bytes,
            correlation,
            file: Some((file, published)),
        })
    }
}

/// Whether the result carries both columns the event order sorts by.
fn has_event_columns(result: &QueryResult) -> bool {
    let has = |name: &str| result.columns.iter().any(|column| column.name == name);
    has(trawl_core::schema::TIME) && has(trawl_core::schema::INGESTED)
}

/// How many leading rows a capture keeps, and their wire bytes (module
/// docs, "The cut").
fn cut(
    result: &QueryResult,
    shape: ResultShape,
    max_rows: u64,
    max_wire_bytes: u64,
) -> Result<(usize, u64), ServerError> {
    let rows = u64::try_from(result.rows.len()).unwrap_or(u64::MAX);
    if shape == ResultShape::Whole && rows > max_rows {
        return Err(ServerError::CaptureTooLarge(CaptureOverflow::Rows {
            rows,
            max_rows,
        }));
    }
    let mut wire_bytes: u64 = 0;
    for (index, row) in result.rows.iter().enumerate() {
        let kept = u64::try_from(index).unwrap_or(u64::MAX);
        let len = row_wire_len(&result.columns, row)?;
        let fits = kept < max_rows && wire_bytes.saturating_add(len) <= max_wire_bytes;
        if !fits {
            return match shape {
                ResultShape::Events => Ok((index, wire_bytes)),
                ResultShape::Whole => {
                    Err(ServerError::CaptureTooLarge(CaptureOverflow::WireBytes {
                        max_wire_bytes,
                    }))
                }
            };
        }
        wire_bytes += len;
    }
    Ok((result.rows.len(), wire_bytes))
}

/// One row's cells as the wire encodes them, without framing.
fn row_wire_len(columns: &[Column], row: &[Value]) -> Result<u64, ServerError> {
    if row.len() != columns.len() {
        return Err(ServerError::Internal(format!(
            "a result row has {} cells for {} columns",
            row.len(),
            columns.len()
        )));
    }
    columns
        .iter()
        .zip(row)
        .try_fold(0_u64, |sum, (column, value)| {
            let len = wire_cell_len(&column.name, &column.ty, value).map_err(|e| cell_fault(&e))?;
            Ok(sum.saturating_add(u64::try_from(len).unwrap_or(u64::MAX)))
        })
}

/// A result cell the wire refuses: a producer fault, never the caller's.
fn cell_fault(e: &serde_json::Error) -> ServerError {
    ServerError::Internal(format!("a result cell does not encode: {e}"))
}

/// Facts per column over `result`'s rows (module docs, "Column facts").
fn column_facts(result: &QueryResult) -> Result<Vec<ColumnFacts>, ServerError> {
    result
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let cells = result.rows.iter().map(|row| &row[index]);
            facts_of(column, cells)
        })
        .collect()
}

/// One distinct value a column's facts count: its wire text under the
/// column type, which is also how two values compare.
struct Seen {
    text: String,
    value: Value,
    count: u64,
}

/// The facts of one column over `cells`.
fn facts_of<'a>(
    column: &Column,
    cells: impl Iterator<Item = &'a Value>,
) -> Result<ColumnFacts, ServerError> {
    let cap = usize::try_from(DISTINCT_CAP).unwrap_or(usize::MAX);
    let mut nulls: u64 = 0;
    let mut seen: Vec<Seen> = Vec::new();
    let mut capped = false;
    let times = column.name == trawl_core::schema::TIME && column.ty == ColumnType::Timestamp;
    let mut bounds: Option<(DateTime<Utc>, &str, DateTime<Utc>, &str)> = None;
    let mut unordered = false;
    for value in cells {
        if matches!(value, Value::Null) {
            nulls += 1;
            continue;
        }
        if times && !unordered {
            match value {
                Value::String(text) => match DateTime::parse_from_rfc3339(text) {
                    Ok(at) => {
                        let text = text.as_str();
                        let at = at.with_timezone(&Utc);
                        bounds = Some(match bounds {
                            None => (at, text, at, text),
                            Some((min, min_text, max, max_text)) => {
                                let (min, min_text) = if at < min {
                                    (at, text)
                                } else {
                                    (min, min_text)
                                };
                                let (max, max_text) = if at > max {
                                    (at, text)
                                } else {
                                    (max, max_text)
                                };
                                (min, min_text, max, max_text)
                            }
                        });
                    }
                    // `infinity` and years past 9999 order in no RFC 3339
                    // reading: no earliest or latest rather than a wrong one.
                    Err(_) => unordered = true,
                },
                _ => unordered = true,
            }
        }
        if capped {
            continue;
        }
        let text = wire_cell_text(&column.name, &column.ty, value).map_err(|e| cell_fault(&e))?;
        if let Some(entry) = seen.iter_mut().find(|entry| entry.text == text) {
            entry.count += 1;
        } else {
            seen.push(Seen {
                text,
                value: value.clone(),
                count: 1,
            });
            capped = seen.len() >= cap;
        }
    }
    let distinct = u64::try_from(seen.len()).unwrap_or(u64::MAX);
    let values = if seen.len() <= LISTED_VALUES_MAX && listable(&seen)? {
        // Most frequent first; a stable sort keeps ties in first-seen order.
        seen.sort_by_key(|entry| std::cmp::Reverse(entry.count));
        Some(
            seen.into_iter()
                .map(|entry| ValueCount {
                    value: entry.value,
                    count: entry.count,
                })
                .collect(),
        )
    } else {
        None
    };
    let (min, max) = match bounds {
        Some((_, min, _, max)) if !unordered => (
            Some(Value::String(min.to_owned())),
            Some(Value::String(max.to_owned())),
        ),
        _ => (None, None),
    };
    Ok(ColumnFacts {
        nulls,
        distinct,
        distinct_capped: capped,
        values,
        min,
        max,
    })
}

/// Whether every value is short enough to list, as its self-tagged cell.
fn listable(seen: &[Seen]) -> Result<bool, ServerError> {
    for entry in seen {
        let len =
            wire_cell_len("value", &ColumnType::Mixed, &entry.value).map_err(|e| cell_fault(&e))?;
        if len > VALUE_TEXT_MAX {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The engine's storage flag as the wire spells it.
const fn wire_stored_as(stored_as: FileStoredAs) -> StoredAs {
    match stored_as {
        FileStoredAs::Native => StoredAs::Native,
        FileStoredAs::WireText => StoredAs::WireText,
    }
}

/// The wire's storage flag as the engine's reader takes it.
const fn file_stored_as(stored_as: StoredAs) -> FileStoredAs {
    match stored_as {
        StoredAs::Native => FileStoredAs::Native,
        StoredAs::WireText => FileStoredAs::WireText,
    }
}

/// The row key `key_id`'s capture commits: `query` is the text as sent,
/// `kind` the read that made it.
fn new_capture(
    results: &ResultsState,
    key_id: i64,
    id: String,
    kind: CaptureKind,
    query: String,
    resolved: &bounds::Resolved,
    captured: &Captured,
) -> Result<NewCapture, ServerError> {
    let created_at = results.clock.now();
    let expires_at = i64::try_from(results.capture.ttl_secs)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .and_then(|ttl| created_at.checked_add_signed(ttl))
        .ok_or_else(|| ServerError::Internal("the capture deadline is out of range".into()))?;
    let (source, earliest, latest) = match resolved.source {
        Source::Live { earliest, latest } => (ResultSource::Live, Some(earliest), Some(latest)),
        Source::Stored => (ResultSource::Stored, None, None),
    };
    let (file_bytes, digest) = captured.file.as_ref().map_or((0, None), |(_, published)| {
        (published.file_bytes, Some(published.digest))
    });
    Ok(NewCapture {
        id,
        key_id,
        created_at,
        expires_at,
        query,
        executed_query: resolved.executed.clone(),
        source,
        shape: captured.shape,
        earliest,
        latest,
        columns: captured.columns.clone(),
        column_facts: captured.facts.clone(),
        captured_rows: captured.kept_rows,
        total_rows: captured.total_rows,
        wire_bytes: captured.wire_bytes,
        file_bytes,
        digest,
        format_version: CAPTURE_FORMAT_VERSION,
        kind,
        correlation: captured.correlation.clone(),
    })
}

/// A published capture file whose row is not committed yet.
///
/// Dropped armed, it unlinks the file: the capture ended without its row.
/// The publish refused to replace anything, so the name is this capture's
/// own and nothing else is removed. However it is dropped, the capture
/// leaves [`InFlight`] after that, never before.
///
/// It is armed only until the capture's first insert can reach the store
/// ([`commit`]). From then on a row may name the file, so it is disarmed
/// through the inserts and any grace ([`in_doubt`]): a task cut short, as
/// at shutdown, leaves the file for boot and the sweep instead of
/// unlinking one that a row may name. A taken id on the first insert,
/// which committed nothing, discards the file, and so does a grace that
/// finds no row ([`COMMIT_GRACE`]).
struct Uncommitted {
    root: Arc<ResultsRoot>,
    id: String,
    armed: bool,
    /// Dropped after [`Drop::drop`] has run: fields drop after the body.
    _in_flight: Entered,
}

impl Uncommitted {
    /// The row is committed: the file stays.
    fn commit(mut self) {
        self.armed = false;
    }

    /// Whether the row committed is unknown: the file stays for boot and
    /// the sweep, which remove it once no live row names it.
    fn leave(mut self) {
        self.armed = false;
    }

    /// Whether the row commits is not known from here on: disarmed, and
    /// still in flight.
    fn doubt(mut self) -> Self {
        self.armed = false;
        self
    }

    /// The row did not come: unlink the file now, then leave [`InFlight`].
    fn discard(mut self) {
        self.armed = true;
    }
}

impl Drop for Uncommitted {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = self.root.unlink(&self.id) {
            tracing::warn!(
                event_type = "results_unlink_failed",
                error = %error,
                "could not remove an uncommitted capture's file; the sweep removes it"
            );
        }
    }
}

/// Commit a capture's row, then settle its reservation and keep its file.
///
/// Runs in a task of its own, so a client that goes away cannot cut it
/// short and leave the outcome unknown. The insert's outcome decides the
/// file:
///
/// - inserted: settle, keep the file;
/// - the id is taken: a fresh 128-bit id that names an existing row is a
///   generator fault, never a race to retry, so 500; the file goes;
/// - the insert failed, which may mean it committed and the answer was lost
///   with the connection, or that a backend whose client is gone is still
///   committing it: fence it by running the same insert again. Inserted,
///   or the id taken, the row is committed: settle and keep the file. The
///   fence failing too leaves the commit in doubt: the answer is the first
///   insert's error, and the file and the reservation go to a task that
///   holds both for [`COMMIT_GRACE`] and then reads the row ([`in_doubt`]).
///
/// The file is disarmed before the first insert can reach the store: from
/// then on a row may commit whatever this task learns. A task dropped at
/// either await, as when trawld shuts down with the commit pending, leaves
/// the file for boot and the sweep. The taken id, which committed nothing,
/// removes it here, and so does a grace that finds no row ([`in_doubt`]);
/// a row that commits after that grace reads as `file_missing`, as
/// [`COMMIT_GRACE`] states.
async fn commit(
    results: Arc<ResultsState>,
    new: NewCapture,
    file: Option<Uncommitted>,
    reservation: Reservation,
) -> Result<CaptureRow, ServerError> {
    let file = file.map(Uncommitted::doubt);
    match results.store.insert(&new).await {
        Ok(CaptureInsert::Inserted) => {}
        Ok(CaptureInsert::IdTaken) => {
            if let Some(file) = file {
                file.discard();
            }
            return Err(ServerError::Internal(
                "a freshly drawn result id names an existing capture".into(),
            ));
        }
        // The fence: the same row again, on the next connection the pool
        // hands out. The id is the primary key, so Postgres orders the two
        // inserts. A first insert whose statement ran but whose transaction
        // has not ended makes the fence wait on that row's key until it
        // commits (the id is taken) or aborts (the fence inserts). One that
        // never reached the backend lets the fence insert, and then meets
        // the taken id itself if it arrives. Taken here, unlike on the
        // first insert, the id names the first insert's own row.
        //
        // The fence has no timeout. Waiting is the answer it exists to
        // give, and this task holds the file in flight and the reservation
        // charged while it waits.
        Err(insert) => {
            let Ok(fenced) = results.store.insert(&new).await else {
                in_doubt(results, new.id, new.key_id, file, reservation);
                return Err(insert.into());
            };
            tracing::warn!(
                event_type = "results_commit_fenced",
                result_id = %new.id,
                fence = match fenced {
                    CaptureInsert::Inserted => "inserted",
                    CaptureInsert::IdTaken => "id_taken",
                },
                error = %insert,
                "a capture's insert answered an error and its fence settled it; the row is \
                 committed and the capture keeps its file"
            );
        }
    }
    reservation.settle();
    if let Some(file) = file {
        file.commit();
    }
    Ok(committed_row(new))
}

/// How long a capture whose commit is in doubt keeps its file in flight and
/// its reservation charged before its row is read (module docs, "A
/// capture"). The fallback for a double fault: the insert and its fence
/// both failed.
///
/// The wait covers a backend still finishing either insert after its
/// client saw the connection fail. Such a backend learns that its client
/// is gone when it next writes to it, after the commit, so it finishes the
/// statement; `client_connection_check_interval`, when an operator sets
/// it, only abandons the statement sooner, which commits nothing. The
/// first insert waits on no other session: the table has no foreign key,
/// the id is fresh, and trawld takes the only conflicting table locks for
/// migrations, at boot. A fence waits on the first insert's row alone, and
/// only until that insert's transaction ends. What remains is I/O: the
/// heap and index pages, and the commit's WAL flush, which postgres does
/// not bound.
/// [`crate::store::PURGE_COMMIT_BOUND`] judges that a commit behind an
/// ordinary busy checkpoint finishes within thirty seconds. Missing here
/// costs more than there: a row that lands after its file was unlinked is
/// listed as the owner's and answers 409 until it expires. Waiting longer
/// holds one reservation and one file of at most `max_file_bytes` for the
/// grace, and only after two store failures. So the grace is twice that
/// bound.
///
/// No wait covers a commit stalled past it, as on a wedged volume, after a
/// fence that failed. Its row lands without its file and reads as
/// `file_missing`, a named outcome.
pub const COMMIT_GRACE: Duration = Duration::from_secs(60);

/// Settle a capture whose insert and fence both failed, in a task of its
/// own: keep `file` in flight, disarmed, and `reservation` charged for
/// [`COMMIT_GRACE`], then read the row. Found, one of the inserts landed:
/// settle and keep the file. Absent, neither did: unlink the file, and the
/// reservation lapses. A read that fails leaves the file to the sweep and
/// lets the reservation lapse; by then the grace has passed.
///
/// The reservation stays charged through the grace because the store's
/// usage cannot count a commit still on its way: released early, it would
/// admit a capture past the quota if the row lands. Held, it can refuse
/// the key's captures at its quota for the grace, after a store failure.
/// The task ending at shutdown leaves the file for boot.
fn in_doubt(
    results: Arc<ResultsState>,
    id: String,
    key_id: i64,
    file: Option<Uncommitted>,
    reservation: Reservation,
) {
    tracing::warn!(
        event_type = "results_commit_in_doubt",
        grace_secs = COMMIT_GRACE.as_secs(),
        "a capture's insert and its fence both failed; the commit may still land, so its file \
         and reservation are held for the grace"
    );
    #[cfg(any(test, feature = "test-support"))]
    let held = results.store.faults().take_grace();
    tokio::spawn(async move {
        #[cfg(any(test, feature = "test-support"))]
        let settled = if let Some(held) = held {
            Some(held.wait().await)
        } else {
            tokio::time::sleep(COMMIT_GRACE).await;
            None
        };
        #[cfg(not(any(test, feature = "test-support")))]
        tokio::time::sleep(COMMIT_GRACE).await;

        match results.store.get(&id, key_id).await {
            Ok(Some(_)) => {
                reservation.settle();
                if let Some(file) = file {
                    file.commit();
                }
                tracing::info!(
                    event_type = "results_commit_settled",
                    outcome = "committed",
                    "a capture whose commit was in doubt committed late and keeps its file"
                );
            }
            Ok(None) => {
                if let Some(file) = file {
                    file.discard();
                }
                tracing::info!(
                    event_type = "results_commit_settled",
                    outcome = "absent",
                    "a capture whose commit was in doubt has no row after the grace; its file \
                     is removed"
                );
            }
            Err(_) => {
                if let Some(file) = file {
                    file.leave();
                }
                tracing::warn!(
                    event_type = "results_commit_settled",
                    outcome = "unknown",
                    "a capture whose commit was in doubt cannot be read back after the grace; \
                     the sweep removes its file once no live row names it"
                );
            }
        }
        drop(reservation);
        #[cfg(any(test, feature = "test-support"))]
        if let Some(settled) = settled {
            let _ = settled.send(());
        }
    });
}

/// The row an insert of `new` committed.
fn committed_row(new: NewCapture) -> CaptureRow {
    CaptureRow {
        id: new.id,
        key_id: new.key_id,
        created_at: new.created_at,
        expires_at: Some(new.expires_at),
        gone_at: None,
        state: ResultState::Temporary,
        query: new.query,
        executed_query: new.executed_query,
        source: new.source,
        shape: new.shape,
        earliest: new.earliest,
        latest: new.latest,
        columns: new.columns,
        column_facts: new.column_facts,
        captured_rows: new.captured_rows,
        total_rows: new.total_rows,
        wire_bytes: new.wire_bytes,
        file_bytes: new.file_bytes,
        digest: new.digest,
        file_missing: false,
        format_version: new.format_version,
        investigation_id: None,
        kind: new.kind,
        correlation: new.correlation,
    }
}

// -- reads -------------------------------------------------------------------

/// The caller's live captures, newest first.
///
/// # Errors
/// 403 without the `query` permission; a store failure.
pub async fn list(state: &AppState, verified: &VerifiedKey) -> Result<ResultList, ServerError> {
    require_query(verified)?;
    let now = state.results.clock.now();
    let rows = state.results.store.list(verified.id, now).await?;
    Ok(ResultList {
        results: rows.iter().map(CaptureRow::to_wire).collect(),
    })
}

/// One page of capture `id`: rows `offset` up to `offset + limit` of the
/// columns `columns` names (module docs, "Reads").
///
/// `limit` defaults to and may not pass `[capture] page_max_rows`;
/// `offset` defaults to 0 and may not pass the captured rows. `columns`
/// is display names separated by commas; each selects every column with
/// that name, and the page carries the selected columns in stored order,
/// with their positions. A name that holds a comma cannot be selected
/// alone; leave `columns` out to read every column.
///
/// # Errors
/// 403, 404, 410, 409 and 500 as the lifecycle decides; 400 for a limit,
/// an offset or a name the capture refuses.
pub async fn page(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    columns: Option<&str>,
) -> Result<ResultPage, ServerError> {
    let results = &state.results;
    let row = readable(results, verified, id).await?;
    let page_max_rows = results.capture.page_max_rows;
    let limit = limit.unwrap_or(page_max_rows);
    if limit > page_max_rows {
        return Err(ServerError::BadRequest(format!(
            "limit {limit} is above the page limit of {page_max_rows} rows"
        )));
    }
    let offset = offset.unwrap_or(0);
    if offset > row.captured_rows {
        return Err(ServerError::BadRequest(format!(
            "offset {offset} is past the result's {} rows",
            row.captured_rows
        )));
    }
    let positions = project(&row.columns, columns)?;

    let rows = if row.columns.is_empty() {
        // No columns, no file: each row is empty.
        let returned = limit.min(row.captured_rows - offset);
        vec![Vec::new(); usize::try_from(returned).unwrap_or(usize::MAX)]
    } else {
        let path = checked_file(state, &row).await?;
        let types: Vec<ColumnType> = row.columns.iter().map(|c| c.ty.clone()).collect();
        let stored_as: Vec<FileStoredAs> = row
            .columns
            .iter()
            .map(|c| file_stored_as(c.stored_as))
            .collect();
        let selected = positions.clone();
        let pool = state.query.pool.clone();
        let read = tokio::task::spawn_blocking(move || {
            #[cfg(any(test, feature = "test-support"))]
            pool.reach(crate::pool::seam::Seam::Opening);
            let executor = pool.detached_executor()?;
            executor.read_capture_page(&path, &types, &stored_as, &selected, offset, limit)
        })
        .await
        .map_err(|e| ServerError::from_join("capture page read", e))?;
        match read {
            Ok(rows) => rows,
            Err(error) => return Err(read_failed(state, &row, error).await),
        }
    };

    let page = QueryResult {
        columns: positions
            .iter()
            .map(|&position| {
                let column = &row.columns[position];
                Column::new(column.name.clone(), column.ty.clone())
            })
            .collect(),
        rows,
    };
    let pagination = PaginationMeta {
        limit: usize::try_from(limit).unwrap_or(usize::MAX),
        offset: usize::try_from(offset).unwrap_or(usize::MAX),
        returned: page.rows.len(),
        total: usize::try_from(row.captured_rows).unwrap_or(usize::MAX),
    };
    Ok(ResultPage {
        result: row.to_wire(),
        positions,
        page,
        pagination,
    })
}

/// A byte range of one cell of capture `id`: row `row`, the column at
/// 0-based position `column`, over the cell's #214 wire JSON text.
///
/// `offset` defaults to 0 and must sit on a character boundary, as every
/// `next_offset` does. `length` defaults to and may not pass
/// `[capture] max_wire_bytes`, which holds any captured cell whole, and is
/// at least [`CELL_LENGTH_MIN`]. The range is cut back to a character
/// boundary, so it can be shorter than asked for.
///
/// # Errors
/// 403, 404, 410, 409 and 500 as the lifecycle decides; 400 for a row, a
/// column, an offset or a length the capture refuses.
#[allow(clippy::too_many_arguments)]
pub async fn cell(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    row: u64,
    column: usize,
    offset: Option<u64>,
    length: Option<u64>,
) -> Result<ResultCell, ServerError> {
    let results = &state.results;
    let record = readable(results, verified, id).await?;
    let Some(meta) = record.columns.get(column).cloned() else {
        return Err(ServerError::BadRequest(format!(
            "column {column} is past the result's {} columns",
            record.columns.len()
        )));
    };
    if row >= record.captured_rows {
        return Err(ServerError::BadRequest(format!(
            "row {row} is past the result's {} rows",
            record.captured_rows
        )));
    }
    let max_length = results.capture.max_wire_bytes;
    let length = length.unwrap_or(max_length);
    if length > max_length {
        return Err(ServerError::BadRequest(format!(
            "length {length} is above the cell read limit of {max_length} bytes"
        )));
    }
    if length < CELL_LENGTH_MIN {
        return Err(ServerError::BadRequest(format!(
            "length {length} is below {CELL_LENGTH_MIN} bytes, the longest character"
        )));
    }

    let path = checked_file(state, &record).await?;
    let pool = state.query.pool.clone();
    let ty = meta.ty.clone();
    let stored_as = file_stored_as(meta.stored_as);
    let read = tokio::task::spawn_blocking(move || {
        #[cfg(any(test, feature = "test-support"))]
        pool.reach(crate::pool::seam::Seam::Opening);
        let executor = pool.detached_executor()?;
        executor.read_capture_cell(&path, &ty, stored_as, row, column)
    })
    .await
    .map_err(|e| ServerError::from_join("capture cell read", e))?;
    let value = match read {
        Ok(value) => value,
        Err(error) => return Err(read_failed(state, &record, error).await),
    }
    .ok_or_else(|| {
        ServerError::Internal("a captured result's file holds fewer rows than its record".into())
    })?;
    let text = wire_cell_text(&meta.name, &meta.ty, &value).map_err(|e| cell_fault(&e))?;
    cell_range(meta.ty, &text, offset.unwrap_or(0), length)
}

/// `[offset, offset + length)` of `text`, the end cut back to a character
/// boundary.
fn cell_range(
    ty: ColumnType,
    text: &str,
    offset: u64,
    length: u64,
) -> Result<ResultCell, ServerError> {
    let total = text.len();
    let start = usize::try_from(offset)
        .ok()
        .filter(|&start| start <= total)
        .ok_or_else(|| {
            ServerError::BadRequest(format!("offset {offset} is past the cell's {total} bytes"))
        })?;
    if !text.is_char_boundary(start) {
        return Err(ServerError::BadRequest(format!(
            "offset {offset} falls inside a character; start at 0 or at a next_offset"
        )));
    }
    let mut end = start.saturating_add(usize::try_from(length).unwrap_or(usize::MAX));
    end = end.min(total);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let as_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    Ok(ResultCell {
        ty,
        offset: as_u64(start),
        length: as_u64(end - start),
        total_bytes: as_u64(total),
        next_offset: (end < total).then(|| as_u64(end)),
        text: text[start..end].to_owned(),
    })
}

/// The positions `names` selects, ascending; every position when `names`
/// is absent.
fn project(columns: &[ResultColumn], names: Option<&str>) -> Result<Vec<usize>, ServerError> {
    let Some(names) = names else {
        return Ok((0..columns.len()).collect());
    };
    let mut positions = BTreeSet::new();
    for name in names.split(',') {
        let mut found = false;
        for (position, column) in columns.iter().enumerate() {
            if column.name == name {
                positions.insert(position);
                found = true;
            }
        }
        if !found {
            return Err(ServerError::BadRequest(format!(
                "the result has no column named {name:?}"
            )));
        }
    }
    Ok(positions.into_iter().collect())
}

/// The caller's capture `id`, if it may be read now (module docs,
/// "Reads"): everything but the file's own checks.
async fn readable(
    results: &ResultsState,
    verified: &VerifiedKey,
    id: &str,
) -> Result<CaptureRow, ServerError> {
    require_query(verified)?;
    if !root::is_result_id(id) {
        return Err(not_found());
    }
    let row = results
        .store
        .get(id, verified.id)
        .await?
        .ok_or_else(not_found)?;
    if let Some(refusal) = gone(results, &row).await {
        return Err(refusal);
    }
    if row.file_missing {
        return Err(ServerError::ResultUnavailable);
    }
    if row.format_version != CAPTURE_FORMAT_VERSION {
        return Err(ServerError::Internal(format!(
            "a captured result is stored in format {}, which this server does not read",
            row.format_version
        )));
    }
    Ok(row)
}

/// What a read of `row` answers when the row alone says the capture can no
/// longer be read: 410 `result_gone` for a pruned or expired row, or a
/// temporary one past its deadline, which is expired on the spot; 404 once
/// its tombstone is past `tombstone_secs`. `None` for a live row.
async fn gone(results: &ResultsState, row: &CaptureRow) -> Option<ServerError> {
    let now = results.clock.now();
    let (reason, gone_at) = match row.state {
        ResultState::Pruned => (GoneReason::Pruned, row.gone_at),
        ResultState::Expired => (GoneReason::Expired, row.gone_at),
        ResultState::Temporary => match row.expires_at {
            Some(at) if at <= now => {
                expire_now(results, row, now).await;
                // The tombstone clock starts at the deadline, not now.
                (GoneReason::Expired, Some(at))
            }
            _ => return None,
        },
        ResultState::Kept => return None,
    };
    let tombstone = i64::try_from(results.capture.tombstone_secs)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .unwrap_or(TimeDelta::MAX);
    let past = gone_at
        .and_then(|at| at.checked_add_signed(tombstone))
        .is_some_and(|end| end <= now);
    Some(if past {
        not_found()
    } else {
        ServerError::ResultGone { reason }
    })
}

/// Whether `row` is still live as the store holds it now: `Ok` when it is,
/// else what a read of it answers now ([`gone`]; 404 for a row the sweep
/// has since deleted). Read under the same key, so the answer is the
/// caller's own.
async fn still_live(results: &ResultsState, row: &CaptureRow) -> Result<(), ServerError> {
    let current = results
        .store
        .get(&row.id, row.key_id)
        .await?
        .ok_or_else(not_found)?;
    gone(results, &current).await.map_or(Ok(()), Err)
}

/// A temporary capture a read found past its deadline: mark it expired and
/// unlink its file now, not at the next sweep. A failure is logged and
/// left to the sweep, which retries both; the read answers 410 either way.
async fn expire_now(results: &ResultsState, row: &CaptureRow, now: DateTime<Utc>) {
    if let Err(error) = results.store.mark_expired(&row.id, now).await {
        tracing::warn!(
            event_type = "results_expire_failed",
            error = %error,
            "could not mark an expired capture; the sweep retries"
        );
        return;
    }
    if row.file_bytes == 0 {
        return;
    }
    let root = Arc::clone(&results.root);
    let id = row.id.clone();
    match tokio::task::spawn_blocking(move || root.unlink(&id)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(
            event_type = "results_unlink_failed",
            error = %error,
            "could not remove an expired capture's file; the sweep removes it"
        ),
        Err(error) => tracing::warn!(
            event_type = "results_unlink_failed",
            error = %crate::error::join_failure_text("capture unlink", error),
            "could not remove an expired capture's file; the sweep removes it"
        ),
    }
}

/// The path of `row`'s file once its digest matches: 409 when the file is
/// gone, which marks the row, and 500 `result_corrupt` when it differs. A
/// file the server removed because the capture expired or was pruned meanwhile
/// answers what a read of the capture answers now ([`missing_file`]).
async fn checked_file(
    state: &AppState,
    row: &CaptureRow,
) -> Result<std::path::PathBuf, ServerError> {
    let path = state
        .results
        .root
        .path_of(&row.id)
        .map_err(EngineError::Io)?;
    match verify(state, row).await? {
        Verified::Ok => Ok(path),
        Verified::Missing => Err(missing_file(&state.results, row).await),
        Verified::Mismatch => Err(corrupt(row)),
    }
}

/// `row`'s file checked against its recorded digest, on the blocking pool.
async fn verify(state: &AppState, row: &CaptureRow) -> Result<Verified, ServerError> {
    let digest = row.digest.ok_or_else(|| {
        ServerError::Internal("a captured result with columns has no digest".into())
    })?;
    let root = Arc::clone(&state.results.root);
    let id = row.id.clone();
    #[cfg(any(test, feature = "test-support"))]
    let pool = state.query.pool.clone();
    Ok(tokio::task::spawn_blocking(move || {
        #[cfg(any(test, feature = "test-support"))]
        pool.reach(crate::pool::seam::Seam::Verifying);
        root.verify(&id, &digest)
    })
    .await
    .map_err(|e| ServerError::from_join("capture digest check", e))?
    .map_err(EngineError::Io)?)
}

/// What a read answers when `row`'s file was not there.
///
/// The server unlinks a capture's file only after the store marked its row
/// gone (module docs, "Reads"), and a gone row never lives again. So a row
/// read back as gone answers as gone, and only a row still live after the
/// file was found missing lost it out of band: that row is marked and the
/// read answers 409.
async fn missing_file(results: &ResultsState, row: &CaptureRow) -> ServerError {
    if let Err(refusal) = still_live(results, row).await {
        return refusal;
    }
    if let Err(error) = results.store.mark_file_missing(&row.id).await {
        tracing::warn!(
            event_type = "results_mark_missing_failed",
            error = %error,
            "could not mark a capture whose file is missing"
        );
    }
    ServerError::ResultUnavailable
}

/// What a read answers when the engine could not read `row`'s file, which
/// passed its digest check: the store and the file say why, never the
/// engine's text. A row gone since answers as gone; for a live row the file
/// is checked again, and a missing or changed file answers as
/// [`checked_file`] does. A file still intact leaves the engine's own
/// failure.
async fn read_failed(state: &AppState, row: &CaptureRow, error: EngineError) -> ServerError {
    if let Err(refusal) = still_live(&state.results, row).await {
        return refusal;
    }
    match verify(state, row).await {
        Ok(Verified::Ok) => ServerError::Engine(error),
        Ok(Verified::Missing) => missing_file(&state.results, row).await,
        Ok(Verified::Mismatch) => corrupt(row),
        Err(refusal) => refusal,
    }
}

/// 500 `result_corrupt` for `row`, whose file does not match its digest.
fn corrupt(row: &CaptureRow) -> ServerError {
    // The id lets an operator find the file; the request's own failure
    // event carries the route and the request id.
    tracing::error!(
        event_type = "result_corrupt",
        result_id = %row.id,
        "a captured result's file does not match its recorded digest"
    );
    ServerError::ResultCorrupt
}

/// 403 without the trawl `query` permission.
fn require_query(verified: &VerifiedKey) -> Result<(), ServerError> {
    if verified.has_permission(Permission::Query) {
        Ok(())
    } else {
        Err(ServerError::Forbidden("insufficient permissions".into()))
    }
}

/// The one answer for an unknown id, another key's capture and a tombstone
/// past its lifetime.
fn not_found() -> ServerError {
    ServerError::NotFound("captured result not found".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::ExecutorPool;

    fn columns(spec: &[(&str, ColumnType)]) -> Vec<Column> {
        spec.iter()
            .map(|(name, ty)| Column::new(*name, ty.clone()))
            .collect()
    }

    fn text(s: &str) -> Value {
        Value::String(s.to_owned())
    }

    fn events(rows: usize, message_len: usize) -> QueryResult {
        QueryResult {
            columns: columns(&[
                ("_time", ColumnType::Timestamp),
                ("_ingested", ColumnType::Timestamp),
                ("message", ColumnType::String),
            ]),
            rows: (0..rows)
                .map(|i| {
                    vec![
                        text(&format!("2026-10-23T09:00:{:02}.000000Z", 59 - i % 60)),
                        text("2026-10-23T09:01:00.000000Z"),
                        text(&"x".repeat(message_len)),
                    ]
                })
                .collect(),
        }
    }

    fn row_len(result: &QueryResult) -> u64 {
        row_wire_len(&result.columns, &result.rows[0]).unwrap()
    }

    #[test]
    fn an_event_cut_keeps_the_rows_before_the_first_that_does_not_fit() {
        let result = events(10, 20);
        let per_row = row_len(&result);
        assert_eq!(
            cut(&result, ResultShape::Events, 100, per_row * 3).unwrap(),
            (3, per_row * 3)
        );
        assert_eq!(
            cut(&result, ResultShape::Events, 100, per_row * 3 + per_row - 1).unwrap(),
            (3, per_row * 3)
        );
        assert_eq!(
            cut(&result, ResultShape::Events, 4, u64::MAX).unwrap(),
            (4, per_row * 4)
        );
        assert_eq!(
            cut(&result, ResultShape::Events, 100, u64::MAX).unwrap(),
            (10, per_row * 10)
        );
        // A first row too large keeps none.
        assert_eq!(
            cut(&result, ResultShape::Events, 100, per_row - 1).unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn a_whole_result_over_the_cap_is_refused_with_the_hint() {
        let result = events(10, 20);
        let per_row = row_len(&result);
        let rows = cut(&result, ResultShape::Whole, 9, u64::MAX).unwrap_err();
        assert!(matches!(
            rows,
            ServerError::CaptureTooLarge(CaptureOverflow::Rows {
                rows: 10,
                max_rows: 9
            })
        ));
        assert!(
            rows.to_string()
                .contains("aggregate further or narrow the window")
        );
        let bytes = cut(&result, ResultShape::Whole, 100, per_row * 10 - 1).unwrap_err();
        assert!(matches!(
            bytes,
            ServerError::CaptureTooLarge(CaptureOverflow::WireBytes { .. })
        ));
        assert_eq!(
            cut(&result, ResultShape::Whole, 10, per_row * 10).unwrap(),
            (10, per_row * 10)
        );
    }

    #[test]
    fn a_cell_that_does_not_fit_its_type_is_a_server_fault() {
        let result = QueryResult {
            columns: columns(&[("status", ColumnType::Integer)]),
            rows: vec![vec![text("200")]],
        };
        assert!(matches!(
            cut(&result, ResultShape::Events, 10, 1000),
            Err(ServerError::Internal(_))
        ));
    }

    #[test]
    fn an_event_shape_needs_both_order_columns() {
        assert!(has_event_columns(&events(1, 1)));
        let only_time = QueryResult {
            columns: columns(&[("_time", ColumnType::Timestamp)]),
            rows: Vec::new(),
        };
        assert!(!has_event_columns(&only_time));
        assert!(!has_event_columns(&QueryResult::empty()));
    }

    #[test]
    fn facts_count_nulls_distinct_values_and_the_time_range() {
        let column = Column::new("_time", ColumnType::Timestamp);
        let cells = [
            text("2026-10-23T09:00:02.000000Z"),
            Value::Null,
            text("2026-10-23T09:00:01.000000Z"),
            text("2026-10-23T09:00:02.000000Z"),
        ];
        let facts = facts_of(&column, cells.iter()).unwrap();
        assert_eq!(facts.nulls, 1);
        assert_eq!(facts.distinct, 2);
        assert!(!facts.distinct_capped);
        let values = facts.values.unwrap();
        assert_eq!(values[0].value, text("2026-10-23T09:00:02.000000Z"));
        assert_eq!(values[0].count, 2);
        assert_eq!(values[1].count, 1);
        assert_eq!(facts.min, Some(text("2026-10-23T09:00:01.000000Z")));
        assert_eq!(facts.max, Some(text("2026-10-23T09:00:02.000000Z")));
    }

    #[test]
    fn facts_stop_counting_at_the_cap_and_list_nothing_past_five() {
        let column = Column::new("host", ColumnType::String);
        let cells: Vec<Value> = (0..9).map(|i| text(&format!("h{i}"))).collect();
        let facts = facts_of(&column, cells.iter()).unwrap();
        assert_eq!(facts.distinct, DISTINCT_CAP);
        assert!(facts.distinct_capped);
        assert!(facts.values.is_none());
        assert!(facts.min.is_none() && facts.max.is_none());

        let five: Vec<Value> = (0..5).map(|i| text(&format!("h{i}"))).collect();
        let facts = facts_of(&column, five.iter()).unwrap();
        assert_eq!(facts.distinct, 5);
        assert_eq!(facts.values.unwrap().len(), 5);
    }

    #[test]
    fn a_long_value_unlists_the_column_rather_than_being_cut() {
        let column = Column::new("message", ColumnType::String);
        // `{"type":"string","value":"…"}` is 28 bytes around the text.
        let fits = text(&"a".repeat(VALUE_TEXT_MAX - 28));
        let facts = facts_of(&column, [fits.clone()].iter()).unwrap();
        assert_eq!(facts.values.unwrap()[0].value, fits);
        let long = text(&"a".repeat(VALUE_TEXT_MAX - 27));
        let facts = facts_of(&column, [fits, long].iter()).unwrap();
        assert_eq!(facts.distinct, 2);
        assert!(facts.values.is_none());
    }

    #[test]
    fn floats_count_by_their_wire_text() {
        let column = Column::new("ratio", ColumnType::Float);
        let cells = [
            Value::Float(f64::NAN),
            Value::Float(f64::NAN),
            Value::Float(0.0),
        ];
        let facts = facts_of(&column, cells.iter()).unwrap();
        assert_eq!(facts.distinct, 2);
    }

    #[test]
    fn an_unordered_time_gives_no_range() {
        let column = Column::new("_time", ColumnType::Timestamp);
        let cells = [text("2026-10-23T09:00:02.000000Z"), text("infinity")];
        let facts = facts_of(&column, cells.iter()).unwrap();
        assert!(facts.min.is_none() && facts.max.is_none());
    }

    fn result_columns(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|name| ResultColumn {
                name: (*name).to_owned(),
                ty: ColumnType::String,
                stored_as: StoredAs::Native,
            })
            .collect()
    }

    #[test]
    fn a_projection_answers_every_match_in_stored_order() {
        let columns = result_columns(&["_time", "host", "status", "host"]);
        assert_eq!(project(&columns, None).unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(
            project(&columns, Some("host,_time")).unwrap(),
            vec![0, 1, 3]
        );
        assert_eq!(project(&columns, Some("status,status")).unwrap(), vec![2]);
        assert!(matches!(
            project(&columns, Some("host,nope")),
            Err(ServerError::BadRequest(_))
        ));
        assert!(matches!(
            project(&columns, Some("")),
            Err(ServerError::BadRequest(_))
        ));
    }

    #[test]
    fn cell_ranges_reassemble_and_never_split_a_character() {
        let text = "\"żółw → 🐢\"";
        let mut joined = String::new();
        let mut offset = 0;
        loop {
            let range = cell_range(ColumnType::String, text, offset, 5).unwrap();
            assert!(range.length <= 5 && range.length > 0, "{range:?}");
            assert_eq!(range.total_bytes, text.len() as u64);
            joined.push_str(&range.text);
            match range.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(joined, text);

        let whole = cell_range(ColumnType::String, text, 0, 1 << 20).unwrap();
        assert_eq!(whole.text, text);
        assert_eq!(whole.next_offset, None);
        let end = cell_range(ColumnType::String, text, text.len() as u64, 8).unwrap();
        assert_eq!(end.text, "");
        assert_eq!(end.next_offset, None);
        // Inside `ż`, and past the end.
        assert!(matches!(
            cell_range(ColumnType::String, text, 2, 8),
            Err(ServerError::BadRequest(_))
        ));
        assert!(matches!(
            cell_range(ColumnType::String, text, text.len() as u64 + 1, 8),
            Err(ServerError::BadRequest(_))
        ));
    }

    /// A selection that keeps every other row before the cut and reports,
    /// after it, how many rows it saw and the messages the cut kept.
    struct EveryOther {
        kept: Arc<parking_lot::Mutex<Vec<String>>>,
        seen: u64,
    }

    impl Selection for EveryOther {
        fn before_cut(
            &mut self,
            result: &mut QueryResult,
            _: &PhaseClock,
        ) -> Result<(), ServerError> {
            self.seen = u64::try_from(result.rows.len()).unwrap();
            let mut index = 0;
            result.rows.retain(|_| {
                index += 1;
                index % 2 == 1
            });
            Ok(())
        }

        fn after_cut(&mut self, kept: &QueryResult) -> Result<Option<Correlation>, ServerError> {
            let message = kept
                .columns
                .iter()
                .position(|column| column.name == "message")
                .unwrap();
            *self.kept.lock() = kept
                .rows
                .iter()
                .map(|row| match &row[message] {
                    Value::String(text) => text.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            Ok(Some(sightings(self.seen, kept.rows.len())))
        }
    }

    /// A selection that renames a column, which no selection may do.
    struct Renames;

    impl Selection for Renames {
        fn before_cut(
            &mut self,
            result: &mut QueryResult,
            _: &PhaseClock,
        ) -> Result<(), ServerError> {
            result.columns[0].name = "renamed".into();
            Ok(())
        }

        fn after_cut(&mut self, _: &QueryResult) -> Result<Option<Correlation>, ServerError> {
            panic!("a refused selection never reaches the cut")
        }
    }

    fn sightings(seen: u64, kept: usize) -> Correlation {
        Correlation::Sightings(trawl_api::results::SightingsFacts {
            literal: "m".into(),
            matches_total: seen,
            captured: u64::try_from(kept).unwrap(),
            groups: Vec::new(),
            fields: Vec::new(),
            raw_only_rows: 0,
            denominators: trawl_api::results::SightingsDenominators {
                field_counts_over: u64::try_from(kept).unwrap(),
                service_counts_over: seen,
            },
            follow_ups: Vec::new(),
        })
    }

    /// A pool over a hot buffer holding six events of `prod/svc`, one a
    /// second from 00:05:00, with no stored data, and a results state on
    /// the test's database whose captures keep at most two rows.
    fn lane_fixture(db: sqlx::PgPool) -> (tempfile::TempDir, ExecutorPool, Arc<ResultsState>) {
        use crate::bus::IngestBatch;
        use crate::config::CaptureConfig;
        use crate::hot_buffer::{HotBuffer, HotBufferConfig};
        use crate::results::{ResultsClock, charge::CaptureGate};
        use crate::store::ResultsStore;

        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let event = |i: usize| {
            let mut event = serde_json::Map::new();
            event.insert("_time".into(), format!("2026-01-03T00:05:0{i}Z").into());
            event.insert("_ingested".into(), "2026-01-03T00:06:00Z".into());
            event.insert("service".into(), "svc".into());
            event.insert("message".into(), format!("m{i}").into());
            event
        };
        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 1 << 20,
        }));
        hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: "prod/svc_hot".into(),
            service: "svc".into(),
            byte_size: 64,
            events: (0..6).map(event).collect(),
        }));
        let pool = ExecutorPool::for_test(data.to_str().unwrap().to_owned(), 1, 100_000, Some(hot))
            .unwrap();

        let root = Arc::new(
            crate::results::root::ResultsRoot::open_apart_from(
                &dir.path().join("data.results"),
                &[],
            )
            .unwrap(),
        );
        let capture = CaptureConfig {
            max_rows: 2,
            ..CaptureConfig::default()
        };
        let store = ResultsStore::new(db);
        let clock = ResultsClock::fixed("2026-01-03T00:10:00Z".parse().unwrap());
        let gate = CaptureGate::new(&capture, store.clone(), Arc::clone(&root), clock.clone());
        let results = Arc::new(ResultsState {
            root,
            store,
            capture,
            clock,
            gate,
            in_flight: Arc::default(),
        });
        (dir, pool, results)
    }

    /// The bounds a capture of the fixture's events resolves to.
    fn fixture_bounds() -> bounds::Resolved {
        bounds::Resolved {
            executed: "* | sort -_time, -_ingested".into(),
            source: Source::Live {
                earliest: "2026-01-03T00:00:00Z".parse().unwrap(),
                latest: "2026-01-03T01:00:00Z".parse().unwrap(),
            },
            shape: Shape::Events,
        }
    }

    /// Run the fixture's capture through the real lane with `selection`.
    async fn through_the_lane(
        pool: &ExecutorPool,
        results: &ResultsState,
        id: &str,
        selection: Box<dyn Selection>,
    ) -> Result<Captured, ServerError> {
        let resolved = fixture_bounds();
        let Source::Live { earliest, latest } = resolved.source else {
            unreachable!()
        };
        let keep = Keep {
            root: Arc::clone(&results.root),
            in_flight: Arc::clone(&results.in_flight),
            id: id.to_owned(),
            shape: resolved.shape,
            selection,
            max_rows: results.capture.max_rows,
            max_wire_bytes: results.capture.max_wire_bytes,
            max_file_bytes: results.capture.max_file_bytes,
        };
        pool.capture(
            pool.allocate_query_id(),
            &resolved.executed,
            CaptureSource::Window {
                start: earliest,
                end: latest,
                max_bytes: u64::MAX,
            },
            RowCap::Refuse(100),
            Deadline::after(Duration::from_secs(60)),
            0,
            WorkContext::key(WorkKind::Capture, 7),
            Box::new(move |input: CaptureInput<'_>| keep.run(input)),
        )
        .await
    }

    /// Commit `captured` as key 7's capture of `kind` and read it back.
    async fn stored(
        results: &ResultsState,
        id: &str,
        kind: CaptureKind,
        mut captured: Captured,
    ) -> CaptureRow {
        let new = new_capture(
            results,
            7,
            id.to_owned(),
            kind,
            "*".into(),
            &fixture_bounds(),
            &captured,
        )
        .unwrap();
        assert_eq!(
            results.store.insert(&new).await.unwrap(),
            crate::store::CaptureInsert::Inserted
        );
        if let Some((file, _)) = captured.file.take() {
            file.commit();
        }
        let row = results.store.get(id, 7).await.unwrap().expect("the row");
        assert_eq!(row, committed_row(new), "the row as the commit answers it");
        row
    }

    /// The messages of a stored capture's file, in stored order.
    fn stored_messages(
        pool: &ExecutorPool,
        results: &ResultsState,
        row: &CaptureRow,
    ) -> Vec<Value> {
        let message = row
            .columns
            .iter()
            .position(|column| column.name == "message")
            .unwrap();
        let types: Vec<ColumnType> = row.columns.iter().map(|c| c.ty.clone()).collect();
        let stored_as: Vec<FileStoredAs> = row
            .columns
            .iter()
            .map(|c| file_stored_as(c.stored_as))
            .collect();
        pool.detached_executor()
            .unwrap()
            .read_capture_page(
                &results.root.path_of(&row.id).unwrap(),
                &types,
                &stored_as,
                &[message],
                0,
                100,
            )
            .unwrap()
            .into_iter()
            .map(|mut row| row.remove(0))
            .collect()
    }

    /// A selection runs inside the real lane around the cut: before it,
    /// over every executed row, newest first; after it, over the rows the
    /// cut kept, which are the rows the file holds. Its facts reach the
    /// row with the capture's kind, and a search's wire is spelled as it
    /// was before the correlation reads.
    #[sqlx::test]
    async fn a_selection_runs_around_the_cut_and_its_facts_are_stored(db: sqlx::PgPool) {
        let (_dir, pool, results) = lane_fixture(db);

        let kept = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let selection = EveryOther {
            kept: Arc::clone(&kept),
            seen: 0,
        };
        let id = new_result_id();
        let captured = through_the_lane(&pool, &results, &id, Box::new(selection))
            .await
            .unwrap();
        // m5..m0 executed; m5, m3 and m1 selected; the cut keeps two.
        assert_eq!(*kept.lock(), ["m5", "m3"]);
        assert_eq!((captured.total_rows, captured.kept_rows), (3, 2));
        assert_eq!(captured.correlation, Some(sightings(6, 2)));
        let row = stored(&results, &id, CaptureKind::Sightings, captured).await;
        assert_eq!(
            stored_messages(&pool, &results, &row),
            [text("m5"), text("m3")]
        );
        let wire = row.to_wire();
        assert_eq!(wire.kind, CaptureKind::Sightings);
        assert_eq!(wire.correlation, Some(sightings(6, 2)));
        assert_eq!((wire.captured_rows, wire.total_rows), (2, 3));
        assert!(wire.truncated);

        let id = new_result_id();
        let captured = through_the_lane(&pool, &results, &id, Box::new(NoSelection))
            .await
            .unwrap();
        assert_eq!((captured.total_rows, captured.kept_rows), (6, 2));
        assert_eq!(captured.correlation, None);
        let row = stored(&results, &id, CaptureKind::Search, captured).await;
        assert_eq!(
            stored_messages(&pool, &results, &row),
            [text("m5"), text("m4")]
        );
        let wire = serde_json::to_value(row.to_wire()).unwrap();
        let keys: BTreeSet<&str> = wire
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "id",
                "query",
                "executed_query",
                "source",
                "shape",
                "earliest",
                "latest",
                "created_at",
                "expires_at",
                "state",
                "columns",
                "captured_rows",
                "total_rows",
                "wire_bytes",
                "file_bytes",
                "truncated",
                "column_facts",
                "format_version",
            ]),
            "a search's wire names exactly the fields it named before the correlation reads"
        );
    }

    /// A selection that changes the columns fails the capture as the
    /// server's fault, and nothing is published.
    #[sqlx::test]
    async fn a_selection_that_changes_the_columns_fails_the_capture(db: sqlx::PgPool) {
        let (_dir, pool, results) = lane_fixture(db);
        let id = new_result_id();
        let refused = through_the_lane(&pool, &results, &id, Box::new(Renames)).await;
        assert!(
            matches!(refused, Err(ServerError::Internal(_))),
            "{:?}",
            refused.err()
        );
        assert!(results.root.open_file(&id).unwrap().is_none());
        assert!(results.in_flight.snapshot().is_empty());
    }

    #[test]
    fn result_ids_are_32_lowercase_hex_and_fresh() {
        let a = new_result_id();
        let b = new_result_id();
        assert!(root::is_result_id(&a), "{a}");
        assert_ne!(a, b);
    }
}
