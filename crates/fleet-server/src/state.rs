//! Shared application state for axum handlers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use fleet_engine::value::SchemaResult;
use tokio::sync::RwLock;

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
    pub schema_cache: Arc<RwLock<Option<CachedSchema>>>,
    /// Query lifecycle tracker (active + history).
    pub tracker: Arc<QueryTracker>,
    /// Query timeout in seconds.
    pub timeout_secs: u64,
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
            ),
            auth_db_path: Arc::new(config.auth.db_path.clone()),
            start_time: Instant::now(),
            schema_cache: Arc::new(RwLock::new(None)),
            tracker: Arc::new(QueryTracker::new()),
            timeout_secs: config.server.timeout_secs,
        }
    }
}
