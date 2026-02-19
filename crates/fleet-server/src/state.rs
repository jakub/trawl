//! Shared application state for axum handlers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;
use std::time::Instant;
use tokio::sync::Semaphore;

use fleet_auth::{HistoryStore, KeyStore, SavedQueryStore};
use fleet_engine::value::SchemaResult;

use crate::auth::AuthCache;
use crate::bus::LocalEventBus;
use crate::config::{Config, RateLimitConfig};
use crate::hot_buffer::{HotBuffer, HotBufferConfig};
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
    /// Event bus for real-time fanout to subscribers (None if ingest is disabled).
    pub event_bus: Option<Arc<LocalEventBus>>,
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
}

/// A cached field value sample with an expiry timestamp.
#[derive(Debug, Clone)]
pub struct CachedFieldValues {
    /// Sampled distinct values for the field.
    pub values: Vec<String>,
    /// When this cache entry was created.
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
    ) -> Result<(Self, HttpConfig), fleet_auth::AuthError> {
        let key_store = KeyStore::open(&config.auth.db_path)?;
        let history = HistoryStore::open(&config.auth.db_path)?;
        let saved = SavedQueryStore::open(&config.auth.db_path)?;

        let (wal_writer, event_bus, hot_buffer) = if config.ingest.enabled {
            let writer = WalWriter::new(config.wal_dir());
            let bus = LocalEventBus::new(config.ingest.event_bus_capacity);
            let buffer = HotBuffer::new(HotBufferConfig {
                max_events: config.ingest.hot_buffer_max_events,
                max_bytes: config.ingest.hot_buffer_max_bytes,
            });
            (
                Some(Arc::new(writer)),
                Some(Arc::new(bus)),
                Some(Arc::new(buffer)),
            )
        } else {
            (None, None, None)
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
                sse_semaphore: Arc::new(Semaphore::new(config.server.max_sse_connections)),
                query_log: None,
            },
            auth: AuthState {
                key_store: Arc::new(Mutex::new(key_store)),
                history: Arc::new(Mutex::new(history)),
                saved: Arc::new(Mutex::new(saved)),
                db_path: Arc::new(config.auth.db_path.clone()),
                auth_cache,
            },
            ingest: IngestState {
                wal_writer,
                event_bus,
            },
            start_time: Instant::now(),
            total_queries: Arc::new(AtomicU64::new(0)),
            metrics_handle,
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
