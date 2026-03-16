// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Periodic server stats emitter.
//!
//! Emits a `server_stats` INFO event at a fixed interval with
//! key metrics (pool utilization, hot buffer, uptime). Since
//! `WalLayer` ingests all tracing events into parquet, these
//! stats are queryable with trawl's own DSL.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Spawn the periodic stats emitter.
///
/// Emits `server_stats` every `interval`. Stops when `shutdown_rx`
/// receives a signal.
pub fn spawn_stats_emitter(
    state: &AppState,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let start_time = state.start_time;
    let total_queries = Arc::clone(&state.total_queries);
    let pool = state.query.pool.clone();
    let sse_semaphore = Arc::clone(&state.query.sse_semaphore);
    let hot_buffer = state.query.hot_buffer.clone();
    let fallback_glob = pool.fallback_glob().to_string();
    let wal_dir = state
        .ingest
        .wal_writer
        .as_ref()
        .map(|w| w.dir().to_path_buf());

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the immediate first tick.
        tick.tick().await;

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    emit_stats(
                        start_time,
                        &total_queries,
                        &pool,
                        &sse_semaphore,
                        hot_buffer.as_ref(),
                    );
                    crate::metrics::collect_gauges(hot_buffer.as_ref(), &fallback_glob, wal_dir.as_deref());
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!(
                        event_type = "lifecycle",
                        action = "stats_emitter_stop",
                        "stats emitter shutting down"
                    );
                    break;
                }
            }
        }
    })
}

/// Emit a single `server_stats` event.
fn emit_stats(
    start_time: Instant,
    total_queries: &std::sync::atomic::AtomicU64,
    pool: &crate::pool::ExecutorPool,
    sse_semaphore: &tokio::sync::Semaphore,
    hot_buffer: Option<&Arc<crate::hot_buffer::HotBuffer>>,
) {
    let uptime_secs = start_time.elapsed().as_secs();
    let total_queries = total_queries.load(std::sync::atomic::Ordering::Relaxed);
    let pool_available = pool.available_permits();
    let pool_max = pool.capacity();
    let pool_active = pool_max - pool_available;
    let sse_available = sse_semaphore.available_permits();

    let (hot_buffer_events, hot_buffer_bytes, hot_buffer_batches) = if let Some(buf) = hot_buffer {
        (buf.event_count(), buf.byte_count(), buf.batch_count())
    } else {
        (0, 0, 0)
    };

    tracing::info!(
        event_type = "server_stats",
        uptime_secs,
        total_queries,
        pool_available,
        pool_max,
        pool_active,
        sse_available,
        hot_buffer_events,
        hot_buffer_bytes,
        hot_buffer_batches,
        "periodic server stats"
    );
}
