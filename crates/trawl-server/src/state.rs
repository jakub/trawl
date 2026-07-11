// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared application state for axum handlers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use trawl_api::{DashboardSnapshot, ServiceSchema};
use trawl_auth::{HistoryStore, SavedQueryStore, ScheduleStore};
use trawl_engine::value::SchemaResult;

use crate::bus::LocalEventBus;
use crate::config::{Config, RateLimitConfig};
use crate::hot_buffer::{HotBuffer, HotBufferConfig};
use crate::ingest::pipeline::PipelineWriter;
use crate::ingest::wal::WalWriter;
use crate::pool::ExecutorPool;
use crate::query_log::QueryLog;
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
    /// Server start time (for uptime calculation).
    pub start_time: Instant,
    /// Total queries executed since startup (for stats endpoint).
    pub total_queries: Arc<AtomicU64>,
    /// Prometheus metrics handle for rendering the scrape endpoint.
    pub metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    /// Latest dashboard snapshot, updated every ~1s by the snapshot collector.
    /// Available even when the terminal monitor is disabled (systemd, `--no-monitor`).
    pub dashboard_snapshot: Arc<Mutex<Option<DashboardSnapshot>>>,
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
    /// Schema cache TTL in seconds.
    pub schema_cache_ttl_secs: u64,
    /// Maximum rows for export responses (bypasses `max_result_rows`).
    pub max_export_rows: usize,
    /// Cached schema introspection result with TTL.
    ///
    /// Uses `Mutex` (not `RwLock`) to prevent thundering herd: only one
    /// request refreshes the cache while others wait on the lock.
    pub schema_cache: Arc<tokio::sync::Mutex<Option<CachedSchema>>>,
    /// Cached field value samples for autocomplete (shared TTL with schema cache).
    pub field_values_cache: Arc<tokio::sync::Mutex<HashMap<String, CachedFieldValues>>>,
    /// Hot buffer for fresh events not yet compacted to parquet.
    pub hot_buffer: Option<Arc<HotBuffer>>,
    /// Pre-computed per-service schema from background refresh job.
    /// Uses `parking_lot::Mutex` (like `dashboard_snapshot`) for fast reads.
    pub service_schema_cache: Arc<Mutex<Option<CachedServiceSchema>>>,
    /// Semaphore bounding concurrent SSE streaming connections.
    pub sse_semaphore: Arc<Semaphore>,
    /// Optional ndjson query debug log.
    pub query_log: Option<Arc<QueryLog>>,
}

/// Authentication state: fleet keystore plus the transitional `SQLite`
/// app-state stores (history, saved queries, schedules — ADR-0004 slice 3
/// moves these to postgres).
#[derive(Debug, Clone)]
pub struct AuthState {
    /// Fleet-auth Postgres keystore. Cheap to clone (Arc-backed pool +
    /// verification cache live inside).
    pub key_store: fleet_auth::KeyStore,
    /// State for `fleet_auth::require_bearer`. trawld never reads session
    /// cookies (that's trawl-web's job in slice 2), so the embedded session
    /// key is a generated throwaway — mirrors fleet-auth's own bearer-only
    /// test wiring. A KeyStore-only bearer state is a parked fleet-auth
    /// follow-up.
    pub session_state: fleet_auth::SessionState,
    /// Shared `HistoryStore` connection for query history persistence.
    pub history: Arc<Mutex<HistoryStore>>,
    /// Shared `SavedQueryStore` connection for saved queries.
    pub saved: Arc<Mutex<SavedQueryStore>>,
    /// Shared `ScheduleStore` connection for scheduled queries and report runs.
    pub schedule: Arc<Mutex<ScheduleStore>>,
    /// Memoised keystore liveness ping, shared by every `/health` probe.
    ///
    /// `/health` is unauthenticated and unthrottled (it sits outside
    /// `require_bearer` and `rate_limit_middleware`), so a burst of probes must
    /// not stampede the small, shared keystore connection pool that bearer
    /// verification depends on. See [`AuthState::ping_cached`].
    pub auth_ping: Arc<tokio::sync::Mutex<Option<CachedAuthPing>>>,
}

/// A cached keystore liveness-ping outcome with an expiry timestamp.
#[derive(Debug, Clone)]
pub struct CachedAuthPing {
    /// Ping outcome: `Ok(())` on success, `Err(msg)` on failure/timeout.
    result: Result<(), String>,
    /// When this ping was performed.
    checked_at: Instant,
}

impl AuthState {
    /// Liveness ping against the fleet keystore, memoised to protect the pool.
    ///
    /// The result is cached for [`Self::PING_CACHE_TTL`]; within a window every
    /// probe reuses the last outcome and touches no connection, collapsing any
    /// burst of unauthenticated `/health` requests into at most one ping per
    /// window. The `tokio::sync::Mutex` also serialises refreshes, so at most
    /// one in-flight ping holds a keystore connection at any instant (mirrors
    /// the thundering-herd guard on the schema cache).
    ///
    /// The ping is bounded by [`Self::PING_TIMEOUT`]: a slow/partitioned
    /// keystore reports a timeout error rather than blocking on sqlx's full
    /// connect/acquire deadline. Failed and timed-out pings are cached too, so
    /// a downed keystore cannot turn every probe into a fresh stall.
    ///
    /// Returns `Ok(())` when the keystore answered, or `Err(msg)` describing the
    /// failure or timeout.
    pub async fn ping_cached(&self) -> Result<(), String> {
        ping_cached_with(
            &self.auth_ping,
            Self::PING_CACHE_TTL,
            Self::PING_TIMEOUT,
            || self.key_store.ping(),
        )
        .await
    }

    /// Upper bound on a single keystore liveness ping.
    ///
    /// Chosen well under the k8s probe timeout (default 1s is exceeded, but the
    /// auth check is non-critical → `Degraded` → HTTP 200, so a slow keystore
    /// never fails liveness) and far under sqlx's ~30s connect/acquire deadline.
    const PING_TIMEOUT: Duration = Duration::from_secs(2);

    /// How long a keystore ping outcome is reused before a fresh probe.
    const PING_CACHE_TTL: Duration = Duration::from_secs(5);
}

/// Memoise a liveness ping behind a TTL and a timeout bound.
///
/// Extracted from [`AuthState::ping_cached`] so the TTL, timeout, and
/// error-caching behaviour can be exercised with a controllable pinger (see the
/// module tests) rather than only against a live keystore. Within `ttl` of the
/// last probe the cached outcome (success *or* failure) is returned and `ping`
/// is never invoked; otherwise `ping` runs under a `timeout` bound and its
/// result — including a timeout mapped to `Err` — is cached before returning.
async fn ping_cached_with<F, Fut, E>(
    cache: &tokio::sync::Mutex<Option<CachedAuthPing>>,
    ttl: Duration,
    timeout: Duration,
    ping: F,
) -> Result<(), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let mut guard = cache.lock().await;
    if let Some(cached) = guard.as_ref()
        && cached.checked_at.elapsed() < ttl
    {
        return cached.result.clone();
    }

    let result = match tokio::time::timeout(timeout, ping()).await {
        Ok(res) => res.map_err(|e| e.to_string()),
        Err(_) => Err(format!(
            "keystore ping timed out after {}s",
            timeout.as_secs()
        )),
    };
    *guard = Some(CachedAuthPing {
        result: result.clone(),
        checked_at: Instant::now(),
    });
    result
}

/// Ingest pipeline state.
#[derive(Debug, Clone)]
pub struct IngestState {
    /// WAL writer for ingested events (None if ingest is disabled).
    pub wal_writer: Option<Arc<WalWriter>>,
    /// Shared pipeline writer (WAL + hot buffer + event bus). None if ingest is disabled.
    pub pipeline: Option<Arc<PipelineWriter>>,
    /// Event bus for real-time fanout to subscribers (None if ingest is disabled).
    pub event_bus: Option<Arc<LocalEventBus>>,
    /// Total events ingested since startup (for monitor dashboard).
    pub total_events: Arc<AtomicU64>,
    /// Total events rejected since startup (for monitor dashboard).
    pub total_rejected: Arc<AtomicU64>,
    /// Syslog ingest counters (None if syslog is disabled).
    pub syslog_stats: Option<Arc<SyslogStats>>,
    /// Compaction cycle counters (None if ingest is disabled).
    pub compaction_stats: Option<Arc<CompactionStats>>,
}

/// Shared counters for syslog ingest stats (dashboard + monitoring).
pub struct SyslogStats {
    /// Total events received via UDP.
    pub events_udp: AtomicU64,
    /// Total events received via TCP.
    pub events_tcp: AtomicU64,
    /// Total unparseable syslog messages.
    pub parse_errors: AtomicU64,
    /// Total events dropped due to channel backpressure.
    pub dropped: AtomicU64,
    /// Current active TCP connections (incremented on connect, decremented on close).
    pub tcp_connections: AtomicU64,
}

impl Default for SyslogStats {
    fn default() -> Self {
        Self {
            events_udp: AtomicU64::new(0),
            events_tcp: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            tcp_connections: AtomicU64::new(0),
        }
    }
}

impl SyslogStats {
    /// Combined event count (UDP + TCP).
    pub fn total_events(&self) -> u64 {
        self.events_udp.load(Ordering::Relaxed) + self.events_tcp.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for SyslogStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyslogStats")
            .field("events_udp", &self.events_udp.load(Ordering::Relaxed))
            .field("events_tcp", &self.events_tcp.load(Ordering::Relaxed))
            .field("parse_errors", &self.parse_errors.load(Ordering::Relaxed))
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .field(
                "tcp_connections",
                &self.tcp_connections.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// Shared counters for compaction cycle tracking (dashboard + monitoring).
pub struct CompactionStats {
    /// Unix epoch seconds of last successful compaction (0 = never).
    pub last_run_epoch_secs: AtomicU64,
    /// Total successful compaction cycles.
    pub total_runs: AtomicU64,
    /// Total compaction failures: failed WAL-compaction cycles PLUS
    /// per-service daily-rollup failures and quarantined-input data-loss
    /// counts (so this can exceed `total_runs` — it is a failure/data-loss
    /// tally, not a cycle count).
    pub total_errors: AtomicU64,
}

impl Default for CompactionStats {
    fn default() -> Self {
        Self {
            last_run_epoch_secs: AtomicU64::new(0),
            total_runs: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
        }
    }
}

impl std::fmt::Debug for CompactionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactionStats")
            .field(
                "last_run_epoch_secs",
                &self.last_run_epoch_secs.load(Ordering::Relaxed),
            )
            .field("total_runs", &self.total_runs.load(Ordering::Relaxed))
            .field("total_errors", &self.total_errors.load(Ordering::Relaxed))
            .finish()
    }
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
    /// Earliest date from partition directory names (YYYY-MM-DD).
    pub earliest_date: Option<String>,
    /// Latest date from partition directory names (YYYY-MM-DD).
    pub latest_date: Option<String>,
    /// Total byte size of all parquet files.
    pub total_bytes: u64,
    /// Distinct service names from parquet filenames.
    pub services: Vec<String>,
}

/// A cached field value sample with an expiry timestamp.
#[derive(Debug, Clone)]
pub struct CachedFieldValues {
    /// Sampled distinct values for the field.
    pub values: Vec<String>,
    /// When this cache entry was created.
    pub cached_at: Instant,
}

/// Pre-computed per-service schema from the background refresh job.
#[derive(Debug, Clone)]
pub struct CachedServiceSchema {
    /// Per-service schema and statistics.
    pub services: Vec<ServiceSchema>,
    /// When this cache was last refreshed.
    pub cached_at: Instant,
}

/// Build [`AuthState`]: connect the fleet keystore (eagerly), derive the
/// bearer-only [`fleet_auth::SessionState`], and open the transitional
/// `SQLite` app-state stores.
async fn build_auth_state(config: &Config) -> Result<AuthState, crate::error::ServerError> {
    let database_url = config
        .auth
        .resolve_database_url()
        .map_err(|e| crate::error::ServerError::Internal(e.to_string()))?;

    // Eager connect + ping: a dead backend is a distinct, loud startup
    // error — trawld cannot authenticate anyone without it.
    let key_store = fleet_auth::KeyStore::connect(&database_url)
        .await
        .map_err(|e| {
            crate::error::ServerError::ServiceUnavailable(format!(
                "fleet auth backend unreachable at startup (is postgres up and migrated via \
                 `fleet-admin migrate`?): {e}"
            ))
        })?;
    key_store.ping().await.map_err(|e| {
        crate::error::ServerError::ServiceUnavailable(format!(
            "fleet auth backend failed ping at startup: {e}"
        ))
    })?;

    // trawld only ever runs require_bearer; the session key is a
    // throwaway (see AuthState::session_state).
    let session_state = fleet_auth::SessionState::new(
        key_store.clone(),
        Arc::new(fleet_auth::SessionKey::generate()),
        Arc::new(
            fleet_auth::SessionConfig::new("fleet_session", "trawl")
                .map_err(crate::error::ServerError::from)?,
        ),
    )
    .map_err(crate::error::ServerError::from)?;

    // Legacy-db quarantine (ADR-0004): the transitional app-state store keys
    // its rows on postgres key ids, which are unrelated to the old sqlite
    // keystore's. Refuse to open a pre-cutover keystore file (under any name)
    // so a fresh pg key can't inherit a legacy sqlite key's rows. The config
    // basename guard only catches the default `auth.db`; this catches renames.
    trawl_auth::reject_legacy_keystore(&config.auth.db_path)?;
    let history = HistoryStore::open(&config.auth.db_path)?;
    let saved = SavedQueryStore::open(&config.auth.db_path)?;
    let schedule = ScheduleStore::open(&config.auth.db_path)?;

    Ok(AuthState {
        key_store,
        session_state,
        history: Arc::new(Mutex::new(history)),
        saved: Arc::new(Mutex::new(saved)),
        schedule: Arc::new(Mutex::new(schedule)),
        auth_ping: Arc::new(tokio::sync::Mutex::new(None)),
    })
}

impl AppState {
    /// Construct app state from a validated [`Config`].
    ///
    /// Connects to the fleet-auth Postgres keystore (eagerly — trawld fails
    /// fast at startup when the auth backend is unreachable) and opens the
    /// transitional `SQLite` app-state stores.
    pub async fn from_config(
        config: &Config,
        metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    ) -> Result<(Self, HttpConfig), crate::error::ServerError> {
        let auth = build_auth_state(config).await?;

        let (wal_writer, event_bus, hot_buffer, pipeline) = if config.ingest.enabled {
            let writer = Arc::new(WalWriter::new(config.wal_dir()));
            let bus = Arc::new(LocalEventBus::new(config.ingest.event_bus_capacity));
            let buffer = Arc::new(HotBuffer::new(HotBufferConfig {
                max_events: config.ingest.hot_buffer_max_events,
                max_bytes: config.ingest.hot_buffer_max_bytes,
            }));
            let pipeline = Arc::new(PipelineWriter::new(
                Arc::clone(&writer),
                Some(Arc::clone(&buffer)),
                Some(Arc::clone(&bus)),
            ));
            (Some(writer), Some(bus), Some(buffer), Some(pipeline))
        } else {
            (None, None, None, None)
        };

        let state = Self {
            query: QueryState {
                pool: ExecutorPool::new(
                    config.data.base_dir().to_string_lossy().into_owned(),
                    config.server.max_concurrent_queries,
                    config.server.max_result_rows,
                    hot_buffer.clone(),
                ),
                timeout_secs: config.server.timeout_secs,
                tracker: Arc::new(QueryTracker::with_capacity(config.server.max_query_history)),
                schema_cache_ttl_secs: config.server.schema_cache_ttl_secs,
                max_export_rows: config.server.max_export_rows,
                schema_cache: Arc::new(tokio::sync::Mutex::new(None)),
                field_values_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                hot_buffer,
                service_schema_cache: Arc::new(Mutex::new(None)),
                sse_semaphore: Arc::new(Semaphore::new(config.server.max_sse_connections)),
                query_log: None,
            },
            auth,
            ingest: IngestState {
                wal_writer,
                pipeline,
                event_bus,
                total_events: Arc::new(AtomicU64::new(0)),
                total_rejected: Arc::new(AtomicU64::new(0)),
                syslog_stats: if config.syslog.enabled && config.ingest.enabled {
                    Some(Arc::new(SyslogStats::default()))
                } else {
                    None
                },
                compaction_stats: if config.ingest.enabled {
                    Some(Arc::new(CompactionStats::default()))
                } else {
                    None
                },
            },
            start_time: Instant::now(),
            total_queries: Arc::new(AtomicU64::new(0)),
            metrics_handle,
            dashboard_snapshot: Arc::new(Mutex::new(None)),
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

#[cfg(test)]
mod tests {
    use super::{CachedAuthPing, ping_cached_with};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    const LONG_TTL: Duration = Duration::from_secs(3600);
    const LONG_TIMEOUT: Duration = Duration::from_secs(60);

    /// A successful ping is cached: within the TTL a second probe returns the
    /// stored `Ok` without re-invoking the pinger.
    #[tokio::test]
    async fn caches_ok_within_ttl() {
        let cache: TokioMutex<Option<CachedAuthPing>> = TokioMutex::new(None);
        let calls = AtomicUsize::new(0);
        let ping = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<(), String>(())
        };

        assert_eq!(
            ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, ping).await,
            Ok(())
        );
        assert_eq!(
            ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, ping).await,
            Ok(())
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second probe must reuse the cache"
        );
    }

    /// A failed ping is cached too: within the TTL the downed-keystore error is
    /// replayed without touching the pinger, so a burst cannot stampede a dead
    /// backend.
    #[tokio::test]
    async fn caches_err_within_ttl() {
        let cache: TokioMutex<Option<CachedAuthPing>> = TokioMutex::new(None);
        let calls = AtomicUsize::new(0);
        let ping = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), String>("backend down".to_string())
        };

        let first = ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, ping).await;
        let second = ping_cached_with(&cache, LONG_TTL, LONG_TIMEOUT, ping).await;
        assert_eq!(first, Err("backend down".to_string()));
        assert_eq!(second, Err("backend down".to_string()));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "errors must be cached, not retried"
        );
    }

    /// Once the TTL has elapsed the cache is bypassed and the pinger runs again.
    /// A zero TTL makes every probe expired, so each call re-probes.
    #[tokio::test]
    async fn refreshes_after_ttl_expiry() {
        let cache: TokioMutex<Option<CachedAuthPing>> = TokioMutex::new(None);
        let calls = AtomicUsize::new(0);
        let ping = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<(), String>(())
        };

        ping_cached_with(&cache, Duration::ZERO, LONG_TIMEOUT, ping)
            .await
            .unwrap();
        ping_cached_with(&cache, Duration::ZERO, LONG_TIMEOUT, ping)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expired cache must re-probe"
        );
    }

    /// A ping that outlives the timeout bound is mapped to a timeout `Err`
    /// rather than blocking, and that error is cached like any other outcome.
    #[tokio::test(start_paused = true)]
    async fn maps_timeout_to_err() {
        let cache: TokioMutex<Option<CachedAuthPing>> = TokioMutex::new(None);
        let timeout = Duration::from_secs(2);
        let ping = || async {
            // Far outlives the 2s bound; paused-clock auto-advance fires the
            // timeout first, so the test itself never really waits.
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok::<(), String>(())
        };

        let result = ping_cached_with(&cache, LONG_TTL, timeout, ping).await;
        assert_eq!(result, Err("keystore ping timed out after 2s".to_string()));

        // The timeout error is now cached: a probe within the TTL replays it
        // without invoking a (this time instant) pinger.
        let cached =
            ping_cached_with(&cache, LONG_TTL, timeout, || async { Ok::<(), String>(()) }).await;
        assert_eq!(cached, Err("keystore ping timed out after 2s".to_string()));
    }
}
