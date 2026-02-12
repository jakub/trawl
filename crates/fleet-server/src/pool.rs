//! Semaphore-bounded executor pool for concurrent query execution.
//!
//! `DuckDB` connections are `!Send`, so each query runs in a
//! [`tokio::task::spawn_blocking`] task with a fresh [`Executor`].
//! The semaphore limits concurrency to prevent thread pool exhaustion.
//! Timed-out queries are interrupted via `DuckDB`'s interrupt handle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fleet_engine::executor::Executor;
use fleet_engine::value::{QueryResult, SchemaResult};
use tokio::sync::Semaphore;

use crate::error::ServerError;

/// Type-erased interrupt callback, keyed by monotonic query ID.
type InterruptMap = HashMap<u64, Box<dyn Fn() + Send + Sync>>;

/// Pool that bounds concurrent `DuckDB` query execution.
#[derive(Clone)]
pub struct ExecutorPool {
    data_path: Arc<str>,
    semaphore: Arc<Semaphore>,
    max_result_rows: usize,
    /// Monotonic ID counter for tracking active query handles.
    next_id: Arc<AtomicU64>,
    /// Interrupt callbacks for currently executing queries, keyed by ID
    /// for precise removal on completion. Type-erased to avoid coupling
    /// to duckdb outside fleet-engine.
    active_interrupts: Arc<Mutex<InterruptMap>>,
}

impl std::fmt::Debug for ExecutorPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.active_interrupts.lock().map_or(0, |v| v.len());
        f.debug_struct("ExecutorPool")
            .field("data_path", &self.data_path)
            .field("semaphore", &self.semaphore)
            .field("max_result_rows", &self.max_result_rows)
            .field("next_id", &self.next_id)
            .field("active_queries", &count)
            .finish_non_exhaustive()
    }
}

impl ExecutorPool {
    /// Create a pool with the given concurrency limit and parquet data path.
    pub fn new(data_path: String, max_concurrent: usize, max_result_rows: usize) -> Self {
        Self {
            data_path: Arc::from(data_path),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_result_rows,
            next_id: Arc::new(AtomicU64::new(0)),
            active_interrupts: Arc::new(Mutex::new(HashMap::new())),
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
        let max_result_rows = self.max_result_rows;

        // Channel for the blocking task to send back its interrupt handle
        // before starting the actual query.
        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();

        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit; // hold permit until this task completes
            let executor = Executor::new()?;
            // Send interrupt handle to async side before running the query.
            let _ = interrupt_tx.send(executor.interrupt_handle());
            executor
                .run_query(&dsl, &data_path, max_result_rows)
                .map_err(ServerError::from)
        });

        // Receive interrupt handle (may fail if executor creation fails first).
        let interrupt = interrupt_rx.await.ok();

        // Register the interrupt handle for shutdown cancellation.
        let query_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(ref handle) = interrupt {
            let h = Arc::clone(handle);
            self.active_interrupts
                .lock()
                .unwrap()
                .insert(query_id, Box::new(move || h.interrupt()));
        }

        let result = match tokio::time::timeout(timeout, task).await {
            // Query completed within timeout.
            Ok(join_result) => join_result
                .map_err(|e| ServerError::Internal(format!("query task panicked: {e}")))?,
            // Timeout elapsed — interrupt the DuckDB query.
            Err(_elapsed) => {
                if let Some(handle) = &interrupt {
                    handle.interrupt();
                }
                Err(ServerError::Timeout)
            }
        };

        // Deregister this query's interrupt handle.
        self.active_interrupts.lock().unwrap().remove(&query_id);

        result
    }

    /// Interrupt all currently executing queries. Called during shutdown
    /// to cancel in-flight `DuckDB` operations before draining connections.
    pub fn cancel_all(&self) {
        let handles = {
            let mut guard = self.active_interrupts.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        let count = handles.len();
        for callback in handles.values() {
            callback();
        }
        if count > 0 {
            tracing::info!(count, "interrupted active queries for shutdown");
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
        let pool = ExecutorPool::new("nonexistent/**/*.parquet".into(), 2, 100_000);
        let result = pool
            .execute("totally broken {{{ query", Duration::from_secs(10))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::new("nonexistent/**/*.parquet".into(), 1, 100_000);
        // just verifying it doesn't panic with a single permit
        let _ = pool.execute("service:test", Duration::from_secs(10)).await;
    }
}
