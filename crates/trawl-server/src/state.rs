// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared application state for axum handlers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use trawl_api::{DashboardSnapshot, SchemaColumnResponse, ServiceSchema};

use crate::bus::LocalEventBus;
use crate::config::{Config, RateLimitConfig};
use crate::hot_buffer::{HotBuffer, HotBufferConfig};
use crate::ingest::pipeline::PipelineWriter;
use crate::ingest::wal::WalWriter;
use crate::ping::{PingCache, ping_cached_with};
use crate::pool::ExecutorPool;
use crate::query_log::QueryLog;
use crate::store::StorageState;
use crate::tracker::QueryTracker;

/// Shared state injected into handlers via axum's `State` extractor.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Query execution resources.
    pub query: QueryState,
    /// Authentication resources.
    pub auth: AuthState,
    /// Postgres app-state stores (history, saved queries, schedules, runs).
    pub storage: StorageState,
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
    /// The repin engine. `Some` exactly when ingest is enabled: a query-only
    /// node owns nothing under the data root, so `POST /api/v1/schema/repin`
    /// answers 503 there.
    pub repin: Option<Arc<crate::repin::RepinEngine>>,
    /// The pin garbage collector. `Some` on the same terms as
    /// [`Self::repin`]: proving a pin dead means reading every parquet
    /// footer under the data root, and a query-only node owns none of
    /// them.
    pub gc: Option<Arc<crate::catalog::gc::PinGc>>,
}

/// Maximum concurrent admin dashboard-stats SSE streams. Hard-coded (no
/// config knob): the endpoint is `ServerManage`-gated, so this only bounds
/// a handful of admin browser tabs.
const MAX_DASHBOARD_STREAMS: usize = 8;

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
    /// The longest age any env still keeps data for, in seconds
    /// ([`crate::retention::maximum_enabled_age_secs`]); `None` when some
    /// env keeps its data forever. `/api/v1/schema` windows catalog fields
    /// on `last_seen` against it so autocomplete stops offering fields
    /// whose data has aged out everywhere (`?all=true` lifts the window).
    ///
    /// Resolved once at construction: retention config is fixed for the
    /// process, and the schema column cache's two slots are keyed on
    /// whether the window applied, which only holds while the horizon is
    /// constant.
    pub retention_horizon_secs: Option<u64>,
    /// Maximum rows for export responses (bypasses `max_result_rows`).
    pub max_export_rows: usize,
    /// Cached corpus facts (dates/bytes/services/file count from the
    /// filesystem walk) with TTL.
    ///
    /// Uses `Mutex` (not `RwLock`) to prevent thundering herd: only one
    /// request refreshes the cache while others wait on the lock.
    pub schema_cache: Arc<tokio::sync::Mutex<Option<CachedCorpusFacts>>>,
    /// Cached unscoped `/api/v1/schema` column set (a catalog SELECT that
    /// aggregates every service's observations), under the same TTL and the
    /// same thundering-herd discipline as the corpus facts. Two slots,
    /// indexed by whether the retention window applied, because there are
    /// exactly two unscoped request shapes (`?all=true` lifts the window) and
    /// each entry must survive requests of the other shape: this cache is
    /// the only bound on the whole-table aggregate behind it (migration
    /// 0003), so one shared slot would let alternating `/schema` and
    /// `/schema?all=true` traffic evict each other into a 100% miss rate,
    /// every miss running the aggregate while holding the mutex.
    pub schema_columns_cache: Arc<tokio::sync::Mutex<[Option<CachedSchemaColumns>; 2]>>,
    /// Cached field value samples for autocomplete (shared TTL with schema cache).
    pub field_values_cache: Arc<tokio::sync::Mutex<HashMap<String, CachedFieldValues>>>,
    /// Hot buffer for fresh events not yet compacted to parquet.
    pub hot_buffer: Option<Arc<HotBuffer>>,
    /// Pre-computed per-service schema from background refresh job.
    /// Uses `parking_lot::Mutex` (like `dashboard_snapshot`) for fast reads.
    pub service_schema_cache: Arc<Mutex<Option<CachedServiceSchema>>>,
    /// In-process field-catalog pin cache: hydrated at boot from
    /// `field_types`, refreshed by compaction after every `pin_missing`, so
    /// the query path never touches postgres for pins.
    pub field_catalog: Arc<crate::catalog::FieldCatalog>,
    /// What the analyzer currently calls degraded, reloaded on the
    /// schema-refresh tick.
    ///
    /// Two request paths stamp from it — `QueryResponse.degraded_fields`
    /// (the notice) and `ServiceSchema.degraded_fields` (the badge) — and
    /// neither may reach postgres to do it, so the whole
    /// [`DegradedSnapshot`] is swapped in as one `Arc`: a reader clones the
    /// handle under the lock and walks it outside, and the two halves can
    /// never be read from different generations. Staleness is bounded by one
    /// refresh tick, which is nothing against a condition measured in days.
    pub degraded_fields: Arc<Mutex<Arc<DegradedSnapshot>>>,
    /// Semaphore bounding concurrent SSE streaming connections.
    pub sse_semaphore: Arc<Semaphore>,
    /// Semaphore bounding concurrent admin dashboard-stats streams.
    ///
    /// Separate from `sse_semaphore` so long-lived footer connections
    /// neither consume user query-stream slots nor pollute the monitor's
    /// `sse_active` stat (computed from `sse_semaphore` permits).
    pub dashboard_sse_semaphore: Arc<Semaphore>,
    /// Optional ndjson query debug log.
    pub query_log: Option<Arc<QueryLog>>,
}

/// Authentication state: the fleet keystore and its bearer middleware state.
/// The app-state stores live in [`StorageState`]; they are app state, not
/// auth.
#[derive(Debug, Clone)]
pub struct AuthState {
    /// Fleet-auth Postgres keystore. Cheap to clone (Arc-backed pool +
    /// verification cache live inside).
    pub key_store: fleet_auth::KeyStore,
    /// State for `fleet_auth::require_bearer_only`. trawld never reads session
    /// cookies (that is trawl-web's job), so it holds a keystore-only
    /// [`fleet_auth::BearerState`] with no session key/cookie config at all:
    /// the type forbids ever mounting `require_session` against a
    /// meaningless key.
    pub bearer_state: fleet_auth::BearerState,
    /// Memoised keystore liveness ping, shared by every `/health` probe.
    ///
    /// `/health` is unauthenticated and unthrottled (it sits outside
    /// `require_bearer_only` and `rate_limit_middleware`), so a burst of probes must
    /// not stampede the small, shared keystore connection pool that bearer
    /// verification depends on. See [`AuthState::ping_cached`].
    pub auth_ping: Arc<PingCache>,
}

impl AuthState {
    /// Wrap an already-connected keystore. No I/O: the eager connect + ping
    /// that proves the backend live belongs to the caller.
    ///
    /// trawld only ever runs `require_bearer_only`: it verifies bearer tokens
    /// and never touches session cookies, so it carries just the keystore —
    /// no fabricated session key/config (see [`AuthState::bearer_state`]).
    #[must_use]
    pub fn from_key_store(key_store: fleet_auth::KeyStore) -> Self {
        let bearer_state = fleet_auth::BearerState::new(key_store.clone());
        Self {
            key_store,
            bearer_state,
            auth_ping: Arc::new(PingCache::new(None)),
        }
    }

    /// Liveness ping against the fleet keystore, memoised to protect the pool.
    ///
    /// The result is cached for [`Self::PING_CACHE_TTL`]; within a window every
    /// probe reuses the last outcome and touches no connection, collapsing any
    /// burst of unauthenticated `/health` requests into at most one ping per
    /// window. Refreshes are serialised, so at most one in-flight ping holds a
    /// keystore connection at any instant (mirrors the thundering-herd guard
    /// on the schema cache).
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
            "keystore",
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
    /// Effective env allowlist (never empty; ADR-0009). Events with an
    /// unlisted `env` hard-reject.
    pub envs: Arc<[String]>,
    /// Fills a missing `env` on ingested events.
    pub default_env: Arc<str>,
    /// Parsed `trusted_relays` CIDRs: a host-less event from one of these
    /// peers is rejected instead of peer-repaired. Parsed boot-fatally —
    /// a warn-skipped entry would fail open into host repair.
    pub trusted_relays: Arc<[crate::syslog::CidrEntry]>,
    /// The boot-resolved per-profile `_severity`/`_time` source lists
    /// (ADR-0013).
    ///
    /// Resolved in `main`, before the tracing subscriber, and handed in:
    /// the telemetry layer needs the same policy and is built earlier
    /// still, so resolving it here would mean two resolutions of one
    /// config, the shape that lets two doors disagree. Boot-fatal there,
    /// like `trusted_relays`: a source list that silently never matches is
    /// worse than a refusal to start.
    pub derivation: Arc<crate::ingest::producer::Derivation>,
    /// Repin/compaction interlock. `Some` exactly when ingest is enabled:
    /// a query-only node runs no compaction and refuses repin requests
    /// outright.
    pub repin_coordinator: Option<Arc<crate::repin::RepinCoordinator>>,
}

/// Parse `[ingest] trusted_relays` CIDRs, boot-fatally.
///
/// Deliberately not the syslog `parse_cidrs` warn-skip: a skipped relay
/// CIDR would fail open, peer-repairing host-less events from that relay
/// where the operator configured a reject.
fn parse_trusted_relays(
    cidrs: &[String],
) -> Result<Arc<[crate::syslog::CidrEntry]>, crate::error::ServerError> {
    let mut entries = Vec::with_capacity(cidrs.len());
    for cidr in cidrs {
        let entry = crate::syslog::parse_cidr(cidr).ok_or_else(|| {
            crate::error::ServerError::Internal(format!(
                "invalid CIDR {cidr:?} in ingest.trusted_relays — refusing to \
                 start (a skipped entry would repair hosts where a reject was \
                 configured)"
            ))
        })?;
        if let Some(twin) = crate::syslog::mapped_cover_twin(&entry) {
            entries.push(twin);
        }
        entries.push(entry);
    }
    Ok(entries.into())
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
    /// Total compaction failures: failed WAL-compaction cycles plus
    /// per-service daily-rollup failures and quarantined-input data-loss
    /// counts. It can exceed `total_runs`, being a failure/data-loss tally
    /// rather than a cycle count.
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
    /// Per-key rate limiting config.
    pub rate_limit: RateLimitConfig,
}

/// Cached corpus facts from the parquet filesystem walk, with an expiry
/// timestamp. Purely filesystem truth — schema columns come from the field
/// catalog and are never cached here.
#[derive(Debug, Clone)]
pub struct CachedCorpusFacts {
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
    /// Number of parquet files on disk.
    pub file_count: u64,
}

/// A cached `/api/v1/schema` column set, with an expiry timestamp.
///
/// Only the unscoped listing is cached. `?service=` is client-chosen and
/// unbounded, so keying a map on it would be an unbounded cache — and the
/// scoped listing is already bounded by the pin cap through the
/// `field_services (service, field)` index (migration 0003), while the
/// unscoped one aggregates every service's observations and is what the
/// autocomplete polls. The windowed/unwindowed shape lives in which slot
/// of `schema_columns_cache` holds the entry, not in the entry itself.
#[derive(Debug, Clone)]
pub struct CachedSchemaColumns {
    /// The catalog-served columns, already in display order.
    pub columns: Vec<SchemaColumnResponse>,
    /// When this cache entry was created.
    pub cached_at: Instant,
    /// [`crate::catalog::FieldCatalog::repin_generation`] at the moment the
    /// entry's postgres read was issued. An entry built under an older
    /// generation names a type a repin has since replaced, so it is stale
    /// however young it is.
    pub pins_generation: u64,
}

impl CachedSchemaColumns {
    /// The single freshness rule for this cache: an entry serves iff it is
    /// inside the TTL and was built under the catalog's current repin
    /// generation. Callers must not re-check `cached_at` themselves; a
    /// second rule lets the endpoint serve a retyped field's old type for
    /// up to a TTL after a repin cutover.
    ///
    /// Accepted residual: the cutover is two-phase (the transactional
    /// postgres flip, then [`crate::catalog::FieldCatalog::repin`]), so a
    /// read landing between them still sees the old generation. That window
    /// is microseconds, and postgres-first is load-bearing for boot-replay
    /// crash consistency, so it is inherent rather than fixed here. What
    /// this rule removes is the TTL-scale staleness.
    #[must_use]
    pub fn serve(&self, ttl_secs: u64, generation: u64) -> Option<Vec<SchemaColumnResponse>> {
        (self.pins_generation == generation && self.cached_at.elapsed().as_secs() < ttl_secs)
            .then(|| self.columns.clone())
    }
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

/// One generation of the degraded-field picture: the fields the analyzer
/// indicts, plus which senders' data indicted them.
///
/// Built and swapped as a whole by
/// [`crate::schema_refresh::refresh_degraded_fields`], the only writer.
/// Both halves come from one pair of postgres reads inside one
/// `REPEATABLE READ` transaction, so the badge can never name a field the
/// notice does not, nor the reverse.
///
/// `by_service` is keyed by `fields` at construction (a pair naming a field
/// outside the set is dropped), which is the invariant that makes the two
/// halves one fact rather than two caches that agree by luck.
#[derive(Debug, Default)]
pub struct DegradedSnapshot {
    /// Every degraded field, install-wide — the set the query notice's
    /// membership test runs against.
    pub fields: BTreeSet<String>,
    /// Service to the degraded fields that service actually conflicted on.
    ///
    /// Private: the only sanctioned read is [`Self::services_of`]. A caller
    /// holding the map could join it the wrong way round, badging every
    /// service that merely carries a degraded column, which is the false
    /// positive this snapshot exists to prevent.
    by_service: BTreeMap<String, Vec<String>>,
}

impl DegradedSnapshot {
    /// Build a generation from the degraded set and the `(field, service)`
    /// evidence pairs.
    ///
    /// Pairs are grouped by service, sorted and deduplicated; a pair whose
    /// field is not in `fields` is dropped rather than trusted. The store
    /// reads both halves under one `REPEATABLE READ` snapshot and keys the
    /// pair read on the set, so such a row should be unreachable — the drop
    /// is the belt to that braces.
    #[must_use]
    pub fn new(
        fields: BTreeSet<String>,
        pairs: impl IntoIterator<Item = crate::store::ConflictServicePair>,
    ) -> Self {
        let mut by_service: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for pair in pairs {
            if fields.contains(&pair.field) {
                by_service.entry(pair.service).or_default().push(pair.field);
            }
        }
        for services in by_service.values_mut() {
            services.sort();
            services.dedup();
        }
        Self { fields, by_service }
    }

    /// The degraded fields `service` has actually conflicted on, sorted.
    ///
    /// Empty for a service that merely carries a degraded field's column
    /// without ever having disagreed with its pin — the whole point.
    #[must_use]
    pub fn services_of(&self, service: &str) -> Vec<String> {
        self.by_service.get(service).cloned().unwrap_or_default()
    }
}

/// Build [`AuthState`]: connect the fleet keystore (eagerly) and wrap it as
/// a bearer-only [`fleet_auth::BearerState`].
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

    Ok(AuthState::from_key_store(key_store))
}

/// Build [`StorageState`]: connect the dedicated `trawl` app-state database,
/// take the sole-writer advisory lock, and boot-migrate the schema.
async fn build_storage_state(config: &Config) -> Result<StorageState, crate::error::ServerError> {
    let database_url = config
        .storage
        .resolve_database_url()
        .map_err(|e| crate::error::ServerError::Internal(e.to_string()))?;

    StorageState::connect(&database_url)
        .await
        .map_err(|e| match e {
            crate::store::StoreError::LockHeld => {
                crate::error::ServerError::ServiceUnavailable(e.to_string())
            }
            crate::store::StoreError::Migration(m) => {
                crate::error::ServerError::ServiceUnavailable(format!(
                    "trawl app-state database migration failed at startup: {m}"
                ))
            }
            other => crate::error::ServerError::ServiceUnavailable(format!(
                "trawl app-state database unreachable at startup (is postgres up and the \
                 [storage] database provisioned? Check [storage] database_url or TRAWL_DATABASE_URL): {other}"
            )),
        })
}

impl AppState {
    /// Construct app state from a validated [`Config`].
    ///
    /// Connects to the fleet-auth Postgres keystore and the trawl app-state
    /// database (both eagerly — trawld fails fast at startup when either
    /// backend is unreachable; the app-state boot also takes the sole-writer
    /// advisory lock and runs migrations).
    pub async fn from_config(
        config: &Config,
        metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
        derivation: Arc<crate::ingest::producer::Derivation>,
    ) -> Result<(Self, HttpConfig), crate::error::ServerError> {
        let auth = build_auth_state(config).await?;
        let storage = build_storage_state(config).await?;

        Self::from_parts(config, metrics_handle, derivation, auth, storage).await
    }

    /// Assemble app state over already-connected backends.
    ///
    /// Still async: it awaits the catalog pin load that hydrates the
    /// in-process pin cache.
    #[allow(clippy::too_many_lines)] // linear assembly, clearer unsplit
    pub async fn from_parts(
        config: &Config,
        metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
        derivation: Arc<crate::ingest::producer::Derivation>,
        auth: AuthState,
        storage: StorageState,
    ) -> Result<(Self, HttpConfig), crate::error::ServerError> {
        // Hydrate the in-process pin cache from the migrated catalog so the
        // first query already sees the pins (zero postgres I/O per query).
        let field_catalog = Arc::new(crate::catalog::FieldCatalog::new());
        let pins = storage.catalog.load_pins().await.map_err(|e| {
            crate::error::ServerError::ServiceUnavailable(format!(
                "failed to load field-catalog pins at startup: {e}"
            ))
        })?;
        field_catalog.replace(pins);

        let repin_coordinator = if config.ingest.enabled {
            Some(Arc::new(crate::repin::RepinCoordinator::new()))
        } else {
            None
        };

        let (wal_writer, event_bus, hot_buffer, pipeline) = if config.ingest.enabled {
            let writer = Arc::new(WalWriter::new(config.wal_dir()));
            let bus = Arc::new(LocalEventBus::new(config.ingest.event_bus_capacity));
            let buffer = Arc::new(
                HotBuffer::new(HotBufferConfig {
                    max_events: config.ingest.hot_buffer_max_events,
                    max_bytes: config.ingest.hot_buffer_max_bytes,
                })
                // Snapshots carry the catalog pins so the hot branch of the
                // query union is conformed to the write-time invariant.
                .with_field_catalog(Arc::clone(&field_catalog)),
            );
            let pipeline = Arc::new(PipelineWriter::new(
                Arc::clone(&writer),
                Some(Arc::clone(&buffer)),
                Some(Arc::clone(&bus)),
            ));
            (Some(writer), Some(bus), Some(buffer), Some(pipeline))
        } else {
            (None, None, None, None)
        };

        // One horizon feeds the `/api/v1/schema` window and the pin-gc
        // floor: two readers of the same retention config must not answer
        // differently about how far back the corpus still reaches.
        let retention_horizon_secs = crate::retention::maximum_enabled_age_secs(&config.retention);

        let state = Self {
            query: QueryState {
                pool: ExecutorPool::new(
                    config.data.base_dir().to_string_lossy().into_owned(),
                    config.server.max_concurrent_queries,
                    config.server.max_result_rows,
                    hot_buffer.clone(),
                )
                // Comparison typing: the pool snapshots the full pin set per
                // query. The catalog is constructed unconditionally above,
                // so query-only nodes are covered.
                .with_field_catalog(Arc::clone(&field_catalog)),
                timeout_secs: config.server.timeout_secs,
                tracker: Arc::new(QueryTracker::with_capacity(config.server.max_query_history)),
                schema_cache_ttl_secs: config.server.schema_cache_ttl_secs,
                retention_horizon_secs,
                max_export_rows: config.server.max_export_rows,
                schema_cache: Arc::new(tokio::sync::Mutex::new(None)),
                schema_columns_cache: Arc::new(tokio::sync::Mutex::new([None, None])),
                field_values_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                field_catalog,
                degraded_fields: Arc::new(Mutex::new(Arc::new(DegradedSnapshot::default()))),
                hot_buffer,
                service_schema_cache: Arc::new(Mutex::new(None)),
                sse_semaphore: Arc::new(Semaphore::new(config.server.max_sse_connections)),
                dashboard_sse_semaphore: Arc::new(Semaphore::new(MAX_DASHBOARD_STREAMS)),
                query_log: None,
            },
            auth,
            storage,
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
                envs: config.ingest.effective_envs().into(),
                default_env: config.ingest.default_env.as_str().into(),
                trusted_relays: parse_trusted_relays(&config.ingest.trusted_relays)?,
                derivation,
                repin_coordinator: repin_coordinator.clone(),
            },
            start_time: Instant::now(),
            total_queries: Arc::new(AtomicU64::new(0)),
            metrics_handle,
            dashboard_snapshot: Arc::new(Mutex::new(None)),
            repin: None,
            gc: None,
        };
        let state = {
            let mut state = state;
            state.repin = repin_coordinator.as_ref().map(|coordinator| {
                Arc::new(crate::repin::RepinEngine::new(
                    state.storage.repin.clone(),
                    state.storage.catalog.clone(),
                    Arc::clone(&state.query.field_catalog),
                    Arc::clone(coordinator),
                    state.query.pool.clone(),
                    config.data.base_dir(),
                    config.ingest.compaction_memory_limit.clone(),
                    config.retention.min_free_disk_bytes,
                ))
            });
            // The retention floor is the same horizon the schema window
            // reads, resolved once above: retention config is fixed for the
            // process, and the gc engine reports the window it applied
            // rather than recomputing it per request.
            state.gc = repin_coordinator.map(|coordinator| {
                Arc::new(crate::catalog::gc::PinGc::new(
                    state.storage.catalog.clone(),
                    state.storage.repin.clone(),
                    Arc::clone(&state.query.field_catalog),
                    coordinator,
                    config.data.base_dir(),
                    retention_horizon_secs,
                ))
            });
            state
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
    use super::*;
    use crate::store::ConflictServicePair;

    fn pair(field: &str, service: &str) -> ConflictServicePair {
        ConflictServicePair {
            field: field.to_owned(),
            service: service.to_owned(),
        }
    }

    fn fields(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn cached_columns(age: Duration, generation: u64) -> CachedSchemaColumns {
        CachedSchemaColumns {
            columns: vec![SchemaColumnResponse {
                name: "status".to_owned(),
                data_type: "BIGINT".to_owned(),
            }],
            cached_at: Instant::now().checked_sub(age).expect("age fits"),
            pins_generation: generation,
        }
    }

    /// Both halves of the one freshness rule, in one place: an entry serves
    /// only while it is young and was built under the pin generation the
    /// reader holds.
    #[test]
    fn cached_columns_serve_only_inside_the_ttl_and_generation() {
        let entry = cached_columns(Duration::from_secs(1), 7);
        assert_eq!(
            entry.serve(60, 7).map(|c| c.len()),
            Some(1),
            "young entry under the current generation serves"
        );
        assert!(
            entry.serve(60, 8).is_none(),
            "a repin invalidates a within-TTL entry immediately"
        );

        let old = cached_columns(Duration::from_secs(90), 7);
        assert!(
            old.serve(60, 7).is_none(),
            "an expired entry is stale even at the same generation"
        );
    }

    /// The construction invariant the whole snapshot rests on: `by_service`
    /// is keyed by `fields`, so no service can ever be badged for a field
    /// the notice does not also call degraded. A pair naming a field outside
    /// the set, which is what a repin clearing evidence between the two
    /// reads produces, is dropped rather than trusted.
    #[test]
    fn by_service_never_names_a_field_outside_the_set() {
        let snapshot = DegradedSnapshot::new(
            fields(&["duration"]),
            vec![
                pair("duration", "svc-a"),
                pair("status", "svc-a"),
                pair("status", "svc-b"),
            ],
        );

        for (service, named) in &snapshot.by_service {
            for field in named {
                assert!(
                    snapshot.fields.contains(field),
                    "{service} badged for {field}, which is not degraded"
                );
            }
        }
        assert_eq!(snapshot.services_of("svc-a"), vec!["duration".to_owned()]);
        assert!(
            snapshot.services_of("svc-b").is_empty(),
            "a service whose only evidence is for an undegraded field is not badged"
        );
    }

    /// Per-service lists are sorted and deduplicated: `field_conflict_stats`
    /// is keyed on `(field, service)` so duplicates cannot arise today, but
    /// the badge's order is a rendered wire fact and must not depend on the
    /// row order postgres happened to return.
    #[test]
    fn services_of_is_sorted_and_deduplicated() {
        let snapshot = DegradedSnapshot::new(
            fields(&["duration", "status", "bytes"]),
            vec![
                pair("status", "svc-a"),
                pair("bytes", "svc-a"),
                pair("duration", "svc-a"),
                pair("status", "svc-a"),
            ],
        );

        assert_eq!(
            snapshot.services_of("svc-a"),
            vec![
                "bytes".to_owned(),
                "duration".to_owned(),
                "status".to_owned()
            ]
        );
    }

    /// A service with no evidence at all — the overwhelmingly common case,
    /// including every service on a healthy install — reads as an empty
    /// list, which the wire then omits entirely.
    #[test]
    fn an_unseen_service_has_no_degraded_fields() {
        let snapshot =
            DegradedSnapshot::new(fields(&["duration"]), vec![pair("duration", "svc-a")]);
        assert!(snapshot.services_of("svc-b").is_empty());
        assert!(DegradedSnapshot::default().services_of("svc-a").is_empty());
    }
}
