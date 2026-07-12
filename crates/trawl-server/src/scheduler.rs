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

use tokio::sync::watch;
use tokio::task::JoinHandle;

use fleet_auth::KeyStore;

use crate::config::SchedulerConfig;
use crate::policy::{Permission, TrawlAuthz as _};
use crate::pool::ExecutorPool;
use crate::store::{RunStatus, ScheduleStore};

/// Spawn the scheduler background task.
///
/// Returns a join handle and shutdown sender. Dropping the sender or
/// sending `true` signals the task to exit.
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

        poll_and_execute(&schedule_store, &key_store, &pool, &config, timeout_secs).await;

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

async fn poll_and_execute(
    schedule_store: &ScheduleStore,
    key_store: &KeyStore,
    pool: &ExecutorPool,
    config: &SchedulerConfig,
    timeout_secs: u64,
) {
    let schedules = match schedule_store.list_enabled_schedules().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                event_type = "scheduler_error",
                error = %e,
                "failed to list enabled schedules"
            );
            return;
        }
    };

    for (schedule, saved_query) in schedules {
        // Check if enough time has passed since last run.
        let should_run = match schedule_store.latest_run(schedule.id).await {
            Ok(Some(last)) => {
                let elapsed = chrono::Utc::now()
                    .signed_duration_since(last.started_at)
                    .num_seconds()
                    .unsigned_abs();
                elapsed >= schedule.interval_secs
            }
            Ok(None) => true, // Never run before.
            Err(e) => {
                tracing::warn!(
                    event_type = "scheduler_error",
                    schedule_id = schedule.id,
                    error = %e,
                    "failed to check latest run"
                );
                false
            }
        };

        if !should_run {
            continue;
        }

        // Gate on key liveness in the fleet keystore (AC6).
        if !owning_key_is_usable(key_store, schedule.id, schedule.key_id).await {
            continue;
        }

        // Claim a run in one transaction: the max_runs check and the insert
        // are atomic (FOR UPDATE on the schedule row), and the partial
        // unique index rejects a second concurrent 'running' row.
        let run_id = match schedule_store
            .claim_run(
                schedule.id,
                saved_query.id,
                &saved_query.query,
                schedule.max_runs,
            )
            .await
        {
            Ok(crate::store::RunClaim::Started(id)) => id,
            Ok(crate::store::RunClaim::AlreadyRunning | crate::store::RunClaim::MaxRunsReached) => {
                continue;
            }
            Err(e) => {
                tracing::error!(
                    event_type = "scheduler_error",
                    schedule_id = schedule.id,
                    error = %e,
                    "failed to start run"
                );
                continue;
            }
        };

        // Spawn execution as a separate task so it doesn't block the poll loop.
        let store = schedule_store.clone();
        let pool = pool.clone();
        let query = saved_query.query.clone();
        let query_name = saved_query.name.clone();
        let max_rows = config.report_max_rows;

        tokio::spawn(async move {
            execute_scheduled_query(
                store,
                pool,
                run_id,
                &query,
                &query_name,
                max_rows,
                timeout_secs,
            )
            .await;
        });
    }
}

/// Whether the schedule's owning key may still run scheduled queries: it
/// must be live in the fleet keystore (active + unexpired) AND hold a trawl
/// grant whose role still carries both [`Permission::Query`] and
/// [`Permission::SavedQuery`] — the two authorities a scheduled saved-query
/// run exercises. Revocation, expiry, grant-stripping, AND role downgrades
/// (analyst → reader/ingest) all stop scheduled execution (AC6): a role that
/// lacks either permission can no longer create or run saved queries
/// interactively, so it must not keep running them on a schedule. Lookup
/// failures skip conservatively.
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
    query_name: &str,
    _max_rows: usize,
    timeout_secs: u64,
) {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    // Execute the query on the pool (no debug capture, UTC timestamps).
    let outcome = pool
        .execute(pool.allocate_query_id(), query, timeout, false, 0)
        .await;

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start.elapsed().as_millis() as u64;

    match outcome.result {
        Ok(query_result) => {
            let row_count = query_result.rows.len();

            // Write result as parquet file.
            let (result_path, result_data) =
                write_result_parquet(&pool, run_id, query_name, &query_result);

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
            // Check if it was a timeout (ServerError::Timeout) or other error.
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
/// - `Ok(true)`  — the run row was updated; the file it points at stays.
/// - `Ok(false)` — the row was cascade-deleted mid-flight (its saved query or
///   schedule is gone); the file we just wrote is orphaned, so remove it.
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
        Ok(true) => {}
        Ok(false) => {
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
            // A transient app-state DB error (pg restart/failover). This is an
            // AMBIGUOUS COMMIT: for a single autocommit UPDATE the COMMIT can
            // land server-side while the client's ack is lost, so sqlx returns
            // Err even though the row was written to status='success' with
            // result_path set. We therefore must NOT delete the parquet here —
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
/// `running`. If the success COMMIT actually landed (status is already
/// `success`), the flip matches zero rows and we preserve the committed result
/// instead of unlinking the file it points at. If the DB is still down the flip
/// also fails and boot-time `cleanup_stale_runs` is the backstop.
///
/// Owns the unlink-vs-preserve decision for the recovery outcome:
/// - `Ok(true)`  — the success never committed (row was `running`); the parquet
///   we wrote is now orphaned — remove it.
/// - `Ok(false)` — no running row matched: either the ambiguous success
///   committed (its `result_path` is live) or the run was cascade-deleted (a
///   bounded on-disk leak). Either way, leave the file.
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
        Ok(true) => {
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
        Ok(false) => {}
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

/// Write a `QueryResult` to a parquet file under `{data_dir}/scheduled/{name}/`.
///
/// Returns `(Some(relative_path), None)` on success, or `(None, Some(blob))`
/// as a zstd-JSON fallback if parquet writing fails.
fn write_result_parquet(
    pool: &ExecutorPool,
    run_id: i64,
    query_name: &str,
    result: &trawl_api::value::QueryResult,
) -> (Option<String>, Option<Vec<u8>>) {
    if result.rows.is_empty() {
        return (None, None);
    }

    let base = pool.base_dir().trim_end_matches('/');
    let relative = format!("scheduled/{query_name}/run_{run_id}.parquet");
    let full_path = format!("{base}/{relative}");
    let temp_path = format!("{full_path}.tmp");

    // Ensure the parent directory exists.
    if let Err(e) = std::fs::create_dir_all(format!("{base}/scheduled/{query_name}")) {
        tracing::warn!(
            event_type = "scheduler_parquet_error",
            run_id,
            error = %e,
            "failed to create scheduled result directory, falling back to blob"
        );
        return zstd_fallback(result);
    }

    // Create a temporary executor for the parquet write.
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

/// Compress a `QueryResult` as a zstd JSON blob (fallback when parquet write fails).
fn zstd_fallback(result: &trawl_api::value::QueryResult) -> (Option<String>, Option<Vec<u8>>) {
    let blob = serde_json::to_vec(result)
        .ok()
        .and_then(|json| zstd::encode_all(json.as_slice(), 3).ok());
    (None, blob)
}

/// Pg-backed coverage for the ambiguous-commit recovery *wiring* — the
/// orchestration that pairs each store outcome with an unlink-or-preserve file
/// action. The store primitives (`finish_run`, `fail_run_if_running`) are unit
/// tested in `tests/store_pg.rs`; these tests pin the file-cleanup decision the
/// bool return alone doesn't prove (swapping the arms would leak files or
/// destroy committed results with the store tests still green). The Err/Err
/// double-fault arm needs fault injection and stays uncovered.
#[cfg(test)]
mod pg_tests {
    use sqlx::PgPool;

    use super::{finish_run_or_recover, recover_ambiguous_finish};
    use crate::store::{RunStatus, SavedQueryStore, ScheduleStore, StoreError};

    /// Seed a saved query + schedule + started (`running`) run, returning the
    /// schedule store, the owning saved-query id, and the run id.
    async fn seed_run(pool: &PgPool, name: &str) -> (ScheduleStore, i64, i64) {
        let saved_store = SavedQueryStore::new(pool.clone());
        let sched_store = ScheduleStore::new(pool.clone());
        let saved = saved_store.create(1, name, "q").await.unwrap();
        let sched = sched_store
            .create_schedule(saved.id, 1, 300, None)
            .await
            .unwrap();
        let rid = sched_store
            .start_run(sched.id, saved.id, "q")
            .await
            .unwrap()
            .unwrap();
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

    /// `finish_run` Ok(true): the committed result's file is preserved.
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

    /// `finish_run` Ok(false): a run cascade-deleted mid-flight orphans the
    /// file we just wrote, so the wiring must unlink it.
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

    /// Recovery `fail_run_if_running` Ok(true): the success never committed
    /// (row still `running`), so the parquet is orphaned and must be unlinked.
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

    /// Recovery `fail_run_if_running` Ok(false): the ambiguous success already
    /// committed, so the guard matches zero rows and the live file is preserved.
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
