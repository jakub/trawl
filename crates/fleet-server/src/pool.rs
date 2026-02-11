//! Semaphore-bounded executor pool for concurrent query execution.
//!
//! `DuckDB` connections are `!Send`, so each query runs in a
//! [`tokio::task::spawn_blocking`] task with a fresh [`Executor`].
//! The semaphore limits concurrency to prevent thread pool exhaustion.

use std::sync::Arc;

use fleet_engine::executor::Executor;
use fleet_engine::value::{QueryResult, SchemaResult};
use tokio::sync::Semaphore;

use crate::error::ServerError;

/// Pool that bounds concurrent `DuckDB` query execution.
#[derive(Debug, Clone)]
pub struct ExecutorPool {
    data_path: Arc<str>,
    semaphore: Arc<Semaphore>,
}

impl ExecutorPool {
    /// Create a pool with the given concurrency limit and parquet data path.
    pub fn new(data_path: String, max_concurrent: usize) -> Self {
        Self {
            data_path: Arc::from(data_path),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// Execute a DSL query, blocking on semaphore acquisition if at capacity.
    pub async fn execute(&self, dsl: &str) -> Result<QueryResult, ServerError> {
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                max_concurrent = self.semaphore.available_permits() + 1,
                "executor pool at capacity, query queued"
            );
        }

        let wait_start = std::time::Instant::now();
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        let wait_ms = wait_start.elapsed().as_millis();
        if wait_ms > 0 {
            tracing::debug!(wait_ms, "semaphore permit acquired");
        }

        let dsl = dsl.to_owned();
        let data_path = Arc::clone(&self.data_path);

        tokio::task::spawn_blocking(move || {
            let executor = Executor::new()?;
            executor
                .run_query(&dsl, &data_path)
                .map_err(ServerError::from)
        })
        .await
        .map_err(|e| ServerError::Internal(format!("query task panicked: {e}")))?
    }

    /// Introspect the data source schema. Does NOT consume a semaphore permit
    /// since DESCRIBE queries are lightweight metadata-only operations.
    pub async fn describe_schema(&self) -> Result<SchemaResult, ServerError> {
        let data_path = Arc::clone(&self.data_path);

        tokio::task::spawn_blocking(move || {
            let executor = Executor::new()?;
            executor
                .describe_schema(&data_path)
                .map_err(ServerError::from)
        })
        .await
        .map_err(|e| ServerError::Internal(format!("schema task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_rejects_invalid_dsl() {
        let pool = ExecutorPool::new("nonexistent/**/*.parquet".into(), 2);
        let result = pool.execute("totally broken {{{ query").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::new("nonexistent/**/*.parquet".into(), 1);
        // just verifying it doesn't panic with a single permit
        let _ = pool.execute("service:test").await;
    }
}
