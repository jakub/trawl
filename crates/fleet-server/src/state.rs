//! Shared application state for axum handlers.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::pool::ExecutorPool;

/// Shared state injected into handlers via axum's `State` extractor.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Bounded executor pool for query execution.
    pub pool: ExecutorPool,
    /// Path to the `SQLite` auth database.
    pub auth_db_path: Arc<PathBuf>,
    /// Server start time (for health endpoint uptime).
    pub start_time: std::time::Instant,
}

impl AppState {
    /// Construct app state from a validated [`Config`].
    pub fn from_config(config: &Config) -> Self {
        Self {
            pool: ExecutorPool::new(
                config.data.path.clone(),
                config.server.max_concurrent_queries,
            ),
            auth_db_path: Arc::new(config.auth.db_path.clone()),
            start_time: std::time::Instant::now(),
        }
    }
}
