// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background scheduler for periodic query execution.
//!
//! Polls [`ScheduleStore`] for enabled schedules and spawns query execution
//! tasks on the [`ExecutorPool`]. Results are written as parquet files (with
//! a zstd-JSON blob fallback) and recorded as report runs in the app-state
//! database.

use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use fleet_auth::KeyStore;

use crate::config::SchedulerConfig;
use crate::policy::{Permission, TrawlAuthz as _};
use crate::pool::ExecutorPool;
use crate::report_window::{format_window_bound, truncate_to_micros};
use crate::store::{ClaimedRun, DueClaim, FinishOutcome, FlipOutcome, RunStatus, ScheduleStore};

/// Spawn the scheduler background task.
///
/// The task exits on the first change to `shutdown_rx`, or when its sender
/// is dropped; the value itself is never read.
pub fn spawn_scheduler(
    schedule_store: ScheduleStore,
    key_store: KeyStore,
    pool: ExecutorPool,
    config: SchedulerConfig,
    timeout_secs: u64,
    shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(scheduler_loop(
        schedule_store,
        key_store,
        pool,
        config,
        timeout_secs,
        shutdown_rx,
    ))
}

async fn scheduler_loop(
    schedule_store: ScheduleStore,
    key_store: KeyStore,
    pool: ExecutorPool,
    config: SchedulerConfig,
    timeout_secs: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let poll_interval = Duration::from_secs(config.poll_interval_secs);
    let mut tick = tokio::time::interval(poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Clean up any runs left in 'running' state from a previous crash.
    // Safe against live siblings: the boot-time advisory lock guarantees
    // this process is the only trawld on this database.
    match schedule_store.cleanup_stale_runs().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            event_type = "scheduler_stale_cleanup",
            count = n,
            "cleaned up stale report runs"
        ),
        Err(e) => tracing::error!(
            event_type = "scheduler_error",
            error = %e,
            "failed to cleanup stale runs"
        ),
    }

    let mut retention_counter: u64 = 0;
    // Run retention every ~360 ticks (roughly hourly at 10s poll interval).
    let retention_interval = 3600 / config.poll_interval_secs.max(1);

    tracing::info!(
        event_type = "scheduler_started",
        poll_interval_secs = config.poll_interval_secs,
        report_max_rows = config.report_max_rows,
        "scheduler started"
    );

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown_rx.changed() => {
                tracing::info!(event_type = "scheduler_shutdown", "scheduler shutting down");
                return;
            }
        }

        // ONE clock reading per tick, truncated to the microsecond every
        // stored bound shares (postgres TIMESTAMPTZ, DuckDB TIMESTAMP, and
        // the rendered `earliest=`/`latest=` text). Sampling per schedule
        // would let two schedules in the same poll disagree about which
        // boundary has passed.
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

        // Periodic retention cleanup.
        retention_counter += 1;
        if retention_counter >= retention_interval {
            retention_counter = 0;
            match schedule_store
                .delete_old_runs(config.report_retention_days, config.max_runs_per_schedule)
                .await
            {
                Ok((_count, paths)) => {
                    // Clean up parquet files from disk for deleted runs.
                    // Paths from the DB are relative (e.g. "scheduled/foo/run_1.parquet"),
                    // resolved against the data directory by remove_result_file.
                    let base = pool.base_dir();
                    for path in paths {
                        remove_result_file(base, &path);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        event_type = "scheduler_retention_error",
                        error = %e,
                        "failed to clean up old report runs"
                    );
                }
            }
        }
    }
}

/// One scheduler tick: claim and spawn every schedule due at `now`.
///
/// `now` is a parameter rather than a clock reading, and [`scheduler_loop`]
/// samples it once per tick, so every schedule in a poll is judged against
/// the same instant. Tests drive this function directly with an explicit
/// instant: tiling, catch-up clamping and lag are all statements about
/// hours of coverage, and there is no other way to make them fast.
///
/// Returns the spawned execution tasks. The loop drops them — each run
/// records its own outcome through `finish_run` — and tests await them.
pub async fn poll_and_execute(
    schedule_store: &ScheduleStore,
    key_store: &KeyStore,
    pool: &ExecutorPool,
    config: &SchedulerConfig,
    timeout_secs: u64,
    now: DateTime<Utc>,
) -> Vec<JoinHandle<()>> {
    let mut spawned = Vec::new();

    let schedules = match schedule_store.list_enabled_schedules().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                event_type = "scheduler_error",
                error = %e,
                "failed to list enabled schedules"
            );
            return spawned;
        }
    };

    // The listing is the enumeration and nothing more: which schedules
    // exist. Every value the run depends on — cadence, window, watermark,
    // and above all the saved DSL — is re-read inside `claim_due_run`,
    // under the row locks it takes.
    for (schedule, _saved_query) in schedules {
        // Key liveness comes BEFORE the claim, so an unusable key leaves the
        // fire cursor exactly where it was. The cursor is the coverage
        // boundary: skipping the claim means a `since_last` schedule covers
        // the whole outage in one window once the key is usable again,
        // instead of losing every boundary that passed while it was not.
        if !owning_key_is_usable(key_store, schedule.id, schedule.key_id).await {
            continue;
        }

        // Plan, materialize, claim and move the cursor — one transaction.
        let claimed = match schedule_store
            .claim_due_run(schedule.id, now, config.max_catchup_intervals)
            .await
        {
            Ok(DueClaim::Started(claimed)) => claimed,
            Ok(
                DueClaim::NotDue
                | DueClaim::Advanced
                | DueClaim::AlreadyRunning
                | DueClaim::MaxRunsReached,
            ) => continue,
            Err(e) => {
                // The class, never the message: a window failure's Display
                // can quote the saved DSL and the parser's own text, and
                // this event lands in the retained `service=trawld` corpus.
                tracing::error!(
                    event_type = "scheduler_error",
                    schedule_id = schedule.id,
                    error_class = e.class(),
                    "failed to claim a due run; the schedule's fire cursor is unchanged"
                );
                continue;
            }
        };

        if let Some(window) = claimed.window
            && window.truncated
        {
            metrics::counter!(crate::metrics::SCHEDULER_WINDOW_TRUNCATED_TOTAL).increment(1);
            // The bounds are instants, not operator text, so they are safe
            // as event fields. They are fields and never metric labels: an
            // instant is unbounded cardinality.
            tracing::warn!(
                event_type = "scheduler_window_truncated",
                schedule_id = schedule.id,
                run_id = claimed.run_id,
                window_start = %format_window_bound(window.start),
                window_end = %format_window_bound(window.end),
                "report window clamped to max_catchup_intervals; the span before its start stays uncovered"
            );
        }

        // Execute the text the claim RESOLVED and stored, never the saved
        // DSL from the listing: with a window those two differ by the very
        // bounds the run row claims to cover.
        let ClaimedRun {
            run_id,
            query_name,
            resolved_query,
            ..
        } = claimed;
        let store = schedule_store.clone();
        let pool = pool.clone();
        let max_rows = config.report_max_rows;

        spawned.push(tokio::spawn(async move {
            execute_scheduled_query(
                store,
                pool,
                run_id,
                &resolved_query,
                &query_name,
                max_rows,
                timeout_secs,
            )
            .await;
        }));
    }

    spawned
}

/// Whether the schedule's owning key may still run scheduled queries.
///
/// It must be live in the fleet keystore (active and unexpired) and its
/// roles must still resolve both [`Permission::Query`] and
/// [`Permission::SavedQuery`], the two authorities a scheduled saved-query
/// run exercises. Revocation, expiry and role changes (analyst to
/// reader/ingest) therefore all stop scheduled execution: a key that
/// cannot create or run saved queries interactively must not keep running
/// them on a schedule. Lookup failures skip conservatively.
async fn owning_key_is_usable(key_store: &KeyStore, schedule_id: i64, key_id: i64) -> bool {
    let live = match key_store.get_live_key_by_id(key_id).await {
        Ok(live) => live,
        Err(e) => {
            tracing::warn!(
                event_type = "scheduler_error",
                schedule_id,
                key_id,
                error = %e,
                "failed to check key liveness; skipping schedule this tick"
            );
            return false;
        }
    };
    let usable = live.is_some_and(|k| {
        k.has_permission(Permission::Query) && k.has_permission(Permission::SavedQuery)
    });
    if !usable {
        tracing::info!(
            event_type = "scheduler_skip",
            schedule_id,
            key_id,
            "skipping schedule: owning key is revoked, expired, or lacks query/saved-query permission"
        );
    }
    usable
}

pub(crate) async fn execute_scheduled_query(
    schedule_store: ScheduleStore,
    pool: ExecutorPool,
    run_id: i64,
    query: &str,
    _query_name: &str,
    _max_rows: usize,
    timeout_secs: u64,
) {
    let start = std::time::Instant::now();
    // One budget for this attempt, stamped before the first pool wait
    // (ADR-0024): a scheduled run that spends its whole timeout queueing
    // does not then get another one to execute in.
    let deadline = crate::deadline::Deadline::after(Duration::from_secs(timeout_secs));

    // The same admission door the HTTP entry points use (ADR-0024). A
    // query stored before that door existed can be over a cap, and this
    // is where it stops: THIS attempt fails with the diagnostic, recorded
    // as the run's error. The schedule keeps its row and its cursor, so
    // the operator repairs the saved DSL rather than re-enabling a
    // schedule the server disabled behind their back.
    let result = match crate::admission::check_dsl(query) {
        Err(refusal) => Err(refusal),
        // Execute the query on the pool (no debug capture, UTC timestamps).
        Ok(()) => {
            pool.execute(
                pool.allocate_query_id(),
                query,
                deadline,
                false,
                0,
                crate::pool::WorkContext::system(crate::pool::WorkKind::Scheduled),
            )
            .await
            .result
        }
    };

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(query_result) => {
            let row_count = query_result.rows.len();

            // Write result as parquet file.
            let (result_path, result_data) = write_result_parquet(&pool, run_id, &query_result);

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
                duration_ms,
                row_count,
                result_path = result_path.as_deref().unwrap_or("(blob)"),
                "scheduled query completed"
            );
        }
        Err(e) => {
            let (status, error_msg) = if matches!(e, crate::error::ServerError::Timeout) {
                (RunStatus::Timeout, format!("{e}"))
            } else {
                (RunStatus::Error, format!("{e}"))
            };

            if let Err(e2) = schedule_store
                .finish_run(
                    run_id,
                    status,
                    duration_ms,
                    None,
                    Some(&error_msg),
                    None,
                    None,
                )
                .await
            {
                tracing::error!(
                    event_type = "scheduler_error",
                    run_id,
                    error = %e2,
                    "failed to finish run"
                );
            }

            tracing::warn!(
                event_type = "scheduled_query_failed",
                run_id,
                duration_ms,
                status = status.as_str(),
                error = %e,
                "scheduled query failed"
            );
        }
    }
}

/// Persist a successful run's result, then reconcile the parquet file on disk
/// with whatever the store actually committed.
///
/// Owns the unlink-vs-preserve decision for the success outcome:
/// - [`FinishOutcome::Persisted`]  — the run row was updated; the file it points
///   at stays.
/// - [`FinishOutcome::RunDeleted`] — the row was cascade-deleted mid-flight (its
///   saved query or schedule is gone); the file we just wrote is orphaned, so
///   remove it.
/// - `Err(_)`    — an ambiguous commit; recovery is delegated to
///   [`recover_ambiguous_finish`].
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
            // The run row was cascade-deleted mid-flight: the file we just
            // wrote is orphaned — remove it.
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
            // A transient app-state DB error (pg restart/failover) is an
            // ambiguous commit: for a single autocommit UPDATE the COMMIT can
            // land server-side while the client's ack is lost, so sqlx returns
            // Err even though the row was written to status='success' with
            // result_path set. So the parquet must not be deleted here —
            // recovery flips the row only while it is still 'running'.
            tracing::error!(
                event_type = "scheduler_error",
                run_id,
                error = %e,
                result_path = result_path.unwrap_or("(blob)"),
                "failed to finish run (ambiguous commit); recovering via guarded flip"
            );
            recover_ambiguous_finish(
                schedule_store,
                base_dir,
                run_id,
                duration_ms,
                result_path,
                &e,
            )
            .await;
        }
    }
}

/// Recover from an ambiguous `finish_run` commit and reconcile the parquet on
/// disk, without ever destroying a success that actually committed.
///
/// Guarded state transition: flip the row to `error` only while it is still
/// `running`. If the success commit actually landed (status is already
/// `success`), the flip matches zero rows and the committed result is
/// preserved rather than unlinking the file it points at. If the DB is still
/// down the flip also fails and boot-time `cleanup_stale_runs` is the
/// backstop.
///
/// Owns the unlink-vs-preserve decision for the recovery outcome:
/// - [`FlipOutcome::FlippedToError`] — the success never committed (row was
///   `running`); the parquet we wrote is now orphaned — remove it.
/// - [`FlipOutcome::NotRunning`]     — no running row matched: either the
///   ambiguous success committed (its `result_path` is live) or the run was
///   cascade-deleted (a bounded on-disk leak). Either way, leave the file.
/// - `Err(_)`    — the flip itself failed (DB still down); the run stays wedged
///   until restart and the file is left in place.
pub(crate) async fn recover_ambiguous_finish(
    schedule_store: &ScheduleStore,
    base_dir: &str,
    run_id: i64,
    duration_ms: u64,
    result_path: Option<&str>,
    err: &crate::store::StoreError,
) {
    match schedule_store
        .fail_run_if_running(
            run_id,
            duration_ms,
            &format!("result persistence failed: {err}"),
        )
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
        Err(e2) => {
            tracing::error!(
                event_type = "scheduler_error",
                run_id,
                error = %e2,
                "failed to flip wedged run to error; run stays stuck until restart"
            );
        }
    }
}

/// Best-effort removal of a report-run parquet file from disk.
///
/// `relative` is a DB-stored path like `scheduled/foo/run_1.parquet`; it is
/// resolved against the executor's `base_dir`. Returns `true` if the file was
/// removed, `false` (with a logged warning) on failure. Shared by retention
/// cleanup, mid-flight orphan cleanup, and the run-deletion handler.
pub(crate) fn remove_result_file(base_dir: &str, relative: &str) -> bool {
    let full = format!("{}/{relative}", base_dir.trim_end_matches('/'));
    if let Err(e) = std::fs::remove_file(&full) {
        tracing::warn!(
            event_type = "result_file_cleanup_error",
            path = %full,
            error = %e,
            "failed to delete parquet result file"
        );
        false
    } else {
        true
    }
}

/// Write a `QueryResult` to a parquet file under `{data_dir}/scheduled/`, keyed only by the stable run ID.
///
/// Returns `(Some(relative_path), None)` on success, or `(None, Some(blob))`
/// as a zstd-JSON fallback if parquet writing fails.
///
/// A ZERO-ROW result always takes the blob. Parquet cannot carry it: the
/// writer serialises the rows as ndjson and lets `read_json` infer the
/// schema, so with no rows there is no schema to write and
/// `write_query_result_to_parquet` returns without creating a file. The blob
/// is the one representation that keeps the column NAMES, it is what
/// `get_report_run` already reads back, and it is what
/// `from_saved::resolve_latest` turns into an empty typed source. It also
/// costs almost nothing: a few dozen bytes of compressed JSON for a run
/// whose whole content is its header.
///
/// Recording it matters because `run=latest` resolves the NEWEST successful
/// run and refuses to look past it. A run persisted with neither a path nor
/// a blob would be a success nothing can read (ADR-0018 ruling 13).
fn write_result_parquet(
    pool: &ExecutorPool,
    run_id: i64,
    result: &trawl_api::value::QueryResult,
) -> (Option<String>, Option<Vec<u8>>) {
    if result.rows.is_empty() {
        return zstd_fallback(result);
    }

    let base = pool.base_dir().trim_end_matches('/');
    let relative = format!("scheduled/run_{run_id}.parquet");
    let full_path = format!("{base}/{relative}");
    let temp_path = format!("{full_path}.tmp");

    if let Err(e) = std::fs::create_dir_all(format!("{base}/scheduled")) {
        tracing::warn!(
            event_type = "scheduler_parquet_error",
            run_id,
            error = %e,
            "failed to create scheduled result directory, falling back to blob"
        );
        return zstd_fallback(result);
    }

    let executor = match trawl_engine::executor::Executor::new() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                event_type = "scheduler_parquet_error",
                run_id,
                error = %e,
                "failed to create executor for parquet write, falling back to blob"
            );
            return zstd_fallback(result);
        }
    };

    // Write to temp file, then atomic rename.
    let temp = std::path::Path::new(&temp_path);
    let final_path = std::path::Path::new(&full_path);
    if let Err(e) = executor.write_query_result_to_parquet(result, temp) {
        tracing::warn!(
            event_type = "scheduler_parquet_error",
            run_id,
            error = %e,
            "failed to write parquet result, falling back to blob"
        );
        let _ = std::fs::remove_file(temp);
        return zstd_fallback(result);
    }

    if let Err(e) = std::fs::rename(temp, final_path) {
        tracing::warn!(
            event_type = "scheduler_parquet_error",
            run_id,
            error = %e,
            "failed to rename parquet temp file, falling back to blob"
        );
        let _ = std::fs::remove_file(temp);
        return zstd_fallback(result);
    }

    (Some(relative), None)
}

/// Compress a `QueryResult` as a zstd JSON blob (the representation for a
/// zero-row run, and the fallback when a parquet write fails).
fn zstd_fallback(result: &trawl_api::value::QueryResult) -> (Option<String>, Option<Vec<u8>>) {
    let blob = serde_json::to_vec(result)
        .ok()
        .and_then(|json| zstd::encode_all(json.as_slice(), 3).ok());
    (None, blob)
}

/// Read back what [`zstd_fallback`] wrote: a zstd-compressed JSON
/// `QueryResult`, or `None` for an absent or unreadable blob.
///
/// Lives beside the writer so the two halves of the representation are one
/// pair. Both readers call it: `get_report_run`, which serves the run over
/// HTTP, and `from_saved::resolve_latest`, which turns a zero-row run's
/// column names back into a queryable source.
pub(crate) fn decode_result_blob(blob: Option<Vec<u8>>) -> Option<trawl_api::value::QueryResult> {
    let compressed = blob?;
    let decompressed = zstd::decode_all(compressed.as_slice())
        .inspect_err(|e| {
            tracing::warn!(
                event_type = "run_result_blob_zstd_decode_failed",
                error = %e,
                "failed to zstd-decode a report run's result blob"
            );
        })
        .ok()?;
    serde_json::from_slice::<trawl_api::value::QueryResult>(&decompressed)
        .inspect_err(|e| {
            tracing::warn!(
                event_type = "run_result_blob_deserialize_failed",
                error = %e,
                "failed to deserialize a report run's result blob"
            );
        })
        .ok()
}

/// Pg-backed coverage for the ambiguous-commit recovery *wiring* — the
/// orchestration that pairs each store outcome with an unlink-or-preserve file
/// action. The store primitives (`finish_run`, `fail_run_if_running`) are unit
/// tested in `tests/store_pg.rs`; these tests pin the file-cleanup decision the
/// store outcome alone doesn't prove (swapping the arms would leak files or
/// destroy committed results with the store tests still green). The Err/Err
/// double-fault arm needs fault injection and stays uncovered.
#[cfg(test)]
mod pg_tests {
    use sqlx::PgPool;

    use super::{finish_run_or_recover, recover_ambiguous_finish};
    use crate::store::{RunClaim, RunStatus, SavedQueryStore, ScheduleStore, StoreError};

    /// Seed a saved query + schedule + started (`running`) run, returning the
    /// schedule store, the owning saved-query id, and the run id.
    async fn seed_run(pool: &PgPool, name: &str) -> (ScheduleStore, i64, i64) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let sched_store = ScheduleStore::new(pool.clone());
        let saved = saved_store.create(1, name, "q").await.unwrap();
        let sched = sched_store
            .create_schedule(saved.id, 1, 300, None, None, 0, chrono::Utc::now())
            .await
            .unwrap();
        let rid = match sched_store
            .claim_run(sched.id, saved.id, "q", None, None)
            .await
            .unwrap()
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };
        (sched_store, saved.id, rid)
    }

    /// Write a dummy result file at `base_dir/rel`.
    fn touch(base_dir: &str, rel: &str) {
        let full = format!("{base_dir}/{rel}");
        std::fs::create_dir_all(std::path::Path::new(&full).parent().unwrap()).unwrap();
        std::fs::write(&full, b"parquet").unwrap();
    }

    fn exists(base_dir: &str, rel: &str) -> bool {
        std::path::Path::new(&format!("{base_dir}/{rel}")).exists()
    }

    /// `finish_run` → `Persisted`: the committed result's file is preserved.
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

    /// `finish_run` → `RunDeleted`: a run cascade-deleted mid-flight orphans
    /// the file just written, so the wiring must unlink it.
    #[sqlx::test]
    async fn finish_run_or_recover_unlinks_orphan_after_cascade_delete(pool: PgPool) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let (store, saved_id, rid) = seed_run(&pool, "orphan").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/orphan/run.parquet";
        touch(base, rel);

        // Cascade-delete the run row mid-flight.
        saved_store.delete(saved_id, 1).await.unwrap();

        finish_run_or_recover(&store, base, rid, 10, 1, None, Some(rel)).await;

        assert!(
            !exists(base, rel),
            "cascade-deleted run's orphaned file must be removed"
        );
    }

    /// Recovery `fail_run_if_running` → `FlippedToError`: the success never
    /// committed (row still `running`), so the parquet is orphaned and must
    /// be unlinked.
    #[sqlx::test]
    async fn recover_ambiguous_finish_unlinks_when_run_still_running(pool: PgPool) {
        let (store, _saved, rid) = seed_run(&pool, "wedged").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/wedged/run.parquet";
        touch(base, rel);

        let err = StoreError::Validation("boom".to_owned());
        recover_ambiguous_finish(&store, base, rid, 5, Some(rel), &err).await;

        assert!(
            !exists(base, rel),
            "a run flipped from running to error orphans its file — remove it"
        );
        let run = store.get_run(rid, 1).await.unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Error);
        assert_eq!(run.result_path, None);
    }

    /// Recovery `fail_run_if_running` → `NotRunning`: the ambiguous success
    /// already committed, so the guard matches zero rows and the live file is
    /// preserved.
    #[sqlx::test]
    async fn recover_ambiguous_finish_preserves_committed_success(pool: PgPool) {
        let (store, _saved, rid) = seed_run(&pool, "committed").await;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let rel = "scheduled/committed/run.parquet";
        touch(base, rel);

        // The ambiguous commit actually landed: the row is already 'success'.
        store
            .finish_run(rid, RunStatus::Success, 10, Some(7), None, None, Some(rel))
            .await
            .unwrap();

        let err = StoreError::Validation("boom".to_owned());
        recover_ambiguous_finish(&store, base, rid, 5, Some(rel), &err).await;

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
}

#[cfg(test)]
mod tests {
    use super::remove_result_file;

    /// An existing file is unlinked and the removal reports `true`.
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

    /// A missing file is a no-op that reports `false` (gates orphan logging).
    #[test]
    fn remove_result_file_returns_false_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();

        assert!(
            !remove_result_file(base, "scheduled/foo/does_not_exist.parquet"),
            "missing file must report false"
        );
    }
}
