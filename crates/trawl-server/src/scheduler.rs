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
use crate::store::ScheduleStore;

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

#[allow(clippy::too_many_lines)] // outcome handling incl. orphan cleanup is cohesive
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

            match schedule_store
                .finish_run(
                    run_id,
                    "success",
                    duration_ms,
                    Some(row_count),
                    None,
                    result_data.as_deref(),
                    result_path.as_deref(),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    // The run row was cascade-deleted mid-flight (its saved
                    // query or schedule is gone): the file we just wrote is
                    // orphaned — remove it.
                    if let Some(ref relative) = result_path
                        && remove_result_file(pool.base_dir(), relative)
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
                    tracing::error!(
                        event_type = "scheduler_error",
                        run_id,
                        error = %e,
                        "failed to finish run"
                    );
                }
            }

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
                ("timeout", format!("{e}"))
            } else {
                ("error", format!("{e}"))
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
                status,
                error = %e,
                "scheduled query failed"
            );
        }
    }
}

/// Write a `QueryResult` to a parquet file under `{data_dir}/scheduled/{name}/`.
///
/// Returns `(Some(relative_path), None)` on success, or `(None, Some(blob))`
/// as a zstd-JSON fallback if parquet writing fails.
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
