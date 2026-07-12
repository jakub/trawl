// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

use tokio::sync::Semaphore;
use trawl_engine::executor::Executor;
use trawl_engine::value::{QueryResult, SchemaResult};

use crate::error::ServerError;
use crate::hot_buffer::HotBuffer;
use crate::source::compute_source;

/// Test-only delay injected into the blocking query task so timeout and
/// cancellation tests can reliably win the `select!`/cancel race. Zero means
/// no delay. Compiled for unit tests and, via the `test-support` feature,
/// for this crate's own integration tests — no production consumer enables
/// that feature, so release builds never carry it.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_QUERY_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// Type-erased interrupt callback, keyed by monotonic query ID.
type InterruptMap = HashMap<u64, Box<dyn Fn() + Send + Sync>>;

/// Debug info captured from the pool's blocking execution path.
///
/// Only populated when `capture_debug` is true (i.e. query log is active).
#[derive(Debug)]
pub struct PoolDebugInfo {
    /// The computed source argument passed to `read_parquet()`.
    pub computed_source: String,
    /// Number of glob patterns in the source list.
    pub glob_count: usize,
    /// Service name extracted from the DSL, if any.
    pub service_filter: Option<String>,
    /// Time filter duration in seconds, if any.
    pub time_filter_secs: Option<u64>,
    /// Whether the source fell back to recursive glob.
    pub is_fallback: bool,
    /// Hot buffer status: "disabled", "empty", or "active".
    pub hot_status: &'static str,
    /// Hot buffer event count at snapshot time.
    pub hot_events: usize,
    /// Hot buffer batch count at snapshot time.
    pub hot_batches: usize,
    /// Hot buffer estimated byte size at snapshot time.
    pub hot_bytes: usize,
    /// Generated SQL (parameterized).
    pub sql: String,
    /// SQL parameter values (Display form).
    pub params: Vec<String>,
    /// Time spent waiting for pool permit (ms).
    pub pool_wait_ms: u64,
}

/// Query result paired with optional debug info.
#[derive(Debug)]
pub struct ExecuteOutcome {
    /// The query result (success or error).
    pub result: Result<QueryResult, ServerError>,
    /// Debug info, populated only when `capture_debug` was true.
    pub debug: Option<PoolDebugInfo>,
}

/// Pool that bounds concurrent `DuckDB` query execution.
///
/// Executors are pre-created at startup and reused across queries.
/// Each executor holds a connection to the same in-memory `DuckDB`
/// database, sharing cached metadata.
#[derive(Clone)]
pub struct ExecutorPool {
    /// Base directory for parquet data (e.g. `/var/lib/trawl/data`).
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
    /// to duckdb outside trawl-engine.
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
#[allow(clippy::too_many_arguments)]
fn run_query_blocking(
    executor: Executor,
    dsl: &str,
    source: &str,
    hot_buffer: Option<&Arc<HotBuffer>>,
    max_result_rows: usize,
    utc_offset_secs: i32,
    capture_debug: bool,
    fallback_glob: &str,
    pool_wait_ms: u64,
) -> (
    Executor,
    Result<QueryResult, ServerError>,
    Option<PoolDebugInfo>,
) {
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

    // Capture debug info if requested (query log is active).
    let debug = if capture_debug {
        Some(capture_pool_debug(
            dsl,
            source,
            hot_buffer,
            fallback_glob,
            pool_wait_ms,
        ))
    } else {
        None
    };

    // catch_unwind ensures the executor is always returned to the
    // pool even if DuckDB panics (e.g. corrupt parquet file).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(ref hot_file) = hot_tempfile {
            // Safety: we verified UTF-8 validity above.
            let hot_path = hot_file.path().to_str().unwrap_or_default();
            executor
                .run_query_with_hot(dsl, source, hot_path, max_result_rows, utc_offset_secs)
                .map_err(ServerError::from)
        } else {
            executor
                .run_query(dsl, source, max_result_rows, utc_offset_secs)
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

    (executor, result, debug)
}

/// Capture debug info about source selection, hot buffer state, and SQL generation.
///
/// This is cheap (re-parse + emit is <1ms) and only runs when the query log is active.
fn capture_pool_debug(
    dsl: &str,
    source: &str,
    hot_buffer: Option<&Arc<HotBuffer>>,
    fallback_glob: &str,
    pool_wait_ms: u64,
) -> PoolDebugInfo {
    let glob_count = if source.starts_with('[') {
        source.matches(',').count() + 1
    } else {
        1
    };
    let is_fallback = source == fallback_glob || source.ends_with("/**/*.parquet");

    // Re-parse AST to extract filters and emit SQL (cheap, <1ms).
    let (service_filter, time_filter_secs, sql, params) =
        if let Ok(ast) = trawl_core::parser::parse(dsl) {
            let service = ast.search.groups.first().and_then(|g| {
                g.iter().find_map(|t| {
                    if let trawl_core::ast::SearchToken::FieldFilter(trawl_core::ast::FieldFilter {
                        field,
                        op: trawl_core::ast::FilterOp::Eq,
                        value: trawl_core::ast::FilterValue::Literal(s),
                    }) = &t.node
                        && field == "service"
                    {
                        return Some(s.clone());
                    }
                    None
                })
            });
            let time_secs = ast
                .search
                .time_filter
                .as_ref()
                .map(|tf| tf.node.duration.to_seconds());
            let (sql, params) = match trawl_core::emitter::emit(&ast, source) {
                Ok(emitted) => (
                    emitted.sql,
                    emitted.params.iter().map(ToString::to_string).collect(),
                ),
                Err(_) => (String::new(), vec![]),
            };
            (service, time_secs, sql, params)
        } else {
            (None, None, String::new(), vec![])
        };

    let (hot_status, hot_events, hot_batches, hot_bytes) = match hot_buffer {
        None => ("disabled", 0, 0, 0),
        Some(hb) => {
            let events = hb.event_count();
            if events == 0 {
                ("empty", 0, hb.batch_count(), 0)
            } else {
                ("active", events, hb.batch_count(), hb.byte_count())
            }
        }
    };

    PoolDebugInfo {
        computed_source: source.to_owned(),
        glob_count,
        service_filter,
        time_filter_secs,
        is_fallback,
        hot_status,
        hot_events,
        hot_batches,
        hot_bytes,
        sql,
        params,
        pool_wait_ms,
    }
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

    /// Allocate a query id from the pool's counter.
    ///
    /// This is the single id authority: callers pass the returned id to
    /// [`execute`](Self::execute) / [`execute_with_source`](Self::execute_with_source) /
    /// [`export_parquet`](Self::export_parquet), and — when the query is
    /// user-visible — to `QueryTracker::start`, so `/queries` listings and
    /// [`cancel_by_id`](Self::cancel_by_id) speak the same id space.
    pub fn allocate_query_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Execute a DSL query, blocking on semaphore acquisition if at capacity.
    ///
    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id);
    /// it keys the interrupt handle for [`cancel_by_id`](Self::cancel_by_id).
    ///
    /// If the query exceeds `timeout`, the `DuckDB` connection is interrupted
    /// and the query is aborted. The executor is reclaimed asynchronously
    /// once the interrupted task completes.
    ///
    /// When `capture_debug` is true, captures source selection, hot buffer
    /// state, and generated SQL for the query debug log.
    ///
    /// `utc_offset_secs` is applied to all timestamp values in the result.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(
        &self,
        query_id: u64,
        dsl: &str,
        timeout: Duration,
        capture_debug: bool,
        utc_offset_secs: i32,
    ) -> ExecuteOutcome {
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                event_type = "pool_pressure",
                max_concurrent = self.max_concurrent,
                "executor pool at capacity, query queued"
            );
        }

        let wait_start = std::time::Instant::now();
        let semaphore = Arc::clone(&self.semaphore);
        let Ok(permit) = semaphore.acquire_owned().await else {
            return ExecuteOutcome {
                result: Err(ServerError::Internal("executor pool shut down".into())),
                debug: None,
            };
        };

        let wait_ms = wait_start.elapsed().as_millis();
        #[allow(clippy::cast_possible_truncation)]
        let pool_wait_ms = wait_ms as u64;
        tracing::info!(
            event_type = "pool_acquired",
            wait_ms,
            available = self.semaphore.available_permits(),
            "semaphore permit acquired"
        );

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

            // Test-only: sleep before the query so timeout/cancellation tests
            // can reliably win the race against spawn_blocking.
            #[cfg(any(test, feature = "test-support"))]
            {
                let delay = TEST_QUERY_DELAY_MS.load(Ordering::Relaxed);
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
            }

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
                utc_offset_secs,
                capture_debug,
                &fallback_glob,
                pool_wait_ms,
            )
        });

        // Receive interrupt handle (may fail if the task panics before sending).
        let interrupt = interrupt_rx.await.ok();

        // Register the interrupt handle for shutdown/API cancellation.
        if let Some(ref handle) = interrupt {
            let h = Arc::clone(handle);
            self.active_interrupts
                .lock()
                .insert(query_id, Box::new(move || h.interrupt()));
        }

        // Use select! so the JoinHandle remains available for async
        // executor reclamation if the timeout branch wins.
        let outcome = tokio::select! {
            // Query completed within timeout — return executor to pool.
            join_result = &mut task => {
                match join_result {
                    Ok((executor, result, debug)) => {
                        self.return_executor(executor);
                        ExecuteOutcome { result, debug }
                    }
                    Err(e) => ExecuteOutcome {
                        result: Err(ServerError::Internal(format!("query task panicked: {e}"))),
                        debug: None,
                    },
                }
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
                        Ok((executor, _, _)) => {
                            idle.lock().push(executor);
                        }
                        Err(e) => {
                            tracing::warn!(event_type = "task_panic", error = %e, "timed-out query task panicked");
                        }
                    }
                });
                ExecuteOutcome {
                    result: Err(ServerError::Timeout),
                    debug: None,
                }
            }
        };

        // Deregister this query's interrupt handle.
        self.active_interrupts.lock().remove(&query_id);

        outcome
    }

    /// Execute a DSL query with a pre-computed source, skipping glob computation.
    ///
    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    ///
    /// Used for `| from saved` queries where the source is a `read_parquet()`
    /// expression pointing at scheduled result files, not the normal data dir.
    /// The hot buffer is intentionally skipped — saved query results are
    /// self-contained parquet files.
    #[allow(clippy::too_many_lines)]
    pub async fn execute_with_source(
        &self,
        query_id: u64,
        dsl: &str,
        source: &str,
        timeout: Duration,
        capture_debug: bool,
        utc_offset_secs: i32,
    ) -> ExecuteOutcome {
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                event_type = "pool_pressure",
                max_concurrent = self.max_concurrent,
                "executor pool at capacity, query queued"
            );
        }

        let wait_start = std::time::Instant::now();
        let semaphore = Arc::clone(&self.semaphore);
        let Ok(permit) = semaphore.acquire_owned().await else {
            return ExecuteOutcome {
                result: Err(ServerError::Internal("executor pool shut down".into())),
                debug: None,
            };
        };

        let wait_ms = wait_start.elapsed().as_millis();
        #[allow(clippy::cast_possible_truncation)]
        let pool_wait_ms = wait_ms as u64;
        tracing::info!(
            event_type = "pool_acquired",
            wait_ms,
            available = self.semaphore.available_permits(),
            "semaphore permit acquired (from saved)"
        );

        let executor = self.take_executor();

        let dsl = dsl.to_owned();
        let source = source.to_owned();
        let fallback_glob = Arc::clone(&self.fallback_glob);
        let max_result_rows = self.max_result_rows;

        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();

        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _ = interrupt_tx.send(executor.interrupt_handle());

            #[cfg(any(test, feature = "test-support"))]
            {
                let delay = TEST_QUERY_DELAY_MS.load(Ordering::Relaxed);
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
            }

            tracing::debug!(
                event_type = "query_source",
                source = %source,
                "using pre-computed source (from saved)"
            );

            // No hot buffer — saved query results are self-contained.
            run_query_blocking(
                executor,
                &dsl,
                &source,
                None,
                max_result_rows,
                utc_offset_secs,
                capture_debug,
                &fallback_glob,
                pool_wait_ms,
            )
        });

        let interrupt = interrupt_rx.await.ok();

        if let Some(ref handle) = interrupt {
            let h = Arc::clone(handle);
            self.active_interrupts
                .lock()
                .insert(query_id, Box::new(move || h.interrupt()));
        }

        let outcome = tokio::select! {
            join_result = &mut task => {
                match join_result {
                    Ok((executor, result, debug)) => {
                        self.return_executor(executor);
                        ExecuteOutcome { result, debug }
                    }
                    Err(e) => ExecuteOutcome {
                        result: Err(ServerError::Internal(format!("query task panicked: {e}"))),
                        debug: None,
                    },
                }
            }
            () = tokio::time::sleep(timeout) => {
                if let Some(handle) = &interrupt {
                    handle.interrupt();
                }
                let idle = Arc::clone(&self.idle);
                tokio::spawn(async move {
                    match task.await {
                        Ok((executor, _, _)) => {
                            idle.lock().push(executor);
                        }
                        Err(e) => {
                            tracing::warn!(event_type = "task_panic", error = %e, "timed-out from-saved query task panicked");
                        }
                    }
                });
                ExecuteOutcome {
                    result: Err(ServerError::Timeout),
                    debug: None,
                }
            }
        };

        self.active_interrupts.lock().remove(&query_id);

        outcome
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

    /// Get the base data directory path.
    pub fn base_dir(&self) -> &str {
        &self.base_dir
    }

    /// Get the fallback glob pattern for queries.
    pub fn fallback_glob(&self) -> &Arc<str> {
        &self.fallback_glob
    }

    /// Lightweight health check: acquire a permit, grab an executor, run
    /// `SELECT 1`, and return it. Proves the pool and `DuckDB` are functional.
    pub async fn ping(&self) -> Result<(), ServerError> {
        let semaphore = Arc::clone(&self.semaphore);
        let Ok(permit) = semaphore.acquire_owned().await else {
            return Err(ServerError::Internal("executor pool shut down".into()));
        };

        let executor = self.take_executor();

        let (executor, result) = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                executor.ping().map_err(ServerError::from)
            }));
            let result = match result {
                Ok(r) => r,
                Err(_) => Err(ServerError::Internal("ping panicked".into())),
            };
            (executor, result)
        })
        .await
        .map_err(|e| ServerError::Internal(format!("ping task panicked: {e}")))?;

        self.return_executor(executor);
        result
    }

    /// Export query results to Parquet via `DuckDB` `COPY TO`.
    ///
    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    ///
    /// Acquires a pool executor, writes to a temp file, and returns the
    /// raw bytes. Respects the hot buffer for fresh event visibility.
    pub async fn export_parquet(
        &self,
        query_id: u64,
        dsl: &str,
        max_rows: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, ServerError> {
        let semaphore = Arc::clone(&self.semaphore);
        let Ok(permit) = semaphore.acquire_owned().await else {
            return Err(ServerError::Internal("executor pool shut down".into()));
        };

        let executor = self.take_executor();
        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let fallback_glob = Arc::clone(&self.fallback_glob);
        let hot_buffer = self.hot_buffer.clone();

        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();

        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _ = interrupt_tx.send(executor.interrupt_handle());
            let source = compute_source(&base_dir, &dsl, &fallback_glob);

            // Write to a temp file, then read it back as bytes.
            let tmp = tempfile::NamedTempFile::new()
                .map_err(|e| ServerError::Internal(format!("failed to create temp file: {e}")))?;
            let tmp_path = tmp.path().to_owned();

            // Snapshot hot buffer for fresh events.
            let hot_tempfile = hot_buffer
                .as_ref()
                .and_then(|hb| hb.snapshot())
                .filter(|f| f.path().to_str().is_some());

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(ref hot_file) = hot_tempfile {
                    let hot_path = hot_file.path().to_str().unwrap_or_default();
                    executor.export_parquet_with_hot(&dsl, &source, hot_path, &tmp_path, max_rows)
                } else {
                    executor.export_parquet(&dsl, &source, &tmp_path, max_rows)
                }
                .map_err(ServerError::from)
            }));

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
                    Err(ServerError::Internal(format!("export panicked: {msg}")))
                }
            };

            let bytes = result.and_then(|()| {
                std::fs::read(&tmp_path).map_err(|e| {
                    ServerError::Internal(format!("failed to read parquet temp file: {e}"))
                })
            });

            Ok::<(Executor, Result<Vec<u8>, ServerError>), ServerError>((executor, bytes))
        });

        let interrupt = interrupt_rx.await.ok();

        if let Some(ref handle) = interrupt {
            let h = Arc::clone(handle);
            self.active_interrupts
                .lock()
                .insert(query_id, Box::new(move || h.interrupt()));
        }

        let outcome = tokio::select! {
            join_result = &mut task => {
                match join_result {
                    Ok(Ok((executor, result))) => {
                        self.return_executor(executor);
                        result
                    }
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(ServerError::Internal(format!("export task panicked: {e}"))),
                }
            }
            () = tokio::time::sleep(timeout) => {
                if let Some(handle) = &interrupt {
                    handle.interrupt();
                }
                let idle = Arc::clone(&self.idle);
                tokio::spawn(async move {
                    if let Ok(Ok((executor, _))) = task.await {
                        idle.lock().push(executor);
                    }
                });
                Err(ServerError::Timeout)
            }
        };

        self.active_interrupts.lock().remove(&query_id);
        outcome
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
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "| | invalid",
                Duration::from_secs(10),
                false,
                0,
            )
            .await;
        assert!(outcome.result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        // just verifying it doesn't panic with a single permit
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Duration::from_secs(10),
                false,
                0,
            )
            .await;
    }

    #[tokio::test]
    async fn pool_reuses_executors() {
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);

        // Run two sequential queries — both should succeed and the pool
        // should have the same number of idle executors before and after.
        let idle_before = pool.idle.lock().len();
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Duration::from_secs(10),
                false,
                0,
            )
            .await;
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Duration::from_secs(10),
                false,
                0,
            )
            .await;
        let idle_after = pool.idle.lock().len();

        assert_eq!(
            idle_before, idle_after,
            "executors should be returned to pool"
        );
    }

    #[test]
    fn fallback_glob_derived_from_base_dir() {
        let pool = ExecutorPool::new("/var/lib/trawl/data".into(), 1, 100_000, None);
        assert_eq!(&*pool.fallback_glob, "/var/lib/trawl/data/**/*.parquet");
    }

    #[test]
    fn fallback_glob_strips_trailing_slash() {
        let pool = ExecutorPool::new("/var/lib/trawl/data/".into(), 1, 100_000, None);
        assert_eq!(&*pool.fallback_glob, "/var/lib/trawl/data/**/*.parquet");
    }

    #[tokio::test]
    async fn pool_timeout_returns_error() {
        // Make the blocking task sleep so the timeout reliably fires first.
        TEST_QUERY_DELAY_MS.store(200, Ordering::Relaxed);
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "*",
                Duration::from_millis(10),
                false,
                0,
            )
            .await;
        TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);
        assert!(
            matches!(outcome.result, Err(ServerError::Timeout)),
            "expected Timeout, got: {:?}",
            outcome.result
        );
    }

    #[tokio::test]
    async fn pool_executor_reclaimed_after_timeout() {
        // Make the blocking task sleep so the timeout reliably fires first.
        TEST_QUERY_DELAY_MS.store(200, Ordering::Relaxed);
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        let idle_before = pool.idle.lock().len();

        // Trigger a timeout.
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "*",
                Duration::from_millis(10),
                false,
                0,
            )
            .await;

        // Wait for the blocking task to finish (200ms delay) + margin
        // for the async reclamation task to push the executor back.
        tokio::time::sleep(Duration::from_millis(500)).await;
        TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);

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
