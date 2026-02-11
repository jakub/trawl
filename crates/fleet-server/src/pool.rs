//! Semaphore-bounded executor pool for concurrent query execution.
//!
//! `DuckDB` connections are `!Send`, so each query runs in a
//! [`tokio::task::spawn_blocking`] task with a fresh [`Executor`].
//! The semaphore limits concurrency to prevent thread pool exhaustion.
//! Timed-out queries are interrupted via `DuckDB`'s interrupt handle.

use std::sync::Arc;
use std::time::Duration;

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
    ///
    /// If the query exceeds `timeout`, the `DuckDB` connection is interrupted
    /// and the query is aborted. The semaphore permit is held until the
    /// blocking task finishes (which happens promptly after interruption).
    pub async fn execute(&self, dsl: &str, timeout: Duration) -> Result<QueryResult, ServerError> {
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                max_concurrent = self.semaphore.available_permits() + 1,
                "executor pool at capacity, query queued"
            );
        }

        let wait_start = std::time::Instant::now();
        let semaphore = Arc::clone(&self.semaphore);
        let permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        let wait_ms = wait_start.elapsed().as_millis();
        if wait_ms > 0 {
            tracing::debug!(wait_ms, "semaphore permit acquired");
        }

        let dsl = dsl.to_owned();
        let data_path = Arc::clone(&self.data_path);

        // Channel for the blocking task to send back its interrupt handle
        // before starting the actual query.
        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();

        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit; // hold permit until this task completes
            let executor = Executor::new()?;
            // Send interrupt handle to async side before running the query.
            let _ = interrupt_tx.send(executor.interrupt_handle());
            executor
                .run_query(&dsl, &data_path)
                .map_err(ServerError::from)
        });

        // Receive interrupt handle (may fail if executor creation fails first).
        let interrupt = interrupt_rx.await.ok();

        match tokio::time::timeout(timeout, task).await {
            // Query completed within timeout.
            Ok(join_result) => join_result
                .map_err(|e| ServerError::Internal(format!("query task panicked: {e}")))?,
            // Timeout elapsed — interrupt the DuckDB query.
            Err(_elapsed) => {
                if let Some(handle) = interrupt {
                    handle.interrupt();
                }
                Err(ServerError::Timeout)
            }
        }
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
        let result = pool
            .execute("totally broken {{{ query", Duration::from_secs(10))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::new("nonexistent/**/*.parquet".into(), 1);
        // just verifying it doesn't panic with a single permit
        let _ = pool.execute("service:test", Duration::from_secs(10)).await;
    }
}
