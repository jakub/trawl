//! Snapshot collection for the monitor dashboard.
//!
//! On each tick, reads atomics, semaphore permits, and tracker snapshots
//! to build a [`MonitorSnapshot`] for rendering. All reads are cheap and
//! non-blocking — no subscriptions or channels needed.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fleet_api::{ActiveQuerySnapshot, CompletedQuerySnapshot};

use crate::state::AppState;

/// Complete state snapshot consumed by the UI renderer on each tick.
#[derive(Debug)]
pub struct MonitorSnapshot {
    // -- header --
    pub hostname: String,
    pub listen_addr: String,
    pub uptime: Duration,
    pub version: &'static str,
    pub healthy: bool,

    // -- executor pool --
    pub pool_capacity: usize,
    pub pool_active: usize,

    // -- hot buffer --
    pub hot_buffer_events: usize,
    pub hot_buffer_max_events: usize,
    pub hot_buffer_bytes: usize,
    pub hot_buffer_max_bytes: usize,
    pub hot_buffer_batches: usize,

    // -- query throughput --
    pub total_queries: u64,
    pub query_rate: f64,
    pub query_errors: u64,
    pub query_timeouts: u64,

    // -- ingest --
    pub ingest_events: u64,
    pub ingest_rate: f64,
    pub ingest_rejected: u64,

    // -- SSE --
    pub sse_active: usize,
    pub sse_max: usize,

    // -- scheduler --
    pub scheduler_enabled: bool,
    pub scheduler_schedules: usize,

    // -- queries --
    pub recent_queries: Vec<CompletedQuerySnapshot>,
    pub active_queries: Vec<ActiveQuerySnapshot>,
}

/// Tracks counter deltas between ticks to compute rates (events/sec, queries/sec).
#[derive(Debug)]
pub struct RateTracker {
    last_query_count: u64,
    last_ingest_count: u64,
    last_tick: Instant,
    pub query_rate: f64,
    pub ingest_rate: f64,
}

impl Default for RateTracker {
    fn default() -> Self {
        Self {
            last_query_count: 0,
            last_ingest_count: 0,
            last_tick: Instant::now(),
            query_rate: 0.0,
            ingest_rate: 0.0,
        }
    }
}

impl RateTracker {
    /// Update rates from current counter values. Call once per tick.
    #[allow(clippy::cast_precision_loss)] // dashboard display — u64→f64 precision loss is fine
    pub fn update(&mut self, total_queries: u64, total_ingest: u64) {
        let elapsed = self.last_tick.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            let query_delta = total_queries.saturating_sub(self.last_query_count);
            let ingest_delta = total_ingest.saturating_sub(self.last_ingest_count);
            self.query_rate = query_delta as f64 / elapsed;
            self.ingest_rate = ingest_delta as f64 / elapsed;
        }
        self.last_query_count = total_queries;
        self.last_ingest_count = total_ingest;
        self.last_tick = Instant::now();
    }
}

/// Holds references to `AppState` fields plus cached values that don't change.
#[derive(Debug)]
pub struct MonitorState {
    state: AppState,
    hostname: String,
    listen_addr: String,
    sse_max: usize,
    scheduler_enabled: bool,
    pub rate_tracker: RateTracker,
    /// Health check runs on a slower cadence (every N ticks).
    health_counter: u32,
    last_healthy: bool,
}

impl MonitorState {
    /// Create a new monitor state from an `AppState` and server config values.
    pub fn new(
        state: AppState,
        listen_addr: String,
        sse_max: usize,
        scheduler_enabled: bool,
    ) -> Self {
        let hostname =
            hostname::get().map_or_else(|_| "unknown".into(), |h| h.to_string_lossy().into_owned());

        Self {
            state,
            hostname,
            listen_addr,
            sse_max,
            scheduler_enabled,
            rate_tracker: RateTracker::default(),
            health_counter: 0,
            last_healthy: true,
        }
    }

    /// Collect a snapshot of all dashboard fields. Cheap reads only.
    pub fn snapshot(&mut self) -> MonitorSnapshot {
        let pool = &self.state.query.pool;
        let tracker = &self.state.query.tracker;

        let pool_capacity = pool.capacity();
        let pool_active = pool_capacity - pool.available_permits();

        // Hot buffer stats (zeros when ingest is disabled).
        let (hb_events, hb_max_events, hb_bytes, hb_max_bytes, hb_batches) =
            if let Some(ref buf) = self.state.query.hot_buffer {
                let cfg = buf.config();
                (
                    buf.event_count(),
                    cfg.max_events,
                    buf.byte_count(),
                    cfg.max_bytes,
                    buf.batch_count(),
                )
            } else {
                (0, 0, 0, 0, 0)
            };

        let total_queries = self.state.total_queries.load(Ordering::Relaxed);
        let ingest_events = self.state.ingest.total_events.load(Ordering::Relaxed);
        let ingest_rejected = self.state.ingest.total_rejected.load(Ordering::Relaxed);

        self.rate_tracker.update(total_queries, ingest_events);

        // Count errors and timeouts from recent history.
        let recent = tracker.recent();
        let query_errors = recent.iter().filter(|q| q.error.is_some()).count() as u64;
        let query_timeouts = recent.iter().filter(|q| q.timed_out).count() as u64;

        let active_queries = tracker.active();

        // SSE connections: total permits minus available.
        let sse_available = self.state.query.sse_semaphore.available_permits();
        let sse_active = self.sse_max.saturating_sub(sse_available);

        // Scheduler: count enabled schedules (slower operation, cached in snapshot).
        let scheduler_schedules = if self.scheduler_enabled {
            self.state
                .auth
                .schedule
                .lock()
                .list_enabled_schedules()
                .map_or(0, |v| v.len())
        } else {
            0
        };

        // Health check on slower cadence (every 30 ticks).
        self.health_counter += 1;
        if self.health_counter >= 30 {
            self.health_counter = 0;
            self.last_healthy = pool.available_permits() > 0;
        }

        MonitorSnapshot {
            hostname: self.hostname.clone(),
            listen_addr: self.listen_addr.clone(),
            uptime: self.state.start_time.elapsed(),
            version: fleet_core::version::PKG_VERSION,
            healthy: self.last_healthy,
            pool_capacity,
            pool_active,
            hot_buffer_events: hb_events,
            hot_buffer_max_events: hb_max_events,
            hot_buffer_bytes: hb_bytes,
            hot_buffer_max_bytes: hb_max_bytes,
            hot_buffer_batches: hb_batches,
            total_queries,
            query_rate: self.rate_tracker.query_rate,
            query_errors,
            query_timeouts,
            ingest_events,
            ingest_rate: self.rate_tracker.ingest_rate,
            ingest_rejected,
            sse_active,
            sse_max: self.sse_max,
            scheduler_enabled: self.scheduler_enabled,
            scheduler_schedules,
            recent_queries: recent,
            active_queries,
        }
    }
}

/// Create a `MonitorState` from `AppState` and config values.
///
/// Convenience wrapper that extracts the relevant config fields.
pub fn from_app_state(
    state: AppState,
    listen_addr: &str,
    sse_max: usize,
    scheduler_enabled: bool,
) -> MonitorState {
    MonitorState::new(state, listen_addr.to_owned(), sse_max, scheduler_enabled)
}
