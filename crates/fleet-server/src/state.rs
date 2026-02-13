//! Shared application state for axum handlers.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::Instant;

use fleet_auth::KeyStore;
use fleet_engine::value::SchemaResult;

use crate::config::{Config, RateLimitConfig};
use crate::ingest::wal::WalWriter;
use crate::pool::ExecutorPool;
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
    /// Server start time (for health endpoint uptime).
    pub start_time: Instant,
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
    /// Cached schema introspection result with TTL.
    ///
    /// Uses `Mutex` (not `RwLock`) to prevent thundering herd: only one
    /// request refreshes the cache while others wait on the lock.
    pub schema_cache: Arc<tokio::sync::Mutex<Option<CachedSchema>>>,
}

/// Authentication state: key store and database path.
#[derive(Debug, Clone)]
pub struct AuthState {
    /// Shared `KeyStore` connection, opened once at startup.
    pub key_store: Arc<Mutex<KeyStore>>,
    /// Path to the `SQLite` auth database (kept for admin commands).
    pub db_path: Arc<PathBuf>,
}

/// Ingest pipeline state.
#[derive(Debug, Clone)]
pub struct IngestState {
    /// WAL writer for ingested events (None if ingest is disabled).
    pub wal_writer: Option<Arc<WalWriter>>,
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

/// How long schema cache entries are valid (seconds).
pub const SCHEMA_CACHE_TTL_SECS: u64 = 60;

impl AppState {
    /// Construct app state from a validated [`Config`].
    ///
    /// Opens the auth database once at startup. Returns an error if the
    /// database cannot be opened or initialized.
    pub fn from_config(config: &Config) -> Result<(Self, HttpConfig), fleet_auth::AuthError> {
        let key_store = KeyStore::open(&config.auth.db_path)?;

        let wal_writer = if config.ingest.enabled {
            let writer = WalWriter::new(config.wal_dir());
            Some(Arc::new(writer))
        } else {
            None
        };

        let state = Self {
            query: QueryState {
                pool: ExecutorPool::new(
                    config.data.base_dir().to_string_lossy().into_owned(),
                    config.server.max_concurrent_queries,
                    config.server.max_result_rows,
                ),
                timeout_secs: config.server.timeout_secs,
                tracker: Arc::new(QueryTracker::new()),
                schema_cache: Arc::new(tokio::sync::Mutex::new(None)),
            },
            auth: AuthState {
                key_store: Arc::new(Mutex::new(key_store)),
                db_path: Arc::new(config.auth.db_path.clone()),
            },
            ingest: IngestState { wal_writer },
            start_time: Instant::now(),
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
