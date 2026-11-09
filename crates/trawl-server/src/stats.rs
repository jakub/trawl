// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Periodic server stats emitter.
//!
//! Emits a `server_stats` INFO event at a fixed interval with
//! key metrics (pool utilization, hot buffer, uptime). With internal
//! telemetry enabled, `WalLayer` persists the event like any other
//! trawld log record, so these stats are queryable with trawl's own DSL.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Spawn the periodic stats emitter.
///
/// Emits `server_stats` and refreshes the shared gauges every `interval`.
/// A gauge collection that panics is logged and the next tick collects
/// again. Stops when `shutdown_rx` receives a signal.
pub fn spawn_stats_emitter(
    state: &AppState,
    interval: Duration,
    shutdown_rx: watch::Receiver<bool>,
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
    let repin_jobs = Arc::clone(&state.repin_jobs);
    let engine_sample = state.engine_sample.clone();
    let scratch_dir = pool.scratch().path().to_path_buf();

    tokio::spawn(run(interval, shutdown_rx, move || {
        emit_stats(
            start_time,
            &total_queries,
            &pool,
            &sse_semaphore,
            hot_buffer.as_ref(),
        );
        let hot_buffer = hot_buffer.clone();
        let publication = pool.publication();
        let fallback_glob = fallback_glob.clone();
        let wal_dir = wal_dir.clone();
        let repin_jobs = Arc::clone(&repin_jobs);
        let scratch_dir = scratch_dir.clone();
        let query = crate::metrics::QueryGauges::read(&pool, &engine_sample);
        move || {
            crate::metrics::collect_gauges(
                hot_buffer.as_ref(),
                &publication,
                &fallback_glob,
                wal_dir.as_deref(),
                &scratch_dir,
                query,
                &repin_jobs,
            );
        }
    }))
}

/// Run `on_tick` every `interval`, skipping the immediate first tick, and
/// run the blocking work it returns on the blocking pool.
///
/// Storage scans and competing collection attempts can block, so the work
/// is awaited inside the tick: ticks never overlap, and shutdown waits for
/// the collection in flight before leaving the loop. Work that panics or is
/// cancelled emits `stats_collection_failed` and the loop goes on. The
/// caches keep their last complete sample (ADR-0033), so nothing is cleared
/// or zeroed here.
async fn run<F, W>(interval: Duration, mut shutdown_rx: watch::Receiver<bool>, mut on_tick: F)
where
    F: FnMut() -> W,
    W: FnOnce() + Send + 'static,
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let work = on_tick();
                if let Err(e) = tokio::task::spawn_blocking(work).await {
                    // The join error's own text can quote the panic payload.
                    tracing::warn!(
                        event_type = "stats_collection_failed",
                        error = %crate::error::join_failure_text("stats gauge collection", e),
                        "stats gauge collection did not complete"
                    );
                }
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
    // A subset of pool_active, not an addition to it (ADR-0024).
    let pool_retained = pool.retained();
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
        pool_retained,
        sse_available,
        hot_buffer_events,
        hot_buffer_bytes,
        hot_buffer_batches,
        "periodic server stats"
    );
}

/// Event capture for the loop tests below.
#[cfg(test)]
mod support {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub(super) type Events = Arc<Mutex<Vec<HashMap<String, String>>>>;

    struct Fields<'a>(&'a mut HashMap<String, String>);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
    }

    pub(super) struct Capture(pub(super) Events);

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
}

/// The first collection panics with a payload that names a path; the
/// next tick collects again, the failure event quotes neither, and
/// shutdown still ends the loop. The panic's default stderr report is
/// expected test output.
#[cfg(test)]
#[tokio::test(start_paused = true)]
async fn a_failed_collection_does_not_end_the_loop() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::mpsc;
    use tracing_subscriber::prelude::*;

    use support::{Capture, Events};

    // The run future is polled on this thread, so its events land here.
    let events = Events::default();
    let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&events)));
    let _capturing = tracing::subscriber::set_default(subscriber);

    let calls = Arc::new(AtomicUsize::new(0));
    let (collected_tx, mut collected_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let on_tick = {
        let calls = Arc::clone(&calls);
        move || {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            let collected_tx = collected_tx.clone();
            move || {
                if call == 0 {
                    std::panic::panic_any("PRIVATE-PAYLOAD /secret/path");
                }
                collected_tx.send(call).unwrap();
            }
        }
    };
    let handle = tokio::spawn(run(Duration::from_secs(60), shutdown_rx, on_tick));

    // Paused time advances to each tick on its own once nothing else
    // can run; the second tick follows the panicked first.
    assert_eq!(collected_rx.recv().await, Some(1));

    shutdown_tx.send(true).unwrap();
    handle.await.expect("the stats loop returns after shutdown");

    let events = events.lock().unwrap();
    let failures: Vec<_> = events
        .iter()
        .filter(|e| e.get("event_type").map(String::as_str) == Some("stats_collection_failed"))
        .collect();
    assert_eq!(failures.len(), 1, "one failure event: {events:?}");
    for value in failures[0].values() {
        assert!(
            !value.contains("PRIVATE") && !value.contains("/secret"),
            "the failure event quotes the panic payload: {value}"
        );
    }
    assert_eq!(
        failures[0].get("error").map(String::as_str),
        Some("stats gauge collection task panicked")
    );
}

/// Shutdown sent while a collection runs waits for that collection.
#[cfg(test)]
#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_the_collection_in_flight() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::mpsc;

    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let finished = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let on_tick = {
        let finished = Arc::clone(&finished);
        move || {
            // Only the first collection holds; any later one is a no-op.
            let gate = release_rx.lock().unwrap().take();
            let started_tx = started_tx.clone();
            let finished = Arc::clone(&finished);
            move || {
                if let Some(gate) = gate {
                    started_tx.send(()).unwrap();
                    gate.recv().unwrap();
                    finished.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    };
    let handle = tokio::spawn(run(Duration::from_secs(60), shutdown_rx, on_tick));

    started_rx.recv().await.unwrap();
    shutdown_tx.send(true).unwrap();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !handle.is_finished(),
        "the loop left before its collection finished"
    );

    release_tx.send(()).unwrap();
    handle.await.expect("the stats loop returns after shutdown");
    assert_eq!(finished.load(Ordering::SeqCst), 1);
}
