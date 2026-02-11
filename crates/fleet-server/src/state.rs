//! Shared application state for axum handlers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use fleet_engine::value::SchemaResult;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::pool::ExecutorPool;
use crate::tracker::QueryTracker;

/// Shared state injected into handlers via axum's `State` extractor.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Bounded executor pool for query execution.
    pub pool: ExecutorPool,
    /// Path to the `SQLite` auth database.
    pub auth_db_path: Arc<PathBuf>,
    /// Server start time (for health endpoint uptime).
    pub start_time: Instant,
    /// Cached schema introspection result with TTL.
    ///
    /// Uses `Mutex` (not `RwLock`) to prevent thundering herd: only one
    /// request refreshes the cache while others wait on the lock.
    pub schema_cache: Arc<Mutex<Option<CachedSchema>>>,
    /// Query lifecycle tracker (active + history).
    pub tracker: Arc<QueryTracker>,
    /// Query timeout in seconds.
    pub timeout_secs: u64,
    /// Maximum request body size in bytes.
    pub max_request_body_bytes: usize,
    /// Maximum concurrent HTTP requests.
    pub max_concurrent_requests: usize,
    /// Graceful shutdown drain timeout in seconds.
    pub shutdown_drain_secs: u64,
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
    pub fn from_config(config: &Config) -> Self {
        Self {
            pool: ExecutorPool::new(
                config.data.path.clone(),
                config.server.max_concurrent_queries,
                config.server.max_result_rows,
            ),
            auth_db_path: Arc::new(config.auth.db_path.clone()),
            start_time: Instant::now(),
            schema_cache: Arc::new(Mutex::new(None)),
            tracker: Arc::new(QueryTracker::new()),
            timeout_secs: config.server.timeout_secs,
            max_request_body_bytes: config.server.max_request_body_bytes,
            max_concurrent_requests: config.server.max_concurrent_requests,
            shutdown_drain_secs: config.server.shutdown_drain_secs,
        }
    }
}
