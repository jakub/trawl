// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snapshot collection for the monitor dashboard.
//!
//! On each tick, reads atomics, semaphore permits, and tracker snapshots
//! to build a [`MonitorSnapshot`] for rendering. All reads are cheap and
//! non-blocking — no subscriptions or channels needed.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use trawl_api::{ActiveQuerySnapshot, CompletedQuerySnapshot, DashboardSnapshot};

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
    /// The subset of `pool_active` held by work whose request already
    /// answered (ADR-0024).
    pub pool_retained: usize,

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

    // -- ingest (HTTP) --
    pub ingest_events: u64,
    pub ingest_rate: f64,
    pub ingest_rejected: u64,

    // -- syslog --
    pub syslog_enabled: bool,
    pub syslog_events_udp: u64,
    pub syslog_events_tcp: u64,
    pub syslog_rate: f64,
    pub syslog_parse_errors: u64,
    pub syslog_dropped: u64,
    pub syslog_tcp_connections: u64,

    // -- WAL --
    pub wal_files: u64,
    pub wal_bytes: u64,

    // -- compaction --
    pub last_compaction_secs: Option<u64>,
    pub compaction_runs: u64,
    pub compaction_errors: u64,

    // -- storage --
    pub parquet_files: u64,
    pub parquet_bytes: u64,

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

impl MonitorSnapshot {
    /// Convert to the shared wire type for API responses and shared rendering.
    pub fn to_dashboard_snapshot(&self) -> DashboardSnapshot {
        DashboardSnapshot {
            hostname: self.hostname.clone(),
            listen_addr: self.listen_addr.clone(),
            uptime_secs: self.uptime.as_secs(),
            version: self.version.to_owned(),
            healthy: self.healthy,
            pool_capacity: self.pool_capacity,
            pool_active: self.pool_active,
            pool_retained: self.pool_retained,
            hot_buffer_events: self.hot_buffer_events,
            hot_buffer_max_events: self.hot_buffer_max_events,
            hot_buffer_bytes: self.hot_buffer_bytes,
            hot_buffer_max_bytes: self.hot_buffer_max_bytes,
            hot_buffer_batches: self.hot_buffer_batches,
            total_queries: self.total_queries,
            query_rate: self.query_rate,
            query_errors: self.query_errors,
            query_timeouts: self.query_timeouts,
            ingest_events: self.ingest_events,
            ingest_rate: self.ingest_rate,
            ingest_rejected: self.ingest_rejected,
            syslog_enabled: self.syslog_enabled,
            syslog_events_udp: self.syslog_events_udp,
            syslog_events_tcp: self.syslog_events_tcp,
            syslog_rate: self.syslog_rate,
            syslog_parse_errors: self.syslog_parse_errors,
            syslog_dropped: self.syslog_dropped,
            syslog_tcp_connections: self.syslog_tcp_connections,
            wal_files: self.wal_files,
            wal_bytes: self.wal_bytes,
            last_compaction_secs: self.last_compaction_secs,
            compaction_runs: self.compaction_runs,
            compaction_errors: self.compaction_errors,
            parquet_files: self.parquet_files,
            parquet_bytes: self.parquet_bytes,
            sse_active: self.sse_active,
            sse_max: self.sse_max,
            scheduler_enabled: self.scheduler_enabled,
            scheduler_schedules: self.scheduler_schedules,
            recent_queries: self.recent_queries.clone(),
            active_queries: self.active_queries.clone(),
        }
    }
}

/// Tracks counter deltas between ticks to compute rates (events/sec, queries/sec).
///
/// Uses an exponential moving average (EMA) to smooth out bursty ingest
/// patterns. With a smoothing factor of 0.3, it takes ~5 ticks for a
/// step change to reach ~83% of the new value — enough to eliminate the
/// 0/1000/0/1000 flicker while still being responsive.
#[derive(Debug)]
pub struct RateTracker {
    last_query_count: u64,
    last_ingest_count: u64,
    last_syslog_count: u64,
    last_tick: Instant,
    pub query_rate: f64,
    pub ingest_rate: f64,
    pub syslog_rate: f64,
}

/// EMA smoothing factor (0..1). Lower = smoother but slower to respond.
const RATE_SMOOTHING: f64 = 0.3;

impl Default for RateTracker {
    fn default() -> Self {
        Self {
            last_query_count: 0,
            last_ingest_count: 0,
            last_syslog_count: 0,
            last_tick: Instant::now(),
            query_rate: 0.0,
            ingest_rate: 0.0,
            syslog_rate: 0.0,
        }
    }
}

impl RateTracker {
    /// Update rates from current counter values. Call once per tick.
    #[allow(clippy::cast_precision_loss)] // dashboard display — u64→f64 precision loss is fine
    pub fn update(&mut self, total_queries: u64, total_ingest: u64, total_syslog: u64) {
        let elapsed = self.last_tick.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            let query_delta = total_queries.saturating_sub(self.last_query_count);
            let ingest_delta = total_ingest.saturating_sub(self.last_ingest_count);
            let syslog_delta = total_syslog.saturating_sub(self.last_syslog_count);
            let instant_query = query_delta as f64 / elapsed;
            let instant_ingest = ingest_delta as f64 / elapsed;
            let instant_syslog = syslog_delta as f64 / elapsed;
            self.query_rate += RATE_SMOOTHING * (instant_query - self.query_rate);
            self.ingest_rate += RATE_SMOOTHING * (instant_ingest - self.ingest_rate);
            self.syslog_rate += RATE_SMOOTHING * (instant_syslog - self.syslog_rate);
        }
        self.last_query_count = total_queries;
        self.last_ingest_count = total_ingest;
        self.last_syslog_count = total_syslog;
        self.last_tick = Instant::now();
    }
}

/// The `AppState` handle plus the values the dashboard carries between ticks.
#[derive(Debug)]
pub struct MonitorState {
    state: AppState,
    hostname: String,
    listen_addr: String,
    sse_max: usize,
    scheduler_enabled: bool,
    syslog_enabled: bool,
    pub rate_tracker: RateTracker,
    /// Health check runs on a slower cadence (every N ticks).
    health_counter: u32,
    last_healthy: bool,
    /// Enabled-schedule count, pushed by the async snapshot collector loop
    /// (the store is async; `snapshot()` stays sync and just reads this).
    cached_schedule_count: usize,
}

impl MonitorState {
    pub fn new(
        state: AppState,
        listen_addr: String,
        sse_max: usize,
        scheduler_enabled: bool,
        syslog_enabled: bool,
    ) -> Self {
        let hostname =
            hostname::get().map_or_else(|_| "unknown".into(), |h| h.to_string_lossy().into_owned());

        Self {
            state,
            hostname,
            listen_addr,
            sse_max,
            scheduler_enabled,
            syslog_enabled,
            rate_tracker: RateTracker::default(),
            health_counter: 0,
            last_healthy: true,
            cached_schedule_count: 0,
        }
    }

    /// Update the enabled-schedule count shown on the dashboard. Called by
    /// the async snapshot collector, which owns the (async) store access.
    pub fn set_schedule_count(&mut self, count: usize) {
        self.cached_schedule_count = count;
    }

    /// Whether the scheduler is enabled (the collector only polls the
    /// schedule count when it is).
    pub fn scheduler_enabled(&self) -> bool {
        self.scheduler_enabled
    }

    /// Collect a snapshot of all dashboard fields. Cheap reads only.
    #[allow(clippy::too_many_lines)]
    pub fn snapshot(&mut self) -> MonitorSnapshot {
        let pool = &self.state.query.pool;
        let tracker = &self.state.query.tracker;

        let pool_capacity = pool.capacity();
        let pool_active = pool_capacity - pool.available_permits();
        let pool_retained = pool.retained();

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

        // Syslog stats (zeros when syslog is disabled).
        let (syslog_udp, syslog_tcp, syslog_parse, syslog_drop, syslog_conns) =
            if let Some(ref stats) = self.state.ingest.syslog_stats {
                (
                    stats.events_udp.load(Ordering::Relaxed),
                    stats.events_tcp.load(Ordering::Relaxed),
                    stats.parse_errors.load(Ordering::Relaxed),
                    stats.dropped.load(Ordering::Relaxed),
                    stats.tcp_connections.load(Ordering::Relaxed),
                )
            } else {
                (0, 0, 0, 0, 0)
            };
        let syslog_total = syslog_udp + syslog_tcp;

        self.rate_tracker
            .update(total_queries, ingest_events, syslog_total);

        // Count errors and timeouts from recent history.
        let recent = tracker.recent();
        let query_errors = recent.iter().filter(|q| q.error.is_some()).count() as u64;
        let query_timeouts = recent.iter().filter(|q| q.timed_out).count() as u64;

        let active_queries = tracker.active();

        // SSE connections: total permits minus available.
        let sse_available = self.state.query.sse_semaphore.available_permits();
        let sse_active = self.sse_max.saturating_sub(sse_available);

        // Scheduler: the enabled-schedule count is pushed by the async
        // snapshot collector via `set_schedule_count` (the pg store is
        // async and this method must stay sync/cheap).
        let scheduler_schedules = self.cached_schedule_count;

        // Health check on slower cadence (every 30 ticks).
        self.health_counter += 1;
        if self.health_counter >= 30 {
            self.health_counter = 0;
            self.last_healthy = pool.available_permits() > 0;
        }

        // Compaction stats (None/zeros when ingest is disabled).
        let (last_compaction_secs, compaction_runs, compaction_errors) =
            if let Some(ref stats) = self.state.ingest.compaction_stats {
                let epoch_secs = stats.last_run_epoch_secs.load(Ordering::Relaxed);
                let last_secs = if epoch_secs == 0 {
                    None
                } else {
                    let now_epoch = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    Some(now_epoch.saturating_sub(epoch_secs))
                };
                (
                    last_secs,
                    stats.total_runs.load(Ordering::Relaxed),
                    stats.total_errors.load(Ordering::Relaxed),
                )
            } else {
                (None, 0, 0)
            };

        // WAL and parquet stats from cached gauge values.
        let (wal_files, wal_bytes) = crate::metrics::cached_wal_stats();
        let (parquet_files, parquet_bytes) = crate::metrics::cached_parquet_stats();

        MonitorSnapshot {
            hostname: self.hostname.clone(),
            listen_addr: self.listen_addr.clone(),
            uptime: self.state.start_time.elapsed(),
            version: trawl_core::version::PKG_VERSION,
            healthy: self.last_healthy,
            pool_capacity,
            pool_active,
            pool_retained,
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
            syslog_enabled: self.syslog_enabled,
            syslog_events_udp: syslog_udp,
            syslog_events_tcp: syslog_tcp,
            syslog_rate: self.rate_tracker.syslog_rate,
            syslog_parse_errors: syslog_parse,
            syslog_dropped: syslog_drop,
            syslog_tcp_connections: syslog_conns,
            wal_files,
            wal_bytes,
            last_compaction_secs,
            compaction_runs,
            compaction_errors,
            parquet_files,
            parquet_bytes,
            sse_active,
            sse_max: self.sse_max,
            scheduler_enabled: self.scheduler_enabled,
            scheduler_schedules,
            recent_queries: recent,
            active_queries,
        }
    }
}

/// Convenience wrapper over [`MonitorState::new`] that takes `listen_addr`
/// by reference.
pub fn from_app_state(
    state: AppState,
    listen_addr: &str,
    sse_max: usize,
    scheduler_enabled: bool,
    syslog_enabled: bool,
) -> MonitorState {
    MonitorState::new(
        state,
        listen_addr.to_owned(),
        sse_max,
        scheduler_enabled,
        syslog_enabled,
    )
}
