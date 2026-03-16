// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background scheduler for periodic query execution.
//!
//! Polls [`ScheduleStore`] for enabled schedules and spawns query execution
//! tasks on the [`ExecutorPool`]. Results are zstd-compressed and stored as
//! report runs in the auth database.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use trawl_auth::KeyStore;
use trawl_auth::schedule::ScheduleStore;

use crate::config::SchedulerConfig;
use crate::pool::ExecutorPool;

/// Spawn the scheduler background task.
///
/// Returns a join handle and shutdown sender. Dropping the sender or
/// sending `true` signals the task to exit.
pub fn spawn_scheduler(
    schedule_store: Arc<Mutex<ScheduleStore>>,
    key_store: Arc<Mutex<KeyStore>>,
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
    schedule_store: Arc<Mutex<ScheduleStore>>,
    key_store: Arc<Mutex<KeyStore>>,
    pool: ExecutorPool,
    config: SchedulerConfig,
    timeout_secs: u64,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let poll_interval = Duration::from_secs(config.poll_interval_secs);
    let mut tick = tokio::time::interval(poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Clean up any runs left in 'running' state from a previous crash.
    {
        let store = schedule_store.lock();
        match store.cleanup_stale_runs() {
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

        poll_and_execute(&schedule_store, &key_store, &pool, &config, timeout_secs);

        // Periodic retention cleanup.
        retention_counter += 1;
        if retention_counter >= retention_interval {
            retention_counter = 0;
            let store = schedule_store.lock();
            if let Err(e) =
                store.delete_old_runs(config.report_retention_days, config.max_runs_per_schedule)
            {
                tracing::warn!(
                    event_type = "scheduler_retention_error",
                    error = %e,
                    "failed to clean up old report runs"
                );
            }
        }
    }
}

fn poll_and_execute(
    schedule_store: &Arc<Mutex<ScheduleStore>>,
    key_store: &Arc<Mutex<KeyStore>>,
    pool: &ExecutorPool,
    config: &SchedulerConfig,
    timeout_secs: u64,
) {
    let schedules = {
        let store = schedule_store.lock();
        match store.list_enabled_schedules() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    event_type = "scheduler_error",
                    error = %e,
                    "failed to list enabled schedules"
                );
                return;
            }
        }
    };

    for (schedule, saved_query) in schedules {
        // Check if max_runs reached.
        if let Some(max) = schedule.max_runs {
            let count = schedule_store.lock().count_runs(schedule.id).unwrap_or(0);
            if count >= max {
                continue;
            }
        }

        // Check if enough time has passed since last run.
        let should_run = {
            let store = schedule_store.lock();
            match store.latest_run(schedule.id) {
                Ok(Some(last)) => {
                    let last_started = chrono::DateTime::parse_from_rfc3339(&last.started_at)
                        .map(|dt| dt.timestamp())
                        .unwrap_or(0);
                    let now = chrono::Utc::now().timestamp();
                    let elapsed = (now - last_started).unsigned_abs();
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
            }
        };

        if !should_run {
            continue;
        }

        // Verify the owning API key is still active.
        let key_active = {
            let ks = key_store.lock();
            ks.is_key_active(schedule.key_id).unwrap_or(false)
        };
        if !key_active {
            tracing::debug!(
                event_type = "scheduler_skip",
                schedule_id = schedule.id,
                key_id = schedule.key_id,
                "skipping schedule: owning key is inactive"
            );
            continue;
        }

        // Atomically start a run (prevents concurrent execution).
        let run_id = {
            let store = schedule_store.lock();
            match store.start_run(schedule.id, saved_query.id, &saved_query.query) {
                Ok(Some(id)) => id,
                Ok(None) => continue, // Already running.
                Err(e) => {
                    tracing::error!(
                        event_type = "scheduler_error",
                        schedule_id = schedule.id,
                        error = %e,
                        "failed to start run"
                    );
                    continue;
                }
            }
        };

        // Spawn execution as a separate task so it doesn't block the poll loop.
        let store = Arc::clone(schedule_store);
        let pool = pool.clone();
        let query = saved_query.query.clone();
        let max_rows = config.report_max_rows;

        tokio::spawn(async move {
            execute_scheduled_query(store, pool, run_id, &query, max_rows, timeout_secs).await;
        });
    }
}

async fn execute_scheduled_query(
    schedule_store: Arc<Mutex<ScheduleStore>>,
    pool: ExecutorPool,
    run_id: i64,
    query: &str,
    _max_rows: usize,
    timeout_secs: u64,
) {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    // Execute the query on the pool (no debug capture, UTC timestamps).
    let outcome = pool.execute(query, timeout, false, 0).await;

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start.elapsed().as_millis() as u64;

    let store = schedule_store.lock();

    match outcome.result {
        Ok(query_result) => {
            let row_count = query_result.rows.len();

            // Compress result as zstd JSON.
            let result_data = serde_json::to_vec(&query_result)
                .ok()
                .and_then(|json| zstd::encode_all(json.as_slice(), 3).ok());

            if let Err(e) = store.finish_run(
                run_id,
                "success",
                duration_ms,
                Some(row_count),
                None,
                result_data.as_deref(),
            ) {
                tracing::error!(
                    event_type = "scheduler_error",
                    run_id,
                    error = %e,
                    "failed to finish run"
                );
            }

            tracing::info!(
                event_type = "scheduled_query_completed",
                run_id,
                duration_ms,
                row_count,
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

            if let Err(e2) =
                store.finish_run(run_id, status, duration_ms, None, Some(&error_msg), None)
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
