//! Semaphore-bounded executor pool for concurrent query execution.
//!
//! Pre-creates a pool of [`Executor`] instances sharing the same underlying
//! `DuckDB` database via [`Executor::try_clone`]. Long-lived connections
//! benefit from `DuckDB`'s internal metadata caching (parquet file stats,
//! column statistics, prepared statement cache).
//!
//! The semaphore limits concurrency, and each permit corresponds to exactly
//! one pooled executor. Timed-out queries are interrupted via `DuckDB`'s
//! interrupt handle; the executor is reclaimed asynchronously once the
//! interrupted task completes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use std::time::Duration;

use fleet_engine::executor::Executor;
use fleet_engine::value::{QueryResult, SchemaResult};
use tokio::sync::Semaphore;

use crate::error::ServerError;
use crate::hot_buffer::HotBuffer;
use crate::source::compute_source;

/// Type-erased interrupt callback, keyed by monotonic query ID.
type InterruptMap = HashMap<u64, Box<dyn Fn() + Send + Sync>>;

/// Pool that bounds concurrent `DuckDB` query execution.
///
/// Executors are pre-created at startup and reused across queries.
/// Each executor holds a connection to the same in-memory `DuckDB`
/// database, sharing cached metadata.
#[derive(Clone)]
pub struct ExecutorPool {
    /// Base directory for parquet data (e.g. `/var/lib/fleet/data`).
    base_dir: Arc<str>,
    /// Full recursive glob for queries without a time filter.
    fallback_glob: Arc<str>,
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
    max_result_rows: usize,
    /// Monotonic ID counter for tracking active query handles.
    next_id: Arc<AtomicU64>,
    /// Interrupt callbacks for currently executing queries, keyed by ID
    /// for precise removal on completion. Type-erased to avoid coupling
    /// to duckdb outside fleet-engine.
    active_interrupts: Arc<Mutex<InterruptMap>>,
    /// Pre-created executors sharing the same `DuckDB` database.
    /// The semaphore guarantees an executor is available when a permit
    /// is acquired, so `pop()` only fails after a task panic (which is
    /// handled by creating a replacement).
    idle: Arc<Mutex<Vec<Executor>>>,
    /// Hot buffer for fresh events not yet compacted to parquet.
    hot_buffer: Option<Arc<HotBuffer>>,
}

impl std::fmt::Debug for ExecutorPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active = self.active_interrupts.lock().len();
        let idle = self.idle.lock().len();
        f.debug_struct("ExecutorPool")
            .field("base_dir", &self.base_dir)
            .field("fallback_glob", &self.fallback_glob)
            .field("semaphore", &self.semaphore)
            .field("max_result_rows", &self.max_result_rows)
            .field("next_id", &self.next_id)
            .field("active_queries", &active)
            .field("idle_executors", &idle)
            .finish_non_exhaustive()
    }
}

/// Run a query with panic recovery and optional hot buffer union.
///
/// Returns the executor (for pool return) and the query result.
/// Called inside `spawn_blocking` — all I/O here is synchronous.
fn run_query_blocking(
    executor: Executor,
    dsl: &str,
    source: &str,
    hot_buffer: Option<&Arc<HotBuffer>>,
    max_result_rows: usize,
) -> (Executor, Result<QueryResult, ServerError>) {
    // Snapshot hot buffer to a temp ndjson file so fresh events
    // are visible to this query via UNION ALL BY NAME. Returns a
    // cached Arc when the buffer hasn't changed since the last snapshot.
    let hot_tempfile = hot_buffer.and_then(|hb| hb.snapshot());

    // Filter out hot files whose paths aren't valid UTF-8 (required by
    // DuckDB's file reader). This is extremely unlikely on any modern OS
    // but avoids a panic in production.
    let hot_tempfile = hot_tempfile.and_then(|f| {
        if f.path().to_str().is_some() {
            Some(f)
        } else {
            tracing::warn!("hot buffer temp file path is not valid UTF-8, skipping hot source");
            None
        }
    });

    // catch_unwind ensures the executor is always returned to the
    // pool even if DuckDB panics (e.g. corrupt parquet file).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(ref hot_file) = hot_tempfile {
            // Safety: we verified UTF-8 validity above.
            let hot_path = hot_file.path().to_str().unwrap_or_default();
            executor
                .run_query_with_hot(dsl, source, hot_path, max_result_rows)
                .map_err(ServerError::from)
        } else {
            executor
                .run_query(dsl, source, max_result_rows)
                .map_err(ServerError::from)
        }
    }));
    // hot_tempfile drops here → temp file auto-deleted
    let result = match result {
        Ok(r) => r,
        Err(payload) => {
            let msg = match payload.downcast_ref::<&str>() {
                Some(s) => (*s).to_owned(),
                None => match payload.downcast_ref::<String>() {
                    Some(s) => s.clone(),
                    None => "unknown panic".to_owned(),
                },
            };
            Err(ServerError::Internal(format!("query panicked: {msg}")))
        }
    };

    (executor, result)
}

impl ExecutorPool {
    /// Create a pool with the given concurrency limit and base data directory.
    ///
    /// Pre-creates `max_concurrent` executors sharing the same in-memory
    /// `DuckDB` database. Panics if the database cannot be initialized
    /// (fatal at startup — the server cannot function without `DuckDB`).
    pub fn new(
        base_dir: String,
        max_concurrent: usize,
        max_result_rows: usize,
        hot_buffer: Option<Arc<HotBuffer>>,
    ) -> Self {
        let root = Executor::new().expect("failed to create DuckDB connection at startup");
        let mut executors = Vec::with_capacity(max_concurrent);
        for _ in 1..max_concurrent {
            executors.push(
                root.try_clone()
                    .expect("failed to clone DuckDB connection at startup"),
            );
        }
        executors.push(root);

        let fallback_glob: Arc<str> =
            Arc::from(format!("{}/**/*.parquet", base_dir.trim_end_matches('/')));

        Self {
            base_dir: Arc::from(base_dir),
            fallback_glob,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            max_result_rows,
            next_id: Arc::new(AtomicU64::new(0)),
            active_interrupts: Arc::new(Mutex::new(HashMap::new())),
            idle: Arc::new(Mutex::new(executors)),
            hot_buffer,
        }
    }

    /// Take an executor from the pool.
    ///
    /// The semaphore guarantees availability. If the pool is unexpectedly
    /// empty (e.g. after a task panic lost an executor), creates a fresh
    /// replacement that won't share the cached database.
    fn take_executor(&self) -> Executor {
        let mut pool = self.idle.lock();
        pool.pop().unwrap_or_else(|| {
            tracing::warn!(
                event_type = "pool_pressure",
                "executor pool unexpectedly empty, creating replacement"
            );
            Executor::new().expect("failed to create replacement DuckDB connection")
        })
    }

    /// Return an executor to the pool for reuse.
    fn return_executor(&self, executor: Executor) {
        self.idle.lock().push(executor);
    }

    /// Execute a DSL query, blocking on semaphore acquisition if at capacity.
    ///
    /// If the query exceeds `timeout`, the `DuckDB` connection is interrupted
    /// and the query is aborted. The executor is reclaimed asynchronously
    /// once the interrupted task completes.
    pub async fn execute(&self, dsl: &str, timeout: Duration) -> Result<QueryResult, ServerError> {
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                event_type = "pool_pressure",
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
            tracing::debug!(
                event_type = "pool_acquired",
                wait_ms,
                "semaphore permit acquired"
            );
        }

        let executor = self.take_executor();

        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let fallback_glob = Arc::clone(&self.fallback_glob);
        let max_result_rows = self.max_result_rows;
        let hot_buffer = self.hot_buffer.clone();

        // Channel for the blocking task to send back its interrupt handle
        // before starting the actual query.
        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();

        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit; // hold permit until this task completes
            // Send interrupt handle to async side before running the query.
            let _ = interrupt_tx.send(executor.interrupt_handle());
            let source = compute_source(&base_dir, &dsl, &fallback_glob);
            let file_globs: usize = if source.starts_with('[') {
                source.matches(',').count() + 1
            } else {
                1
            };
            tracing::debug!(
                event_type = "query_source",
                file_globs,
                source = %source,
                "computed query source"
            );

            run_query_blocking(
                executor,
                &dsl,
                &source,
                hot_buffer.as_ref(),
                max_result_rows,
            )
        });

        // Receive interrupt handle (may fail if the task panics before sending).
        let interrupt = interrupt_rx.await.ok();

        // Register the interrupt handle for shutdown cancellation.
        let query_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(ref handle) = interrupt {
            let h = Arc::clone(handle);
            self.active_interrupts
                .lock()
                .insert(query_id, Box::new(move || h.interrupt()));
        }

        // Use select! so the JoinHandle remains available for async
        // executor reclamation if the timeout branch wins.
        let result = tokio::select! {
            // Query completed within timeout — return executor to pool.
            join_result = &mut task => {
                let (executor, result) = join_result
                    .map_err(|e| ServerError::Internal(format!("query task panicked: {e}")))?;
                self.return_executor(executor);
                result
            }
            // Timeout elapsed — interrupt the DuckDB query and reclaim
            // the executor asynchronously once the interrupt completes.
            () = tokio::time::sleep(timeout) => {
                if let Some(handle) = &interrupt {
                    handle.interrupt();
                }
                let idle = Arc::clone(&self.idle);
                tokio::spawn(async move {
                    match task.await {
                        Ok((executor, _)) => {
                            idle.lock().push(executor);
                        }
                        Err(e) => {
                            tracing::warn!(event_type = "task_panic", error = %e, "timed-out query task panicked");
                        }
                    }
                });
                Err(ServerError::Timeout)
            }
        };

        // Deregister this query's interrupt handle.
        self.active_interrupts.lock().remove(&query_id);

        result
    }

    /// Interrupt all currently executing queries. Called during shutdown
    /// to cancel in-flight `DuckDB` operations before draining connections.
    pub fn cancel_all(&self) {
        let handles = {
            let mut guard = self.active_interrupts.lock();
            std::mem::take(&mut *guard)
        };
        let count = handles.len();
        for callback in handles.values() {
            callback();
        }
        if count > 0 {
            tracing::info!(
                event_type = "lifecycle",
                count,
                "interrupted active queries for shutdown"
            );
        }
    }

    /// Cancel a specific query by ID. Returns true if the query was found
    /// and interrupted, false if the query had already completed or the ID
    /// was invalid.
    pub fn cancel_by_id(&self, query_id: u64) -> bool {
        if let Some(callback) = self.active_interrupts.lock().remove(&query_id) {
            callback();
            true
        } else {
            false
        }
    }

    /// Get the configured maximum result rows limit.
    pub fn max_result_rows(&self) -> usize {
        self.max_result_rows
    }

    /// Get the number of available query slots.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Get the total pool capacity (max concurrent queries).
    pub fn capacity(&self) -> usize {
        self.max_concurrent
    }

    /// Get the fallback glob pattern for queries.
    pub fn fallback_glob(&self) -> &Arc<str> {
        &self.fallback_glob
    }

    /// Introspect the data source schema.
    ///
    /// Uses a fresh connection rather than a pooled executor since schema
    /// queries are lightweight metadata-only operations, cached server-side
    /// with a 60s TTL (see [`crate::state::SCHEMA_CACHE_TTL_SECS`]).
    pub async fn describe_schema(&self) -> Result<SchemaResult, ServerError> {
        let glob = Arc::clone(&self.fallback_glob);

        tokio::task::spawn_blocking(move || {
            let executor = Executor::new()?;
            executor.describe_schema(&glob).map_err(ServerError::from)
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
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);
        // Must start with `|` to trigger a parse error — bare text is valid DSL.
        let result = pool.execute("| | invalid", Duration::from_secs(10)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        // just verifying it doesn't panic with a single permit
        let _ = pool.execute("service:test", Duration::from_secs(10)).await;
    }

    #[tokio::test]
    async fn pool_reuses_executors() {
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);

        // Run two sequential queries — both should succeed and the pool
        // should have the same number of idle executors before and after.
        let idle_before = pool.idle.lock().len();
        let _ = pool.execute("service:test", Duration::from_secs(10)).await;
        let _ = pool.execute("service:test", Duration::from_secs(10)).await;
        let idle_after = pool.idle.lock().len();

        assert_eq!(
            idle_before, idle_after,
            "executors should be returned to pool"
        );
    }

    #[test]
    fn fallback_glob_derived_from_base_dir() {
        let pool = ExecutorPool::new("/var/lib/fleet/data".into(), 1, 100_000, None);
        assert_eq!(&*pool.fallback_glob, "/var/lib/fleet/data/**/*.parquet");
    }

    #[test]
    fn fallback_glob_strips_trailing_slash() {
        let pool = ExecutorPool::new("/var/lib/fleet/data/".into(), 1, 100_000, None);
        assert_eq!(&*pool.fallback_glob, "/var/lib/fleet/data/**/*.parquet");
    }

    #[tokio::test]
    async fn pool_timeout_returns_error() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        // 1ns timeout — the blocking task can't possibly complete this fast.
        let result = pool.execute("*", Duration::from_nanos(1)).await;
        assert!(
            matches!(result, Err(ServerError::Timeout)),
            "expected Timeout, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn pool_executor_reclaimed_after_timeout() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        let idle_before = pool.idle.lock().len();

        // Trigger a timeout.
        let _ = pool.execute("*", Duration::from_nanos(1)).await;

        // Wait briefly for the async reclamation task to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let idle_after = pool.idle.lock().len();
        assert_eq!(
            idle_before, idle_after,
            "executor should be reclaimed after timeout"
        );
    }

    #[tokio::test]
    async fn cancel_all_clears_interrupts() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        // No active queries — cancel_all should be a no-op.
        pool.cancel_all();
        assert!(pool.active_interrupts.lock().is_empty());
    }
}
