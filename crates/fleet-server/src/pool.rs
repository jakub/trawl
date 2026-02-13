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

use chrono::Timelike as _;
use fleet_core::ast::SearchToken;
use fleet_engine::executor::Executor;
use fleet_engine::value::{QueryResult, SchemaResult};
use fleet_engine::{QueryEngine, SchemaIntrospector};
use tokio::sync::Semaphore;

use crate::error::ServerError;

/// Time filter padding (seconds) to account for WAL compaction delay.
/// Events ingested at time T may land in the hourly partition for T + ~20s,
/// so we widen the file selection window by one hour.
const TIME_FILTER_PADDING_SECS: u64 = 3600;

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

/// Compute the `read_parquet()` source argument, scoped to relevant
/// hour-directories when the query contains a time filter.
///
/// Returns a `DuckDB` list literal like `['path/14/*.parquet', 'path/15/*.parquet']`
/// when time-scoping is possible, or falls back to the recursive glob.
fn compute_source(base_dir: &str, dsl: &str, fallback_glob: &str) -> String {
    let Ok(ast) = fleet_core::parser::parse(dsl) else {
        return fallback_glob.to_owned();
    };

    let time_filter = ast.search.tokens.iter().find_map(|t| {
        if let SearchToken::TimeFilter(tf) = &t.node {
            Some(tf.duration)
        } else {
            None
        }
    });

    let Some(duration) = time_filter else {
        return fallback_glob.to_owned();
    };

    let total_secs = duration
        .to_seconds()
        .saturating_add(TIME_FILTER_PADDING_SECS);

    let now = chrono::Utc::now();
    let start = now - chrono::Duration::seconds(i64::try_from(total_secs).unwrap_or(i64::MAX));

    let mut globs = Vec::new();
    let mut cursor = start
        .date_naive()
        .and_hms_opt(start.hour(), 0, 0)
        .expect("valid hour from Timelike::hour()");
    let end = now.naive_utc();
    let base = base_dir.trim_end_matches('/');

    while cursor <= end {
        let day = cursor.format("%Y-%m-%d");
        let hour = cursor.format("%H");
        globs.push(format!("'{base}/{day}/{hour}/*.parquet'"));
        cursor += chrono::Duration::hours(1);
    }

    if globs.is_empty() {
        return fallback_glob.to_owned();
    }

    format!("[{}]", globs.join(", "))
}

impl ExecutorPool {
    /// Create a pool with the given concurrency limit and base data directory.
    ///
    /// Pre-creates `max_concurrent` executors sharing the same in-memory
    /// `DuckDB` database. Panics if the database cannot be initialized
    /// (fatal at startup — the server cannot function without `DuckDB`).
    pub fn new(base_dir: String, max_concurrent: usize, max_result_rows: usize) -> Self {
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
            max_result_rows,
            next_id: Arc::new(AtomicU64::new(0)),
            active_interrupts: Arc::new(Mutex::new(HashMap::new())),
            idle: Arc::new(Mutex::new(executors)),
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
            tracing::warn!("executor pool unexpectedly empty, creating replacement");
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

        let executor = self.take_executor();

        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let fallback_glob = Arc::clone(&self.fallback_glob);
        let max_result_rows = self.max_result_rows;

        // Channel for the blocking task to send back its interrupt handle
        // before starting the actual query.
        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();

        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit; // hold permit until this task completes
            // Send interrupt handle to async side before running the query.
            let _ = interrupt_tx.send(executor.interrupt_handle());
            let source = compute_source(&base_dir, &dsl, &fallback_glob);
            let result = executor
                .run_query(&dsl, &source, max_result_rows)
                .map_err(ServerError::from);
            (executor, result)
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
                            tracing::warn!("timed-out query task panicked: {e}");
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
            tracing::info!(count, "interrupted active queries for shutdown");
        }
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
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000);
        let result = pool
            .execute("totally broken {{{ query", Duration::from_secs(10))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000);
        // just verifying it doesn't panic with a single permit
        let _ = pool.execute("service:test", Duration::from_secs(10)).await;
    }

    #[tokio::test]
    async fn pool_reuses_executors() {
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000);

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
    fn compute_source_no_time_filter_returns_fallback() {
        let source = compute_source("/data", "service:nginx", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/*.parquet");
    }

    #[test]
    fn compute_source_bad_dsl_returns_fallback() {
        let source = compute_source("/data", "broken {{{ query", "/data/**/*.parquet");
        assert_eq!(source, "/data/**/*.parquet");
    }

    #[test]
    fn compute_source_with_time_filter_returns_list() {
        let source = compute_source("/data", "service:nginx last:1h", "/data/**/*.parquet");
        // Should be a list of hour-directory globs, not the fallback.
        assert!(
            source.starts_with('['),
            "expected list format, got: {source}"
        );
        assert!(source.ends_with(']'), "expected list format, got: {source}");
        assert!(
            source.contains("*.parquet"),
            "expected parquet globs, got: {source}"
        );
        // With 1h + 1h padding, should have ~2-3 hour entries.
        let count = source.matches("*.parquet").count();
        assert!(
            (2..=4).contains(&count),
            "expected 2-4 hour globs for last:1h, got {count}: {source}"
        );
    }

    #[test]
    fn compute_source_strips_trailing_slash() {
        let source = compute_source("/data/", "last:1h", "/data/**/*.parquet");
        assert!(!source.contains("//"), "double slashes in source: {source}");
    }

    #[test]
    fn fallback_glob_derived_from_base_dir() {
        let pool = ExecutorPool::new("/var/lib/fleet/data".into(), 1, 100_000);
        assert_eq!(&*pool.fallback_glob, "/var/lib/fleet/data/**/*.parquet");
    }

    #[test]
    fn fallback_glob_strips_trailing_slash() {
        let pool = ExecutorPool::new("/var/lib/fleet/data/".into(), 1, 100_000);
        assert_eq!(&*pool.fallback_glob, "/var/lib/fleet/data/**/*.parquet");
    }
}
