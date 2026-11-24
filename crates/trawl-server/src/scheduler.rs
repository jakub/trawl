// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background scheduler for periodic query execution (ADR-0018).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use fleet_auth::KeyStore;

use crate::config::SchedulerConfig;
use crate::error::{CauseKind, ServerError};
use crate::policy::{Permission, TrawlAuthz as _};
use crate::pool::{ExecutorPool, RunLease};
use crate::publication::CorpusUnsettled;
use crate::query_timing::{Outcome, TimingGuard};
use crate::report_window::{ReportWindow, format_window_bound, truncate_to_micros};
use crate::results::ResultsState;
use crate::store::{
    ClaimedRun, DUE_SCHEDULE_PAGE, DueClaim, FinishOutcome, FlipOutcome, RunStatus, ScheduleStore,
};

pub fn spawn_scheduler(
    schedule_store: ScheduleStore,
    key_store: KeyStore,
    pool: ExecutorPool,
    results: Arc<ResultsState>,
    config: SchedulerConfig,
    timeout_secs: u64,
    shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(scheduler_loop(
        schedule_store,
        key_store,
        pool,
        results,
        config,
        timeout_secs,
        shutdown_rx,
    ))
}

async fn scheduler_loop(
    schedule_store: ScheduleStore,
    key_store: KeyStore,
    pool: ExecutorPool,
    results: Arc<ResultsState>,
    config: SchedulerConfig,
    timeout_secs: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let poll_interval = Duration::from_secs(config.poll_interval_secs);
    let mut tick = tokio::time::interval(poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut retention_counter: u64 = 0;
    let retention_interval = 3600 / config.poll_interval_secs.max(1);

    tracing::info!(
        event_type = "scheduler_started",
        poll_interval_secs = config.poll_interval_secs,
        "scheduler started"
    );

    let mut corpus = CorpusWatch::default();

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown_rx.changed() => {
                tracing::info!(event_type = "scheduler_shutdown", "scheduler shutting down");
                return;
            }
        }

        // Only the log reads the gate here; `poll_and_execute` makes its
        // own decision, so the two never have to agree.
        corpus.observe(pool.publication().unsettled());

        // ONE clock reading per tick, truncated to the microsecond every
        // stored bound shares. Sampling per schedule would let two schedules in
        // the same poll disagree about which boundary has passed.
        let now = truncate_to_micros(chrono::Utc::now());
        // Dropping the handles detaches the executions: each records its own
        // outcome through `finish_run`, so the loop never waits on one.
        drop(
            poll_and_execute(
                &schedule_store,
                &key_store,
                &pool,
                &config,
                timeout_secs,
                now,
            )
            .await,
        );

        retention_counter += 1;
        if retention_counter >= retention_interval {
            retention_counter = 0;
            match schedule_store
                .delete_old_runs(config.report_retention_days, config.max_runs_per_schedule)
                .await
            {
                Ok((_count, paths)) => {
                    // Paths from the DB are relative (e.g. "scheduled/foo/run_1.parquet"),
                    // resolved against the data directory by remove_result_file.
                    let base = pool.base_dir();
                    for path in paths {
                        remove_result_file(base, &path);
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        event_type = "scheduler_retention_error",
                        error = %e,
                        "failed to clean up old report runs"
                    );
                    let fault = ServerError::from(e);
                    tracing::warn!(
                        event_type = "scheduler_retention_error",
                        error_class = fault.error_class(),
                        cause_kind = fault.cause_kind().as_str(),
                        "failed to clean up old report runs"
                    );
                }
            }
            // Captured results retire on their own clock, which tests
            // move; the sweep logs its counts and its failures.
            crate::results::sweep::sweep(&results, results.clock.now()).await;
        }
    }
}

#[derive(Debug, Default)]
struct CorpusWatch {
    logged: Option<CorpusUnsettled>,
}

impl CorpusWatch {
    fn observe(&mut self, now: Option<CorpusUnsettled>) -> bool {
        if now == self.logged {
            return false;
        }
        if let Some(reason) = now {
            tracing::info!(
                event_type = "scheduler_paused",
                reason = reason.label(),
                "corpus unsettled; the scheduler claims no run until it settles"
            );
        } else {
            tracing::info!(
                event_type = "scheduler_resumed",
                "corpus settled; the scheduler claims due runs again"
            );
        }
        self.logged = now;
        true
    }
}

pub async fn poll_and_execute(
    schedule_store: &ScheduleStore,
    key_store: &KeyStore,
    pool: &ExecutorPool,
    config: &SchedulerConfig,
    timeout_secs: u64,
    now: DateTime<Utc>,
) -> Vec<JoinHandle<()>> {
    let mut tick = Tick::default();
    claim_due(
        schedule_store,
        key_store,
        pool,
        config,
        timeout_secs,
        now,
        &mut tick,
    )
    .await;
    // Every exit of the claim pass, an early one included, comes back here,
    // so each unusable key is logged once per tick whichever way it ended.
    for (&key_id, &schedules) in &tick.unusable {
        tracing::info!(
            event_type = "scheduler_skip",
            key_id,
            schedules,
            "skipped this key's due schedules: it is revoked, expired, or lacks query/saved-query permission"
        );
    }
    tick.spawned
}

#[derive(Default)]
struct Tick {
    spawned: Vec<JoinHandle<()>>,
    unusable: BTreeMap<i64, u64>,
}

async fn claim_due(
    schedule_store: &ScheduleStore,
    key_store: &KeyStore,
    pool: &ExecutorPool,
    config: &SchedulerConfig,
    timeout_secs: u64,
    now: DateTime<Utc>,
    tick: &mut Tick,
) {
    if pool.publication().unsettled().is_some() {
        return;
    }

    // With no lease free the tick could claim nothing, so it lists
    // nothing. Only an observation: each claim below takes its own lease.
    if pool.scheduled_leases_available() == 0 {
        return;
    }

    // Each page starts after the last row the previous page LISTED. No
    // schedule returns in the same tick through this tick's own claims: a
    // claim moves the cursor past `now` or leaves it where it was, behind
    // that row. A concurrent cursor write, such as a manual run finishing,
    // can list one again, and its claim reads it again under its lock.
    let mut after = None;
    loop {
        let page = match schedule_store
            .list_due_schedule_page(now, after, DUE_SCHEDULE_PAGE)
            .await
        {
            Ok(page) => page,
            Err(e) => {
                tracing::debug!(
                    event_type = "scheduler_error",
                    error = %e,
                    "failed to list due schedules"
                );
                let fault = ServerError::from(e);
                tracing::error!(
                    event_type = "scheduler_error",
                    error_class = fault.error_class(),
                    cause_kind = fault.cause_kind().as_str(),
                    "failed to list due schedules"
                );
                return;
            }
        };
        let last_page = page.len() < DUE_SCHEDULE_PAGE;
        after = page.last().map(|due| (due.next_fire_at, due.id));

        for due in page {
            if let Some(skipped) = tick.unusable.get_mut(&due.key_id) {
                *skipped += 1;
                continue;
            }
            #[cfg(any(test, feature = "test-support"))]
            pool.reach(crate::pool::seam::Seam::KeyChecked);
            match owning_key(key_store, due.id, due.key_id).await {
                KeyCheck::Usable => {}
                KeyCheck::Unusable => {
                    tick.unusable.insert(due.key_id, 1);
                    continue;
                }
                KeyCheck::LookupFailed => continue,
            }

            let Some(lease) = pool.try_lease_scheduled() else {
                return;
            };
            let Some((claimed, lease)) = claim_leased(
                schedule_store,
                due.id,
                now,
                config.max_catchup_intervals,
                lease,
            )
            .await
            else {
                continue;
            };

            if let Some(window) = &claimed.window {
                note_truncated_window(due.id, claimed.run_id, window);
            }

            // Execute the text the claim RESOLVED and stored: with a window
            // it differs from the saved DSL by the very bounds the run row
            // claims to cover.
            let ClaimedRun {
                run_id,
                query_name,
                resolved_query,
                ..
            } = claimed;
            let store = schedule_store.clone();
            let pool = pool.clone();

            tick.spawned.push(tokio::spawn(async move {
                execute_scheduled_query(
                    store,
                    pool,
                    run_id,
                    &resolved_query,
                    &query_name,
                    timeout_secs,
                    lease,
                )
                .await;
            }));
        }

        if last_page {
            return;
        }
    }
}

async fn claim_leased(
    schedule_store: &ScheduleStore,
    schedule_id: i64,
    now: DateTime<Utc>,
    max_catchup_intervals: u32,
    lease: RunLease,
) -> Option<(ClaimedRun, RunLease)> {
    match schedule_store
        .claim_due_run(schedule_id, now, max_catchup_intervals, &lease)
        .await
    {
        Ok(DueClaim::Started(claimed)) => Some((claimed, lease)),
        Ok(
            DueClaim::NotDue
            | DueClaim::Advanced
            | DueClaim::AlreadyRunning
            | DueClaim::MaxRunsReached,
        ) => None,
        Err(e) => {
            tracing::error!(
                event_type = "scheduler_error",
                schedule_id,
                error_class = e.class(),
                "failed to claim a due run; the schedule's fire cursor is unchanged"
            );
            None
        }
    }
}

/// One spelling for both claimants, the scheduler tick and a manual run:
/// an operator watching `trawl_scheduler_window_truncated_total` has to see
/// every uncovered span, whichever door the run came through.
pub(crate) fn note_truncated_window(schedule_id: i64, run_id: i64, window: &ReportWindow) {
    if !window.truncated {
        return;
    }
    metrics::counter!(crate::metrics::SCHEDULER_WINDOW_TRUNCATED_TOTAL).increment(1);
    // The bounds are instants, not operator text, so they are safe as event
    // fields. They are fields and never metric labels: an instant is
    // unbounded cardinality.
    tracing::warn!(
        event_type = "scheduler_window_truncated",
        schedule_id,
        run_id,
        window_start = %format_window_bound(window.start),
        window_end = %format_window_bound(window.end),
        "report window clamped to max_catchup_intervals; the span before its start stays uncovered"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyCheck {
    Usable,
    Unusable,
    /// An outage is no proof that the key is gone.
    LookupFailed,
}

async fn owning_key(key_store: &KeyStore, schedule_id: i64, key_id: i64) -> KeyCheck {
    let live = match key_store.get_live_key_by_id(key_id).await {
        Ok(live) => live,
        Err(e) => {
            tracing::debug!(
                event_type = "scheduler_error",
                schedule_id,
                key_id,
                error = %e,
                "failed to check key liveness; skipping schedule this tick"
            );
            // Classified without `ServerError::from`, which would write an
            // `auth.backend` event for a failure this tick swallows.
            let (error_class, cause_kind) = match crate::error::auth_backend_cause(&e) {
                Some((kind, _, _)) => (ServerError::AuthBackend(kind).error_class(), kind),
                None => ("internal", CauseKind::Unknown),
            };
            tracing::warn!(
                event_type = "scheduler_error",
                schedule_id,
                key_id,
                error_class,
                cause_kind = cause_kind.as_str(),
                "failed to check key liveness; skipping schedule this tick"
            );
            return KeyCheck::LookupFailed;
        }
    };
    if live.is_some_and(|k| {
        k.has_permission(Permission::Query) && k.has_permission(Permission::SavedQuery)
    }) {
        KeyCheck::Usable
    } else {
        KeyCheck::Unusable
    }
}

pub(crate) async fn execute_scheduled_query(
    schedule_store: ScheduleStore,
    pool: ExecutorPool,
    run_id: i64,
    query: &str,
    _query_name: &str,
    timeout_secs: u64,
    lease: RunLease,
) {
    let lease = lease.into_hold();
    let start = std::time::Instant::now();
    // One budget for this attempt, stamped before the first pool wait
    // (ADR-0024).
    let deadline = crate::deadline::Deadline::after(Duration::from_secs(timeout_secs));

    // The run's timing account (ADR-0046), keyed by an id allocated before
    // the admission check so a refused run is accounted for too.
    let query_id = pool.allocate_query_id();
    let timing = TimingGuard::for_run(query_id, run_id, start);

    let checked = {
        let _check = timing
            .clock()
            .guard(trawl_engine::timing::QueryPhase::DslCheck);
        crate::admission::admit_holding(&pool, query, deadline, None, Some(Arc::clone(&lease)))
            .await
    };
    let result = if let Err(refusal) = checked {
        Err(refusal)
    } else {
        // No debug capture, UTC timestamps.
        let work = crate::pool::WorkContext::system(crate::pool::WorkKind::Scheduled)
            .with_timing(timing.timing().clone())
            .holding(Arc::clone(&lease));
        pool.execute(query_id, query, deadline, false, work)
            .await
            .result
    };

    // The account closes here, before the result is persisted: the window
    // is the query's, and the run's duration is measured to the same
    // instant.
    let end = std::time::Instant::now();
    timing.emit_complete(
        result
            .as_ref()
            .map_or_else(Outcome::of_error, |_| Outcome::Success),
        end,
    );
    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = end.saturating_duration_since(start).as_millis() as u64;

    match result {
        Ok(query_result) => {
            let row_count = query_result.rows.len();

            #[cfg(any(test, feature = "test-support"))]
            reach_run_writing(&pool);

            let (result_path, result_data) =
                match write_result_parquet(&pool, run_id, &query_result) {
                    Ok(persisted) => persisted,
                    Err(reason) => {
                        fail_unpersisted_run(
                            &schedule_store,
                            run_id,
                            query_id,
                            duration_ms,
                            &reason,
                        )
                        .await;
                        return;
                    }
                };

            finish_run_or_recover(
                &schedule_store,
                pool.base_dir(),
                run_id,
                duration_ms,
                row_count,
                result_data.as_deref(),
                result_path.as_deref(),
            )
            .await;

            tracing::info!(
                event_type = "scheduled_query_completed",
                run_id,
                query_id,
                duration_ms,
                row_count,
                result_path = result_path.as_deref().unwrap_or("(blob)"),
                "scheduled query completed"
            );
        }
        Err(e) => {
            let status = record_failed_run(&schedule_store, run_id, duration_ms, &e).await;

            tracing::warn!(
                event_type = "scheduled_query_failed",
                run_id,
                query_id,
                duration_ms,
                status = status.as_str(),
                error_class = e.error_class(),
                "scheduled query failed"
            );
        }
    }
}

/// The run frame's side of [`crate::pool::seam::Seam::RunWriting`]. A hold
/// parks the thread, and the frame runs on a runtime worker, so on a
/// multi-thread runtime it parks inside `block_in_place`: the worker's
/// other tasks move to another thread instead of waiting behind the hold.
#[cfg(any(test, feature = "test-support"))]
fn reach_run_writing(pool: &ExecutorPool) {
    let reach = || pool.reach(crate::pool::seam::Seam::RunWriting);
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(reach);
    } else {
        reach();
    }
}

async fn record_failed_run(
    schedule_store: &ScheduleStore,
    run_id: i64,
    duration_ms: u64,
    error: &ServerError,
) -> RunStatus {
    let status = if matches!(error, ServerError::Timeout) {
        RunStatus::Timeout
    } else {
        RunStatus::Error
    };
    finish_failed_run(
        schedule_store,
        run_id,
        status,
        duration_ms,
        &error.safe_message(),
    )
    .await;
    status
}

/// Fail a run whose result has neither a file nor a blob. A success here
/// would be one nothing can read, which `run=latest` would then resolve
/// and refuse to look past (ADR-0018 ruling 13).
async fn fail_unpersisted_run(
    schedule_store: &ScheduleStore,
    run_id: i64,
    query_id: u64,
    duration_ms: u64,
    reason: &str,
) {
    finish_failed_run(
        schedule_store,
        run_id,
        RunStatus::Error,
        duration_ms,
        &format!("the run's result could not be stored: {reason}"),
    )
    .await;
    tracing::warn!(
        event_type = "scheduled_query_failed",
        run_id,
        query_id,
        duration_ms,
        status = RunStatus::Error.as_str(),
        error_class = "result_persistence",
        "scheduled query failed"
    );
}

/// A store error is logged; the run then stays `running` until
/// boot-time cleanup.
async fn finish_failed_run(
    schedule_store: &ScheduleStore,
    run_id: i64,
    status: RunStatus,
    duration_ms: u64,
    error_msg: &str,
) {
    if let Err(e) = schedule_store
        .finish_run(
            run_id,
            status,
            duration_ms,
            None,
            Some(error_msg),
            None,
            None,
        )
        .await
    {
        log_store_fault(run_id, e, "failed to finish run");
    }
}

fn log_store_fault(run_id: i64, err: crate::store::StoreError, what: &'static str) {
    tracing::debug!(event_type = "scheduler_error", run_id, error = %err, "{what}");
    let fault = ServerError::from(err);
    tracing::error!(
        event_type = "scheduler_error",
        run_id,
        error_class = fault.error_class(),
        cause_kind = fault.cause_kind().as_str(),
        "{what}"
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_run_or_recover(
    schedule_store: &ScheduleStore,
    base_dir: &str,
    run_id: i64,
    duration_ms: u64,
    row_count: usize,
    result_data: Option<&[u8]>,
    result_path: Option<&str>,
) {
    match schedule_store
        .finish_run(
            run_id,
            RunStatus::Success,
            duration_ms,
            Some(row_count),
            None,
            result_data,
            result_path,
        )
        .await
    {
        Ok(FinishOutcome::Persisted) => {}
        Ok(FinishOutcome::RunDeleted) => {
            if let Some(relative) = result_path
                && remove_result_file(base_dir, relative)
            {
                tracing::info!(
                    event_type = "scheduler_orphan_cleanup",
                    run_id,
                    path = %relative,
                    "run was deleted mid-flight; removed orphaned parquet result"
                );
            }
        }
        Err(e) => {
            // An ambiguous commit: the COMMIT can land server-side while the client's
            // ack is lost. So the parquet must not be deleted here — recovery flips
            // the row only while it is still 'running'.
            recover_ambiguous_finish(
                schedule_store,
                base_dir,
                run_id,
                duration_ms,
                result_path,
                e,
            )
            .await;
        }
    }
}

const RESULT_PERSISTENCE_FAILED: &str = "result persistence failed";

/// Flip the row to `error` only while it is still `running`. If the success
/// commit actually landed, the flip matches zero rows and the committed
/// result is preserved rather than unlinking the file it points at.
pub(crate) async fn recover_ambiguous_finish(
    schedule_store: &ScheduleStore,
    base_dir: &str,
    run_id: i64,
    duration_ms: u64,
    result_path: Option<&str>,
    err: crate::store::StoreError,
) {
    tracing::debug!(
        event_type = "scheduler_error",
        run_id,
        error = %err,
        "failed to finish run (ambiguous commit); recovering via guarded flip"
    );
    let fault = ServerError::from(err);
    tracing::error!(
        event_type = "scheduler_error",
        run_id,
        error_class = fault.error_class(),
        cause_kind = fault.cause_kind().as_str(),
        result_path = result_path.unwrap_or("(blob)"),
        "failed to finish run (ambiguous commit); recovering via guarded flip"
    );
    match schedule_store
        .fail_run_if_running(run_id, duration_ms, RESULT_PERSISTENCE_FAILED)
        .await
    {
        Ok(FlipOutcome::FlippedToError) => {
            if let Some(relative) = result_path
                && remove_result_file(base_dir, relative)
            {
                tracing::info!(
                    event_type = "scheduler_orphan_cleanup",
                    run_id,
                    path = %relative,
                    "ambiguous commit did not land; removed orphaned parquet result"
                );
            }
        }
        Ok(FlipOutcome::NotRunning) => {}
        Err(e2) => log_store_fault(
            run_id,
            e2,
            "failed to flip wedged run to error; run stays stuck until restart",
        ),
    }
}

/// A file that was already gone is not a failure and does not log like one:
/// it is the ordinary state after a `data_dir` repoint. A `NotFound` below a
/// dangling link, a permission error or an I/O error keeps the warning,
/// because the file may still be there.
pub(crate) fn remove_result_file(base_dir: &str, relative: &str) -> bool {
    let full = format!("{}/{relative}", base_dir.trim_end_matches('/'));
    match std::fs::remove_file(&full) {
        Ok(()) => true,
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && crate::env_dirs::is_absent(std::path::Path::new(&full)) =>
        {
            tracing::debug!(
                event_type = "result_file_already_absent",
                path = %full,
                error = %e,
                "no parquet result file to delete: it is already gone"
            );
            tracing::info!(
                event_type = "result_file_already_absent",
                path = %relative,
                "no parquet result file to delete: it is already gone"
            );
            false
        }
        Err(e) => {
            tracing::debug!(
                event_type = "result_file_cleanup_error",
                path = %full,
                error = %e,
                "failed to delete parquet result file"
            );
            tracing::warn!(
                event_type = "result_file_cleanup_error",
                path = %relative,
                error_kind = %e.kind(),
                "failed to delete parquet result file"
            );
            false
        }
    }
}

/// The file is the ordinary representation, zero-row results included:
/// `write_query_result_to_parquet` stores every column in an exact type,
/// so an empty file still carries the run's schema, and `from saved` reads
/// it like any other run. The blob is kept for what parquet cannot hold
/// exactly and for failures:
///
/// - a result with no columns, the answer when the run's own source matched
///   no file at all;
/// - a `mixed` column, or an integer column that needs both a negative and
///   a value above `i64::MAX` (the writer refuses both);
/// - a failed write, directory, executor or rename.
///
/// `get_report_run` serves the blob as the same typed result. `from saved`
/// resolves a zero-column blob to a source with no rows and refuses the
/// rest by name.
fn write_result_parquet(
    pool: &ExecutorPool,
    run_id: i64,
    result: &trawl_api::value::QueryResult,
) -> Result<(Option<String>, Option<Vec<u8>>), String> {
    let base = pool.base_dir().trim_end_matches('/');
    let relative = result_relative_path(run_id);
    let full_path = format!("{base}/{relative}");
    let temp_path = format!("{full_path}.tmp");

    if let Err(e) = std::fs::create_dir_all(format!("{base}/{}", trawl_config::SCHEDULED_DIR_NAME))
    {
        tracing::debug!(
            event_type = "scheduler_parquet_error",
            run_id,
            error = %e,
            "failed to create scheduled result directory, falling back to blob"
        );
        tracing::warn!(
            event_type = "scheduler_parquet_error",
            run_id,
            error_kind = %e.kind(),
            "failed to create scheduled result directory, falling back to blob"
        );
        return zstd_fallback(result);
    }

    // A clone of the query database, so the write spills into the query
    // scratch directory under the query budgets (ADR-0056).
    let executor = match pool.detached_executor() {
        Ok(e) => e,
        Err(e) => {
            log_parquet_engine_fault(
                run_id,
                e,
                "failed to create executor for parquet write, falling back to blob",
            );
            return zstd_fallback(result);
        }
    };

    let temp = std::path::Path::new(&temp_path);
    let final_path = std::path::Path::new(&full_path);
    match executor.write_query_result_to_parquet(result, temp) {
        Ok(_) => {}
        // Not a failure: the result's shape has no exact parquet form, and
        // the blob is its representation. The reason names a column, which
        // is query or sender content, so it stays at DEBUG.
        Err(e @ trawl_engine::error::EngineError::NotParquetStorable { .. }) => {
            tracing::debug!(
                event_type = "scheduler_result_blob",
                run_id,
                reason = %e,
                "the run's result has no exact parquet form"
            );
            tracing::info!(
                event_type = "scheduler_result_blob",
                run_id,
                "storing the run's result as a blob"
            );
            return zstd_fallback(result);
        }
        Err(e) => {
            log_parquet_engine_fault(
                run_id,
                e,
                "failed to write parquet result, falling back to blob",
            );
            let _ = std::fs::remove_file(temp);
            return zstd_fallback(result);
        }
    }

    if let Err(e) = std::fs::rename(temp, final_path) {
        tracing::debug!(
            event_type = "scheduler_parquet_error",
            run_id,
            error = %e,
            "failed to rename parquet temp file, falling back to blob"
        );
        tracing::warn!(
            event_type = "scheduler_parquet_error",
            run_id,
            error_kind = %e.kind(),
            "failed to rename parquet temp file, falling back to blob"
        );
        let _ = std::fs::remove_file(temp);
        return zstd_fallback(result);
    }

    Ok((Some(relative), None))
}

fn result_relative_path(run_id: i64) -> String {
    format!("{}/run_{run_id}.parquet", trawl_config::SCHEDULED_DIR_NAME)
}

fn log_parquet_engine_fault(run_id: i64, err: trawl_engine::error::EngineError, what: &str) {
    tracing::debug!(event_type = "scheduler_parquet_error", run_id, error = %err, "{what}");
    let fault = ServerError::from(err);
    tracing::warn!(
        event_type = "scheduler_parquet_error",
        run_id,
        error_class = fault.error_class(),
        cause_kind = fault.cause_kind().as_str(),
        "{what}"
    );
}

fn zstd_fallback(
    result: &trawl_api::value::QueryResult,
) -> Result<(Option<String>, Option<Vec<u8>>), String> {
    let json = serde_json::to_vec(result).map_err(|e| format!("encoding failed: {e}"))?;
    let blob =
        zstd::encode_all(json.as_slice(), 3).map_err(|e| format!("compression failed: {e}"))?;
    Ok((None, Some(blob)))
}

pub(crate) fn decode_result_blob(blob: Option<Vec<u8>>) -> Option<trawl_api::value::QueryResult> {
    let compressed = blob?;
    let decompressed = zstd::decode_all(compressed.as_slice())
        .inspect_err(|e| {
            // The io::Error carries zstd's codec wording, not an I/O
            // failure, so no io cause covers it.
            tracing::debug!(
                event_type = "run_result_blob_zstd_decode_failed",
                error = %e,
                "failed to zstd-decode a report run's result blob"
            );
            tracing::warn!(
                event_type = "run_result_blob_zstd_decode_failed",
                error_class = "internal",
                cause_kind = CauseKind::Unknown.as_str(),
                "failed to zstd-decode a report run's result blob"
            );
        })
        .ok()?;
    serde_json::from_slice::<trawl_api::value::QueryResult>(&decompressed)
        .inspect_err(|e| {
            // serde's text can quote the stored result's values, and no
            // typed cause covers it.
            tracing::debug!(
                event_type = "run_result_blob_deserialize_failed",
                error = %e,
                "failed to deserialize a report run's result blob"
            );
            tracing::warn!(
                event_type = "run_result_blob_deserialize_failed",
                error_class = "internal",
                cause_kind = CauseKind::Unknown.as_str(),
                "failed to deserialize a report run's result blob"
            );
        })
        .ok()
}

#[cfg(test)]
mod pg_tests {
    use sqlx::PgPool;

    use super::{
        RESULT_PERSISTENCE_FAILED, finish_run_or_recover, record_failed_run,
        recover_ambiguous_finish,
    };
    use crate::error::ServerError;
    use crate::store::{RunClaim, RunStatus, SavedQueryStore, ScheduleStore, StoreError};

    pub(super) async fn seed_run(pool: &PgPool, name: &str) -> (ScheduleStore, i64, i64) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let sched_store = ScheduleStore::new(pool.clone());
        let saved = saved_store.create(1, name, "q").await.unwrap();
        let sched = sched_store
            .create_schedule(saved.id, 1, 300, None, None, 0, chrono::Utc::now())
            .await
            .unwrap();
        let rid = match sched_store
            .claim_run(sched.id, saved.id, "q", None)
            .await
            .unwrap()
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };
        (sched_store, saved.id, rid)
    }

    fn touch(base_dir: &str, rel: &str) {
        let full = format!("{base_dir}/{rel}");
        std::fs::create_dir_all(std::path::Path::new(&full).parent().unwrap()).unwrap();
        std::fs::write(&full, b"parquet").unwrap();
    }

    fn exists(base_dir: &str, rel: &str) -> bool {
        std::path::Path::new(&format!("{base_dir}/{rel}")).exists()
    }

    #[sqlx::test]
    async fn finish_run_or_recover_keeps_file_on_committed_success(pool: PgPool) {
        let (store, _saved, rid) = seed_run(&pool, "kept").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/kept/run.parquet";
        touch(base, rel);

        finish_run_or_recover(&store, base, rid, 10, 1, None, Some(rel)).await;

        assert!(exists(base, rel), "committed success must keep its file");
        let run = store.get_run(rid, 1).await.unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Success);
        assert_eq!(run.result_path.as_deref(), Some(rel));
    }

    #[sqlx::test]
    async fn finish_run_or_recover_unlinks_orphan_after_cascade_delete(pool: PgPool) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let (store, saved_id, rid) = seed_run(&pool, "orphan").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/orphan/run.parquet";
        touch(base, rel);

        saved_store.delete(saved_id, 1).await.unwrap();

        finish_run_or_recover(&store, base, rid, 10, 1, None, Some(rel)).await;

        assert!(
            !exists(base, rel),
            "cascade-deleted run's orphaned file must be removed"
        );
    }

    #[sqlx::test]
    async fn recover_ambiguous_finish_unlinks_when_run_still_running(pool: PgPool) {
        let (store, _saved, rid) = seed_run(&pool, "wedged").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/wedged/run.parquet";
        touch(base, rel);

        let err = StoreError::Validation("boom".to_owned());
        recover_ambiguous_finish(&store, base, rid, 5, Some(rel), err).await;

        assert!(
            !exists(base, rel),
            "a run flipped from running to error orphans its file — remove it"
        );
        let run = store.get_run(rid, 1).await.unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Error);
        assert_eq!(run.result_path, None);
    }

    #[sqlx::test]
    async fn recover_ambiguous_finish_preserves_committed_success(pool: PgPool) {
        let (store, _saved, rid) = seed_run(&pool, "committed").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/committed/run.parquet";
        touch(base, rel);

        store
            .finish_run(rid, RunStatus::Success, 10, Some(7), None, None, Some(rel))
            .await
            .unwrap();

        let err = StoreError::Validation("boom".to_owned());
        recover_ambiguous_finish(&store, base, rid, 5, Some(rel), err).await;

        assert!(
            exists(base, rel),
            "a committed success's file must never be unlinked by recovery"
        );
        let run = store.get_run(rid, 1).await.unwrap().unwrap();
        assert_eq!(
            run.status,
            RunStatus::Success,
            "committed success preserved"
        );
        assert_eq!(run.result_path.as_deref(), Some(rel));
        assert_eq!(run.row_count, Some(7));
    }

    const MARKER: &str = "/var/lib/trawl/data/MARKER-sched/prod/x.parquet";

    async fn read_back(
        store: &ScheduleStore,
        saved_id: i64,
        rid: i64,
    ) -> Vec<crate::store::ReportRun> {
        let one = store.get_run(rid, 1).await.unwrap().unwrap();
        let listed = store
            .list_runs(saved_id, 1, 50, 0)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == rid)
            .expect("the run is listed");
        vec![one, listed]
    }

    #[sqlx::test]
    async fn scheduled_execution_errors_store_the_public_message(pool: PgPool) {
        use trawl_engine::error::EngineError;

        let failures = [
            (
                "database",
                ServerError::Engine(EngineError::Database(duckdb::Error::InvalidColumnName(
                    MARKER.into(),
                ))),
            ),
            (
                "io",
                ServerError::Engine(EngineError::Io(std::io::Error::other(MARKER))),
            ),
        ];
        for (name, failure) in failures {
            let (store, saved_id, rid) = seed_run(&pool, name).await;
            let status = record_failed_run(&store, rid, 9, &failure).await;
            assert_eq!(status, RunStatus::Error, "{name}");
            for run in read_back(&store, saved_id, rid).await {
                assert_eq!(run.status, RunStatus::Error, "{name}: {run:?}");
                assert_eq!(
                    run.error_message.as_deref(),
                    Some("query execution failed"),
                    "{name}: {run:?}"
                );
                assert_eq!(
                    run.error_message,
                    Some(failure.safe_message()),
                    "{name}: the row and the wire carry one message"
                );
                assert!(!format!("{run:?}").contains("MARKER"), "{name}: {run:?}");
            }
        }
    }

    #[sqlx::test]
    async fn scheduled_persistence_recovery_stores_a_fixed_summary(pool: PgPool) {
        let (store, saved_id, rid) = seed_run(&pool, "persistence").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/persistence/run.parquet";
        touch(base, rel);

        let err = StoreError::Unavailable(sqlx::Error::Protocol(MARKER.into()));
        recover_ambiguous_finish(&store, base, rid, 5, Some(rel), err).await;

        assert!(!exists(base, rel), "the flipped run's file is removed");
        for run in read_back(&store, saved_id, rid).await {
            assert_eq!(run.status, RunStatus::Error, "{run:?}");
            assert_eq!(
                run.error_message.as_deref(),
                Some(RESULT_PERSISTENCE_FAILED),
                "{run:?}"
            );
            assert!(!format!("{run:?}").contains("MARKER"), "{run:?}");
        }
    }

    #[derive(Debug)]
    struct LoggedEvent {
        level: tracing::Level,
        fields: std::collections::HashMap<String, String>,
    }

    struct EventFields<'a>(&'a mut std::collections::HashMap<String, String>);

    impl tracing::field::Visit for EventFields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
    }

    #[derive(Clone, Default)]
    struct EventLog(std::sync::Arc<std::sync::Mutex<Vec<LoggedEvent>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLog {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = std::collections::HashMap::new();
            event.record(&mut EventFields(&mut fields));
            self.0.lock().unwrap().push(LoggedEvent {
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    async fn events_of(work: impl std::future::Future<Output = ()>) -> Vec<LoggedEvent> {
        use tracing_subscriber::layer::SubscriberExt;

        let log = EventLog::default();
        let capturing =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));
        work.await;
        drop(capturing);
        std::mem::take(&mut *log.0.lock().unwrap())
    }

    #[sqlx::test]
    async fn scheduler_fault_events_carry_no_error_text(pool: PgPool) {
        use tracing::Level;

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap().to_owned();
        let planted = || StoreError::Unavailable(sqlx::Error::Protocol(MARKER.into()));
        let fields_of = |e: StoreError| {
            let fault = ServerError::from(e);
            (fault.error_class(), fault.cause_kind().as_str())
        };
        let ambiguous = fields_of(planted());
        let closed_pool = fields_of(StoreError::Unavailable(sqlx::Error::PoolClosed));

        let (store, _, ambiguous_run) = seed_run(&pool, "ambiguous").await;
        let (_, _, finish_run) = seed_run(&pool, "finish").await;
        let (_, _, flip_run) = seed_run(&pool, "flip").await;

        let ambiguous_events = events_of(recover_ambiguous_finish(
            &store,
            &base,
            ambiguous_run,
            5,
            None,
            planted(),
        ))
        .await;
        // The rest run against the store with its pool closed, so its
        // writes fail with a real driver error.
        pool.close().await;
        let closed = store;

        let cases = [
            ("ambiguous commit", vec![ambiguous], ambiguous_events),
            (
                "failed finish",
                vec![closed_pool],
                events_of(async {
                    let failure = ServerError::Engine(trawl_engine::error::EngineError::Io(
                        std::io::Error::other(MARKER),
                    ));
                    record_failed_run(&closed, finish_run, 5, &failure).await;
                })
                .await,
            ),
            (
                "failed flip",
                vec![ambiguous, closed_pool],
                events_of(recover_ambiguous_finish(
                    &closed,
                    &base,
                    flip_run,
                    5,
                    None,
                    planted(),
                ))
                .await,
            ),
        ];

        for (label, expected, events) in cases {
            let faults: Vec<_> = events
                .iter()
                .filter(|e| {
                    e.level == Level::ERROR
                        && e.fields.get("event_type").map(String::as_str) == Some("scheduler_error")
                })
                .map(|e| {
                    (
                        e.fields.get("error_class").map_or("", String::as_str),
                        e.fields.get("cause_kind").map_or("", String::as_str),
                    )
                })
                .collect();
            assert_eq!(faults, expected, "{label}: {events:?}");
            for event in events
                .iter()
                .filter(|e| [Level::INFO, Level::WARN, Level::ERROR].contains(&e.level))
            {
                assert!(!event.fields.contains_key("error"), "{label}: {event:?}");
                for (field, value) in &event.fields {
                    assert!(
                        !value.contains("MARKER"),
                        "{label}: {field} carries the error's text at {}: {value}",
                        event.level
                    );
                }
            }
            let texts = events
                .iter()
                .filter(|e| e.level == Level::DEBUG && e.fields.contains_key("error"))
                .count();
            assert_eq!(
                texts,
                expected.len(),
                "{label}: each fault's text is kept at DEBUG: {events:?}"
            );
        }
    }
}

/// Every parse a scheduled run and its store paths make runs off the
/// reactor (ADR-0063). `#[sqlx::test]` drives each case on a current-thread
/// runtime, so the test's thread is its only reactor thread, and a parse
/// recorded anywhere else ran off it.
#[cfg(test)]
mod off_reactor_tests {
    use std::future::Future;

    use chrono::Utc;
    use sqlx::PgPool;

    use crate::report_window::{ScheduleWindow, truncate_to_micros};
    use crate::store::{DueClaim, ManualRunClaim, SavedQueryStore, ScheduleStore};

    async fn parsed_off_the_reactor<F: Future>(marker: &str, work: F) -> F::Output {
        let log = trawl_core::parser::ParseLog::watch(marker);
        let out = work.await;
        let parses = log.parses();
        assert!(!parses.is_empty(), "{marker}: nothing was parsed");
        let here = std::thread::current().id();
        assert!(
            parses.iter().all(|(thread, _)| *thread != here),
            "{marker}: a parse ran on the reactor: {parses:?}"
        );
        out
    }

    /// A saved query with a since-last schedule over `dsl`, written
    /// through the paths that check nothing.
    async fn windowed(pool: &PgPool, name: &str, dsl: &str) -> (ScheduleStore, i64, i64) {
        let saved = SavedQueryStore::new(pool.clone())
            .create(1, name, dsl)
            .await
            .unwrap();
        let schedules = ScheduleStore::new(pool.clone());
        let schedule = schedules
            .create_schedule(
                saved.id,
                1,
                3600,
                None,
                Some(ScheduleWindow::SinceLast),
                0,
                truncate_to_micros(Utc::now()),
            )
            .await
            .unwrap();
        (schedules, saved.id, schedule.id)
    }

    #[sqlx::test]
    async fn a_scheduled_run_prepares_off_the_reactor(pool: PgPool) {
        let data = tempfile::tempdir().unwrap();
        let executors = crate::pool::ExecutorPool::for_test(
            data.path().to_str().unwrap().into(),
            1,
            100_000,
            None,
        )
        .unwrap();
        let (store, _, run_id) = super::pg_tests::seed_run(&pool, "prepared").await;
        let lease = executors.try_lease_scheduled().expect("a free lease");
        parsed_off_the_reactor(
            "zz_cp6b_scheduled_run",
            super::execute_scheduled_query(
                store.clone(),
                executors,
                run_id,
                "zz_cp6b_scheduled_run | where message matches /zz_(a|b)+/",
                "prepared",
                30,
                lease,
            ),
        )
        .await;
        let run = store.get_run(run_id, 1).await.unwrap().unwrap();
        assert!(run.error_message.is_none(), "{run:?}");
    }

    #[sqlx::test]
    async fn a_schedule_window_write_parses_off_the_reactor(pool: PgPool) {
        let saved = SavedQueryStore::new(pool.clone())
            .create(1, "window_write", "zz_cp6b_window_write error")
            .await
            .unwrap();
        parsed_off_the_reactor(
            "zz_cp6b_window_write",
            ScheduleStore::new(pool.clone()).set_schedule_checked(
                saved.id,
                1,
                3600,
                None,
                true,
                Some(ScheduleWindow::SinceLast),
                0,
                Utc::now(),
            ),
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn a_windowed_saved_edit_parses_off_the_reactor(pool: PgPool) {
        let (_, saved_id, _) = windowed(&pool, "windowed_edit", "error").await;
        parsed_off_the_reactor(
            "zz_cp6b_saved_edit",
            SavedQueryStore::new(pool.clone()).update_checked(
                saved_id,
                1,
                "zz_cp6b_saved_edit error",
                None,
            ),
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn a_due_claim_materializes_off_the_reactor(pool: PgPool) {
        let (schedules, _, schedule_id) =
            windowed(&pool, "due_claim", "zz_cp6b_due_claim error").await;
        let lease = crate::pool::RunLease::unattached();
        let claimed = parsed_off_the_reactor(
            "zz_cp6b_due_claim",
            Box::pin(schedules.claim_due_run(schedule_id, Utc::now(), 24, &lease)),
        )
        .await
        .unwrap();
        assert!(matches!(claimed, DueClaim::Started(_)), "{claimed:?}");
    }

    #[sqlx::test]
    async fn a_manual_claim_materializes_off_the_reactor(pool: PgPool) {
        let (schedules, saved_id, _) =
            windowed(&pool, "manual_claim", "zz_cp6b_manual_claim error").await;
        let lease = crate::pool::RunLease::unattached();
        let claimed = parsed_off_the_reactor(
            "zz_cp6b_manual_claim",
            Box::pin(schedules.claim_manual_run(saved_id, 1, 24, &lease)),
        )
        .await
        .unwrap();
        assert!(matches!(claimed, ManualRunClaim::Started(_)), "{claimed:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CorpusWatch, decode_result_blob, remove_result_file, result_relative_path, zstd_fallback,
    };
    use crate::publication::CorpusUnsettled::{RestartBacklog, RollupPending};

    #[test]
    fn reserved_root_entries_agree_across_consumers() {
        let relative = result_relative_path(42);
        let path = std::path::Path::new(&relative);
        assert_eq!(
            path.parent(),
            Some(std::path::Path::new(trawl_config::SCHEDULED_DIR_NAME))
        );
        assert_eq!(relative, "scheduled/run_42.parquet");
        assert!(trawl_config::RESERVED_ROOT_DIRS.contains(&trawl_config::SCHEDULED_DIR_NAME));
    }

    #[test]
    fn corpus_watch_logs_each_change_once() {
        let mut watch = CorpusWatch::default();
        let logged: Vec<bool> = [
            None,
            Some(RestartBacklog),
            Some(RestartBacklog),
            Some(RollupPending),
            Some(RollupPending),
            None,
            None,
        ]
        .into_iter()
        .map(|state| watch.observe(state))
        .collect();
        assert_eq!(logged, [false, true, false, true, false, true, false]);
    }

    #[test]
    fn remove_result_file_removes_existing_and_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        // Trailing slash on base_dir exercises the trim_end_matches join.
        let base = format!("{}/", tmp.path().to_str().unwrap());
        let rel = "scheduled/foo/run_1.parquet";
        let full = format!("{base}{rel}");
        std::fs::create_dir_all(std::path::Path::new(&full).parent().unwrap()).unwrap();
        std::fs::write(&full, b"parquet").unwrap();

        assert!(
            remove_result_file(&base, rel),
            "existing file must report true"
        );
        assert!(!std::path::Path::new(&full).exists(), "file must be gone");
    }

    #[cfg(unix)]
    #[test]
    fn remove_result_file_unlinks_a_dangling_link_at_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let rel = "scheduled/foo/run_1.parquet";
        let full = tmp.path().join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &full).unwrap();

        assert!(remove_result_file(tmp.path().to_str().unwrap(), rel));
        assert!(
            std::fs::symlink_metadata(&full).is_err(),
            "the link is gone"
        );
    }

    #[test]
    fn remove_result_file_returns_false_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();

        assert!(
            !remove_result_file(base, "scheduled/foo/does_not_exist.parquet"),
            "missing file must report false"
        );
    }

    /// The failures are a directory where the file should be, which
    /// `remove_file` refuses with something other than `NotFound`, and a file
    /// below a dangling link, which answers `NotFound` without being absent.
    #[cfg(unix)]
    #[test]
    fn result_file_cleanup_events_carry_no_host_path_or_io_text() {
        use crate::test_events::events_of;

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap().to_owned();
        let abs = tmp.path().display().to_string();
        let blocked = "scheduled/foo/run_dir.parquet";
        std::fs::create_dir_all(tmp.path().join(blocked)).unwrap();
        let missing = "scheduled/foo/does_not_exist.parquet";
        let below_link = "scheduled/linked/run.parquet";
        std::os::unix::fs::symlink(
            tmp.path().join("nowhere"),
            tmp.path().join("scheduled/linked"),
        )
        .unwrap();
        let io_error = |rel: &str| {
            std::fs::remove_file(tmp.path().join(rel))
                .expect_err("the fixture cannot be unlinked as a file")
        };
        let blocked_error = io_error(blocked);

        for (rel, event_type, removal_error) in [
            (missing, "result_file_already_absent", None),
            (
                blocked,
                "result_file_cleanup_error",
                Some((blocked_error.to_string(), blocked_error.kind().to_string())),
            ),
            (
                below_link,
                "result_file_cleanup_error",
                Some((
                    io_error(below_link).to_string(),
                    std::io::ErrorKind::NotFound.to_string(),
                )),
            ),
        ] {
            let events = events_of(|| assert!(!remove_result_file(&base, rel)));
            let loud: Vec<_> = events.iter().filter(|e| e.is_default_level()).collect();
            assert_eq!(loud.len(), 1, "{rel}: {events:?}");
            let event = loud[0];
            assert_eq!(
                event.fields.get("event_type").map(String::as_str),
                Some(event_type),
                "{event:?}"
            );
            assert_eq!(
                event.fields.get("path").map(String::as_str),
                Some(rel),
                "{event:?}"
            );
            assert!(!event.text().contains(&abs), "{event:?}");
            assert!(!event.text().contains("os error"), "{event:?}");
            if let Some((io_text, error_kind)) = &removal_error {
                assert_eq!(event.level, tracing::Level::WARN, "{event:?}");
                assert_eq!(
                    event.fields.get("error_kind"),
                    Some(error_kind),
                    "{event:?}"
                );
                assert!(!event.text().contains(io_text), "{event:?}");
            } else {
                assert_eq!(event.level, tracing::Level::INFO, "{event:?}");
            }

            let debug = events
                .iter()
                .find(|e| e.level == tracing::Level::DEBUG)
                .unwrap_or_else(|| panic!("{rel}: no DEBUG event: {events:?}"));
            assert!(debug.text().contains(&abs), "{debug:?}");
            if let Some((io_text, _)) = &removal_error {
                assert!(debug.text().contains(io_text), "{debug:?}");
            }
        }
    }

    #[sqlx::test]
    async fn scheduled_run_emits_query_timing_and_no_error_text(pool: sqlx::PgPool) {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        use tracing_subscriber::prelude::*;

        type Events = Arc<Mutex<Vec<HashMap<String, String>>>>;

        struct Fields<'a>(&'a mut HashMap<String, String>);

        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().to_owned(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_owned(), value.to_owned());
            }
        }

        struct Capture(Events);

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = HashMap::new();
                event.record(&mut Fields(&mut fields));
                self.0.lock().unwrap().push(fields);
            }
        }

        let data = tempfile::tempdir().unwrap();
        let executors = crate::pool::ExecutorPool::for_test(
            data.path().to_str().unwrap().into(),
            1,
            100_000,
            None,
        )
        .unwrap();
        let (store, _, succeeded) = super::pg_tests::seed_run(&pool, "timed_ok").await;
        let (_, _, failed) = super::pg_tests::seed_run(&pool, "timed_failure").await;

        // The events this test cares about are written on its own thread,
        // where the run's future is polled; the blocking worker's are not
        // needed.
        let events = Events::default();
        let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&events)));
        let capturing = tracing::subscriber::set_default(subscriber);

        super::execute_scheduled_query(
            store.clone(),
            executors.clone(),
            succeeded,
            "* | head 1",
            "timed_ok",
            30,
            executors.try_lease_scheduled().expect("a free lease"),
        )
        .await;
        super::execute_scheduled_query(
            store.clone(),
            executors.clone(),
            failed,
            "* | stats zz_sched_needle()",
            "timed_failure",
            30,
            executors.try_lease_scheduled().expect("a free lease"),
        )
        .await;
        drop(capturing);

        let events = events.lock().unwrap().clone();
        let of = |event_type: &str, run_id: i64| -> Vec<HashMap<String, String>> {
            events
                .iter()
                .filter(|e| {
                    e.get("event_type").map(String::as_str) == Some(event_type)
                        && e.get("run_id") == Some(&run_id.to_string())
                })
                .cloned()
                .collect()
        };
        let field = |e: &HashMap<String, String>, name: &str| e.get(name).cloned();

        for (run_id, outcome, lifecycle) in [
            (succeeded, "success", "scheduled_query_completed"),
            (failed, "error", "scheduled_query_failed"),
        ] {
            let timings = of("query_timing", run_id);
            assert_eq!(timings.len(), 1, "one account per run: {events:?}");
            let timing = &timings[0];
            assert_eq!(field(timing, "kind").as_deref(), Some("scheduled"));
            assert_eq!(field(timing, "outcome").as_deref(), Some(outcome));
            assert_eq!(field(timing, "request_id"), None, "{timing:?}");
            assert!(
                timing.contains_key("query_dsl_check_us"),
                "the admission check is timed: {timing:?}"
            );
            let query_id = field(timing, "query_id").expect("query_id");

            let ended = of(lifecycle, run_id);
            assert_eq!(ended.len(), 1, "{lifecycle}: {events:?}");
            assert_eq!(
                field(&ended[0], "query_id"),
                Some(query_id),
                "{lifecycle} joins its timing on query_id"
            );
        }

        let failure = &of("scheduled_query_failed", failed)[0];
        assert_eq!(field(failure, "error_class").as_deref(), Some("emit"));
        assert!(!failure.contains_key("error"), "{failure:?}");
        let timing = &of("query_timing", failed)[0];
        assert_eq!(field(timing, "error_class").as_deref(), Some("emit"));
        for event in &events {
            assert!(
                !event.values().any(|v| v.contains("zz_sched_needle")),
                "no event carries the error's text: {event:?}"
            );
        }

        let run = store.get_run(failed, 1).await.unwrap().unwrap();
        assert!(
            run.error_message
                .as_deref()
                .is_some_and(|m| m.contains("zz_sched_needle")),
            "the run row stores the public message, which names the emit error's function: {run:?}"
        );
    }

    /// The job is parked twice: before its work runs, and after its work
    /// returned, when everything the work captured has dropped and only the job
    /// is left holding its slot. On a pool of two permits the one lease is the
    /// run's, so no other lease is free until the job lets it go.
    #[sqlx::test]
    async fn a_preparation_job_holds_its_lease(pool: sqlx::PgPool) {
        use crate::pool::seam::Seam;
        use crate::store::RunStatus;

        for seam in [Seam::Preparing, Seam::Prepared] {
            let data = tempfile::tempdir().unwrap();
            let executors = crate::pool::ExecutorPool::for_test(
                data.path().to_str().unwrap().into(),
                2,
                100_000,
                None,
            )
            .unwrap();
            let seams = executors.seams();
            let parked = seams.hold(seam);
            let (store, _, run_id) =
                super::pg_tests::seed_run(&pool, &format!("held_{seam:?}")).await;
            let lease = executors.try_lease_scheduled().expect("a free lease");

            super::execute_scheduled_query(
                store.clone(),
                executors.clone(),
                run_id,
                "* | head 1",
                "held",
                1,
                lease,
            )
            .await;
            let run = store.get_run(run_id, 1).await.unwrap().unwrap();
            assert_eq!(run.status, RunStatus::Timeout, "{seam:?}: {run:?}");
            crate::test_timing::until("the job parks", || parked.arrivals() == 1).await;
            assert!(
                executors.try_lease_scheduled().is_none(),
                "{seam:?}: the parked job keeps the run's lease"
            );

            parked.release();
            crate::test_timing::until("the job gives the lease back", || {
                executors.scheduled_leases_available() == 1
            })
            .await;
            assert_eq!(executors.preparation().in_use(), 0, "{seam:?}");
        }
    }

    #[sqlx::test]
    #[allow(clippy::too_many_lines)] // one fixture per claim outcome
    async fn lease_count_and_return(pool: sqlx::PgPool) {
        use chrono::{DateTime, Utc};

        use crate::pool::ExecutorPool;
        use crate::report_window::{ScheduleWindow, truncate_to_micros};
        use crate::store::{DueClaim, RunClaim, RunStatus, SavedQueryStore, ScheduleStore};

        async fn net(
            pool: &sqlx::PgPool,
            name: &str,
            dsl: &str,
            max_runs: Option<u64>,
            window: Option<ScheduleWindow>,
            fire: DateTime<Utc>,
        ) -> i64 {
            let saved = SavedQueryStore::new(pool.clone())
                .create(1, name, dsl)
                .await
                .unwrap();
            ScheduleStore::new(pool.clone())
                .create_schedule(saved.id, 1, 3600, max_runs, window, 0, fire)
                .await
                .unwrap()
                .id
        }

        const CATCHUP: u32 = 24;

        for (permits, leases) in [(1, 1), (2, 1), (3, 1), (4, 2), (5, 2)] {
            let executors =
                ExecutorPool::for_test("/nonexistent".into(), permits, 100, None).unwrap();
            assert_eq!(
                executors.scheduled_leases_available(),
                leases,
                "{permits} permits"
            );
        }

        let executors = ExecutorPool::for_test("/nonexistent".into(), 1, 100, None).unwrap();
        let clone = executors.clone();
        let lease = executors.try_lease_scheduled().expect("one lease");
        assert!(executors.try_lease_scheduled().is_none(), "only one");
        assert!(
            clone.try_lease_scheduled().is_none(),
            "a clone of the pool shares it"
        );
        let kept = lease.clone();
        drop(lease);
        assert_eq!(
            executors.scheduled_leases_available(),
            0,
            "a clone of the lease keeps it"
        );
        drop(kept);
        assert_eq!(
            executors.scheduled_leases_available(),
            1,
            "the last clone returns it"
        );

        let store = ScheduleStore::new(pool.clone());
        let now = truncate_to_micros(Utc::now());
        let hour = chrono::Duration::hours(1);
        let not_due = net(&pool, "not_due", "error", None, None, now + hour).await;
        let advanced = net(
            &pool,
            "advanced",
            "error",
            None,
            Some(ScheduleWindow::SinceLast),
            now,
        )
        .await;
        sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
            .bind(now + hour * 10)
            .bind(advanced)
            .execute(&pool)
            .await
            .unwrap();
        let running = net(&pool, "running", "error", None, None, now).await;
        let capped = net(&pool, "capped", "error", Some(1), None, now).await;
        for schedule_id in [running, capped] {
            let saved_id: i64 =
                sqlx::query_scalar("SELECT saved_query_id FROM schedules WHERE id = $1")
                    .bind(schedule_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            let RunClaim::Started(run_id) = store
                .claim_run(schedule_id, saved_id, "error", None)
                .await
                .unwrap()
            else {
                panic!("the seeded run starts");
            };
            if schedule_id == capped {
                store
                    .finish_run(run_id, RunStatus::Success, 1, Some(0), None, None, None)
                    .await
                    .unwrap();
            }
        }
        // A query that owns its own time clause under a window: the claim
        // fails, and leaves the cursor alone.
        let failing = net(
            &pool,
            "failing",
            "error last=1h",
            None,
            Some(ScheduleWindow::SinceLast),
            now,
        )
        .await;
        let started = net(&pool, "started", "error", None, None, now).await;

        for (outcome, schedule_id) in [
            ("NotDue", not_due),
            ("Advanced", advanced),
            ("AlreadyRunning", running),
            ("MaxRunsReached", capped),
            ("a failed claim", failing),
        ] {
            let lease = executors.try_lease_scheduled().expect("the lease is free");
            let claimed = super::claim_leased(&store, schedule_id, now, CATCHUP, lease).await;
            assert!(claimed.is_none(), "{outcome}: {claimed:?}");
            assert_eq!(
                executors.scheduled_leases_available(),
                1,
                "{outcome} returned its lease"
            );
        }
        // Each was the outcome it is named for. The ones that write nothing
        // answer the same again, and the advance moved the cursor past
        // `now` without a run.
        for (schedule_id, outcome) in [
            (not_due, DueClaim::NotDue),
            (running, DueClaim::AlreadyRunning),
            (capped, DueClaim::MaxRunsReached),
        ] {
            let lease = executors.try_lease_scheduled().expect("the lease is free");
            assert_eq!(
                store
                    .claim_due_run(schedule_id, now, CATCHUP, &lease)
                    .await
                    .unwrap(),
                outcome
            );
        }
        let lease = executors.try_lease_scheduled().expect("the lease is free");
        assert!(
            store
                .claim_due_run(failing, now, CATCHUP, &lease)
                .await
                .is_err()
        );
        drop(lease);
        let advanced_cursor: DateTime<Utc> =
            sqlx::query_scalar("SELECT next_fire_at FROM schedules WHERE id = $1")
                .bind(advanced)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(advanced_cursor, now + hour);
        assert_eq!(store.count_runs(advanced).await.unwrap(), 0);

        let lease = executors.try_lease_scheduled().expect("the lease is free");
        let (claimed, lease) = super::claim_leased(&store, started, now, CATCHUP, lease)
            .await
            .expect("a due schedule starts a run");
        assert_eq!(
            executors.scheduled_leases_available(),
            0,
            "a started run keeps its lease"
        );
        let run = store.get_run(claimed.run_id, 1).await.unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Running);
        drop(lease);
        assert_eq!(executors.scheduled_leases_available(), 1);
    }

    #[test]
    fn the_blob_round_trips_a_typed_result() {
        use trawl_api::value::{Column, ColumnType, QueryResult, Value};
        let result = QueryResult {
            columns: vec![Column::new("x", ColumnType::Mixed)],
            rows: vec![
                vec![Value::UInt(u64::MAX)],
                vec![Value::String("abc".into())],
            ],
        };
        let (path, blob) = zstd_fallback(&result).expect("the result encodes");
        assert!(path.is_none());
        assert_eq!(decode_result_blob(blob), Some(result));
    }

    #[test]
    fn the_blob_refuses_a_result_the_codec_refuses() {
        use trawl_api::value::{Column, ColumnType, QueryResult, Value};
        let result = QueryResult {
            columns: vec![Column::new("n", ColumnType::Integer)],
            rows: vec![vec![Value::String("7".into())]],
        };
        let reason = zstd_fallback(&result).expect_err("a string cell is not an integer");
        assert!(
            reason.contains("encoding failed") && reason.contains("\"n\""),
            "{reason}"
        );
    }

    #[test]
    fn result_blob_event_carries_no_column_name() {
        use crate::test_events::events_of;
        use trawl_api::value::{Column, ColumnType, QueryResult, Value};

        // A mixed column has no exact parquet form, so the run stores a
        // blob. Its name is query or sender content (a renamed field, an
        // `extract kv` key), so the default-level event must not carry the
        // refusal that quotes it.
        const MARKER: &str = "zz_content_marker";
        let result = QueryResult {
            columns: vec![Column::new(MARKER, ColumnType::Mixed)],
            rows: vec![
                vec![Value::Integer(1)],
                vec![Value::String("word".to_owned())],
            ],
        };
        let data = tempfile::tempdir().unwrap();
        let executors = crate::pool::ExecutorPool::for_test(
            data.path().display().to_string(),
            1,
            100_000,
            None,
        )
        .unwrap();

        let mut stored = None;
        let events = events_of(|| {
            stored = Some(super::write_result_parquet(&executors, 1, &result));
        });
        let (path, blob) = stored.unwrap().expect("the blob encodes");
        assert!(
            path.is_none() && blob.is_some(),
            "a mixed column stores a blob"
        );

        let blob_event = events
            .iter()
            .find(|e| {
                e.is_default_level()
                    && e.fields.get("event_type").map(String::as_str)
                        == Some("scheduler_result_blob")
            })
            .unwrap_or_else(|| panic!("no default-level blob event: {events:?}"));
        assert!(!blob_event.text().contains(MARKER), "{blob_event:?}");
        for event in events.iter().filter(|e| e.is_default_level()) {
            assert!(!event.text().contains(MARKER), "{event:?}");
        }
        assert!(
            events
                .iter()
                .any(|e| e.level == tracing::Level::DEBUG && e.text().contains(MARKER)),
            "the refusal keeps its reason at DEBUG: {events:?}"
        );
    }

    /// The arms: the result directory blocked by a file, the engine's ndjson
    /// staging path blocked by a directory (an [`EngineError::Io`]), and the
    /// final path blocked by a non-empty directory, so the rename fails. The
    /// detached executor arm takes the same engine path and is not provoked here.
    ///
    /// [`EngineError::Io`]: trawl_engine::error::EngineError::Io
    #[test]
    fn parquet_fallback_events_carry_no_error_text() {
        use crate::test_events::events_of;
        use trawl_api::value::{Column, ColumnType, QueryResult, Value};

        type Block = fn(&std::path::Path);

        let result = QueryResult {
            columns: vec![Column::new("n", ColumnType::Integer)],
            rows: vec![vec![Value::Integer(7)]],
        };
        let block_dir = |base: &std::path::Path| {
            std::fs::write(base.join("scheduled"), b"").unwrap();
        };
        let block_write = |base: &std::path::Path| {
            std::fs::create_dir_all(base.join("scheduled/run_1.parquet.tmp.ndjson.tmp")).unwrap();
        };
        let block_rename = |base: &std::path::Path| {
            std::fs::create_dir_all(base.join("scheduled/run_1.parquet/inner")).unwrap();
        };
        let cases: [(&str, Block, &[&str]); 3] = [
            ("directory", block_dir, &["error_kind"]),
            ("engine write", block_write, &["error_class", "cause_kind"]),
            ("rename", block_rename, &["error_kind"]),
        ];
        for (arm, block, carried) in cases {
            let data = tempfile::tempdir().unwrap();
            let abs = data.path().display().to_string();
            block(data.path());
            let executors =
                crate::pool::ExecutorPool::for_test(abs.clone(), 1, 100_000, None).unwrap();

            let mut stored = None;
            let events = events_of(|| {
                stored = Some(super::write_result_parquet(&executors, 1, &result));
            });
            let (path, blob) = stored.unwrap().expect("the blob fallback encodes");
            assert!(path.is_none() && blob.is_some(), "{arm}: no blob fallback");

            for event in events.iter().filter(|e| e.is_default_level()) {
                assert!(!event.text().contains(&abs), "{arm}: {event:?}");
                assert!(!event.text().contains("os error"), "{arm}: {event:?}");
                assert!(!event.fields.contains_key("error"), "{arm}: {event:?}");
            }
            let warn = events
                .iter()
                .find(|e| {
                    e.level == tracing::Level::WARN
                        && e.fields.get("event_type").map(String::as_str)
                            == Some("scheduler_parquet_error")
                })
                .unwrap_or_else(|| panic!("{arm}: no WARN: {events:?}"));
            for field in carried {
                assert!(warn.fields.contains_key(*field), "{arm}: {warn:?}");
            }
            let debug = events
                .iter()
                .find(|e| {
                    e.level == tracing::Level::DEBUG
                        && e.fields.get("event_type").map(String::as_str)
                            == Some("scheduler_parquet_error")
                })
                .unwrap_or_else(|| panic!("{arm}: no DEBUG: {events:?}"));
            assert!(debug.text().contains("os error"), "{arm}: {debug:?}");
        }
    }
}
