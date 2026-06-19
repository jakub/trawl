// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared application state for axum handlers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use std::time::Instant;
use tokio::sync::Semaphore;

use trawl_api::{DashboardSnapshot, ServiceSchema};
use trawl_auth::{HistoryStore, KeyStore, SavedQueryStore, ScheduleStore};
use trawl_engine::value::SchemaResult;

use crate::auth::AuthCache;
use crate::bus::LocalEventBus;
use crate::config::{Config, RateLimitConfig};
use crate::hot_buffer::{HotBuffer, HotBufferConfig};
use crate::ingest::pipeline::PipelineWriter;
use crate::ingest::wal::WalWriter;
use crate::pool::ExecutorPool;
use crate::query_log::QueryLog;
use crate::tracker::QueryTracker;

/// Shared state injected into handlers via axum's `State` extractor.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Query execution resources.
    pub query: QueryState,
    /// Authentication resources.
    pub auth: AuthState,
    /// Ingest pipeline resources.
    pub ingest: IngestState,
    /// Server start time (for uptime calculation).
    pub start_time: Instant,
    /// Total queries executed since startup (for stats endpoint).
    pub total_queries: Arc<AtomicU64>,
    /// Prometheus metrics handle for rendering the scrape endpoint.
    pub metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    /// Latest dashboard snapshot, updated every ~1s by the snapshot collector.
    /// Available even when the terminal monitor is disabled (systemd, `--no-monitor`).
    pub dashboard_snapshot: Arc<Mutex<Option<DashboardSnapshot>>>,
}

/// Query execution state: pool, tracker, timeout, and schema cache.
#[derive(Debug, Clone)]
pub struct QueryState {
    /// Bounded executor pool for query execution.
    pub pool: ExecutorPool,
    /// Query timeout in seconds.
    pub timeout_secs: u64,
    /// Query lifecycle tracker (active + history).
    pub tracker: Arc<QueryTracker>,
    /// Schema cache TTL in seconds.
    pub schema_cache_ttl_secs: u64,
    /// Maximum rows for export responses (bypasses `max_result_rows`).
    pub max_export_rows: usize,
    /// Cached schema introspection result with TTL.
    ///
    /// Uses `Mutex` (not `RwLock`) to prevent thundering herd: only one
    /// request refreshes the cache while others wait on the lock.
    pub schema_cache: Arc<tokio::sync::Mutex<Option<CachedSchema>>>,
    /// Cached field value samples for autocomplete (shared TTL with schema cache).
    pub field_values_cache: Arc<tokio::sync::Mutex<HashMap<String, CachedFieldValues>>>,
    /// Hot buffer for fresh events not yet compacted to parquet.
    pub hot_buffer: Option<Arc<HotBuffer>>,
    /// Pre-computed per-service schema from background refresh job.
    /// Uses `parking_lot::Mutex` (like `dashboard_snapshot`) for fast reads.
    pub service_schema_cache: Arc<Mutex<Option<CachedServiceSchema>>>,
    /// Semaphore bounding concurrent SSE streaming connections.
    pub sse_semaphore: Arc<Semaphore>,
    /// Optional ndjson query debug log.
    pub query_log: Option<Arc<QueryLog>>,
}

/// Authentication state: key store, history store, saved queries, and database path.
#[derive(Debug, Clone)]
pub struct AuthState {
    /// Shared `KeyStore` connection, opened once at startup.
    pub key_store: Arc<Mutex<KeyStore>>,
    /// Shared `HistoryStore` connection for query history persistence.
    pub history: Arc<Mutex<HistoryStore>>,
    /// Shared `SavedQueryStore` connection for saved queries.
    pub saved: Arc<Mutex<SavedQueryStore>>,
    /// Shared `ScheduleStore` connection for scheduled queries and report runs.
    pub schedule: Arc<Mutex<ScheduleStore>>,
    /// Path to the `SQLite` auth database (kept for admin commands).
    pub db_path: Arc<PathBuf>,
    /// In-memory auth token cache (skips argon2id on hits).
    pub auth_cache: Arc<AuthCache>,
}

/// Ingest pipeline state.
#[derive(Debug, Clone)]
pub struct IngestState {
    /// WAL writer for ingested events (None if ingest is disabled).
    pub wal_writer: Option<Arc<WalWriter>>,
    /// Shared pipeline writer (WAL + hot buffer + event bus). None if ingest is disabled.
    pub pipeline: Option<Arc<PipelineWriter>>,
    /// Event bus for real-time fanout to subscribers (None if ingest is disabled).
    pub event_bus: Option<Arc<LocalEventBus>>,
    /// Total events ingested since startup (for monitor dashboard).
    pub total_events: Arc<AtomicU64>,
    /// Total events rejected since startup (for monitor dashboard).
    pub total_rejected: Arc<AtomicU64>,
    /// Syslog ingest counters (None if syslog is disabled).
    pub syslog_stats: Option<Arc<SyslogStats>>,
    /// Compaction cycle counters (None if ingest is disabled).
    pub compaction_stats: Option<Arc<CompactionStats>>,
}

/// Shared counters for syslog ingest stats (dashboard + monitoring).
pub struct SyslogStats {
    /// Total events received via UDP.
    pub events_udp: AtomicU64,
    /// Total events received via TCP.
    pub events_tcp: AtomicU64,
    /// Total unparseable syslog messages.
    pub parse_errors: AtomicU64,
    /// Total events dropped due to channel backpressure.
    pub dropped: AtomicU64,
    /// Current active TCP connections (incremented on connect, decremented on close).
    pub tcp_connections: AtomicU64,
}

impl Default for SyslogStats {
    fn default() -> Self {
        Self {
            events_udp: AtomicU64::new(0),
            events_tcp: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            tcp_connections: AtomicU64::new(0),
        }
    }
}

impl SyslogStats {
    /// Combined event count (UDP + TCP).
    pub fn total_events(&self) -> u64 {
        self.events_udp.load(Ordering::Relaxed) + self.events_tcp.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for SyslogStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyslogStats")
            .field("events_udp", &self.events_udp.load(Ordering::Relaxed))
            .field("events_tcp", &self.events_tcp.load(Ordering::Relaxed))
            .field("parse_errors", &self.parse_errors.load(Ordering::Relaxed))
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .field(
                "tcp_connections",
                &self.tcp_connections.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// Shared counters for compaction cycle tracking (dashboard + monitoring).
pub struct CompactionStats {
    /// Unix epoch seconds of last successful compaction (0 = never).
    pub last_run_epoch_secs: AtomicU64,
    /// Total successful compaction cycles.
    pub total_runs: AtomicU64,
    /// Total compaction failures: failed WAL-compaction cycles PLUS
    /// per-service daily-rollup failures and quarantined-input data-loss
    /// counts (so this can exceed `total_runs` — it is a failure/data-loss
    /// tally, not a cycle count).
    pub total_errors: AtomicU64,
}

impl Default for CompactionStats {
    fn default() -> Self {
        Self {
            last_run_epoch_secs: AtomicU64::new(0),
            total_runs: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
        }
    }
}

impl std::fmt::Debug for CompactionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactionStats")
            .field(
                "last_run_epoch_secs",
                &self.last_run_epoch_secs.load(Ordering::Relaxed),
            )
            .field("total_runs", &self.total_runs.load(Ordering::Relaxed))
            .field("total_errors", &self.total_errors.load(Ordering::Relaxed))
            .finish()
    }
}

/// HTTP transport config consumed at router/server construction time.
///
/// These fields are only needed when building the axum router and TLS
/// listener — no handler accesses them, so they stay out of `AppState`.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Maximum request body size in bytes.
    pub max_request_body_bytes: usize,
    /// Maximum concurrent HTTP requests.
    pub max_concurrent_requests: usize,
    /// Graceful shutdown drain timeout in seconds.
    pub shutdown_drain_secs: u64,
    /// Allowed CORS origins (empty = no CORS headers sent).
    pub cors_allowed_origins: Vec<String>,
    /// Max body size for ingest requests (None if ingest disabled).
    pub ingest_max_body_bytes: Option<usize>,
    /// Per-role rate limiting config.
    pub rate_limit: RateLimitConfig,
}

/// A cached schema result with an expiry timestamp.
#[derive(Debug, Clone)]
pub struct CachedSchema {
    /// The cached schema data.
    pub result: SchemaResult,
    /// When this cache entry was created.
    pub cached_at: Instant,
    /// Earliest date from partition directory names (YYYY-MM-DD).
    pub earliest_date: Option<String>,
    /// Latest date from partition directory names (YYYY-MM-DD).
    pub latest_date: Option<String>,
    /// Total byte size of all parquet files.
    pub total_bytes: u64,
    /// Distinct service names from parquet filenames.
    pub services: Vec<String>,
}

/// A cached field value sample with an expiry timestamp.
#[derive(Debug, Clone)]
pub struct CachedFieldValues {
    /// Sampled distinct values for the field.
    pub values: Vec<String>,
    /// When this cache entry was created.
    pub cached_at: Instant,
}

/// Pre-computed per-service schema from the background refresh job.
#[derive(Debug, Clone)]
pub struct CachedServiceSchema {
    /// Per-service schema and statistics.
    pub services: Vec<ServiceSchema>,
    /// When this cache was last refreshed.
    pub cached_at: Instant,
}

impl AppState {
    /// Construct app state from a validated [`Config`].
    ///
    /// Opens the auth database once at startup. Returns an error if the
    /// database cannot be opened or initialized.
    pub fn from_config(
        config: &Config,
        metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    ) -> Result<(Self, HttpConfig), trawl_auth::AuthError> {
        let key_store = KeyStore::open(&config.auth.db_path)?;
        let history = HistoryStore::open(&config.auth.db_path)?;
        let saved = SavedQueryStore::open(&config.auth.db_path)?;
        let schedule = ScheduleStore::open(&config.auth.db_path)?;

        let (wal_writer, event_bus, hot_buffer, pipeline) = if config.ingest.enabled {
            let writer = Arc::new(WalWriter::new(config.wal_dir()));
            let bus = Arc::new(LocalEventBus::new(config.ingest.event_bus_capacity));
            let buffer = Arc::new(HotBuffer::new(HotBufferConfig {
                max_events: config.ingest.hot_buffer_max_events,
                max_bytes: config.ingest.hot_buffer_max_bytes,
            }));
            let pipeline = Arc::new(PipelineWriter::new(
                Arc::clone(&writer),
                Some(Arc::clone(&buffer)),
                Some(Arc::clone(&bus)),
            ));
            (Some(writer), Some(bus), Some(buffer), Some(pipeline))
        } else {
            (None, None, None, None)
        };

        let auth_cache = Arc::new(AuthCache::new(std::time::Duration::from_secs(
            config.auth.auth_cache_ttl_secs,
        )));

        let state = Self {
            query: QueryState {
                pool: ExecutorPool::new(
                    config.data.base_dir().to_string_lossy().into_owned(),
                    config.server.max_concurrent_queries,
                    config.server.max_result_rows,
                    hot_buffer.clone(),
                ),
                timeout_secs: config.server.timeout_secs,
                tracker: Arc::new(QueryTracker::with_capacity(config.server.max_query_history)),
                schema_cache_ttl_secs: config.server.schema_cache_ttl_secs,
                max_export_rows: config.server.max_export_rows,
                schema_cache: Arc::new(tokio::sync::Mutex::new(None)),
                field_values_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                hot_buffer,
                service_schema_cache: Arc::new(Mutex::new(None)),
                sse_semaphore: Arc::new(Semaphore::new(config.server.max_sse_connections)),
                query_log: None,
            },
            auth: AuthState {
                key_store: Arc::new(Mutex::new(key_store)),
                history: Arc::new(Mutex::new(history)),
                saved: Arc::new(Mutex::new(saved)),
                schedule: Arc::new(Mutex::new(schedule)),
                db_path: Arc::new(config.auth.db_path.clone()),
                auth_cache,
            },
            ingest: IngestState {
                wal_writer,
                pipeline,
                event_bus,
                total_events: Arc::new(AtomicU64::new(0)),
                total_rejected: Arc::new(AtomicU64::new(0)),
                syslog_stats: if config.syslog.enabled && config.ingest.enabled {
                    Some(Arc::new(SyslogStats::default()))
                } else {
                    None
                },
                compaction_stats: if config.ingest.enabled {
                    Some(Arc::new(CompactionStats::default()))
                } else {
                    None
                },
            },
            start_time: Instant::now(),
            total_queries: Arc::new(AtomicU64::new(0)),
            metrics_handle,
            dashboard_snapshot: Arc::new(Mutex::new(None)),
        };

        let http = HttpConfig {
            max_request_body_bytes: config.server.max_request_body_bytes,
            max_concurrent_requests: config.server.max_concurrent_requests,
            shutdown_drain_secs: config.server.shutdown_drain_secs,
            cors_allowed_origins: config.server.cors_allowed_origins.clone(),
            ingest_max_body_bytes: if config.ingest.enabled {
                Some(config.ingest.max_body_bytes)
            } else {
                None
            },
            rate_limit: config.server.rate_limit.clone(),
        };

        Ok((state, http))
    }
}
