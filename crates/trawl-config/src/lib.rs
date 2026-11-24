// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared configuration types for the trawl workspace.

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

pub mod color;
pub mod fs;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub data: DataConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub ingest: IngestConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub syslog: SyslogConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub investigation: InvestigationConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to bind the HTTPS listener (e.g. "127.0.0.1:5514").
    #[serde(default = "default_http_addr")]
    pub http_addr: String,

    /// Query execution timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Maximum concurrent queries (bounds the executor pool). Default: the
    /// CPU count, at most [`MAX_DEFAULT_CONCURRENT_QUERIES`] (4 when the CPU
    /// count is unknown).
    #[serde(default = "default_max_concurrent_queries")]
    pub max_concurrent_queries: usize,

    /// Maximum number of rows a query can return before being rejected.
    #[serde(default = "default_max_result_rows")]
    pub max_result_rows: usize,

    /// Maximum number of rows allowed in export responses (default: 1M).
    /// Exports bypass `max_result_rows` to support larger downloads.
    #[serde(default = "default_max_export_rows")]
    pub max_export_rows: usize,

    /// Maximum request body size in bytes (default: 128 KB).
    /// Accepts human-readable sizes like `"128K"`, `"1M"`.
    #[serde(
        default = "default_max_request_body_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub max_request_body_bytes: usize,

    /// Maximum HTTP requests in progress across the whole listener
    /// (default: 32, range 1 to 65,536; ADR-0054).
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,

    /// Graceful shutdown drain timeout in seconds (default: 30).
    #[serde(default = "default_shutdown_drain_secs")]
    pub shutdown_drain_secs: u64,

    /// Optional JSON log file. Opened when either ingest or internal telemetry
    /// is disabled. When both are enabled, server events use the ingest
    /// pipeline and this path is not opened.
    pub log_file: Option<PathBuf>,

    /// Path to TLS certificate (PEM). If omitted, a self-signed cert is auto-generated.
    pub tls_cert_path: Option<PathBuf>,

    /// Path to TLS private key (PEM). If omitted, a self-signed key is auto-generated.
    pub tls_key_path: Option<PathBuf>,

    /// How often to check cert files for changes, in seconds (default: 300).
    /// Set to 0 to disable automatic cert reload.
    #[serde(default = "default_tls_reload_interval_secs")]
    pub tls_reload_interval_secs: u64,

    /// Optional query debug log path. When set, every query execution is
    /// logged as ndjson to this file for `tail -f | jq` debugging.
    ///
    /// Sensitive: entries combine identity, raw query text, SQL parameter
    /// values, source paths, and result samples. The file is owner-only
    /// (`0600`) on Unix and bounded by `query_log_max_bytes`.
    pub query_log: Option<PathBuf>,

    /// Size cap for the query debug log. Past it the file rolls over to a
    /// single retained `<path>.1`. Default: 100 MiB. `0` disables
    /// rollover (unbounded file). Accepts human-readable sizes like
    /// `"100M"`.
    #[serde(
        default = "default_query_log_max_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub query_log_max_bytes: usize,

    /// Allowed CORS origins (e.g. `["https://trawl.example.com"]`).
    /// Empty list (default) means no CORS headers are sent, so the browser's
    /// same-origin policy blocks all cross-origin requests.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    /// Schema cache TTL in seconds (default: 60). The one knob behind every
    /// cached schema read: the unscoped `GET /schema` columns and corpus
    /// facts, the `/schema/values` samples, and the background schema-refresh
    /// interval. A `?service=`-scoped column read is always fresh.
    #[serde(default = "default_schema_cache_ttl_secs")]
    pub schema_cache_ttl_secs: u64,

    /// Maximum number of completed queries kept in the history ring buffer
    /// (default: 1000). Visible via `GET /api/v1/queries` to any key holding
    /// the `query` permission.
    #[serde(default = "default_max_query_history")]
    pub max_query_history: usize,

    /// Maximum concurrent SSE streaming connections (default: 32).
    /// Returns 429 when exceeded to prevent resource exhaustion.
    #[serde(default = "default_max_sse_connections")]
    pub max_sse_connections: usize,

    /// Per-key rate limiting (requests per minute). 0 = disabled.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,

    /// Monitor dashboard refresh interval in milliseconds (default: 1000).
    /// Only used when trawld runs interactively with a TTY.
    #[serde(default = "default_monitor_refresh_ms")]
    pub monitor_refresh_ms: u64,

    /// Query scratch directory (ADR-0056): each query database's private
    /// spill directory, the hot-buffer query snapshots and the Parquet
    /// export staging files. Unset means [`Config::query_scratch_dir`]'s
    /// default, the data root's sibling `<data.path>.query-scratch`. It may
    /// not be the data root, the WAL directory or a repin staging root, nor
    /// sit inside or above any of them. A value is an absolute path with no
    /// `..` component and no glob metacharacter.
    pub query_scratch_dir: Option<PathBuf>,

    /// Root where captured results live (ADR-0057). Unset means [`Config::results_dir`]'s
    /// default, the data root's sibling `<data.path>.results`. It follows
    /// the query scratch directory's rules and must also stay apart from the
    /// query scratch directory: neither may equal, sit inside or contain the other.
    pub results_dir: Option<PathBuf>,

    /// The origin trawld puts in front of every link to Search that it
    /// hands out (ADR-0058), such as `https://trawl.example.com`. It is never
    /// derived from the `Host`, `X-Forwarded-*` or `Forwarded` headers of a
    /// request, and when it is unset no link is emitted.
    pub link_origin: Option<String>,

    /// The query database's spill cap, `DuckDB`'s `max_temp_directory_size`,
    /// shared by every query, export and field sample (default: 4 GiB).
    /// Accepts human-readable sizes like `"4G"`. Zero is refused.
    #[serde(
        default = "default_query_spill_max_bytes",
        deserialize_with = "deserialize_nonzero_byte_size"
    )]
    pub query_spill_max_bytes: NonZeroU64,

    /// The query database's memory limit, `DuckDB`'s `memory_limit`
    /// (default: 2 GiB). It bounds `DuckDB`-managed memory, not the
    /// process. Accepts human-readable sizes like `"2G"`. Zero is refused.
    #[serde(
        default = "default_query_memory_limit",
        deserialize_with = "deserialize_nonzero_byte_size"
    )]
    pub query_memory_limit: NonZeroU64,

    /// The query database's worker threads, `DuckDB`'s `threads`
    /// (default: 4). Independent of `max_concurrent_queries`, which counts
    /// queries, not threads. Zero is refused.
    #[serde(
        default = "default_query_threads",
        deserialize_with = "deserialize_nonzero_u32"
    )]
    pub query_threads: NonZeroU32,
}

impl ServerConfig {
    /// [`Config::from_toml`] applies it at parse time, so trawld's own call sites
    /// read the already-resolved field; trawl-web deserializes the file itself
    /// and calls this directly when deriving its upstream URL.
    #[must_use]
    pub fn resolve_http_addr(&self) -> String {
        Self::resolve_http_addr_from(
            std::env::var("TRAWL_HTTP_ADDR").ok().as_deref(),
            &self.http_addr,
        )
    }

    /// Pure resolution core, split out for testability (mutating process
    /// env in tests is forbidden under `unsafe_code = "forbid"`).
    fn resolve_http_addr_from(env_value: Option<&str>, configured: &str) -> String {
        Self::http_addr_override(env_value).map_or_else(|| configured.to_owned(), str::to_owned)
    }

    /// The `TRAWL_HTTP_ADDR` rule on its own: `env_value` replaces `[server]
    /// http_addr` unless it is unset or empty, and `None` means the
    /// configured address applies.
    ///
    /// trawl-web observes the variable itself and needs to know which of the
    /// two applied, so it calls this rather than
    /// [`Self::resolve_http_addr`]. The caller turns the variable into text
    /// first; both callers treat a value that is not UTF-8 as unset.
    #[must_use]
    pub fn http_addr_override(env_value: Option<&str>) -> Option<&str> {
        env_value.filter(|s| !s.is_empty())
    }
}

/// Per-key rate limiting in requests per minute (ADR-0006).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Requests/minute allowed per API key on the interactive API routes
    /// (default: 100). 0 = disabled.
    #[serde(default = "default_rate_limit_rpm")]
    pub default_rpm: u32,
    /// Requests/minute allowed per API key on `/api/v1/ingest`
    /// (default: 1000). 0 = disabled.
    #[serde(default = "default_ingest_rate_limit_rpm")]
    pub ingest_rpm: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataConfig {
    /// Directory containing parquet files (e.g. "/var/lib/trawl/data").
    ///
    /// Must be a directory path. Glob metacharacters (`*`, `?`, `[`, `]`,
    /// `{`, `}`) and a `..` component are rejected when daemon
    /// configuration is validated.
    pub path: String,
}

impl DataConfig {
    pub fn base_dir(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestConfig {
    #[serde(default = "default_ingest_enabled")]
    pub enabled: bool,

    /// Maximum request body size for ingest (bytes). Default: 16 MB.
    /// Accepts human-readable sizes like `"16M"`, `"1G"`.
    #[serde(
        default = "default_ingest_max_body_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub max_body_bytes: usize,

    /// WAL directory path. Defaults to `{data.base_dir}/wal/`. A `..`
    /// component is rejected when daemon configuration is validated.
    pub wal_dir: Option<PathBuf>,

    /// How often the compaction task runs (seconds). Default: 10.
    #[serde(default = "default_compaction_interval_secs")]
    pub compaction_interval_secs: u64,

    /// Write internal server events to the ingest pipeline as `service:trawld`.
    /// Enables querying server telemetry via the trawl DSL for dashboards
    /// and audit trails. Default: true (when ingest is enabled).
    #[serde(default = "default_internal_telemetry")]
    pub internal_telemetry: bool,

    /// Consolidate per-service hourly parquet files into daily files for
    /// older dates. Runs on the compaction tick; skips today's directory.
    /// Dramatically reduces file count for long lookback queries.
    /// Default: true.
    #[serde(default = "default_daily_rollup")]
    pub daily_rollup: bool,

    /// Capacity of the in-memory event bus broadcast channel.
    /// When the channel is full, slow subscribers miss messages and
    /// receive a `Lagged` notification. Default: 4096.
    #[serde(default = "default_event_bus_capacity")]
    pub event_bus_capacity: usize,

    /// Maximum number of events the hot buffer holds: events not yet
    /// compacted to parquet, plus the space producers have reserved for
    /// events they are writing. Default: 100,000.
    ///
    /// Ingest is admitted against this cap before it is written
    /// (ADR-0043).
    #[serde(default = "default_hot_buffer_max_events")]
    pub hot_buffer_max_events: usize,

    /// Maximum serialized bytes the hot buffer holds, counted the same way
    /// as `hot_buffer_max_events` (resident plus reserved) and admitted
    /// against the same way: 15/16 for HTTP and syslog, the full cap for
    /// the server's own telemetry, and refusal when it is full.
    /// Default: 100 MB. Accepts human-readable sizes like `"100M"`, `"1G"`.
    #[serde(
        default = "default_hot_buffer_max_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub hot_buffer_max_bytes: usize,

    /// How often server stats (pool utilization, SSE connections, hot buffer
    /// metrics) are emitted as telemetry events (seconds). Default: 60.
    /// Lower values give more granular observability at the cost of telemetry
    /// volume. Must be greater than 0.
    #[serde(default = "default_stats_interval_secs")]
    pub stats_interval_secs: u64,

    /// How often buffered tracing events are flushed to WAL (seconds).
    /// Default: 1. Lower values reduce latency for self-hosted dashboards
    /// but increase I/O.
    #[serde(default = "default_telemetry_flush_interval_secs")]
    pub telemetry_flush_interval_secs: u64,

    /// One cap on all the memory internal telemetry holds while the WAL is
    /// unhealthy: the active buffer, the retry queue, and the batch in
    /// flight through a write. Default: 16 MiB. Accepts human-readable
    /// sizes like `"16M"`. Unlike `hot_buffer_max_bytes`, which counts
    /// serialized bytes only, the charge is an estimate: serialized ndjson
    /// bytes plus the retained event maps
    /// (which hold roughly the same payload again) plus a fixed per-event
    /// overhead. Enforced as events arrive: over budget the oldest queued
    /// batches are shed first and then the incoming event itself, counted
    /// in `trawl_telemetry_events_dropped_total{reason="buffer_cap"}`. Must
    /// be at least [`MIN_TELEMETRY_BUFFER_MAX_BYTES`] (64 KiB): set
    /// `internal_telemetry = false` to turn self-telemetry off.
    #[serde(
        default = "default_telemetry_buffer_max_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub telemetry_buffer_max_bytes: usize,

    /// Maximum WAL files per compaction chunk. Larger backlogs are split
    /// into chunks of this size and merged incrementally. Default: 500.
    /// At most [`MAX_COMPACTION_CHUNK_SIZE`].
    #[serde(default = "default_compaction_chunk_size")]
    pub compaction_chunk_size: usize,

    /// `DuckDB` memory limit for compaction connections. Forces spill-to-disk
    /// earlier rather than letting `DuckDB` use 80% of container RAM.
    /// Compaction, conform and repin each open a database with it, and they
    /// can run at the same time. Accepts `DuckDB` memory strings like
    /// `"2GiB"`, `"512MiB"`; `DuckDB` reads `GB` and `MB` as powers of ten.
    /// Default: `"1GiB"`.
    #[serde(default = "default_compaction_memory_limit")]
    pub compaction_memory_limit: String,

    /// Fills a missing `env` on ingested events (recorded as the
    /// `env.defaulted` repair). Must be a member of the effective env
    /// allowlist and pass the env charset. Default: `"prod"` (ADR-0009).
    #[serde(default = "default_env_name")]
    pub default_env: String,

    /// Environment allowlist: an event whose `env` is not listed here
    /// hard-rejects with a typed reason — repairing it into `default_env`
    /// would misfile data in the wrong path root permanently. Omitted or
    /// empty means implicitly `[default_env]` (see
    /// [`IngestConfig::effective_envs`]). Every entry must match
    /// `[a-z0-9_-]{1,32}`; `wal` and `scheduled` are reserved (ADR-0009).
    #[serde(default)]
    pub envs: Vec<String>,

    /// CIDR blocks of trusted relays/collectors. An event with no `host`
    /// from a peer inside any of these blocks is rejected instead of
    /// repaired — behind a relay the peer address is confidently wrong.
    /// Parsed (boot-fatal on a bad entry) by the server at startup.
    #[serde(default)]
    pub trusted_relays: Vec<String>,

    /// Wire keys `_severity` derives from, in precedence order — first
    /// mappable wins (ADR-0013).
    ///
    /// Entries take the bare-string shorthand (`"level"`) or the typed
    /// form (`{ field = "syslog_severity", dialect = "syslog" }`); the
    /// dialect governs numerics only.
    ///
    /// Empty is legal and means "derive nothing". Default: [`DEFAULT_SEVERITY_FROM`].
    ///
    /// Semantics are validated boot-fatally by the server:
    /// this crate owns the shape, trawl-server owns the rules.
    #[serde(default = "default_severity_from")]
    pub severity_from: Vec<DerivationSourceSpec>,

    /// Wire keys `_time` derives from, in precedence order — first
    /// present wins, and only `_time` itself is consumed (ADR-0013 §2).
    ///
    /// Must contain `_time`, which is also the sole reserved name
    /// permitted here (server-validated, boot-fatal). A `dialect` on an
    /// entry is an error — dialects govern severity numerics alone.
    /// Default: [`DEFAULT_TIME_FROM`].
    #[serde(default = "default_time_from")]
    pub time_from: Vec<DerivationSourceSpec>,
}

/// One entry of a derivation source list, in either of its two TOML
/// spellings (ADR-0013).
///
/// ```toml
/// severity_from = ["severity", { field = "syslog_severity", dialect = "syslog" }]
/// ```
///
/// `dialect` is an `Option` rather than a defaulted `String` so the server
/// can tell "explicitly otel" from "unset" — `time_from` refuses the key
/// outright, and refusing it means refusing the SPELLING, not the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivationSourceSpec {
    /// The shorthand: `"level"` ≡ `{ field = "level" }`.
    Bare(String),
    /// The typed form. Unknown keys are an error rather than a silent
    /// default — a typo'd `dialct` would otherwise read as plain `otel`
    /// and quietly stop inverting a syslog feed.
    Typed {
        /// The wire key to read.
        field: String,
        /// `otel` | `syslog`; absent means the server's default (`otel`).
        dialect: Option<String>,
    },
}

/// Hand-written because `deny_unknown_fields` is not a serde variant
/// attribute: the typed form is deserialized through a private struct that
/// carries it, then folded back into the public enum shape. The refusal is
/// the point — see [`DerivationSourceSpec::Typed`].
impl<'de> Deserialize<'de> for DerivationSourceSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TypedSource {
            field: String,
            dialect: Option<String>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Spec {
            Bare(String),
            Typed(TypedSource),
        }

        Ok(match Spec::deserialize(deserializer)? {
            Spec::Bare(field) => Self::Bare(field),
            Spec::Typed(TypedSource { field, dialect }) => Self::Typed { field, dialect },
        })
    }
}

impl DerivationSourceSpec {
    /// The wire key this entry names, whichever spelling was used.
    pub fn field(&self) -> &str {
        match self {
            Self::Bare(field) | Self::Typed { field, .. } => field,
        }
    }

    /// The dialect token as written, or `None` when the entry did not
    /// spell one (not the same as writing `dialect = "otel"`).
    pub fn dialect(&self) -> Option<&str> {
        match self {
            Self::Bare(_) => None,
            Self::Typed { dialect, .. } => dialect.as_deref(),
        }
    }
}

/// The packaged `severity_from` default (ADR-0013 §2): `severity` →
/// `severity_text` → `level`, first mappable wins.
///
/// `severity_text` is ordinary sender vocabulary rather than an envelope
/// field, but a shipper that emits it means severity by it, so it stays a
/// source.
pub const DEFAULT_SEVERITY_FROM: &[&str] = &["severity", "severity_text", "level"];

/// The packaged `time_from` default (ADR-0013 §2): `_time` →
/// `timestamp` → `@timestamp`, first present wins.
pub const DEFAULT_TIME_FROM: &[&str] = &["_time", "timestamp", "@timestamp"];

fn bare_sources(names: &[&str]) -> Vec<DerivationSourceSpec> {
    names
        .iter()
        .map(|f| DerivationSourceSpec::Bare((*f).to_owned()))
        .collect()
}

fn default_severity_from() -> Vec<DerivationSourceSpec> {
    bare_sources(DEFAULT_SEVERITY_FROM)
}

fn default_time_from() -> Vec<DerivationSourceSpec> {
    bare_sources(DEFAULT_TIME_FROM)
}

impl IngestConfig {
    /// The effective env allowlist: `envs` when non-empty, else
    /// `[default_env]` — zero-config ingestion works out of the box and
    /// envs are declared at the moment they start being used.
    pub fn effective_envs(&self) -> Vec<String> {
        if self.envs.is_empty() {
            vec![self.default_env.clone()]
        } else {
            self.envs.clone()
        }
    }
}

// The data root's reserved entries (ADR-0009): the names under it that are
// never an env directory. Every consumer reads them from here.

/// The default WAL directory under the data root.
pub const WAL_DIR_NAME: &str = "wal";
/// The directory under the data root that holds scheduled report results.
pub const SCHEDULED_DIR_NAME: &str = "scheduled";
/// The storage epoch marker in the data root.
pub const EPOCH_MARKER: &str = "EPOCH";
/// The marker mirroring the catalog id into the data root.
pub const CATALOG_MARKER: &str = "CATALOG";
/// The marker of a repin job in progress in the data root.
pub const REPIN_MARKER: &str = "REPIN";
/// Prefix of the staged `EPOCH.next.<pid>` file an epoch publication writes.
pub const EPOCH_STAGING_PREFIX: &str = "EPOCH.next.";
/// The marker files in the data root that `server.log_file` must not alias.
pub const STORAGE_MARKERS: [&str; 3] = [EPOCH_MARKER, CATALOG_MARKER, REPIN_MARKER];
/// The reserved directories in the data root.
pub const RESERVED_ROOT_DIRS: [&str; 2] = [WAL_DIR_NAME, SCHEDULED_DIR_NAME];
/// Env names reserved for sibling directories under the data root.
pub const RESERVED_ENV_NAMES: &[&str] = &RESERVED_ROOT_DIRS;
/// Suffix of the repin shadow (next-generation) root. A sibling of the data
/// root, not an entry in it.
pub const REPIN_SHADOW_SUFFIX: &str = ".repin-next";
/// Suffix of the repin aside (previous-generation) root. A sibling of the
/// data root, not an entry in it.
pub const REPIN_ASIDE_SUFFIX: &str = ".repin-aside";

/// Whether `name`, an entry of the data root, is one trawl reserves: a
/// reserved directory, a storage marker or a staged epoch file.
pub fn is_reserved_root_entry(name: &str) -> bool {
    RESERVED_ROOT_DIRS.contains(&name)
        || STORAGE_MARKERS.contains(&name)
        || name.starts_with(EPOCH_STAGING_PREFIX)
}

/// Whether `name` is a valid environment name: `[a-z0-9_-]{1,32}`.
///
/// Path encoding is injective by validation (ADR-0009): env is a path
/// segment and is never rewritten on the way to disk, so the charset is
/// the whole safety argument — no dots (dot-leading names), no slashes,
/// no uppercase (case-colliding filesystems).
pub fn is_valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The characters `DuckDB` reads as a glob pattern in a path it is handed.
const GLOB_METACHARACTERS: &[char] = &['*', '?', '[', ']', '{', '}'];

/// Maximum service name length (ADR-0009 path segment cap).
pub const MAX_SERVICE_NAME_LEN: usize = 128;

/// Whether `b` is a byte allowed in a service name: alphanumeric, dash,
/// underscore, dot — no spaces, no slashes.
pub fn is_valid_service_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// Whether `name` is a valid service name: non-empty, at most
/// [`MAX_SERVICE_NAME_LEN`] bytes, [`is_valid_service_char`] throughout,
/// and not dot-leading.
///
/// Path encoding is injective by validation (ADR-0009): service is a path
/// segment carried verbatim into WAL filenames and parquet names, so this
/// predicate is the whole safety argument — no slashes (path escape), no
/// spaces (unquotable globs), no dot-leading names (`.`, `..`, dotfiles).
/// Every ingestion path must funnel service names through it.
pub fn is_valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVICE_NAME_LEN
        && name.bytes().all(is_valid_service_char)
        && !name.starts_with('.')
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            enabled: default_ingest_enabled(),
            max_body_bytes: default_ingest_max_body_bytes(),
            wal_dir: None,
            compaction_interval_secs: default_compaction_interval_secs(),
            internal_telemetry: default_internal_telemetry(),
            daily_rollup: default_daily_rollup(),
            event_bus_capacity: default_event_bus_capacity(),
            hot_buffer_max_events: default_hot_buffer_max_events(),
            hot_buffer_max_bytes: default_hot_buffer_max_bytes(),
            stats_interval_secs: DEFAULT_STATS_INTERVAL_SECS,
            telemetry_flush_interval_secs: DEFAULT_TELEMETRY_FLUSH_INTERVAL_SECS,
            telemetry_buffer_max_bytes: default_telemetry_buffer_max_bytes(),
            compaction_chunk_size: DEFAULT_COMPACTION_CHUNK_SIZE,
            compaction_memory_limit: DEFAULT_COMPACTION_MEMORY_LIMIT.to_string(),
            default_env: default_env_name(),
            envs: Vec::new(),
            trusted_relays: Vec::new(),
            severity_from: default_severity_from(),
            time_from: default_time_from(),
        }
    }
}

fn default_env_name() -> String {
    "prod".to_string()
}

/// Data retention policy settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    /// Delete date directories older than this many days, for every env
    /// without a `[retention.env.<name>]` entry of its own. 0 = those envs
    /// keep their data forever.
    #[serde(default = "default_retention_max_age_days")]
    pub max_age_days: u64,

    /// If free disk space drops below this many bytes, delete date
    /// directories regardless of age until it is back above, highest
    /// expiry ratio (age over the env's limit) first. 0 = disabled.
    /// Accepts human-readable sizes like `"1G"`, `"500M"`.
    #[serde(
        default = "default_retention_min_free_disk_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub min_free_disk_bytes: u64,

    /// How often the retention task runs (seconds).
    #[serde(default = "default_retention_interval_secs")]
    pub retention_interval_secs: u64,

    /// Per-env age overrides, keyed by env name (`[retention.env.lab]`).
    /// An env with no entry keeps `max_age_days`.
    #[serde(default)]
    pub env: BTreeMap<String, EnvRetention>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvRetention {
    /// Delete this env's date directories older than this many days.
    /// 0 = keep forever (still eligible for disk-pressure deletion, ranked
    /// after everything that expires).
    pub max_age_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_age_days: DEFAULT_RETENTION_MAX_AGE_DAYS,
            min_free_disk_bytes: DEFAULT_RETENTION_MIN_FREE_DISK_BYTES,
            retention_interval_secs: DEFAULT_RETENTION_INTERVAL_SECS,
            env: BTreeMap::new(),
        }
    }
}

impl RetentionConfig {
    /// The age limit that applies to `env`, in days: its own entry when it
    /// has one, else the global `max_age_days`. 0 means keep forever.
    #[must_use]
    pub fn max_age_days_for(&self, env: &str) -> u64 {
        self.env
            .get(env)
            .map_or(self.max_age_days, |e| e.max_age_days)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerConfig {
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,

    /// How often the scheduler checks for due schedules (seconds).
    #[serde(default = "default_scheduler_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// How many finished report runs to keep per schedule (retention).
    /// Must be at least 1 (ADR-0018).
    #[serde(default = "default_scheduler_max_runs_per_schedule")]
    pub max_runs_per_schedule: u64,

    /// Delete finished report runs that started more than this many days
    /// ago. A `running` run is never deleted. Must be between 1 and
    /// [`MAX_SCHEDULER_REPORT_RETENTION_DAYS`].
    #[serde(default = "default_scheduler_report_retention_days")]
    pub report_retention_days: u64,

    /// How many intervals of missed coverage a `since_last` report window
    /// may swallow in one catch-up run (ADR-0018 ruling 9).
    #[serde(default = "default_scheduler_max_catchup_intervals")]
    pub max_catchup_intervals: u32,
}

const DEFAULT_SCHEDULER_ENABLED: bool = true;
const DEFAULT_SCHEDULER_POLL_INTERVAL_SECS: u64 = 10;
const DEFAULT_SCHEDULER_MAX_RUNS_PER_SCHEDULE: u64 = 100;
const DEFAULT_SCHEDULER_REPORT_RETENTION_DAYS: u64 = 30;
/// A day of missed coverage at the common hourly cadence.
pub const DEFAULT_SCHEDULER_MAX_CATCHUP_INTERVALS: u32 = 24;

/// The largest catch-up ceiling trawld will start with.
///
/// The planner multiplies this by the schedule's interval and turns the
/// product into one `chrono::TimeDelta`. Both factors are operator-chosen,
/// so the product needs a bound of its own: past what a `TimeDelta` can
/// hold, `plan_due_run` answers `PlanError::Arithmetic` and every due
/// `since_last` run fails on every poll, forever, with the fire cursor
/// never advancing.
pub const MAX_SCHEDULER_CATCHUP_INTERVALS: u32 = 1_000_000;

/// The longest `scheduler.report_retention_days` trawld will start with,
/// about 100 years.
///
/// Retention turns the value into a cutoff date, `now` minus that many
/// days, and the arithmetic has a range: past `i64` days, or far enough to
/// land before chrono's minimum date, the cutoff cannot be computed and
/// retention refuses to run.
pub const MAX_SCHEDULER_REPORT_RETENTION_DAYS: u64 = 36_500;

/// The ten-year duration cap, mirrored from
/// `trawl_server::store::MAX_DURATION_SECS`.
///
/// trawl-server depends on this crate, so the constant cannot travel the
/// other way and the value is copied instead of imported.
/// `config_duration_mirror_matches_the_grammar_cap` over in trawl-server
/// can see both and fails if the copy drifts.
pub const MIRRORED_MAX_DURATION_SECS: u64 = 315_360_000;

fn default_scheduler_enabled() -> bool {
    DEFAULT_SCHEDULER_ENABLED
}
fn default_scheduler_poll_interval_secs() -> u64 {
    DEFAULT_SCHEDULER_POLL_INTERVAL_SECS
}
fn default_scheduler_max_runs_per_schedule() -> u64 {
    DEFAULT_SCHEDULER_MAX_RUNS_PER_SCHEDULE
}
fn default_scheduler_report_retention_days() -> u64 {
    DEFAULT_SCHEDULER_REPORT_RETENTION_DAYS
}
fn default_scheduler_max_catchup_intervals() -> u32 {
    DEFAULT_SCHEDULER_MAX_CATCHUP_INTERVALS
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SCHEDULER_ENABLED,
            poll_interval_secs: DEFAULT_SCHEDULER_POLL_INTERVAL_SECS,
            max_runs_per_schedule: DEFAULT_SCHEDULER_MAX_RUNS_PER_SCHEDULE,
            report_retention_days: DEFAULT_SCHEDULER_REPORT_RETENTION_DAYS,
            max_catchup_intervals: DEFAULT_SCHEDULER_MAX_CATCHUP_INTERVALS,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyslogConfig {
    #[serde(default)]
    pub enabled: bool,

    /// UDP listen address. Default: "0.0.0.0:1514".
    #[serde(default = "default_syslog_addr")]
    pub udp_addr: String,

    /// Enable the UDP listener. Default: true (when syslog is enabled).
    #[serde(default = "default_true")]
    pub udp_enabled: bool,

    /// TCP listen address. Default: "0.0.0.0:1514".
    #[serde(default = "default_syslog_addr")]
    pub tcp_addr: String,

    /// Enable the TCP listener. Default: true (when syslog is enabled).
    #[serde(default = "default_true")]
    pub tcp_enabled: bool,

    /// Maximum concurrent TCP connections. Default: 256.
    #[serde(default = "default_syslog_max_tcp_connections")]
    pub max_tcp_connections: usize,

    /// Batch flush interval in milliseconds. Default: 500.
    #[serde(default = "default_syslog_batch_interval_ms")]
    pub batch_interval_ms: u64,

    /// Maximum events per batch before forced flush. Default: 1000.
    #[serde(default = "default_syslog_batch_max_events")]
    pub batch_max_events: usize,

    /// Default service name when APP-NAME/tag is missing and no source
    /// IP mapping matches. Default: "syslog".
    #[serde(default = "default_syslog_default_service")]
    pub default_service: String,

    /// TCP idle timeout in seconds. Connections that send no data for
    /// this long are closed. Default: 60.
    #[serde(default = "default_syslog_tcp_idle_timeout_secs")]
    pub tcp_idle_timeout_secs: u64,

    /// Maximum events accepted from a single TCP connection before it
    /// is closed. Prevents a single sender from monopolizing the
    /// batcher channel. Default: 100,000.
    #[serde(default = "default_syslog_max_events_per_connection")]
    pub max_events_per_connection: usize,

    /// Source IP allowlist in CIDR notation (e.g. `["192.168.0.0/16"]`).
    /// Bare IPs without a prefix are treated as /32 (IPv4) or /128 (IPv6).
    /// Empty list means all source IPs are accepted.
    ///
    /// NOTE: UDP source IPs can be spoofed on the local network. This
    /// allowlist does not provide authentication for UDP traffic.
    #[serde(default)]
    pub allow_cidrs: Vec<String>,

    /// Map source IPs to service names. Takes priority over APP-NAME/tag
    /// from the syslog message. Useful for appliances that don't set a
    /// meaningful APP-NAME (e.g. `UniFi` consoles). Keys must be IP
    /// addresses; trawl-server folds and checks them (ADR-0050).
    #[serde(default)]
    pub source_service_map: std::collections::HashMap<String, String>,

    /// The zone a zone-less RFC 3164 timestamp is read in: `UTC`, an IANA
    /// name, or a `±HH:MM` offset. Unset means UTC, never the host's zone
    /// (ADR-0050). Held raw here; trawl-server parses it.
    #[serde(default)]
    pub default_timezone: Option<String>,

    /// Per-peer overrides of `default_timezone`, keyed by the exact peer
    /// IP address, folded and checked like `source_service_map`.
    #[serde(default)]
    pub sender_timezones: std::collections::HashMap<String, String>,

    /// Channel capacity for the event queue between listeners and batcher.
    #[serde(default = "default_syslog_channel_capacity")]
    pub channel_capacity: usize,
}

const DEFAULT_SYSLOG_ADDR: &str = "0.0.0.0:1514";
const DEFAULT_SYSLOG_MAX_TCP_CONNECTIONS: usize = 256;
const DEFAULT_SYSLOG_BATCH_INTERVAL_MS: u64 = 500;
const DEFAULT_SYSLOG_BATCH_MAX_EVENTS: usize = 1000;
const DEFAULT_SYSLOG_TCP_IDLE_TIMEOUT_SECS: u64 = 60;
const DEFAULT_SYSLOG_MAX_EVENTS_PER_CONNECTION: usize = 100_000;
const DEFAULT_SYSLOG_CHANNEL_CAPACITY: usize = 10_000;

fn default_syslog_addr() -> String {
    DEFAULT_SYSLOG_ADDR.to_owned()
}

fn default_true() -> bool {
    true
}

fn default_syslog_max_tcp_connections() -> usize {
    DEFAULT_SYSLOG_MAX_TCP_CONNECTIONS
}

fn default_syslog_batch_interval_ms() -> u64 {
    DEFAULT_SYSLOG_BATCH_INTERVAL_MS
}

fn default_syslog_batch_max_events() -> usize {
    DEFAULT_SYSLOG_BATCH_MAX_EVENTS
}

fn default_syslog_default_service() -> String {
    "syslog".to_owned()
}

fn default_syslog_tcp_idle_timeout_secs() -> u64 {
    DEFAULT_SYSLOG_TCP_IDLE_TIMEOUT_SECS
}

fn default_syslog_max_events_per_connection() -> usize {
    DEFAULT_SYSLOG_MAX_EVENTS_PER_CONNECTION
}

fn default_syslog_channel_capacity() -> usize {
    DEFAULT_SYSLOG_CHANNEL_CAPACITY
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            udp_addr: DEFAULT_SYSLOG_ADDR.to_owned(),
            udp_enabled: true,
            tcp_addr: DEFAULT_SYSLOG_ADDR.to_owned(),
            tcp_enabled: true,
            max_tcp_connections: DEFAULT_SYSLOG_MAX_TCP_CONNECTIONS,
            batch_interval_ms: DEFAULT_SYSLOG_BATCH_INTERVAL_MS,
            batch_max_events: DEFAULT_SYSLOG_BATCH_MAX_EVENTS,
            tcp_idle_timeout_secs: DEFAULT_SYSLOG_TCP_IDLE_TIMEOUT_SECS,
            max_events_per_connection: DEFAULT_SYSLOG_MAX_EVENTS_PER_CONNECTION,
            default_service: "syslog".to_owned(),
            allow_cidrs: Vec::new(),
            source_service_map: std::collections::HashMap::new(),
            default_timezone: None,
            sender_timezones: std::collections::HashMap::new(),
            channel_capacity: DEFAULT_SYSLOG_CHANNEL_CAPACITY,
        }
    }
}

// All multipliers are binary (1024-based) because nobody means 1,000,000
// when they write "1M" in a server config file.

fn parse_byte_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty byte size".into());
    }

    let num_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, suffix) = s.split_at(num_end);
    let suffix = suffix.trim();

    let multiplier: u64 = match suffix.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        "T" | "TB" | "TIB" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("unknown byte size suffix: {suffix:?}")),
    };

    if num_str.contains('.') {
        let num: f64 = num_str
            .parse()
            .map_err(|_| format!("invalid number in byte size: {s:?}"))?;
        #[allow(clippy::cast_precision_loss)]
        let bytes = num * multiplier as f64;
        // `u64::MAX as f64` rounds up to 2^64, so `<` keeps every accepted
        // value inside `u64`; the cast would otherwise saturate silently.
        #[allow(clippy::cast_precision_loss)]
        if !bytes.is_finite() || bytes >= u64::MAX as f64 {
            return Err(BYTE_SIZE_OVERFLOW.into());
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(bytes as u64)
    } else {
        let num: u64 = num_str.parse().map_err(|e: std::num::ParseIntError| {
            if *e.kind() == std::num::IntErrorKind::PosOverflow {
                BYTE_SIZE_OVERFLOW.to_owned()
            } else {
                format!("invalid number in byte size: {s:?}")
            }
        })?;
        num.checked_mul(multiplier)
            .ok_or_else(|| BYTE_SIZE_OVERFLOW.into())
    }
}

const BYTE_SIZE_OVERFLOW: &str = "byte size is too large";

const NONZERO_REQUIRED: &str = "must be greater than zero";

fn deserialize_nonzero_byte_size<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<NonZeroU64, D::Error> {
    let bytes = deserialize_byte_size_u64(de)?;
    NonZeroU64::new(bytes).ok_or_else(|| serde::de::Error::custom(NONZERO_REQUIRED))
}

fn deserialize_nonzero_u32<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<NonZeroU32, D::Error> {
    let count = u32::deserialize(de)?;
    NonZeroU32::new(count).ok_or_else(|| serde::de::Error::custom(NONZERO_REQUIRED))
}

fn deserialize_byte_size<'de, D: serde::Deserializer<'de>>(de: D) -> Result<usize, D::Error> {
    use serde::de;

    struct ByteSizeVisitor;

    impl de::Visitor<'_> for ByteSizeVisitor {
        type Value = usize;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a byte count (integer) or a string like \"128K\", \"16M\", \"1G\"")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<usize, E> {
            usize::try_from(v).map_err(E::custom)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<usize, E> {
            usize::try_from(v).map_err(E::custom)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<usize, E> {
            let bytes = parse_byte_size(v).map_err(E::custom)?;
            usize::try_from(bytes).map_err(E::custom)
        }
    }

    de.deserialize_any(ByteSizeVisitor)
}

fn deserialize_byte_size_u64<'de, D: serde::Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    use serde::de;

    struct ByteSizeVisitor;

    impl de::Visitor<'_> for ByteSizeVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a byte count (integer) or a string like \"128K\", \"16M\", \"1G\"")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            u64::try_from(v).map_err(E::custom)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            parse_byte_size(v).map_err(E::custom)
        }
    }

    de.deserialize_any(ByteSizeVisitor)
}

pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:5514";
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_MAX_RESULT_ROWS: usize = 100_000;
pub const DEFAULT_MAX_EXPORT_ROWS: usize = 1_000_000;
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 128 * 1024;
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 32;
/// Largest accepted `server.max_concurrent_requests`. Tokio's semaphore
/// caps its permits at `Semaphore::MAX_PERMITS`; a test holds this bound
/// below that.
pub const MAX_CONCURRENT_REQUESTS_LIMIT: usize = 65_536;
pub const DEFAULT_SHUTDOWN_DRAIN_SECS: u64 = 30;
/// Default TLS certificate reload interval (seconds). 0 = disabled.
pub const DEFAULT_TLS_RELOAD_INTERVAL_SECS: u64 = 300;
pub const GENERATED_TLS_DIR: &str = "tls";
/// File name of trawld's generated certificate inside [`GENERATED_TLS_DIR`].
/// trawld publishes it world-readable (`0644`) so a proxy running as another
/// user can pin it.
pub const GENERATED_CERT_FILE: &str = "cert.pem";
/// Directory under [`Config::state_dir`] where trawld keeps the private key
/// of its generated certificate. Owner-only (`0700`). It is a sibling of
/// [`GENERATED_TLS_DIR`], not inside it, so a proxy that mounts the
/// certificate directory to pin `cert.pem` never has the key in its mount.
/// Permission bits alone cannot keep it out: Kubernetes' fsGroup ownership
/// walk makes every file on the volume group-readable.
pub const GENERATED_KEY_DIR: &str = "tls-key";
/// File name of trawld's generated private key inside [`GENERATED_KEY_DIR`].
/// Owner-only (`0600`).
pub const GENERATED_KEY_FILE: &str = "key.pem";
pub const DEFAULT_SCHEMA_CACHE_TTL_SECS: u64 = 60;
pub const DEFAULT_MAX_QUERY_HISTORY: usize = 1000;
pub const DEFAULT_INGEST_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_COMPACTION_INTERVAL_SECS: u64 = 10;
pub const DEFAULT_INTERNAL_TELEMETRY: bool = true;
pub const DEFAULT_DAILY_ROLLUP: bool = true;
/// Default compaction chunk size (WAL files per `read_json` call).
pub const DEFAULT_COMPACTION_CHUNK_SIZE: usize = 500;
/// Largest compaction chunk size. A chunk's publication marker names every
/// WAL file the chunk consumed, and the marker reader refuses a marker over
/// a fixed size derived from this count, so a larger chunk could publish a
/// marker that recovery then refuses to read.
pub const MAX_COMPACTION_CHUNK_SIZE: usize = 4096;
pub const DEFAULT_QUERY_SPILL_MAX_BYTES: NonZeroU64 = NonZeroU64::new(4 << 30).unwrap();
pub const DEFAULT_QUERY_MEMORY_LIMIT: NonZeroU64 = NonZeroU64::new(2 << 30).unwrap();
pub const DEFAULT_QUERY_THREADS: NonZeroU32 = NonZeroU32::new(4).unwrap();
pub const MAX_DEFAULT_CONCURRENT_QUERIES: usize = 8;
pub const QUERY_SCRATCH_SUFFIX: &str = ".query-scratch";
pub const RESULTS_SUFFIX: &str = ".results";
/// Default `DuckDB` memory limit for compaction connections (1 GiB). A raw
/// `DuckDB` string, so the unit is spelled `GiB`: `DuckDB` reads `GB` as
/// 10^9 bytes.
pub const DEFAULT_COMPACTION_MEMORY_LIMIT: &str = "1GiB";
pub const DEFAULT_RETENTION_MAX_AGE_DAYS: u64 = 90;
pub const DEFAULT_RETENTION_MIN_FREE_DISK_BYTES: u64 = 6 << 30;
pub const DEFAULT_RETENTION_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_RATE_LIMIT_RPM: u32 = 100;
pub const DEFAULT_INGEST_RATE_LIMIT_RPM: u32 = 1000;
pub const DEFAULT_MAX_SSE_CONNECTIONS: usize = 32;
const FALLBACK_CPU_COUNT: usize = 4;

fn default_ingest_enabled() -> bool {
    true
}

fn default_ingest_max_body_bytes() -> usize {
    DEFAULT_INGEST_MAX_BODY_BYTES
}

fn default_compaction_interval_secs() -> u64 {
    DEFAULT_COMPACTION_INTERVAL_SECS
}

fn default_internal_telemetry() -> bool {
    DEFAULT_INTERNAL_TELEMETRY
}

fn default_daily_rollup() -> bool {
    DEFAULT_DAILY_ROLLUP
}

fn default_compaction_chunk_size() -> usize {
    DEFAULT_COMPACTION_CHUNK_SIZE
}

fn default_compaction_memory_limit() -> String {
    DEFAULT_COMPACTION_MEMORY_LIMIT.to_string()
}

fn default_event_bus_capacity() -> usize {
    // Mirror of `trawl_server::bus::DEFAULT_EVENT_BUS_CAPACITY = 4096`.
    // trawl-server depends on this crate, so the constant cannot be read back
    // from there without a dependency cycle. Nothing checks the two copies for
    // drift, so a bump has to touch both.
    4096
}

pub const DEFAULT_HOT_BUFFER_MAX_EVENTS: usize = 100_000;
pub const DEFAULT_HOT_BUFFER_MAX_BYTES: usize = 100 * 1024 * 1024;

fn default_hot_buffer_max_events() -> usize {
    DEFAULT_HOT_BUFFER_MAX_EVENTS
}

fn default_hot_buffer_max_bytes() -> usize {
    DEFAULT_HOT_BUFFER_MAX_BYTES
}

pub const DEFAULT_STATS_INTERVAL_SECS: u64 = 60;
pub const DEFAULT_TELEMETRY_FLUSH_INTERVAL_SECS: u64 = 1;

fn default_stats_interval_secs() -> u64 {
    DEFAULT_STATS_INTERVAL_SECS
}

fn default_telemetry_flush_interval_secs() -> u64 {
    DEFAULT_TELEMETRY_FLUSH_INTERVAL_SECS
}

/// Default telemetry retry-queue memory cap (16 MiB, estimated charge).
pub const DEFAULT_TELEMETRY_BUFFER_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Smallest accepted telemetry buffer budget (64 KiB, estimated charge).
///
/// The budget must hold at least one `telemetry_dropped` recovery record,
/// which charges about 2 KB. Below that, the record that reports a
/// `buffer_cap` drop is itself refused, which counts a new drop that the
/// next idle flush reports again, so the capacity alert never clears.
pub const MIN_TELEMETRY_BUFFER_MAX_BYTES: usize = 64 * 1024;

fn default_telemetry_buffer_max_bytes() -> usize {
    DEFAULT_TELEMETRY_BUFFER_MAX_BYTES
}

pub const DEFAULT_AUDIT_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Fleet-auth Postgres keystore URL (e.g.
    /// `postgres://user:pass@host:5432/fleet`). The `FLEET_DATABASE_URL`
    /// environment variable takes precedence. trawld refuses to start when
    /// neither is set.
    #[serde(default)]
    pub database_url: Option<String>,

    /// How often to poll the fleet keystore for key changes (seconds).
    /// Detects keys created/revoked out-of-process by fleet-admin and emits
    /// audit events. Set to 0 to disable. Default: 30.
    #[serde(default = "default_audit_interval_secs")]
    pub audit_interval_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            database_url: None,
            audit_interval_secs: default_audit_interval_secs(),
        }
    }
}

impl AuthConfig {
    /// The bare `DATABASE_URL` is deliberately not consulted: that variable
    /// belongs to the sqlx test harness (`#[sqlx::test]` hardwires it), and a
    /// process-wide `DATABASE_URL` must never silently repoint trawld's
    /// keystore.
    pub fn resolve_database_url(&self) -> Result<String, ConfigError> {
        self.resolve_database_url_with_source().map(|(url, _)| url)
    }

    /// [`Self::resolve_database_url`], also naming which source won, so
    /// `trawld --doctor` can say where the URL came from without showing
    /// it.
    pub fn resolve_database_url_with_source(
        &self,
    ) -> Result<(String, DatabaseUrlSource), ConfigError> {
        Self::resolve_database_url_with_source_from(
            std::env::var("FLEET_DATABASE_URL").ok().as_deref(),
            self.database_url.as_deref(),
        )
    }

    /// Pure resolution core, split out for testability (mutating process
    /// env in tests is forbidden under `unsafe_code = "forbid"`).
    #[cfg(test)]
    fn resolve_database_url_from(
        env_value: Option<&str>,
        configured: Option<&str>,
    ) -> Result<String, ConfigError> {
        Self::resolve_database_url_with_source_from(env_value, configured).map(|(url, _)| url)
    }

    fn resolve_database_url_with_source_from(
        env_value: Option<&str>,
        configured: Option<&str>,
    ) -> Result<(String, DatabaseUrlSource), ConfigError> {
        pick_database_url(env_value, configured).ok_or_else(|| {
            ConfigError::Validation(
                "fleet keystore URL required: set [auth] database_url in trawld.toml \
                 or the FLEET_DATABASE_URL environment variable"
                    .into(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseUrlSource {
    Environment,
    ConfigFile,
}

fn pick_database_url(
    env_value: Option<&str>,
    configured: Option<&str>,
) -> Option<(String, DatabaseUrlSource)> {
    let pick = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
    pick(env_value)
        .map(|url| (url, DatabaseUrlSource::Environment))
        .or_else(|| pick(configured).map(|url| (url, DatabaseUrlSource::ConfigFile)))
}

/// Storage settings for trawl's own app-state database (query history,
/// saved queries, schedules, report runs — ADR-0004).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Postgres URL of the dedicated `trawl` app-state database (e.g.
    /// `postgres://trawl:pass@host:5432/trawl`). The `TRAWL_DATABASE_URL`
    /// environment variable takes precedence. trawld refuses to start when
    /// neither is set.
    #[serde(default)]
    pub database_url: Option<String>,
}

impl StorageConfig {
    pub fn resolve_database_url(&self) -> Result<String, ConfigError> {
        self.resolve_database_url_with_source().map(|(url, _)| url)
    }

    /// [`Self::resolve_database_url`], also naming which source won, so
    /// `trawld --doctor` can say where the URL came from without showing
    /// it.
    pub fn resolve_database_url_with_source(
        &self,
    ) -> Result<(String, DatabaseUrlSource), ConfigError> {
        Self::resolve_database_url_with_source_from(
            std::env::var("TRAWL_DATABASE_URL").ok().as_deref(),
            self.database_url.as_deref(),
        )
    }

    /// Pure resolution core, split out for testability (mutating process
    /// env in tests is forbidden under `unsafe_code = "forbid"`).
    #[cfg(test)]
    fn resolve_database_url_from(
        env_value: Option<&str>,
        configured: Option<&str>,
    ) -> Result<String, ConfigError> {
        Self::resolve_database_url_with_source_from(env_value, configured).map(|(url, _)| url)
    }

    fn resolve_database_url_with_source_from(
        env_value: Option<&str>,
        configured: Option<&str>,
    ) -> Result<(String, DatabaseUrlSource), ConfigError> {
        pick_database_url(env_value, configured).ok_or_else(|| {
            ConfigError::Validation(
                "trawl app-state database URL required: set [storage] database_url in \
                 trawld.toml or the TRAWL_DATABASE_URL environment variable"
                    .into(),
            )
        })
    }
}

/// Browser-facing session proxy (`trawl-web`) settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Bind address for the proxy HTTP listener. Default: "127.0.0.1:8090".
    pub bind_addr: Option<String>,

    /// How the proxy reaches trawld. If unset, derived from `[server]`.
    pub upstream_url: Option<String>,

    /// Path to a PEM file holding the CA certificate(s) that trawld's
    /// certificate must chain to. When set, only these roots are trusted
    /// for the upstream connection, and the hostname is still verified.
    /// Unset: the platform trust store.
    pub upstream_ca_path: Option<PathBuf>,

    /// Socket address the proxy connects to instead of resolving the
    /// `upstream_url` host, e.g. `"127.0.0.1:5514"` (ADR-0048).
    ///
    /// Kept as a raw string: `trawl-web` owns the parse and those rules.
    pub upstream_connect_addr: Option<String>,

    /// Path to a file containing the 32-byte AEAD key for cookie encryption.
    /// Either this or `cookie_secret_env` must be set in production.
    pub cookie_secret_path: Option<PathBuf>,

    /// Environment variable name holding a base64-encoded 32-byte AEAD key.
    /// Alternative to `cookie_secret_path`.
    pub cookie_secret_env: Option<String>,

    /// Session TTL in seconds. Default: 86400 (24h).
    pub session_ttl_secs: Option<u64>,

    /// Drop `Secure` on session cookies. Dev-only; must stay `false` in prod.
    #[serde(default)]
    pub allow_insecure_cookies: bool,

    /// Browser-visible origins the proxy accepts cookie-authenticated
    /// requests from, e.g. `["https://trawl.example.com"]` (ADR-0016).
    /// The two spellings of loopback are different origins to a browser,
    /// so `http://127.0.0.1:8090` and `http://localhost:8090` must both
    /// be listed if both are used.
    ///
    /// Kept as raw strings: this crate is trawld's dependency-light config
    /// surface and never links fleet-auth, so validation happens in
    /// `trawl-web` where the one origin parser lives.
    #[serde(default)]
    pub public_origins: Vec<String>,

    /// Parent domain for the shared `fleet_session` SSO cookie (ADR-0004).
    ///
    /// When set (e.g. `".fleet.lab.ktle.net"`), the session cookie carries a
    /// `Domain=` attribute scoping it to the parent domain, so one login is
    /// shared across every fleet app under it (requires the same session key
    /// in all apps). Unset or empty → no `Domain=` attribute; the cookie is
    /// origin-scoped (standalone mode).
    pub shared_domain: Option<String>,
}

fn default_audit_interval_secs() -> u64 {
    DEFAULT_AUDIT_INTERVAL_SECS
}

fn default_http_addr() -> String {
    DEFAULT_HTTP_ADDR.to_owned()
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

fn default_max_concurrent_queries() -> usize {
    default_query_permits(num_cpus())
}

fn default_query_permits(cpus: Option<usize>) -> usize {
    cpus.unwrap_or(FALLBACK_CPU_COUNT)
        .min(MAX_DEFAULT_CONCURRENT_QUERIES)
}

fn default_max_result_rows() -> usize {
    DEFAULT_MAX_RESULT_ROWS
}

fn default_max_export_rows() -> usize {
    DEFAULT_MAX_EXPORT_ROWS
}

fn default_max_request_body_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BODY_BYTES
}

fn default_max_concurrent_requests() -> usize {
    DEFAULT_MAX_CONCURRENT_REQUESTS
}

fn default_shutdown_drain_secs() -> u64 {
    DEFAULT_SHUTDOWN_DRAIN_SECS
}

fn default_tls_reload_interval_secs() -> u64 {
    DEFAULT_TLS_RELOAD_INTERVAL_SECS
}

fn default_schema_cache_ttl_secs() -> u64 {
    DEFAULT_SCHEMA_CACHE_TTL_SECS
}

fn default_max_query_history() -> usize {
    DEFAULT_MAX_QUERY_HISTORY
}

/// Default query debug log size cap (100 MiB; `0` disables rollover).
pub const DEFAULT_QUERY_LOG_MAX_BYTES: usize = 100 * 1024 * 1024;

fn default_query_log_max_bytes() -> usize {
    DEFAULT_QUERY_LOG_MAX_BYTES
}

fn default_max_sse_connections() -> usize {
    DEFAULT_MAX_SSE_CONNECTIONS
}

const DEFAULT_MONITOR_REFRESH_MS: u64 = 1000;

fn default_monitor_refresh_ms() -> u64 {
    DEFAULT_MONITOR_REFRESH_MS
}

fn default_query_spill_max_bytes() -> NonZeroU64 {
    DEFAULT_QUERY_SPILL_MAX_BYTES
}

fn default_query_memory_limit() -> NonZeroU64 {
    DEFAULT_QUERY_MEMORY_LIMIT
}

fn default_query_threads() -> NonZeroU32 {
    DEFAULT_QUERY_THREADS
}

#[must_use]
pub fn data_root_sibling(data_dir: &Path, suffix: &str) -> PathBuf {
    let name = data_dir
        .file_name()
        .map_or_else(|| "data".to_owned(), |n| n.to_string_lossy().into_owned());
    data_dir.with_file_name(format!("{name}{suffix}"))
}

fn default_rate_limit_rpm() -> u32 {
    DEFAULT_RATE_LIMIT_RPM
}

fn default_ingest_rate_limit_rpm() -> u32 {
    DEFAULT_INGEST_RATE_LIMIT_RPM
}

fn default_retention_max_age_days() -> u64 {
    DEFAULT_RETENTION_MAX_AGE_DAYS
}

fn default_retention_min_free_disk_bytes() -> u64 {
    DEFAULT_RETENTION_MIN_FREE_DISK_BYTES
}

fn default_retention_interval_secs() -> u64 {
    DEFAULT_RETENTION_INTERVAL_SECS
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            default_rpm: DEFAULT_RATE_LIMIT_RPM,
            ingest_rpm: DEFAULT_INGEST_RATE_LIMIT_RPM,
        }
    }
}

/// The least `[capture] max_wire_bytes` the config accepts: a cell read's
/// byte range must hold one whole character, and the longest UTF-8
/// character is 4 bytes. The server's cell read minimum is this value, so a
/// smaller wire limit would make every cell read a 400.
pub const CAPTURE_MAX_WIRE_BYTES_MIN: u64 = 4;

const DEFAULT_CAPTURE_MAX_ROWS: u64 = 5_000;
const DEFAULT_CAPTURE_MAX_WIRE_BYTES: u64 = 4 << 20;
const DEFAULT_CAPTURE_MAX_FILE_BYTES: u64 = 8 << 20;
const DEFAULT_CAPTURE_TTL_SECS: u64 = 900;
const DEFAULT_CAPTURE_WINDOW_SECS: u64 = 900;
const DEFAULT_CAPTURE_MAX_SPAN_SECS: u64 = 604_800;
const DEFAULT_CAPTURE_DEADLINE_SECS: u64 = 15;
const DEFAULT_CAPTURE_SOURCE_MAX_BYTES: u64 = 1 << 30;
const DEFAULT_CAPTURE_PER_KEY_RESULTS: u64 = 64;
const DEFAULT_CAPTURE_PER_KEY_BYTES: u64 = 64 << 20;
const DEFAULT_CAPTURE_INSTALLATION_RESULTS: u64 = 512;
const DEFAULT_CAPTURE_INSTALLATION_BYTES: u64 = 512 << 20;
const DEFAULT_CAPTURE_PER_KEY_RUNNING: u64 = 1;
const DEFAULT_CAPTURE_INSTALLATION_RUNNING: u64 = 2;
const DEFAULT_CAPTURE_QUEUE_DEPTH: u64 = 4;
const DEFAULT_CAPTURE_PAGE_MAX_ROWS: u64 = 200;
const DEFAULT_CAPTURE_TOMBSTONE_SECS: u64 = 86_400;

/// Limits of captured results (ADR-0057).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Most rows one capture holds. A result with more matches is cut here and says so.
    #[serde(default = "default_capture_max_rows")]
    pub max_rows: u64,

    /// Most bytes of cell data one capture holds, counted as the rows would travel on the wire (default: 4 MiB). Accepts human-readable sizes like `"4M"`.
    #[serde(
        default = "default_capture_max_wire_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub max_wire_bytes: u64,

    /// Hard cap on one capture's file and the size reserved against the quotas before a capture runs (default: 8 MiB). Accepts human-readable sizes like `"8M"`.
    #[serde(
        default = "default_capture_max_file_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub max_file_bytes: u64,

    /// Seconds a capture without an investigation stays readable after it is created.
    #[serde(default = "default_capture_ttl_secs")]
    pub ttl_secs: u64,

    /// Seconds of history a capture covers, ending now, when the request names no time bounds.
    #[serde(default = "default_capture_default_window_secs")]
    pub default_window_secs: u64,

    /// Longest time span, in seconds, one capture may cover. A wider request is refused before any scan.
    #[serde(default = "default_capture_max_span_secs")]
    pub max_span_secs: u64,

    /// Seconds one capture may take, from resolution through the file write and including its wait for a free slot. The server's query timeout caps it.
    #[serde(default = "default_capture_deadline_secs")]
    pub deadline_secs: u64,

    /// Most bytes of stored data a capture may select to scan, computed before it runs (default: 1 GiB). A capture over it is refused. Accepts human-readable sizes like `"1G"`.
    #[serde(
        default = "default_capture_source_max_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub source_max_bytes: u64,

    /// Most live captures one key may hold.
    #[serde(default = "default_capture_per_key_results")]
    pub per_key_results: u64,

    /// Most capture file bytes one key may hold (default: 64 MiB). Accepts human-readable sizes like `"64M"`.
    #[serde(
        default = "default_capture_per_key_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub per_key_bytes: u64,

    /// Most live captures the whole installation may hold.
    #[serde(default = "default_capture_installation_results")]
    pub installation_results: u64,

    /// Most capture file bytes the whole installation may hold (default: 512 MiB). Accepts human-readable sizes like `"512M"`.
    #[serde(
        default = "default_capture_installation_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub installation_bytes: u64,

    /// Most captures one key may execute at once.
    #[serde(default = "default_capture_per_key_running")]
    pub per_key_running: u64,

    /// Most captures the whole installation may execute at once, inside the query executor pool.
    #[serde(default = "default_capture_installation_running")]
    pub installation_running: u64,

    /// Most captures one key may have waiting for its running slot. The next one is refused.
    #[serde(default = "default_capture_queue_depth")]
    pub queue_depth: u64,

    /// Most rows one page of a stored capture returns.
    #[serde(default = "default_capture_page_max_rows")]
    pub page_max_rows: u64,

    /// Seconds an expired or pruned capture's record stays, so its owner can tell expired from never existed.
    #[serde(default = "default_capture_tombstone_secs")]
    pub tombstone_secs: u64,
}

fn default_capture_max_rows() -> u64 {
    DEFAULT_CAPTURE_MAX_ROWS
}

fn default_capture_max_wire_bytes() -> u64 {
    DEFAULT_CAPTURE_MAX_WIRE_BYTES
}

fn default_capture_max_file_bytes() -> u64 {
    DEFAULT_CAPTURE_MAX_FILE_BYTES
}

fn default_capture_ttl_secs() -> u64 {
    DEFAULT_CAPTURE_TTL_SECS
}

fn default_capture_default_window_secs() -> u64 {
    DEFAULT_CAPTURE_WINDOW_SECS
}

fn default_capture_max_span_secs() -> u64 {
    DEFAULT_CAPTURE_MAX_SPAN_SECS
}

fn default_capture_deadline_secs() -> u64 {
    DEFAULT_CAPTURE_DEADLINE_SECS
}

fn default_capture_source_max_bytes() -> u64 {
    DEFAULT_CAPTURE_SOURCE_MAX_BYTES
}

fn default_capture_per_key_results() -> u64 {
    DEFAULT_CAPTURE_PER_KEY_RESULTS
}

fn default_capture_per_key_bytes() -> u64 {
    DEFAULT_CAPTURE_PER_KEY_BYTES
}

fn default_capture_installation_results() -> u64 {
    DEFAULT_CAPTURE_INSTALLATION_RESULTS
}

fn default_capture_installation_bytes() -> u64 {
    DEFAULT_CAPTURE_INSTALLATION_BYTES
}

fn default_capture_per_key_running() -> u64 {
    DEFAULT_CAPTURE_PER_KEY_RUNNING
}

fn default_capture_installation_running() -> u64 {
    DEFAULT_CAPTURE_INSTALLATION_RUNNING
}

fn default_capture_queue_depth() -> u64 {
    DEFAULT_CAPTURE_QUEUE_DEPTH
}

fn default_capture_page_max_rows() -> u64 {
    DEFAULT_CAPTURE_PAGE_MAX_ROWS
}

fn default_capture_tombstone_secs() -> u64 {
    DEFAULT_CAPTURE_TOMBSTONE_SECS
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_CAPTURE_MAX_ROWS,
            max_wire_bytes: DEFAULT_CAPTURE_MAX_WIRE_BYTES,
            max_file_bytes: DEFAULT_CAPTURE_MAX_FILE_BYTES,
            ttl_secs: DEFAULT_CAPTURE_TTL_SECS,
            default_window_secs: DEFAULT_CAPTURE_WINDOW_SECS,
            max_span_secs: DEFAULT_CAPTURE_MAX_SPAN_SECS,
            deadline_secs: DEFAULT_CAPTURE_DEADLINE_SECS,
            source_max_bytes: DEFAULT_CAPTURE_SOURCE_MAX_BYTES,
            per_key_results: DEFAULT_CAPTURE_PER_KEY_RESULTS,
            per_key_bytes: DEFAULT_CAPTURE_PER_KEY_BYTES,
            installation_results: DEFAULT_CAPTURE_INSTALLATION_RESULTS,
            installation_bytes: DEFAULT_CAPTURE_INSTALLATION_BYTES,
            per_key_running: DEFAULT_CAPTURE_PER_KEY_RUNNING,
            installation_running: DEFAULT_CAPTURE_INSTALLATION_RUNNING,
            queue_depth: DEFAULT_CAPTURE_QUEUE_DEPTH,
            page_max_rows: DEFAULT_CAPTURE_PAGE_MAX_ROWS,
            tombstone_secs: DEFAULT_CAPTURE_TOMBSTONE_SECS,
        }
    }
}

const DEFAULT_EVIDENCE_IDLE_SECS: u64 = 604_800;
const DEFAULT_EVIDENCE_CEILING_SECS: u64 = 2_592_000;
const DEFAULT_EVIDENCE_PER_INVESTIGATION_BYTES: u64 = 64 << 20;
const DEFAULT_EVIDENCE_INSTALLATION_BYTES: u64 = 1 << 30;

/// The evidence policy of investigations (ADR-0059).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InvestigationConfig {
    /// Seconds evidence files stay after the investigation's last change (default: 7 days).
    #[serde(default = "default_evidence_idle_secs")]
    pub evidence_idle_secs: u64,

    /// Seconds after an investigation's creation past which its evidence files never stay, unless `server_manage` extends them (default: 30 days).
    #[serde(default = "default_evidence_ceiling_secs")]
    pub evidence_ceiling_secs: u64,

    /// Most evidence file bytes one investigation may keep, reserved before an attached capture scans (default: 64 MiB). Accepts human-readable sizes like `"64M"`.
    #[serde(
        default = "default_evidence_per_investigation_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub evidence_per_investigation_bytes: u64,

    /// Most evidence file bytes the whole installation may keep (default: 1 GiB). Accepts human-readable sizes like `"1G"`.
    #[serde(
        default = "default_evidence_installation_bytes",
        deserialize_with = "deserialize_byte_size_u64"
    )]
    pub evidence_installation_bytes: u64,
}

fn default_evidence_idle_secs() -> u64 {
    DEFAULT_EVIDENCE_IDLE_SECS
}

fn default_evidence_ceiling_secs() -> u64 {
    DEFAULT_EVIDENCE_CEILING_SECS
}

fn default_evidence_per_investigation_bytes() -> u64 {
    DEFAULT_EVIDENCE_PER_INVESTIGATION_BYTES
}

fn default_evidence_installation_bytes() -> u64 {
    DEFAULT_EVIDENCE_INSTALLATION_BYTES
}

impl Default for InvestigationConfig {
    fn default() -> Self {
        Self {
            evidence_idle_secs: DEFAULT_EVIDENCE_IDLE_SECS,
            evidence_ceiling_secs: DEFAULT_EVIDENCE_CEILING_SECS,
            evidence_per_investigation_bytes: DEFAULT_EVIDENCE_PER_INVESTIGATION_BYTES,
            evidence_installation_bytes: DEFAULT_EVIDENCE_INSTALLATION_BYTES,
        }
    }
}

fn num_cpus() -> Option<usize> {
    std::thread::available_parallelism()
        .ok()
        .map(std::num::NonZero::get)
}

fn expand_tilde(path: &str) -> String {
    shellexpand::tilde(path).into_owned()
}

fn validate_telemetry_buffer_max_bytes(bytes: usize) -> Result<(), ConfigError> {
    if bytes < MIN_TELEMETRY_BUFFER_MAX_BYTES {
        return Err(ConfigError::Validation(format!(
            "ingest.telemetry_buffer_max_bytes must be at least {}K ({} bytes); set \
             ingest.internal_telemetry = false to disable internal telemetry",
            MIN_TELEMETRY_BUFFER_MAX_BYTES / 1024,
            MIN_TELEMETRY_BUFFER_MAX_BYTES
        )));
    }
    Ok(())
}

fn validate_max_catchup_intervals(intervals: u32) -> Result<(), ConfigError> {
    if intervals == 0 || intervals > MAX_SCHEDULER_CATCHUP_INTERVALS {
        return Err(ConfigError::Validation(format!(
            "scheduler.max_catchup_intervals must be between 1 and \
             {MAX_SCHEDULER_CATCHUP_INTERVALS} (it is a count of whole schedule intervals \
             a since_last window may cover in one catch-up run; the ceiling keeps that \
             span inside the range the scheduler can compute)"
        )));
    }
    Ok(())
}

fn validate_report_retention(scheduler: &SchedulerConfig) -> Result<(), ConfigError> {
    if !(1..=MAX_SCHEDULER_REPORT_RETENTION_DAYS).contains(&scheduler.report_retention_days) {
        return Err(ConfigError::Validation(format!(
            "scheduler.report_retention_days must be between 1 and \
             {MAX_SCHEDULER_REPORT_RETENTION_DAYS} (retention deletes finished report runs \
             older than this many days; the ceiling keeps the cutoff date computable)"
        )));
    }
    if scheduler.max_runs_per_schedule == 0 {
        return Err(ConfigError::Validation(
            "scheduler.max_runs_per_schedule must be at least 1 (it is how many finished \
             report runs retention keeps per schedule)"
                .into(),
        ));
    }
    Ok(())
}

const NUMERIC_HOST_RULE: &str = "must name an IPv4 host in dotted-decimal form (four labels, \
                                 0-255, no leading zeros) or a DNS name";

fn parse_link_origin_name_host(host: &str) -> Result<(String, bool), &'static str> {
    use std::net::Ipv4Addr;

    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let label_ok = |label: &&str| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if host.len() > 253 || !labels.iter().all(label_ok) {
        return Err("must name a host of ASCII letters, digits, hyphens and dots");
    }
    // A last label that is all digits, or `0x` and hex digits, is an
    // IPv4 number to a browser (the WHATWG "ends in a number" check), so
    // the host is an address or nothing: `1.2.3`, `127.1`, `0x7f000001`
    // and `trawl.0x1` are not hostnames. Only canonical dotted-decimal
    // is accepted; every other spelling is refused, not parsed.
    let numeric_final = |label: &str| {
        let b = label.as_bytes();
        b.iter().all(u8::is_ascii_digit)
            || (b.len() >= 2
                && b[0] == b'0'
                && b[1].eq_ignore_ascii_case(&b'x')
                && b[2..].iter().all(u8::is_ascii_hexdigit))
    };
    if labels.last().is_some_and(|l| numeric_final(l)) {
        let canonical_octet = |label: &&str| {
            label.bytes().all(|b| b.is_ascii_digit())
                && (*label == "0" || !label.starts_with('0'))
                && label.parse::<u8>().is_ok()
        };
        if labels.len() != 4 || !labels.iter().all(canonical_octet) {
            return Err(NUMERIC_HOST_RULE);
        }
        let ip: Ipv4Addr = host.parse().map_err(|_| NUMERIC_HOST_RULE)?;
        Ok((host, ip.is_loopback()))
    } else {
        let loopback = host == "localhost";
        Ok((host, loopback))
    }
}

/// Parse `[server] link_origin` into its normalized origin (ADR-0058).
/// The error is the rule that failed, as a phrase to follow the key's name;
/// it never echoes the value.
fn parse_link_origin(value: &str) -> Result<String, &'static str> {
    use std::net::Ipv6Addr;

    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("must not contain whitespace or control characters");
    }
    let (scheme, rest) = value
        .split_once("://")
        .ok_or("must be an origin: a scheme, \"://\", a host and an optional port")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return Err("must use the https scheme, or http on a loopback host");
    }
    if rest.contains('?') {
        return Err("must not carry a query");
    }
    if rest.contains('#') {
        return Err("must not carry a fragment");
    }
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    if !matches!(path, "" | "/") {
        return Err("must not carry a path");
    }
    if authority.contains('@') {
        return Err("must not carry userinfo");
    }
    if authority.contains('\\') {
        return Err("must be an origin: a host and an optional port");
    }

    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (inside, after) = bracketed
            .split_once(']')
            .ok_or("must close the bracketed IPv6 host with ']'")?;
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or("must follow the host with an optional :port")?,
            ),
        };
        (inside, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("must name a host");
    }
    let port = match port {
        None => None,
        Some(digits) => {
            let well_formed = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
            match digits.parse::<u16>() {
                Ok(port) if well_formed && port != 0 => Some(port),
                _ => return Err("must use a port between 1 and 65535"),
            }
        }
    };

    let (host, loopback) = if authority.starts_with('[') {
        let ip: Ipv6Addr = host.parse().map_err(|_| "must name a valid IPv6 host")?;
        (format!("[{ip}]"), ip.is_loopback())
    } else {
        parse_link_origin_name_host(host)?
    };

    if scheme == "http" && !loopback {
        return Err(
            "must use https; http is allowed only on a loopback host (localhost, 127.0.0.0/8, ::1)",
        );
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    Ok(match port {
        Some(port) if port != default_port => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    })
}

/// Settings keyed by a peer address. An error inside one names the map and
/// never the key, because the key is configuration too (ADR-0047).
const PEER_KEYED_SETTINGS: [&str; 2] = ["syslog.sender_timezones", "syslog.source_service_map"];

fn without_peer_key(setting: String) -> String {
    PEER_KEYED_SETTINGS
        .into_iter()
        .find(|map| {
            setting
                .strip_prefix(map)
                .is_some_and(|rest| rest.starts_with('.'))
        })
        .map_or(setting, str::to_owned)
}

impl Config {
    /// Parse the current TOML schema with value-free error diagnostics.
    ///
    /// Does not resolve paths, environment overrides, or daemon constraints.
    /// Consumers such as the browser proxy apply their own runtime validation.
    pub fn parse_toml(contents: &str) -> Result<Self, ConfigError> {
        let deserializer =
            toml::de::Deserializer::parse(contents).map_err(|e| ConfigError::Parse {
                path: PathBuf::from("<inline>"),
                setting: "<document>".into(),
                reason: "invalid TOML syntax",
                offset: e.span().map(|span| span.start),
            })?;
        serde_path_to_error::deserialize(deserializer).map_err(|e| ConfigError::Parse {
            path: PathBuf::from("<inline>"),
            setting: if let Some(field) = e
                .inner()
                .message()
                .strip_prefix("missing field `")
                .and_then(|s| s.strip_suffix('`'))
            {
                format!("{}.{field}", e.path())
            } else {
                without_peer_key(e.path().to_string())
            },
            reason: match e.inner().message() {
                m if m.starts_with("unknown field") => "unknown setting",
                NONZERO_REQUIRED => NONZERO_REQUIRED,
                BYTE_SIZE_OVERFLOW => BYTE_SIZE_OVERFLOW,
                _ => "invalid or missing setting",
            },
            offset: e.inner().span().map(|span| span.start),
        })
    }

    /// Parse configuration, resolve paths and overrides, and validate daemon constraints.
    pub fn from_toml(contents: &str) -> Result<Self, ConfigError> {
        let mut config = Self::parse_toml(contents)?;
        config.resolve_paths();
        // Applied before validation so an env-supplied address is held to the
        // same checks as a configured one.
        config.server.http_addr = config.server.resolve_http_addr();
        config.validate()?;
        // Validation has proved the origin parses, so the stored form can be
        // the normalized one without a second error path.
        if let Some(origin) = &config.server.link_origin {
            config.server.link_origin = parse_link_origin(origin).ok();
        }
        Ok(config)
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::Io {
            path: path.as_ref().to_owned(),
            source: e,
        })?;
        Self::from_toml(&contents).map_err(|e| match e {
            ConfigError::Parse {
                setting,
                reason,
                offset,
                ..
            } => ConfigError::Parse {
                path: path.as_ref().to_owned(),
                setting,
                reason,
                offset,
            },
            other => other,
        })
    }

    fn resolve_paths(&mut self) {
        self.data.path = expand_tilde(&self.data.path);
        if let Some(log_file) = &self.server.log_file {
            self.server.log_file = Some(PathBuf::from(expand_tilde(&log_file.to_string_lossy())));
        }
        if let Some(cert) = &self.server.tls_cert_path {
            self.server.tls_cert_path = Some(PathBuf::from(expand_tilde(&cert.to_string_lossy())));
        }
        if let Some(key) = &self.server.tls_key_path {
            self.server.tls_key_path = Some(PathBuf::from(expand_tilde(&key.to_string_lossy())));
        }
        if let Some(wal_dir) = &self.ingest.wal_dir {
            self.ingest.wal_dir = Some(PathBuf::from(expand_tilde(&wal_dir.to_string_lossy())));
        }
        if let Some(query_log) = &self.server.query_log {
            self.server.query_log = Some(PathBuf::from(expand_tilde(&query_log.to_string_lossy())));
        }
        if let Some(scratch) = &self.server.query_scratch_dir {
            self.server.query_scratch_dir =
                Some(PathBuf::from(expand_tilde(&scratch.to_string_lossy())));
        }
        if let Some(results) = &self.server.results_dir {
            self.server.results_dir = Some(PathBuf::from(expand_tilde(&results.to_string_lossy())));
        }
    }

    pub fn wal_dir(&self) -> PathBuf {
        self.ingest
            .wal_dir
            .clone()
            .unwrap_or_else(|| self.data.base_dir().join(WAL_DIR_NAME))
    }

    pub fn query_scratch_dir(&self) -> PathBuf {
        self.server
            .query_scratch_dir
            .clone()
            .unwrap_or_else(|| data_root_sibling(&self.data.base_dir(), QUERY_SCRATCH_SUFFIX))
    }

    pub fn results_dir(&self) -> PathBuf {
        self.server
            .results_dir
            .clone()
            .unwrap_or_else(|| data_root_sibling(&self.data.base_dir(), RESULTS_SUFFIX))
    }

    pub fn state_dir(&self) -> PathBuf {
        self.data
            .base_dir()
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    /// The file may not exist yet: trawld writes it on its first start.
    pub fn generated_cert_path(&self) -> Option<PathBuf> {
        if self.server.tls_cert_path.is_some() {
            return None;
        }
        Some(
            self.state_dir()
                .join(GENERATED_TLS_DIR)
                .join(GENERATED_CERT_FILE),
        )
    }

    pub fn warnings(&self) -> Vec<String> {
        let mut warns = Vec::new();
        if self.server.tls_cert_path.is_none() {
            warns.push(
                "no TLS certificate configured — using auto-generated self-signed certificate"
                    .into(),
            );
        }
        if self.internal_telemetry_enabled() && self.server.log_file.is_some() {
            warns.push(
                "log_file is not opened while ingest and internal_telemetry are enabled; \
                 server events use the ingest pipeline as service:trawld"
                    .into(),
            );
        }
        if self.ingest.internal_telemetry && !self.ingest.enabled {
            warns.push(
                "internal_telemetry requires ingest to be enabled — telemetry disabled".into(),
            );
        }
        if self.syslog.enabled && !self.ingest.enabled {
            warns.push("syslog listener requires ingest to be enabled — syslog disabled".into());
        }
        warns
    }

    pub fn internal_telemetry_enabled(&self) -> bool {
        self.ingest.enabled && self.ingest.internal_telemetry
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.data.path.is_empty() {
            return Err(ConfigError::Validation("data.path cannot be empty".into()));
        }

        // `read_parquet` reads the data directory's path as a pattern while
        // the filesystem presence check resolves it literally. A daemon
        // pointed at `trawl*data` therefore passes startup against a
        // directory of that exact name and then reads the parquet files of
        // every neighbour the glob matches, `trawlXdata` included. Refusing
        // the characters is the only place the two readings can be held
        // together.
        if let Some(found) = self
            .data
            .path
            .chars()
            .find(|c| GLOB_METACHARACTERS.contains(c))
        {
            return Err(ConfigError::Validation(format!(
                "data.path must be a directory path without glob metacharacters; found '{found}'"
            )));
        }

        // A `..` after a component that is missing places `<base>/missing/../x`
        // at `<base>` while trawld later makes it at `<base>/x`, which can be the
        // scratch directory itself (ADR-0056).
        for (key, path) in [
            ("data.path", Path::new(&self.data.path)),
            (
                "ingest.wal_dir",
                self.ingest.wal_dir.as_deref().unwrap_or(Path::new("")),
            ),
        ] {
            if path.components().any(|c| c == Component::ParentDir) {
                return Err(ConfigError::Validation(format!(
                    "{key} must not contain a '..' component"
                )));
            }
        }

        if self.server.max_concurrent_queries == 0 {
            return Err(ConfigError::Validation(
                "server.max_concurrent_queries must be > 0".into(),
            ));
        }

        if !(1..=MAX_CONCURRENT_REQUESTS_LIMIT).contains(&self.server.max_concurrent_requests) {
            return Err(ConfigError::Validation(format!(
                "server.max_concurrent_requests must be between 1 and \
                 {MAX_CONCURRENT_REQUESTS_LIMIT}; got {}",
                self.server.max_concurrent_requests
            )));
        }

        self.validate_query_scratch_dir()?;
        self.validate_results_dir()?;
        if let Some(origin) = &self.server.link_origin {
            parse_link_origin(origin)
                .map_err(|rule| ConfigError::Validation(format!("server.link_origin {rule}")))?;
        }
        self.validate_capture()?;
        self.validate_investigation()?;
        validate_telemetry_buffer_max_bytes(self.ingest.telemetry_buffer_max_bytes)?;
        validate_max_catchup_intervals(self.scheduler.max_catchup_intervals)?;
        validate_report_retention(&self.scheduler)?;

        if self.server.tls_cert_path.is_some() != self.server.tls_key_path.is_some() {
            return Err(ConfigError::Validation(
                "tls_cert_path and tls_key_path must both be set or both omitted".into(),
            ));
        }

        self.validate_ingest_env_names()?;
        self.validate_retention_env_keys()?;

        self.validate_task_periods()?;

        // A zero interval would run normal compaction passes back to back
        // and turn the pressure cooldown (ADR-0043) into no wait at all.
        if self.ingest.enabled && self.ingest.compaction_interval_secs == 0 {
            return Err(ConfigError::Validation(
                "ingest.compaction_interval_secs must be > 0".into(),
            ));
        }

        if self.ingest.compaction_chunk_size > MAX_COMPACTION_CHUNK_SIZE {
            return Err(ConfigError::Validation(format!(
                "ingest.compaction_chunk_size must be at most {MAX_COMPACTION_CHUNK_SIZE} \
                 (a chunk's publication marker names every WAL file it merges)"
            )));
        }

        if self.syslog.enabled {
            if self.syslog.batch_interval_ms == 0 {
                return Err(ConfigError::Validation(
                    "syslog.batch_interval_ms must be > 0".into(),
                ));
            }
            if self.syslog.batch_max_events == 0 {
                return Err(ConfigError::Validation(
                    "syslog.batch_max_events must be > 0".into(),
                ));
            }
            if self.syslog.tcp_enabled && self.syslog.max_tcp_connections == 0 {
                return Err(ConfigError::Validation(
                    "syslog.max_tcp_connections must be > 0 when TCP is enabled".into(),
                ));
            }
            if self.syslog.tcp_enabled && self.syslog.tcp_idle_timeout_secs == 0 {
                return Err(ConfigError::Validation(
                    "syslog.tcp_idle_timeout_secs must be > 0 when TCP is enabled".into(),
                ));
            }
            self.validate_syslog_service_names()?;
        }

        Ok(())
    }

    /// The query scratch directory stays apart from every directory whose
    /// contents trawld owns by name (ADR-0056). A comparison of spellings
    /// cannot see where `..` or the working directory leads, so an explicit
    /// directory must be absolute and free of `..`.
    fn validate_query_scratch_dir(&self) -> Result<(), ConfigError> {
        self.validate_owned_dir(
            "server.query_scratch_dir",
            self.server.query_scratch_dir.as_deref(),
            &self.query_scratch_dir(),
        )
    }

    fn validate_results_dir(&self) -> Result<(), ConfigError> {
        const KEY: &str = "server.results_dir";
        let results = self.results_dir();
        self.validate_owned_dir(KEY, self.server.results_dir.as_deref(), &results)?;
        let scratch = self.query_scratch_dir();
        let relation = if results == scratch {
            "equal to"
        } else if results.starts_with(&scratch) {
            "inside"
        } else if scratch.starts_with(&results) {
            "above"
        } else {
            return Ok(());
        };
        Err(ConfigError::Validation(format!(
            "{KEY} must not be {relation} the query scratch directory \
             (server.query_scratch_dir); set {KEY} to a directory of its own"
        )))
    }

    /// The shared checks of a directory trawld owns by name: `explicit` is
    /// the configured value (shape rules apply only to it), `effective` the
    /// resolved one (glob and overlap rules apply to it).
    fn validate_owned_dir(
        &self,
        key: &str,
        explicit: Option<&Path>,
        effective: &Path,
    ) -> Result<(), ConfigError> {
        if let Some(dir) = explicit {
            if dir.as_os_str().is_empty() {
                return Err(ConfigError::Validation(format!("{key} cannot be empty")));
            }
            if dir.is_relative() {
                return Err(ConfigError::Validation(format!(
                    "{key} must be an absolute path"
                )));
            }
            if dir.components().any(|c| c == Component::ParentDir) {
                return Err(ConfigError::Validation(format!(
                    "{key} must not contain a '..' component"
                )));
            }
        }
        // The snapshot paths in the scratch directory reach `read_json`,
        // which expands a glob as `read_parquet` does `data.path`; the
        // results root is held to the same rule.
        if let Some(found) = effective
            .to_string_lossy()
            .chars()
            .find(|c| GLOB_METACHARACTERS.contains(c))
        {
            return Err(ConfigError::Validation(format!(
                "{key} must be a directory path without glob metacharacters; found '{found}'"
            )));
        }
        let data = self.data.base_dir();
        let owned = [
            (data.clone(), "the data root (data.path)"),
            (self.wal_dir(), "the WAL directory (ingest.wal_dir)"),
            (
                data_root_sibling(&data, REPIN_SHADOW_SUFFIX),
                "the repin shadow root (<data.path>.repin-next)",
            ),
            (
                data_root_sibling(&data, REPIN_ASIDE_SUFFIX),
                "the repin aside root (<data.path>.repin-aside)",
            ),
        ];
        for (root, what) in owned {
            let relation = if effective == root {
                "equal to"
            } else if effective.starts_with(&root) {
                "inside"
            } else if root.starts_with(effective) {
                "above"
            } else {
                continue;
            };
            return Err(ConfigError::Validation(format!(
                "{key} must not be {relation} {what}; set {key} to a directory of its own"
            )));
        }
        Ok(())
    }

    /// A task period is never 0: tokio's `interval` panics on it and
    /// `sleep` spins. Only `auth.audit_interval_secs` and
    /// `server.tls_reload_interval_secs` treat 0 as "skip the task". The
    /// rule holds even when the owning component is off, so a dormant
    /// value cannot fail later when someone turns the component on.
    fn validate_task_periods(&self) -> Result<(), ConfigError> {
        for (key, value) in [
            (
                "ingest.stats_interval_secs",
                self.ingest.stats_interval_secs,
            ),
            (
                "ingest.telemetry_flush_interval_secs",
                self.ingest.telemetry_flush_interval_secs,
            ),
            (
                "retention.retention_interval_secs",
                self.retention.retention_interval_secs,
            ),
            (
                "scheduler.poll_interval_secs",
                self.scheduler.poll_interval_secs,
            ),
            (
                "server.schema_cache_ttl_secs",
                self.server.schema_cache_ttl_secs,
            ),
            ("server.monitor_refresh_ms", self.server.monitor_refresh_ms),
        ] {
            if value == 0 {
                return Err(ConfigError::Validation(format!("{key} must be > 0")));
            }
        }
        Ok(())
    }

    fn validate_capture(&self) -> Result<(), ConfigError> {
        let c = &self.capture;
        for (key, value) in [
            ("max_rows", c.max_rows),
            ("max_wire_bytes", c.max_wire_bytes),
            ("max_file_bytes", c.max_file_bytes),
            ("ttl_secs", c.ttl_secs),
            ("default_window_secs", c.default_window_secs),
            ("max_span_secs", c.max_span_secs),
            ("deadline_secs", c.deadline_secs),
            ("source_max_bytes", c.source_max_bytes),
            ("per_key_results", c.per_key_results),
            ("per_key_bytes", c.per_key_bytes),
            ("installation_results", c.installation_results),
            ("installation_bytes", c.installation_bytes),
            ("per_key_running", c.per_key_running),
            ("installation_running", c.installation_running),
            ("queue_depth", c.queue_depth),
            ("page_max_rows", c.page_max_rows),
            ("tombstone_secs", c.tombstone_secs),
        ] {
            if value == 0 {
                return Err(ConfigError::Validation(format!(
                    "capture.{key} must be > 0"
                )));
            }
        }
        if c.max_wire_bytes < CAPTURE_MAX_WIRE_BYTES_MIN {
            return Err(ConfigError::Validation(format!(
                "capture.max_wire_bytes is {}, below the minimum of {CAPTURE_MAX_WIRE_BYTES_MIN} bytes, the longest UTF-8 character a cell read must hold",
                c.max_wire_bytes
            )));
        }
        for (small, large, small_value, large_value) in [
            (
                "per_key_results",
                "installation_results",
                c.per_key_results,
                c.installation_results,
            ),
            (
                "per_key_bytes",
                "installation_bytes",
                c.per_key_bytes,
                c.installation_bytes,
            ),
            (
                "per_key_running",
                "installation_running",
                c.per_key_running,
                c.installation_running,
            ),
            ("page_max_rows", "max_rows", c.page_max_rows, c.max_rows),
            (
                "default_window_secs",
                "max_span_secs",
                c.default_window_secs,
                c.max_span_secs,
            ),
            (
                "max_wire_bytes",
                "max_file_bytes",
                c.max_wire_bytes,
                c.max_file_bytes,
            ),
        ] {
            if small_value > large_value {
                return Err(ConfigError::Validation(format!(
                    "capture.{small} must be <= capture.{large}"
                )));
            }
        }
        Ok(())
    }

    fn validate_investigation(&self) -> Result<(), ConfigError> {
        let i = &self.investigation;
        for (key, value) in [
            ("evidence_idle_secs", i.evidence_idle_secs),
            ("evidence_ceiling_secs", i.evidence_ceiling_secs),
            (
                "evidence_per_investigation_bytes",
                i.evidence_per_investigation_bytes,
            ),
            ("evidence_installation_bytes", i.evidence_installation_bytes),
        ] {
            if value == 0 {
                return Err(ConfigError::Validation(format!(
                    "investigation.{key} must be > 0"
                )));
            }
        }
        for (small, large, small_value, large_value) in [
            (
                "investigation.evidence_idle_secs",
                "investigation.evidence_ceiling_secs",
                i.evidence_idle_secs,
                i.evidence_ceiling_secs,
            ),
            (
                "investigation.evidence_per_investigation_bytes",
                "investigation.evidence_installation_bytes",
                i.evidence_per_investigation_bytes,
                i.evidence_installation_bytes,
            ),
            (
                "capture.max_file_bytes",
                "investigation.evidence_per_investigation_bytes",
                self.capture.max_file_bytes,
                i.evidence_per_investigation_bytes,
            ),
        ] {
            if small_value > large_value {
                return Err(ConfigError::Validation(format!(
                    "{small} must be <= {large}"
                )));
            }
        }
        Ok(())
    }

    /// Env allowlist (ADR-0009): validated at load, refuse to start
    /// otherwise — env is a path segment and the charset is the whole
    /// injectivity argument.
    fn validate_ingest_env_names(&self) -> Result<(), ConfigError> {
        for (index, env) in self.ingest.envs.iter().enumerate() {
            if !is_valid_env_name(env) {
                return Err(ConfigError::Validation(format!(
                    "ingest.envs[{index}] is not a valid env name (must match [a-z0-9_-]{{1,32}})"
                )));
            }
            if RESERVED_ENV_NAMES.contains(&env.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "ingest.envs[{index}] is reserved"
                )));
            }
        }
        if !is_valid_env_name(&self.ingest.default_env) {
            return Err(ConfigError::Validation(
                "ingest.default_env is not a valid env name (must match [a-z0-9_-]{1,32})".into(),
            ));
        }
        if RESERVED_ENV_NAMES.contains(&self.ingest.default_env.as_str()) {
            return Err(ConfigError::Validation(
                "ingest.default_env is reserved".into(),
            ));
        }
        if !self.ingest.envs.is_empty() && !self.ingest.envs.contains(&self.ingest.default_env) {
            return Err(ConfigError::Validation(
                "ingest.default_env must be a member of ingest.envs".into(),
            ));
        }
        Ok(())
    }

    /// Deliberately not cross-checked against `ingest.envs`: de-listing an
    /// env stops new ingest for it while its data stays on disk, and that
    /// data still needs an age policy.
    fn validate_retention_env_keys(&self) -> Result<(), ConfigError> {
        for env in self.retention.env.keys() {
            if !is_valid_env_name(env) {
                return Err(ConfigError::Validation(format!(
                    "retention.env key {env:?} is not a valid env name \
                     (must match [a-z0-9_-]{{1,32}})"
                )));
            }
            if RESERVED_ENV_NAMES.contains(&env.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "retention.env key {env:?} is reserved — `wal/` and \
                     `scheduled/` live alongside env directories under the \
                     data root"
                )));
            }
        }
        Ok(())
    }

    /// Service names from syslog config reach WAL filenames verbatim
    /// (ADR-0009), so they carry the same charset/dot obligations as
    /// ingested ones — refuse to start rather than write outside the data
    /// tree or produce a name no query can name.
    fn validate_syslog_service_names(&self) -> Result<(), ConfigError> {
        let invalid = |field: String| {
            ConfigError::Validation(format!(
                "{field} is not a valid service name \
                 (1-{MAX_SERVICE_NAME_LEN} chars of [A-Za-z0-9._-], not \
                 dot-leading)"
            ))
        };

        if !is_valid_service_name(&self.syslog.default_service) {
            return Err(invalid("syslog.default_service".to_owned()));
        }
        if !self
            .syslog
            .source_service_map
            .values()
            .all(|service| is_valid_service_name(service))
        {
            return Err(invalid("syslog.source_service_map".to_owned()));
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    // Never retain the TOML error: both its display and debug forms can
    // include config values such as database credentials.
    #[error("{reason} at {setting} in {} (byte offset {offset:?})", path.display())]
    Parse {
        path: PathBuf,
        setting: String,
        reason: &'static str,
        offset: Option<usize>,
    },

    #[error("config validation error: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn unknown_settings_fail_with_paths_without_values() {
        for (table, key) in [
            ("", "servre"),
            ("server", "http_adrr"),
            ("server.rate_limit", "admin"),
            ("server.rate_limit", "analyst"),
            ("server.rate_limit", "reader"),
            ("server.rate_limit", "ingest"),
            ("auth", "db_path"),
            ("auth", "auth_cache_ttl_secs"),
            ("web", "coastwatch_url"),
            ("web", "upstream_conect_addr"),
            ("data", "paht"),
            ("ingest", "enable"),
            ("retention", "max_agge_days"),
            ("retention.env.prod", "max_agge_days"),
            ("scheduler", "enable"),
            ("syslog", "enable"),
            ("storage", "databse_url"),
        ] {
            let mut document = "[server]\n[data]\npath='/data'\n".to_owned();
            if table.is_empty() {
                document.insert_str(0, &format!("{key}='private-secret'\n"));
            } else if table == "server" || table == "data" {
                document = document.replace(
                    &format!("[{table}]"),
                    &format!("[{table}]\n{key}='private-secret'"),
                );
            } else {
                use std::fmt::Write as _;
                writeln!(document, "[{table}]\n{key}='private-secret'").unwrap();
            }
            let error = Config::from_toml(&document).unwrap_err();
            let expected = if table.is_empty() {
                key.to_owned()
            } else {
                format!("{table}.{key}")
            };
            assert!(error.to_string().contains(&expected), "{expected}: {error}");
            assert!(error.to_string().contains("unknown setting"), "{error}");
            assert!(!format!("{error:?}").contains("private-secret"));
        }
    }

    #[test]
    fn dynamic_maps_and_typed_derivation_entries_remain_supported() {
        let config = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[retention.env.prod]
max_age_days = 7
[syslog.source_service_map]
"192.0.2.1" = "router"
[ingest]
severity_from = [{ field = "level", dialect = "syslog" }]
"#,
        )
        .unwrap();
        assert_eq!(config.retention.env["prod"].max_age_days, 7);
        assert_eq!(config.syslog.source_service_map["192.0.2.1"], "router");
        let error = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[ingest]
severity_from = [{ field = "level", dialcet = "private-secret" }]
"#,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("ingest.severity_from[0]"),
            "{error}"
        );
        assert!(!format!("{error:?}").contains("private-secret"));
    }

    #[test]
    fn invalid_values_and_syntax_do_not_echo_credentials() {
        for document in [
            "[server]\ntimeout_secs='postgres://user:private-secret@host/db'\n[data]\npath='/data'",
            "[server]\npassword='private-secret\n[data]\npath='/data'",
            "[server]\n[data]\npath='/data'\n[ingest]\ndefault_env='PRIVATE-secret'",
            "[server]\n[data]\npath='/data'\n[syslog]\nenabled=true\ndefault_service='private-secret/invalid'",
        ] {
            let error = Config::from_toml(document).unwrap_err();
            assert!(!error.to_string().contains("private-secret"));
            assert!(!format!("{error:?}").contains("private-secret"));
        }
    }

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[server]

[data]
path = "/var/lib/trawl/data"

[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.http_addr, "127.0.0.1:5514");
        assert_eq!(config.server.timeout_secs, 30);
        assert!(config.server.max_concurrent_queries > 0);
        assert_eq!(config.data.path, "/var/lib/trawl/data");
        assert!(config.server.log_file.is_none());
        assert!(config.server.tls_cert_path.is_none());
        assert!(config.server.tls_key_path.is_none());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[server]
http_addr = "0.0.0.0:9090"
timeout_secs = 60
max_concurrent_queries = 8
log_file = "/var/log/trawld.log"

[data]
path = "/data"

[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.http_addr, "0.0.0.0:9090");
        assert_eq!(config.server.timeout_secs, 60);
        assert_eq!(config.server.max_concurrent_queries, 8);
        assert_eq!(
            config.server.log_file.as_deref(),
            Some(std::path::Path::new("/var/log/trawld.log"))
        );
    }

    #[test]
    fn max_concurrent_requests_bounds() {
        let with = |value: Option<usize>| -> Config {
            let setting = value
                .map(|v| format!("max_concurrent_requests = {v}"))
                .unwrap_or_default();
            toml::from_str(&format!(
                "[server]\n{setting}\n[data]\npath = \"/data\"\n[auth]\n"
            ))
            .unwrap()
        };

        assert_eq!(DEFAULT_MAX_CONCURRENT_REQUESTS, 32);
        assert_eq!(with(None).server.max_concurrent_requests, 32);
        with(None).validate().unwrap();

        for refused in [0, MAX_CONCURRENT_REQUESTS_LIMIT + 1] {
            let message = with(Some(refused)).validate().unwrap_err().to_string();
            assert!(
                message.contains("server.max_concurrent_requests"),
                "{refused}: the message must name the setting: {message}"
            );
            assert!(
                message.contains("between 1 and 65536"),
                "{refused}: the message must name the range: {message}"
            );
        }
        for accepted in [1, MAX_CONCURRENT_REQUESTS_LIMIT] {
            with(Some(accepted)).validate().unwrap();
        }

        assert_eq!(MAX_CONCURRENT_REQUESTS_LIMIT, 65_536);
        const { assert!(MAX_CONCURRENT_REQUESTS_LIMIT <= tokio::sync::Semaphore::MAX_PERMITS) };
    }

    #[test]
    fn validation_rejects_empty_data_path() {
        let toml = r#"
[server]
[data]
path = ""
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("data.path"));
    }

    #[test]
    fn validation_rejects_zero_concurrency() {
        let toml = r#"
[server]
max_concurrent_queries = 0
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_concurrent_queries"));
    }

    #[test]
    fn validation_rejects_zero_compaction_interval() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
compaction_interval_secs = 0
"#;
        let err = Config::from_toml(toml).unwrap_err();
        assert_eq!(
            err.to_string(),
            "config validation error: ingest.compaction_interval_secs must be > 0"
        );

        let config = Config::from_toml(&format!("{toml}enabled = false\n")).unwrap();
        assert_eq!(config.ingest.compaction_interval_secs, 0);
    }

    #[test]
    fn parse_tls_config() {
        let toml = r#"
[server]
tls_cert_path = "/etc/trawl/cert.pem"
tls_key_path = "/etc/trawl/key.pem"
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.server.tls_cert_path.as_deref(),
            Some(std::path::Path::new("/etc/trawl/cert.pem"))
        );
        assert_eq!(
            config.server.tls_key_path.as_deref(),
            Some(std::path::Path::new("/etc/trawl/key.pem"))
        );
    }

    #[test]
    fn validation_rejects_partial_tls_config() {
        let toml = r#"
[server]
tls_cert_path = "/etc/trawl/cert.pem"
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("tls_cert_path"));
    }

    #[test]
    fn warning_when_no_tls_cert_configured() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let warns = config.warnings();
        assert!(warns.iter().any(|w| w.contains("self-signed")));
    }

    #[test]
    fn no_warning_when_tls_cert_configured() {
        let toml = r#"
[server]
tls_cert_path = "/etc/trawl/cert.pem"
tls_key_path = "/etc/trawl/key.pem"
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let warns = config.warnings();
        assert!(warns.is_empty());
    }

    #[test]
    fn generated_cert_lives_in_the_state_dir_tls_directory() {
        let toml = r#"
[server]
[data]
path = "/var/lib/trawl/data"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.generated_cert_path().as_deref(),
            Some(Path::new("/var/lib/trawl/tls/cert.pem"))
        );
    }

    #[test]
    fn no_generated_cert_when_server_names_its_own() {
        let toml = r#"
[server]
tls_cert_path = "/etc/trawl/cert.pem"
tls_key_path = "/etc/trawl/key.pem"
[data]
path = "/var/lib/trawl/data"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.generated_cert_path(), None);
    }

    #[test]
    fn cors_defaults_to_empty() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.server.cors_allowed_origins.is_empty());
    }

    #[test]
    fn data_dir_refuses_glob_metacharacters() {
        for metacharacter in GLOB_METACHARACTERS {
            let path = format!("/data/private-secret{metacharacter}unfinished");
            let document = format!("[server]\n[data]\npath = '{path}'");
            let error = Config::from_toml(&document).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "config validation error: data.path must be a directory path \
                     without glob metacharacters; found '{metacharacter}'"
                )
            );
            assert!(!format!("{error:?}").contains("private-secret"));
        }

        for path in [
            "/data/*.parquet",
            "/data/**/*.parquet",
            "data?",
            "/data/[ab]",
            "/data/{a,b}",
        ] {
            let document = format!("[server]\n[data]\npath = '{path}'");
            Config::from_toml(&document)
                .expect_err("a path DuckDB would read as a pattern is not a directory");
        }
    }

    #[test]
    fn daemon_data_directory_is_preserved_and_drives_wal_location() {
        for path in [
            "/var/lib/trawl/data",
            "/var/lib/trawl/data/",
            "relative/data",
            "/data with spaces",
        ] {
            let config = Config::from_toml(&format!("[server]\n[data]\npath = '{path}'")).unwrap();
            assert_eq!(config.data.base_dir(), PathBuf::from(path));
            assert_eq!(config.wal_dir(), PathBuf::from(path).join("wal"));
        }
    }

    #[test]
    fn query_scratch_refuses_the_filesystem_root_as_data_root() {
        let err = Config::from_toml("[server]\n[data]\npath = '/'")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("server.query_scratch_dir must not be inside the data root"),
            "{err}"
        );
    }

    #[test]
    fn ingest_defaults_when_omitted() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.ingest.enabled);
        assert_eq!(config.ingest.max_body_bytes, 16 * 1024 * 1024);
        assert!(config.ingest.wal_dir.is_none());
        assert_eq!(config.ingest.compaction_interval_secs, 10);
        assert!(config.ingest.internal_telemetry);
        assert!(config.ingest.daily_rollup);
        assert_eq!(config.wal_dir(), std::path::Path::new("/data/wal"));
    }

    #[test]
    fn cors_parses_origins() {
        let toml = r#"
[server]
cors_allowed_origins = ["https://trawl.example.com", "https://admin.example.com"]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.cors_allowed_origins.len(), 2);
        assert_eq!(
            config.server.cors_allowed_origins[0],
            "https://trawl.example.com"
        );
    }

    #[test]
    fn rate_limit_defaults() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.rate_limit.default_rpm, 100);
        assert_eq!(config.server.rate_limit.ingest_rpm, 1000);
        config.validate().unwrap();
    }

    #[test]
    fn rate_limit_custom_values() {
        let toml = r#"
[server]
[server.rate_limit]
default_rpm = 250
ingest_rpm = 5000
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.rate_limit.default_rpm, 250);
        assert_eq!(config.server.rate_limit.ingest_rpm, 5000);
        config.validate().unwrap();
    }

    #[test]
    fn internal_telemetry_defaults_to_true() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.ingest.internal_telemetry);
        assert!(config.internal_telemetry_enabled());
    }

    #[test]
    fn internal_telemetry_disabled_explicitly() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
internal_telemetry = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ingest.internal_telemetry);
        assert!(!config.internal_telemetry_enabled());
    }

    #[test]
    fn telemetry_buffer_zero_is_boot_fatal_with_the_off_switch() {
        let err = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
telemetry_buffer_max_bytes = 0
"#,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "config validation error: ingest.telemetry_buffer_max_bytes must be at least 64K \
             (65536 bytes); set ingest.internal_telemetry = false to disable internal telemetry"
        );
    }

    #[test]
    fn telemetry_buffer_below_the_floor_is_boot_fatal() {
        let err = Config::from_toml(&format!(
            r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
telemetry_buffer_max_bytes = {}
"#,
            MIN_TELEMETRY_BUFFER_MAX_BYTES - 1
        ))
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "config validation error: ingest.telemetry_buffer_max_bytes must be at least 64K \
             (65536 bytes); set ingest.internal_telemetry = false to disable internal telemetry"
        );
    }

    #[test]
    fn compaction_chunk_size_above_the_marker_bound_is_boot_fatal() {
        let chunk = |size: usize| {
            Config::from_toml(&format!(
                r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
compaction_chunk_size = {size}
"#
            ))
        };
        let err = chunk(MAX_COMPACTION_CHUNK_SIZE + 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "config validation error: ingest.compaction_chunk_size must be at most 4096 \
             (a chunk's publication marker names every WAL file it merges)"
        );
        assert_eq!(
            chunk(MAX_COMPACTION_CHUNK_SIZE)
                .unwrap()
                .ingest
                .compaction_chunk_size,
            MAX_COMPACTION_CHUNK_SIZE
        );
    }

    #[test]
    fn telemetry_buffer_at_the_floor_loads() {
        let config = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
telemetry_buffer_max_bytes = "64K"
"#,
        )
        .unwrap();

        assert_eq!(
            config.ingest.telemetry_buffer_max_bytes,
            MIN_TELEMETRY_BUFFER_MAX_BYTES
        );
    }

    #[test]
    fn scheduler_max_catchup_intervals_defaults_to_a_day_of_hourly_runs() {
        let config = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[auth]
"#,
        )
        .unwrap();
        assert_eq!(config.scheduler.max_catchup_intervals, 24);
        assert_eq!(
            SchedulerConfig::default().max_catchup_intervals,
            DEFAULT_SCHEDULER_MAX_CATCHUP_INTERVALS
        );
    }

    #[test]
    fn scheduler_report_max_rows_is_an_unknown_key() {
        let err = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[auth]
[scheduler]
report_max_rows = 10000
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("unknown setting at scheduler.report_max_rows"),
            "{err}"
        );
    }

    #[test]
    fn scheduler_max_catchup_intervals_outside_the_range_is_boot_fatal() {
        let expected = "config validation error: scheduler.max_catchup_intervals must be \
             between 1 and 1000000 (it is a count of whole schedule intervals a since_last \
             window may cover in one catch-up run; the ceiling keeps that span inside the \
             range the scheduler can compute)";

        for value in [0, MAX_SCHEDULER_CATCHUP_INTERVALS + 1, u32::MAX] {
            let err = Config::from_toml(&format!(
                r#"
[server]
[data]
path = "/data"
[auth]
[scheduler]
max_catchup_intervals = {value}
"#
            ))
            .unwrap_err();
            assert_eq!(err.to_string(), expected, "for {value}");
        }

        let config = Config::from_toml(&format!(
            r#"
[server]
[data]
path = "/data"
[auth]
[scheduler]
max_catchup_intervals = {MAX_SCHEDULER_CATCHUP_INTERVALS}
"#
        ))
        .unwrap();
        assert_eq!(
            config.scheduler.max_catchup_intervals,
            MAX_SCHEDULER_CATCHUP_INTERVALS
        );
    }

    /// Both fields are `u64`, and the toml crate deserializes an integer past
    /// `i64::MAX` into one, so the widest value an operator can write is
    /// `u64::MAX`.
    #[test]
    fn scheduler_retention_knobs_must_be_in_range() {
        let load = |line: String| {
            Config::from_toml(&format!(
                r#"
[server]
[data]
path = "/data"
[auth]
[scheduler]
enabled = false
{line}
"#
            ))
        };

        for value in [0_u64, 36_501, i64::MAX.unsigned_abs(), u64::MAX] {
            let err = load(format!("report_retention_days = {value}")).unwrap_err();
            assert!(matches!(err, ConfigError::Validation(_)), "for {value}");
            let text = err.to_string();
            assert!(
                text.contains("scheduler.report_retention_days")
                    && text.contains("between 1 and 36500"),
                "for {value}: {text}"
            );
        }
        let err = load("max_runs_per_schedule = 0".into()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        let text = err.to_string();
        assert!(
            text.contains("scheduler.max_runs_per_schedule") && text.contains("at least 1"),
            "{text}"
        );

        for value in [1, MAX_SCHEDULER_REPORT_RETENTION_DAYS] {
            let config = load(format!("report_retention_days = {value}")).unwrap();
            assert_eq!(config.scheduler.report_retention_days, value);
        }
        for value in [1_u64, 1_000_000_000_000, u64::MAX] {
            let config = load(format!("max_runs_per_schedule = {value}")).unwrap();
            assert_eq!(config.scheduler.max_runs_per_schedule, value);
        }
    }

    #[test]
    fn scheduler_catchup_span_always_fits_a_timedelta() {
        let widest = u64::from(MAX_SCHEDULER_CATCHUP_INTERVALS) * MIRRORED_MAX_DURATION_SECS;
        let representable = u64::try_from(chrono::TimeDelta::MAX.num_seconds())
            .expect("a TimeDelta's second count is non-negative at its maximum");
        assert!(
            widest <= representable,
            "the widest configurable catch-up span is {widest}s, \
             past the {representable}s a TimeDelta holds"
        );
        assert!(
            chrono::TimeDelta::try_seconds(
                i64::try_from(widest).expect("the widest span fits an i64")
            )
            .is_some(),
            "chrono itself must accept the widest configurable span"
        );
    }

    #[test]
    fn internal_telemetry_requires_ingest_enabled() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
enabled = false
internal_telemetry = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.internal_telemetry_enabled());
        let warns = config.warnings();
        assert!(
            warns
                .iter()
                .any(|w| w.contains("internal_telemetry requires ingest"))
        );
    }

    #[test]
    fn internal_telemetry_warns_only_when_log_file_is_not_opened() {
        let toml = r#"
[server]
log_file = "/var/log/trawld.log"
[data]
path = "/data"
[auth]
"#;
        let mut config: Config = toml::from_str(toml).unwrap();
        for ingest in [false, true] {
            for telemetry in [false, true] {
                for log_file in [None, Some(PathBuf::from("/var/log/trawld.log"))] {
                    config.ingest.enabled = ingest;
                    config.ingest.internal_telemetry = telemetry;
                    config.server.log_file = log_file;
                    let warns = config.warnings();
                    let file_warning = warns.iter().find(|w| w.starts_with("log_file"));
                    assert_eq!(
                        file_warning.is_some(),
                        ingest && telemetry && config.server.log_file.is_some(),
                        "ingest={ingest}, telemetry={telemetry}, log_file={:?}: {warns:?}",
                        config.server.log_file
                    );
                    if let Some(warning) = file_warning {
                        assert_eq!(
                            warning,
                            "log_file is not opened while ingest and internal_telemetry are enabled; \
                             server events use the ingest pipeline as service:trawld"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn daily_rollup_disabled_explicitly() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
daily_rollup = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ingest.daily_rollup);
    }

    #[test]
    fn retention_defaults_when_omitted() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.retention.max_age_days, 90);
        assert_eq!(config.retention.min_free_disk_bytes, 6_442_450_944);
        assert_eq!(config.retention.retention_interval_secs, 3600);
    }

    #[test]
    fn retention_custom_values() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[retention]
max_age_days = 30
min_free_disk_bytes = 0
retention_interval_secs = 1800
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.retention.max_age_days, 30);
        assert_eq!(config.retention.min_free_disk_bytes, 0);
        assert_eq!(config.retention.retention_interval_secs, 1800);
    }

    #[test]
    fn retention_both_disabled() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[retention]
max_age_days = 0
min_free_disk_bytes = 0
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.retention.max_age_days, 0);
        assert_eq!(config.retention.min_free_disk_bytes, 0);
    }

    #[test]
    fn parse_byte_size_raw_number() {
        assert_eq!(parse_byte_size("131072").unwrap(), 131_072);
    }

    #[test]
    fn parse_byte_size_kilobytes() {
        assert_eq!(parse_byte_size("128K").unwrap(), 128 * 1024);
        assert_eq!(parse_byte_size("128KB").unwrap(), 128 * 1024);
        assert_eq!(parse_byte_size("128KiB").unwrap(), 128 * 1024);
    }

    #[test]
    fn parse_byte_size_megabytes() {
        assert_eq!(parse_byte_size("16M").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_byte_size("16MB").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_byte_size("100MiB").unwrap(), 100 * 1024 * 1024);
    }

    #[test]
    fn parse_byte_size_gigabytes() {
        assert_eq!(parse_byte_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("1GiB").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_byte_size_bare_bytes() {
        assert_eq!(parse_byte_size("4096B").unwrap(), 4096);
    }

    #[test]
    fn parse_byte_size_case_insensitive() {
        assert_eq!(parse_byte_size("16m").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_byte_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("128kb").unwrap(), 128 * 1024);
    }

    #[test]
    fn parse_byte_size_with_whitespace() {
        assert_eq!(parse_byte_size("  16M  ").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_byte_size("128 K").unwrap(), 128 * 1024);
    }

    #[test]
    fn parse_byte_size_invalid_suffix() {
        assert!(parse_byte_size("16X").is_err());
    }

    #[test]
    fn parse_byte_size_empty() {
        assert!(parse_byte_size("").is_err());
        assert!(parse_byte_size("   ").is_err());
    }

    #[test]
    fn byte_size_field_accepts_string() {
        let toml = r#"
[server]
max_request_body_bytes = "256K"
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.max_request_body_bytes, 256 * 1024);
    }

    #[test]
    fn byte_size_field_accepts_integer() {
        let toml = r#"
[server]
max_request_body_bytes = 262144
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.max_request_body_bytes, 262_144);
    }

    #[test]
    fn ingest_byte_size_string() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
max_body_bytes = "32M"
hot_buffer_max_bytes = "200M"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ingest.max_body_bytes, 32 * 1024 * 1024);
        assert_eq!(config.ingest.hot_buffer_max_bytes, 200 * 1024 * 1024);
    }

    #[test]
    fn retention_byte_size_string() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[retention]
min_free_disk_bytes = "2G"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.retention.min_free_disk_bytes, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn interval_defaults_when_omitted() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ingest.stats_interval_secs, 60);
        assert_eq!(config.ingest.telemetry_flush_interval_secs, 1);
    }

    #[test]
    fn interval_custom_values() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
stats_interval_secs = 30
telemetry_flush_interval_secs = 5
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ingest.stats_interval_secs, 30);
        assert_eq!(config.ingest.telemetry_flush_interval_secs, 5);
    }

    const TASK_PERIODS: [(&str, &str); 6] = [
        ("ingest", "stats_interval_secs"),
        ("ingest", "telemetry_flush_interval_secs"),
        ("retention", "retention_interval_secs"),
        ("scheduler", "poll_interval_secs"),
        ("server", "schema_cache_ttl_secs"),
        ("server", "monitor_refresh_ms"),
    ];

    fn task_period_doc(section: &str, key: &str, value: Option<u64>) -> String {
        let mut doc = String::new();
        for name in ["server", "data", "auth", "ingest", "retention", "scheduler"] {
            writeln!(doc, "[{name}]").unwrap();
            if name == "data" {
                doc.push_str("path = \"/data\"\n");
            }
            if name == section
                && let Some(value) = value
            {
                writeln!(doc, "{key} = {value}").unwrap();
            }
        }
        doc
    }

    fn task_period_value(config: &Config, key: &str) -> u64 {
        match key {
            "stats_interval_secs" => config.ingest.stats_interval_secs,
            "telemetry_flush_interval_secs" => config.ingest.telemetry_flush_interval_secs,
            "retention_interval_secs" => config.retention.retention_interval_secs,
            "poll_interval_secs" => config.scheduler.poll_interval_secs,
            "schema_cache_ttl_secs" => config.server.schema_cache_ttl_secs,
            "monitor_refresh_ms" => config.server.monitor_refresh_ms,
            other => panic!("not a task period: {other}"),
        }
    }

    #[test]
    fn zero_task_periods_are_refused() {
        for (section, key) in TASK_PERIODS {
            let err = Config::from_toml(&task_period_doc(section, key, Some(0))).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("config validation error: {section}.{key} must be > 0"),
            );
        }
    }

    #[test]
    fn task_periods_accept_defaults_and_one() {
        for (section, key) in TASK_PERIODS {
            let config = Config::from_toml(&task_period_doc(section, key, None)).unwrap();
            assert!(
                task_period_value(&config, key) > 0,
                "{section}.{key} default must be > 0"
            );

            let config = Config::from_toml(&task_period_doc(section, key, Some(1))).unwrap();
            assert_eq!(task_period_value(&config, key), 1, "{section}.{key}");
        }
    }

    #[test]
    fn task_period_rules_ignore_component_enablement() {
        for enabled in [true, false] {
            let doc = format!("[scheduler]\nenabled = {enabled}\npoll_interval_secs = 0\n");
            let err = Config::from_toml(&format!(
                "[server]\n[data]\npath = \"/data\"\n[auth]\n{doc}"
            ))
            .unwrap_err();
            assert_eq!(
                err.to_string(),
                "config validation error: scheduler.poll_interval_secs must be > 0",
                "scheduler.enabled = {enabled}"
            );
        }

        for telemetry in [true, false] {
            let doc = format!(
                "[server]\n[data]\npath = \"/data\"\n[auth]\n[ingest]\n\
                 internal_telemetry = {telemetry}\ntelemetry_flush_interval_secs = 0\n"
            );
            let err = Config::from_toml(&doc).unwrap_err();
            assert_eq!(
                err.to_string(),
                "config validation error: ingest.telemetry_flush_interval_secs must be > 0",
                "internal_telemetry = {telemetry}"
            );
        }

        let config = Config::from_toml(
            "[server]\ntls_reload_interval_secs = 0\n[data]\npath = \"/data\"\n\
             [auth]\naudit_interval_secs = 0\n",
        )
        .unwrap();
        assert_eq!(config.auth.audit_interval_secs, 0);
        assert_eq!(config.server.tls_reload_interval_secs, 0);
    }

    #[test]
    fn web_section_optional_preserves_backcompat() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.web.bind_addr.is_none());
        assert!(config.web.upstream_url.is_none());
        assert!(config.web.upstream_ca_path.is_none());
        assert!(config.web.upstream_connect_addr.is_none());
        assert!(config.web.cookie_secret_path.is_none());
        assert!(config.web.cookie_secret_env.is_none());
        assert!(config.web.session_ttl_secs.is_none());
        assert!(!config.web.allow_insecure_cookies);
        assert!(config.web.public_origins.is_empty());
    }

    #[test]
    fn auth_database_url_parses_from_toml() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
database_url = "postgres://fleet:fleet@localhost:5433/fleet"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.auth.database_url.as_deref(),
            Some("postgres://fleet:fleet@localhost:5433/fleet")
        );
    }

    #[test]
    fn auth_database_url_optional_in_toml() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.auth.database_url.is_none());
    }

    #[test]
    fn resolve_database_url_env_wins() {
        let url = AuthConfig::resolve_database_url_from(
            Some("postgres://env/db"),
            Some("postgres://toml/db"),
        )
        .unwrap();
        assert_eq!(url, "postgres://env/db");
    }

    #[test]
    fn resolve_database_url_falls_back_to_toml() {
        let url = AuthConfig::resolve_database_url_from(None, Some("postgres://toml/db")).unwrap();
        assert_eq!(url, "postgres://toml/db");

        let url =
            AuthConfig::resolve_database_url_from(Some(""), Some("postgres://toml/db")).unwrap();
        assert_eq!(url, "postgres://toml/db");
    }

    #[test]
    fn resolve_database_url_neither_is_an_error() {
        let err = AuthConfig::resolve_database_url_from(None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("database_url"), "got: {msg}");
        assert!(msg.contains("DATABASE_URL"), "got: {msg}");

        let err = AuthConfig::resolve_database_url_from(Some(""), Some("")).unwrap_err();
        assert!(err.to_string().contains("database_url"));
    }

    #[test]
    fn validation_accepts_fleet_database_url() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
database_url = "postgres://fleet:fleet@localhost:5433/fleet"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn web_public_origins_parse_as_written() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[web]
public_origins = ["https://trawl.example.com", "http://localhost:8090"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.web.public_origins,
            ["https://trawl.example.com", "http://localhost:8090"]
        );
    }

    #[test]
    fn storage_database_url_parses_from_toml() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[storage]
database_url = "postgres://trawl:trawl@localhost:5433/trawl"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.storage.database_url.as_deref(),
            Some("postgres://trawl:trawl@localhost:5433/trawl")
        );
    }

    #[test]
    fn storage_section_optional_in_toml() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.storage.database_url.is_none());
    }

    #[test]
    fn storage_resolve_env_wins() {
        let url = StorageConfig::resolve_database_url_from(
            Some("postgres://env/trawl"),
            Some("postgres://toml/trawl"),
        )
        .unwrap();
        assert_eq!(url, "postgres://env/trawl");
    }

    #[test]
    fn storage_resolve_falls_back_to_toml() {
        let url =
            StorageConfig::resolve_database_url_from(None, Some("postgres://toml/trawl")).unwrap();
        assert_eq!(url, "postgres://toml/trawl");

        let url = StorageConfig::resolve_database_url_from(Some(""), Some("postgres://toml/trawl"))
            .unwrap();
        assert_eq!(url, "postgres://toml/trawl");
    }

    #[test]
    fn resolve_names_the_winning_source() {
        use DatabaseUrlSource::{ConfigFile, Environment};
        for (env, file, expected) in [
            (
                Some("postgres://env/db"),
                Some("postgres://toml/db"),
                Environment,
            ),
            (Some("postgres://env/db"), None, Environment),
            (None, Some("postgres://toml/db"), ConfigFile),
            (Some(""), Some("postgres://toml/db"), ConfigFile),
        ] {
            let auth = AuthConfig::resolve_database_url_with_source_from(env, file).unwrap();
            let storage = StorageConfig::resolve_database_url_with_source_from(env, file).unwrap();
            assert_eq!(auth.1, expected, "{env:?} {file:?}");
            assert_eq!(auth, storage);
            assert_eq!(
                auth.0,
                AuthConfig::resolve_database_url_from(env, file).unwrap()
            );
        }
        assert!(StorageConfig::resolve_database_url_with_source_from(Some(""), None).is_err());
    }

    #[test]
    fn storage_resolve_neither_is_a_descriptive_error() {
        let err = StorageConfig::resolve_database_url_from(None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[storage]"), "got: {msg}");
        assert!(msg.contains("TRAWL_DATABASE_URL"), "got: {msg}");
    }

    #[test]
    fn storage_resolve_never_falls_back_to_auth_url() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
database_url = "postgres://fleet:fleet@localhost:5433/fleet"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.storage.database_url.is_none());
        if std::env::var("TRAWL_DATABASE_URL").is_err() {
            let err = config.storage.resolve_database_url().unwrap_err();
            assert!(err.to_string().contains("[storage]"), "got: {err}");
        }
    }

    #[test]
    fn http_addr_env_wins_over_config() {
        assert_eq!(
            ServerConfig::resolve_http_addr_from(Some("0.0.0.0:9999"), "127.0.0.1:5514"),
            "0.0.0.0:9999"
        );
    }

    #[test]
    fn http_addr_falls_back_to_config() {
        assert_eq!(
            ServerConfig::resolve_http_addr_from(None, "127.0.0.1:5514"),
            "127.0.0.1:5514"
        );
    }

    #[test]
    fn http_addr_empty_env_counts_as_unset() {
        assert_eq!(
            ServerConfig::resolve_http_addr_from(Some(""), "127.0.0.1:5514"),
            "127.0.0.1:5514"
        );
    }

    #[test]
    fn auth_resolve_ignores_bare_database_url() {
        let auth = AuthConfig {
            database_url: Some("postgres://toml/fleet".into()),
            ..AuthConfig::default()
        };
        if std::env::var("FLEET_DATABASE_URL").is_err() {
            assert_eq!(
                auth.resolve_database_url().unwrap(),
                "postgres://toml/fleet"
            );
        }
    }

    #[test]
    fn auth_resolve_error_names_fleet_database_url() {
        let err = AuthConfig::resolve_database_url_from(None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("FLEET_DATABASE_URL"), "got: {msg}");
    }

    #[test]
    fn web_section_populated() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[web]
bind_addr = "0.0.0.0:8090"
upstream_url = "https://localhost:5514"
upstream_ca_path = "/etc/trawl/upstream-ca.pem"
upstream_connect_addr = "127.0.0.1:5514"
cookie_secret_path = "/etc/trawl/web.key"
session_ttl_secs = 3600
allow_insecure_cookies = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.web.bind_addr.as_deref(), Some("0.0.0.0:8090"));
        assert_eq!(
            config.web.upstream_url.as_deref(),
            Some("https://localhost:5514")
        );
        assert_eq!(
            config.web.upstream_ca_path.as_deref(),
            Some(std::path::Path::new("/etc/trawl/upstream-ca.pem"))
        );
        assert_eq!(
            config.web.upstream_connect_addr.as_deref(),
            Some("127.0.0.1:5514")
        );
        assert_eq!(
            config.web.cookie_secret_path.as_deref(),
            Some(std::path::Path::new("/etc/trawl/web.key"))
        );
        assert_eq!(config.web.session_ttl_secs, Some(3600));
        assert!(config.web.allow_insecure_cookies);
        assert!(config.web.shared_domain.is_none());
    }

    #[test]
    fn web_shared_domain_parses_from_toml() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[web]
shared_domain = ".fleet.lab.ktle.net"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.web.shared_domain.as_deref(),
            Some(".fleet.lab.ktle.net")
        );
    }

    #[test]
    fn web_shared_domain_absent_is_none() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[web]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.web.shared_domain.is_none());
    }

    fn config_with_ingest(ingest: &str) -> Config {
        let toml = format!(
            r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
{ingest}
"#
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn ingest_env_zero_config_defaults() {
        let config = config_with_ingest("");
        assert_eq!(config.ingest.default_env, "prod");
        assert!(config.ingest.envs.is_empty());
        assert_eq!(config.ingest.effective_envs(), vec!["prod".to_string()]);
        assert!(config.ingest.trusted_relays.is_empty());
        config.validate().expect("zero-config ingest must validate");
    }

    #[test]
    fn ingest_envs_omitted_behaves_as_default_env() {
        let config = config_with_ingest(r#"default_env = "lab""#);
        assert_eq!(config.ingest.effective_envs(), vec!["lab".to_string()]);
        config.validate().expect("default_env alone must validate");
    }

    #[test]
    fn ingest_explicit_envs_parse_and_validate() {
        let config = config_with_ingest(
            r#"
default_env = "prod"
envs = ["prod", "lab"]
trusted_relays = ["10.0.4.0/24"]
"#,
        );
        assert_eq!(
            config.ingest.effective_envs(),
            vec!["prod".to_string(), "lab".to_string()]
        );
        assert_eq!(config.ingest.trusted_relays, vec!["10.0.4.0/24"]);
        config.validate().expect("explicit envs must validate");
    }

    #[test]
    fn validation_rejects_default_env_not_in_envs() {
        let config = config_with_ingest(
            r#"
default_env = "dev"
envs = ["prod", "lab"]
"#,
        );
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("default_env"), "got: {err}");
        assert!(err.contains("ingest.envs"), "got: {err}");
    }

    #[test]
    fn validation_rejects_env_charset_violations() {
        for bad in [
            "Prod",
            "pro.d",
            "pro/d",
            "pro d",
            "",
            "..",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", // 33 chars
        ] {
            let config = config_with_ingest(&format!(r#"envs = ["prod", "{bad}"]"#));
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains("env"),
                "env name {bad:?} must fail validation; got: {err}"
            );
        }
    }

    #[test]
    fn validation_rejects_bad_default_env_charset() {
        let config = config_with_ingest(r#"default_env = "Prod""#);
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("default_env"), "got: {err}");
    }

    #[test]
    fn validation_rejects_reserved_env_names() {
        for reserved in ["wal", "scheduled"] {
            let config = config_with_ingest(&format!(r#"envs = ["prod", "{reserved}"]"#));
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains("reserved"),
                "env name {reserved:?} must be rejected as reserved; got: {err}"
            );

            let config = config_with_ingest(&format!(r#"default_env = "{reserved}""#));
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains("reserved"),
                "default_env {reserved:?} must be rejected as reserved; got: {err}"
            );
        }
    }

    fn config_with_retention(retention: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!(
            r#"
[server]
[data]
path = "/data"
[auth]
[retention]
{retention}
"#
        ))
    }

    #[test]
    fn retention_env_overrides_parse_alongside_the_globals() {
        // The sub-tables go last: TOML puts every scalar after a table
        // header inside that table, so `min_free_disk_bytes` written below
        // `[retention.env.prod]` would be a key of the override.
        let config = config_with_retention(
            r#"
max_age_days = 90
min_free_disk_bytes = "1G"

[retention.env.prod]
max_age_days = 365

[retention.env.lab]
max_age_days = 7
"#,
        )
        .expect("per-env retention must parse and validate");

        assert_eq!(config.retention.max_age_days, 90);
        assert_eq!(config.retention.min_free_disk_bytes, 1024 * 1024 * 1024);
        assert_eq!(config.retention.env.len(), 2);
        assert_eq!(config.retention.env["prod"].max_age_days, 365);
        assert_eq!(config.retention.env["lab"].max_age_days, 7);
    }

    #[test]
    fn max_age_days_for_prefers_the_entry_then_the_global() {
        let config = config_with_retention(
            r"
max_age_days = 90

[retention.env.prod]
max_age_days = 365

[retention.env.scratch]
max_age_days = 0
",
        )
        .expect("per-env retention must parse and validate");

        assert_eq!(config.retention.max_age_days_for("prod"), 365);
        assert_eq!(
            config.retention.max_age_days_for("scratch"),
            0,
            "an explicit 0 is keep-forever for that env, not a fall-through"
        );
        assert_eq!(
            config.retention.max_age_days_for("staging"),
            90,
            "an env with no entry inherits the global"
        );
    }

    #[test]
    fn retention_env_table_without_max_age_days_is_a_load_error() {
        let err = config_with_retention(
            r"
max_age_days = 90

[retention.env.lab]
",
        )
        .expect_err("an empty override table must not load")
        .to_string();
        assert!(err.contains("max_age_days"), "got: {err}");
    }

    #[test]
    fn retention_env_rejects_an_unknown_key() {
        let err = config_with_retention(
            r#"
[retention.env.lab]
max_age_days = 7
min_free_disk_bytes = "1G"
"#,
        )
        .expect_err("disk pressure is install-wide; no per-env knob exists")
        .to_string();
        assert!(err.contains("min_free_disk_bytes"), "got: {err}");
    }

    #[test]
    fn retention_rejects_a_misspelled_sub_table_name() {
        let err = config_with_retention(
            r"
max_age_days = 7

[retention.evn.prod]
max_age_days = 365
",
        )
        .expect_err("a misspelled sub-table must not load")
        .to_string();
        assert!(err.contains("evn"), "got: {err}");
    }

    #[test]
    fn retention_rejects_a_misspelled_scalar() {
        let err = config_with_retention(
            r"
max_age_dayz = 7
",
        )
        .expect_err("a misspelled scalar must not load")
        .to_string();
        assert!(err.contains("max_age_dayz"), "got: {err}");
    }

    #[test]
    fn retention_env_keys_are_held_to_the_env_charset() {
        let err = config_with_retention(
            r"
[retention.env.Prod]
max_age_days = 30
",
        )
        .expect_err("an env key is a directory name under the data root")
        .to_string();
        assert!(err.contains("retention.env"), "got: {err}");
        assert!(err.contains("Prod"), "got: {err}");
    }

    #[test]
    fn retention_env_keys_reject_reserved_names() {
        for reserved in ["wal", "scheduled"] {
            let err = config_with_retention(&format!(
                r"
[retention.env.{reserved}]
max_age_days = 30
"
            ))
            .expect_err("wal/ and scheduled/ are not envs")
            .to_string();
            assert!(
                err.contains("reserved"),
                "retention.env key {reserved:?} must be rejected as reserved; got: {err}"
            );
        }
    }

    #[test]
    fn retention_env_refuses_what_the_chart_passes_through_typed() {
        for (label, table) in [
            ("float value", "[retention.env.prod]\nmax_age_days = 1.9"),
            (
                "string value",
                "[retention.env.prod]\nmax_age_days = \"1.9\"",
            ),
            (
                "typo value",
                "[retention.env.prod]\nmax_age_days = \"not-a-number\"",
            ),
            ("negative value", "[retention.env.prod]\nmax_age_days = -1"),
            (
                "quoted bad key",
                "[retention.env.\"prod] #\"]\nmax_age_days = 365",
            ),
        ] {
            let err = config_with_retention(&format!("max_age_days = 90\n{table}\n"))
                .expect_err(label)
                .to_string();
            assert!(
                !err.is_empty(),
                "{label}: must name the refusal; got: {err}"
            );
        }
        let config = config_with_retention(
            "max_age_days = 90\n[retention.env.\"prod\"]\nmax_age_days = 365\n",
        )
        .expect("a quoted valid key loads");
        assert_eq!(config.retention.max_age_days_for("prod"), 365);
    }

    #[test]
    fn env_name_charset_helper() {
        assert!(is_valid_env_name("prod"));
        assert!(is_valid_env_name("lab-2"));
        assert!(is_valid_env_name("a_b_c"));
        assert!(is_valid_env_name("x"));
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("Prod"));
        assert!(!is_valid_env_name("pro.d"));
        assert!(!is_valid_env_name("pro/d"));
        assert!(!is_valid_env_name("pro\\d"));
        assert!(!is_valid_env_name(&"a".repeat(33)));
        assert!(is_valid_env_name(&"a".repeat(32)));
    }

    #[test]
    fn service_name_charset_helper() {
        assert!(is_valid_service_name("nginx"));
        assert!(is_valid_service_name("my-app_v2.0"));
        assert!(is_valid_service_name("UniFi"));
        assert!(!is_valid_service_name(""));
        assert!(!is_valid_service_name("Living Room AP"));
        assert!(!is_valid_service_name("../../escaped"));
        assert!(!is_valid_service_name(".hidden"));
        assert!(!is_valid_service_name(".."));
        assert!(!is_valid_service_name(
            &"a".repeat(MAX_SERVICE_NAME_LEN + 1)
        ));
        assert!(is_valid_service_name(&"a".repeat(MAX_SERVICE_NAME_LEN)));
    }

    #[test]
    fn syslog_service_config_is_validated_at_load() {
        let with_syslog = |syslog: &str| {
            Config::from_toml(&format!(
                r#"
[server]
[data]
path = "/data"
[auth]
[syslog]
enabled = true
{syslog}
"#
            ))
        };

        let err = with_syslog(r#"default_service = "../../escaped""#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("syslog.default_service"), "got: {err}");

        let err = with_syslog(
            r#"
[syslog.source_service_map]
"192.0.2.250" = "Living Room AP"
"#,
        )
        .unwrap_err();
        let shown = err.to_string();
        assert!(shown.contains("syslog.source_service_map"), "got: {shown}");
        for text in [shown, format!("{err:?}")] {
            assert!(
                !text.contains("192.0.2.250") && !text.contains("Living Room"),
                "the key and the value stay out of the error: {text}"
            );
        }

        with_syslog(
            r#"
default_service = "syslog"

[syslog.source_service_map]
"192.168.1.1" = "unifi-gateway"
"#,
        )
        .expect("valid syslog service names must load");
    }

    #[test]
    fn syslog_timezone_keys_parse_and_default_to_unset() {
        let config = Config::from_toml("[server]\n[data]\npath = \"/data\"\n").unwrap();
        assert_eq!(config.syslog.default_timezone, None);
        assert!(config.syslog.sender_timezones.is_empty());

        let config = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[syslog]
default_timezone = "America/Chicago"

[syslog.sender_timezones]
"192.0.2.10" = "Europe/Warsaw"
"2001:db8::1" = "+05:30"
"::ffff:192.0.2.11" = "UTC"
"#,
        )
        .unwrap();
        assert_eq!(
            config.syslog.default_timezone.as_deref(),
            Some("America/Chicago")
        );
        let zones = &config.syslog.sender_timezones;
        assert_eq!(zones.len(), 3);
        assert_eq!(zones["192.0.2.10"], "Europe/Warsaw");
        assert_eq!(zones["2001:db8::1"], "+05:30");
        assert_eq!(zones["::ffff:192.0.2.11"], "UTC");

        let err = Config::from_toml(
            "[server]\n[data]\npath = \"/data\"\n[syslog]\ndefault_timezon = \"UTC\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("syslog.default_timezon"), "got: {err}");
        assert!(err.contains("unknown setting"), "got: {err}");
    }

    #[test]
    fn syslog_peer_keyed_errors_name_the_map_not_the_key() {
        for map in ["sender_timezones", "source_service_map"] {
            let err = Config::from_toml(&format!(
                "[server]\n[data]\npath = \"/data\"\n[syslog.{map}]\n\"198.51.100.77\" = 5\n"
            ))
            .unwrap_err();
            let shown = err.to_string();
            assert!(
                shown.contains(&format!("at syslog.{map} in")),
                "{map}: {shown}"
            );
            for text in [shown, format!("{err:?}")] {
                assert!(!text.contains("198.51.100.77"), "{map}: {text}");
            }
        }
        assert_eq!(
            without_peer_key("syslog.sender_timezones_x".to_owned()),
            "syslog.sender_timezones_x"
        );
        assert_eq!(
            without_peer_key("syslog.sender_timezones.10.0.0.1".to_owned()),
            "syslog.sender_timezones"
        );
    }

    #[test]
    fn syslog_consecutive_send_failures_limit_is_refused() {
        let err = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[syslog]
consecutive_send_failures_limit = 100
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("consecutive_send_failures_limit"),
            "got: {err}"
        );
    }

    #[test]
    fn derivation_sources_default_to_the_packaged_lists() {
        let config = config_with_ingest("");
        let names = |specs: &[DerivationSourceSpec]| -> Vec<String> {
            specs.iter().map(|s| s.field().to_owned()).collect()
        };
        assert_eq!(names(&config.ingest.severity_from), DEFAULT_SEVERITY_FROM);
        assert_eq!(names(&config.ingest.time_from), DEFAULT_TIME_FROM);
        assert!(
            config
                .ingest
                .severity_from
                .iter()
                .all(|s| s.dialect().is_none())
        );
        assert_eq!(
            config.ingest.severity_from,
            IngestConfig::default().severity_from
        );
        assert_eq!(config.ingest.time_from, IngestConfig::default().time_from);
    }

    #[test]
    fn derivation_sources_accept_both_spellings_in_one_list() {
        let config = config_with_ingest(
            r#"
severity_from = ["level", { field = "syslog_severity", dialect = "syslog" }, { field = "sev" }]
time_from = ["_time"]
"#,
        );
        assert_eq!(
            config.ingest.severity_from,
            vec![
                DerivationSourceSpec::Bare("level".into()),
                DerivationSourceSpec::Typed {
                    field: "syslog_severity".into(),
                    dialect: Some("syslog".into()),
                },
                DerivationSourceSpec::Typed {
                    field: "sev".into(),
                    dialect: None,
                },
            ]
        );
        assert_eq!(
            config.ingest.time_from,
            vec![DerivationSourceSpec::Bare("_time".into())]
        );
    }

    #[test]
    fn derivation_source_empty_list_parses() {
        // Empty `severity_from` is legal ("derive nothing"); empty
        // `time_from` is not — but that is a semantic rule the server
        // enforces, so both must survive deserialization.
        let config = config_with_ingest("severity_from = []\ntime_from = []");
        assert!(config.ingest.severity_from.is_empty());
        assert!(config.ingest.time_from.is_empty());
    }

    #[test]
    fn derivation_source_typo_in_a_typed_entry_is_an_error() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
severity_from = [{ field = "syslog_severity", dialct = "syslog" }]
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err().to_string();
        assert!(err.contains("severity_from"), "got: {err}");
    }

    #[test]
    fn derivation_source_entry_needs_a_field_key() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
time_from = [{ dialect = "otel" }]
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err().to_string();
        assert!(err.contains("time_from"), "got: {err}");
    }

    #[test]
    fn derivation_source_rejects_a_non_string_entry() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
severity_from = [7]
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    fn query_scratch_config(server: &str, ingest: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!(
            "[server]\n{server}\n[data]\npath = \"/var/lib/trawl/data\"\n[ingest]\n{ingest}\n"
        ))
    }

    #[test]
    fn query_scratch_defaults_parse() {
        let config = query_scratch_config("", "").unwrap();
        assert_eq!(config.server.query_scratch_dir, None);
        assert_eq!(
            config.query_scratch_dir(),
            Path::new("/var/lib/trawl/data.query-scratch")
        );
        assert_eq!(config.server.query_spill_max_bytes.get(), 4_294_967_296);
        assert_eq!(config.server.query_memory_limit.get(), 2_147_483_648);
        assert_eq!(config.server.query_threads.get(), 4);
        assert_eq!(
            config.server.max_concurrent_queries,
            default_max_concurrent_queries()
        );
        assert_eq!(config.retention.min_free_disk_bytes, 6_442_450_944);
        assert_eq!(config.ingest.compaction_memory_limit, "1GiB");
    }

    #[test]
    fn query_scratch_defaults_cap_query_permits_at_eight() {
        assert_eq!(default_query_permits(Some(1)), 1);
        assert_eq!(default_query_permits(Some(4)), 4);
        assert_eq!(default_query_permits(Some(8)), 8);
        assert_eq!(default_query_permits(Some(64)), 8);
        assert_eq!(default_query_permits(None), 4);
    }

    #[test]
    fn query_scratch_spill_budget_and_disk_floor_are_independent() {
        for (spill, floor) in [
            ("\"8G\"", "\"1G\""),
            ("\"4G\"", "0"),
            ("\"1M\"", "\"100G\""),
        ] {
            let config = Config::from_toml(&format!(
                "[server]\nquery_spill_max_bytes = {spill}\n[data]\npath = \"/data\"\n\
                 [retention]\nmin_free_disk_bytes = {floor}\n"
            ))
            .unwrap_or_else(|e| panic!("spill={spill} floor={floor}: {e}"));
            assert_eq!(
                config.server.query_spill_max_bytes.get(),
                parse_byte_size(spill.trim_matches('"')).unwrap()
            );
            assert_eq!(
                config.retention.min_free_disk_bytes,
                parse_byte_size(floor.trim_matches('"')).unwrap()
            );
        }
        let config = Config::from_toml(
            "[server]\n[data]\npath = \"/data\"\n[retention]\nmin_free_disk_bytes = 0\n",
        )
        .unwrap();
        assert_eq!(config.server.query_spill_max_bytes.get(), 4_294_967_296);
        let config = Config::from_toml(
            "[server]\nquery_spill_max_bytes = \"64G\"\n[data]\npath = \"/data\"\n",
        )
        .unwrap();
        assert_eq!(config.retention.min_free_disk_bytes, 6_442_450_944);
    }

    #[test]
    fn query_scratch_keys_parse_explicit_values() {
        let config = query_scratch_config(
            "query_scratch_dir = \"/srv/scratch\"\nquery_spill_max_bytes = \"1G\"\n\
             query_memory_limit = 1048576\nquery_threads = 8",
            "",
        )
        .unwrap();
        assert_eq!(config.query_scratch_dir(), Path::new("/srv/scratch"));
        assert_eq!(
            config.server.query_spill_max_bytes.get(),
            1024 * 1024 * 1024
        );
        assert_eq!(config.server.query_memory_limit.get(), 1024 * 1024);
        assert_eq!(config.server.query_threads.get(), 8);
    }

    #[test]
    fn query_scratch_default_follows_a_trailing_slash_data_root() {
        let config =
            Config::from_toml("[server]\n[data]\npath = \"/var/lib/trawl/data/\"\n").unwrap();
        assert_eq!(
            config.query_scratch_dir(),
            Path::new("/var/lib/trawl/data.query-scratch")
        );
    }

    #[test]
    fn query_scratch_refuses_zero_budgets() {
        for (key, value) in [
            ("query_spill_max_bytes", "0"),
            ("query_spill_max_bytes", "\"0M\""),
            ("query_memory_limit", "0"),
            ("query_memory_limit", "\"0B\""),
            ("query_threads", "0"),
        ] {
            let err = query_scratch_config(&format!("{key} = {value}"), "")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("server.{key}")),
                "{key}={value}: {err}"
            );
            assert!(
                err.contains("must be greater than zero"),
                "{key}={value}: {err}"
            );
        }
    }

    #[test]
    fn query_scratch_refuses_overflowing_budgets() {
        for (key, value) in [
            ("query_spill_max_bytes", "\"99999999999T\""),
            ("query_spill_max_bytes", "\"18446744073709551616\""),
            ("query_memory_limit", "\"16777216.5T\""),
            ("query_memory_limit", "\"99999999999999999999999.0\""),
        ] {
            let err = query_scratch_config(&format!("{key} = {value}"), "")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("server.{key}")),
                "{key}={value}: {err}"
            );
            assert!(
                err.contains("byte size is too large"),
                "{key}={value}: {err}"
            );
        }
        let err = query_scratch_config("query_threads = 4294967296", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("server.query_threads"), "{err}");
    }

    #[test]
    fn query_scratch_byte_parse_keeps_the_largest_values_it_can_hold() {
        assert_eq!(parse_byte_size("16777215T").unwrap(), 16_777_215 << 40);
        assert_eq!(parse_byte_size("18446744073709551615").unwrap(), u64::MAX);
        assert_eq!(
            parse_byte_size("16777216T").unwrap_err(),
            BYTE_SIZE_OVERFLOW
        );
    }

    #[test]
    fn query_scratch_refuses_overlap_with_data_wal_and_repin_roots() {
        let wal = "wal_dir = \"/srv/wal\"";
        for (scratch, ingest, relation, root) in [
            ("/var/lib/trawl/data", "", "equal to", "data root"),
            ("/var/lib/trawl/data/", "", "equal to", "data root"),
            ("/var/lib/trawl/data/./scratch", "", "inside", "data root"),
            ("/var/lib/trawl", "", "above", "data root"),
            ("/", "", "above", "data root"),
            ("/srv/wal", wal, "equal to", "WAL directory"),
            ("/srv/wal/scratch", wal, "inside", "WAL directory"),
            ("/srv", wal, "above", "WAL directory"),
            (
                "/var/lib/trawl/data.repin-next",
                "",
                "equal to",
                "repin shadow root",
            ),
            (
                "/var/lib/trawl/data.repin-next/x",
                "",
                "inside",
                "repin shadow root",
            ),
            (
                "/var/lib/trawl/data.repin-aside",
                "",
                "equal to",
                "repin aside root",
            ),
            (
                "/var/lib/trawl/data.repin-aside/x",
                "",
                "inside",
                "repin aside root",
            ),
        ] {
            let err = query_scratch_config(&format!("query_scratch_dir = \"{scratch}\""), ingest)
                .unwrap_err()
                .to_string();
            assert!(err.contains("server.query_scratch_dir"), "{scratch}: {err}");
            assert!(
                err.contains(&format!("must not be {relation} the {root}")),
                "{scratch}: {err}"
            );
            assert!(!err.contains("/var") && !err.contains("/srv"), "{err}");
        }
    }

    #[test]
    fn query_scratch_accepts_siblings_and_lookalike_prefixes() {
        for scratch in [
            "/var/lib/trawl/data.query-scratch",
            "/var/lib/trawl/data-scratch",
            "/var/lib/trawl/dat",
            "/srv/scratch",
        ] {
            let config = query_scratch_config(
                &format!("query_scratch_dir = \"{scratch}\""),
                "wal_dir = \"/srv/wal\"",
            )
            .unwrap_or_else(|e| panic!("{scratch}: {e}"));
            assert_eq!(config.query_scratch_dir(), Path::new(scratch));
        }
    }

    #[test]
    fn query_scratch_refuses_an_empty_path() {
        let err = query_scratch_config("query_scratch_dir = \"\"", "")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("server.query_scratch_dir cannot be empty"),
            "{err}"
        );
    }

    #[test]
    fn query_scratch_refuses_glob_metacharacters() {
        for metacharacter in GLOB_METACHARACTERS {
            let scratch = format!("/srv/private-secret{metacharacter}scratch");
            let err = query_scratch_config(&format!("query_scratch_dir = '{scratch}'"), "")
                .unwrap_err()
                .to_string();
            assert_eq!(
                err,
                format!(
                    "config validation error: server.query_scratch_dir must be a directory \
                     path without glob metacharacters; found '{metacharacter}'"
                )
            );
        }
        for metacharacter in GLOB_METACHARACTERS {
            let err = Config::from_toml(&format!(
                "[server]\n[data]\npath = '/srv/data{metacharacter}x'\n"
            ))
            .unwrap_err()
            .to_string();
            assert!(
                err.contains(&format!(
                    "without glob metacharacters; found '{metacharacter}'"
                )),
                "{err}"
            );
        }
    }

    #[test]
    fn query_scratch_refuses_relative_paths_and_parent_components() {
        for (scratch, reason) in [
            ("scratch", "must be an absolute path"),
            ("./scratch", "must be an absolute path"),
            ("../trawl/data/prod", "must be an absolute path"),
            (
                "/var/lib/trawl/x/../data/prod",
                "must not contain a '..' component",
            ),
            ("/srv/scratch/..", "must not contain a '..' component"),
        ] {
            let err = query_scratch_config(&format!("query_scratch_dir = \"{scratch}\""), "")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("server.query_scratch_dir {reason}")),
                "{scratch}: {err}"
            );
            assert!(!err.contains("/var") && !err.contains("/srv"), "{err}");
        }
        let config = Config::from_toml("[server]\n[data]\npath = \"relative/data\"\n").unwrap();
        assert_eq!(
            config.query_scratch_dir(),
            Path::new("relative/data.query-scratch")
        );
    }

    #[test]
    fn owned_roots_refuse_parent_components() {
        for (key, data, wal) in [
            ("data.path", "/srv/trawl/missing/../scratch", "/srv/wal"),
            ("data.path", "/srv/trawl/data/..", "/srv/wal"),
            ("data.path", "../trawl/data", "/srv/wal"),
            (
                "ingest.wal_dir",
                "/srv/trawl/data",
                "/srv/trawl/missing/../scratch",
            ),
            ("ingest.wal_dir", "/srv/trawl/data", "../wal"),
        ] {
            let document = format!(
                "[server]\nquery_scratch_dir = \"/srv/trawl/scratch\"\n\
                 [data]\npath = \"{data}\"\n[ingest]\nwal_dir = \"{wal}\"\n"
            );
            let err = Config::from_toml(&document)
                .expect_err(&document)
                .to_string();
            assert_eq!(
                err,
                format!("config validation error: {key} must not contain a '..' component"),
                "{document}"
            );
            assert!(!err.contains("/srv") && !err.contains("trawl/"), "{err}");
        }
        let config = Config::from_toml(
            "[server]\n[data]\npath = \"/srv/trawl/./data\"\n[ingest]\nwal_dir = \"/srv/./wal\"\n",
        )
        .unwrap();
        assert_eq!(config.wal_dir(), Path::new("/srv/./wal"));
    }

    fn capture_config(server: &str, capture: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!(
            "[server]\n{server}\n[data]\npath = \"/var/lib/trawl/data\"\n[capture]\n{capture}\n"
        ))
    }

    #[test]
    fn link_origin_is_unset_by_default() {
        let config = capture_config("", "").unwrap();
        assert_eq!(config.server.link_origin, None);
    }

    #[test]
    fn link_origin_accepts_origins_and_stores_the_normalized_form() {
        for (value, stored) in [
            ("https://trawl.example.com", "https://trawl.example.com"),
            (
                "https://trawl.example.com:8443",
                "https://trawl.example.com:8443",
            ),
            ("https://trawl.example.com/", "https://trawl.example.com"),
            ("HTTPS://Trawl.Example.COM", "https://trawl.example.com"),
            ("https://trawl.example.com:443", "https://trawl.example.com"),
            ("http://localhost:8081", "http://localhost:8081"),
            ("http://127.0.0.1:8081", "http://127.0.0.1:8081"),
            ("http://127.1.2.3", "http://127.1.2.3"),
            ("http://[::1]:8081", "http://[::1]:8081"),
            ("http://[0:0:0:0:0:0:0:1]/", "http://[::1]"),
            ("https://[2001:db8::1]:8443", "https://[2001:db8::1]:8443"),
            ("https://192.0.2.7", "https://192.0.2.7"),
        ] {
            let config = capture_config(&format!("link_origin = \"{value}\""), "")
                .unwrap_or_else(|e| panic!("{value} must be accepted: {e}"));
            assert_eq!(
                config.server.link_origin.as_deref(),
                Some(stored),
                "{value}"
            );
        }
    }

    #[test]
    fn link_origin_refuses_what_is_not_an_origin() {
        for (value, rule) in [
            (
                "http://trawl.example.com",
                "must use https; http is allowed only on a loopback host",
            ),
            (
                "http://192.0.2.7:8081",
                "must use https; http is allowed only on a loopback host",
            ),
            (
                "http://[2001:db8::1]",
                "must use https; http is allowed only on a loopback host",
            ),
            ("ftp://trawl.example.com", "must use the https scheme"),
            ("trawl.example.com", "must be an origin"),
            ("https://trawl.example.com/search", "must not carry a path"),
            ("https://trawl.example.com//", "must not carry a path"),
            ("https://trawl.example.com/?q=1", "must not carry a query"),
            ("https://trawl.example.com?q=1", "must not carry a query"),
            (
                "https://trawl.example.com/#top",
                "must not carry a fragment",
            ),
            ("https://user@trawl.example.com", "must not carry userinfo"),
            (
                "https://user:pw@trawl.example.com",
                "must not carry userinfo",
            ),
            ("https://", "must name a host"),
            ("https://:8443", "must name a host"),
            ("https:///search", "must not carry a path"),
            (
                "https://trawl.example.com:",
                "must use a port between 1 and 65535",
            ),
            (
                "https://trawl.example.com:0",
                "must use a port between 1 and 65535",
            ),
            (
                "https://trawl.example.com:65536",
                "must use a port between 1 and 65535",
            ),
            (
                "https://trawl.example.com:+80",
                "must use a port between 1 and 65535",
            ),
            ("https://trawl_example.com", "must name a host of ASCII"),
            ("https://-trawl.example.com", "must name a host of ASCII"),
            ("https://trawl..example.com", "must name a host of ASCII"),
            ("http://[::1", "must close the bracketed IPv6 host"),
            (
                "http://[::1]x",
                "must follow the host with an optional :port",
            ),
            ("https://[not-an-address]", "must name a valid IPv6 host"),
            (" https://trawl.example.com", "must not contain whitespace"),
        ] {
            let err = capture_config(&format!("link_origin = \"{value}\""), "")
                .expect_err(value)
                .to_string();
            assert!(
                err.contains(&format!("server.link_origin {rule}")),
                "{value}: {err}"
            );
            assert!(
                !err.contains(value.trim()),
                "the message echoed the value: {err}"
            );
        }
    }

    #[test]
    fn link_origin_accepts_a_numeric_host_only_as_canonical_dotted_decimal() {
        for (value, stored) in [
            ("http://127.0.0.1:8081", "http://127.0.0.1:8081"),
            ("https://10.0.0.5:443", "https://10.0.0.5"),
            ("https://trawl.example", "https://trawl.example"),
            // A digit-leading label is a name unless it is the last label.
            ("https://1password.example", "https://1password.example"),
            ("https://trawl.1password", "https://trawl.1password"),
        ] {
            let config = capture_config(&format!("link_origin = \"{value}\""), "")
                .unwrap_or_else(|e| panic!("{value} must be accepted: {e}"));
            assert_eq!(
                config.server.link_origin.as_deref(),
                Some(stored),
                "{value}"
            );
        }
    }

    #[test]
    fn link_origin_refuses_every_other_numeric_terminal_host() {
        let rule = "must name an IPv4 host in dotted-decimal form (four labels, 0-255, no leading \
                    zeros) or a DNS name";
        for value in [
            // A 0x-prefixed final label is an IPv4 number to a browser.
            "https://0x100000000",
            "https://trawl.0x1",
            "https://0x7f000001",
            "https://trawl.0X1",
            "http://0x7f.0.0.1",
            "https://127.1",
            "https://2130706433",
            "https://127.000.0.1",
            "https://1.2.3",
            "https://1.2.3.4.5",
            "https://256.0.0.1",
            "https://trawl.1",
        ] {
            let err = capture_config(&format!("link_origin = \"{value}\""), "")
                .expect_err(value)
                .to_string();
            assert!(
                err.contains(&format!("server.link_origin {rule}")),
                "{value}: {err}"
            );
            assert!(
                !err.contains(value.split_once("://").unwrap().1),
                "the message echoed the value: {err}"
            );
        }
    }

    #[test]
    fn results_dir_defaults_to_the_data_root_sibling() {
        let config = capture_config("", "").unwrap();
        assert_eq!(config.server.results_dir, None);
        assert_eq!(
            config.results_dir(),
            Path::new("/var/lib/trawl/data.results")
        );
        assert_eq!(RESULTS_SUFFIX, ".results");
    }

    #[test]
    fn results_dir_explicit_value_normalises_the_tilde() {
        let config = capture_config("results_dir = \"/srv/results\"", "").unwrap();
        assert_eq!(config.results_dir(), Path::new("/srv/results"));

        let home = std::env::var("HOME").expect("HOME is set");
        let config = capture_config("results_dir = \"~/results\"", "").unwrap();
        assert_eq!(config.results_dir(), Path::new(&home).join("results"));
    }

    #[test]
    fn results_dir_follows_the_scratch_directory_rules() {
        for (value, reason) in [
            ("\"\"", "cannot be empty"),
            ("\"results\"", "must be an absolute path"),
            ("\"/srv/../results\"", "must not contain a '..' component"),
            (
                "'/srv/res*'",
                "must be a directory path without glob metacharacters",
            ),
            (
                "\"/var/lib/trawl/data\"",
                "must not be equal to the data root",
            ),
            (
                "\"/var/lib/trawl/data/results\"",
                "must not be inside the data root",
            ),
            ("\"/var/lib/trawl\"", "must not be above the data root"),
            (
                "\"/var/lib/trawl/data.repin-next\"",
                "must not be equal to the repin shadow root",
            ),
        ] {
            let err = capture_config(&format!("results_dir = {value}"), "")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("server.results_dir {reason}")),
                "{value}: {err}"
            );
        }
    }

    #[test]
    fn results_dir_refuses_overlap_with_the_query_scratch_directory() {
        let scratch = "query_scratch_dir = \"/srv/scratch\"";
        for (results, relation) in [
            ("/srv/scratch", "equal to"),
            ("/srv/scratch/", "equal to"),
            ("/srv/scratch/results", "inside"),
            ("/srv", "above"),
        ] {
            let err = capture_config(&format!("{scratch}\nresults_dir = \"{results}\""), "")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!(
                    "server.results_dir must not be {relation} the query scratch directory"
                )),
                "{results}: {err}"
            );
        }
        let err = capture_config(
            "query_scratch_dir = \"/srv/results/scratch\"\nresults_dir = \"/srv/results\"",
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("server.results_dir must not be above the query scratch directory"),
            "{err}"
        );
        let err = capture_config("query_scratch_dir = \"/var/lib/trawl/data.results\"", "")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("server.results_dir must not be equal to the query scratch directory"),
            "{err}"
        );
        let config = capture_config(
            "query_scratch_dir = \"/srv/scratch\"\nresults_dir = \"/srv/scratch-results\"",
            "",
        )
        .unwrap();
        assert_eq!(config.results_dir(), Path::new("/srv/scratch-results"));
    }

    #[test]
    fn capture_defaults_apply_when_the_table_is_absent() {
        let config =
            Config::from_toml("[server]\n[data]\npath = \"/var/lib/trawl/data\"\n").unwrap();
        let capture = &config.capture;
        assert_eq!(capture.max_rows, 5_000);
        assert_eq!(capture.max_wire_bytes, 4 << 20);
        assert_eq!(capture.max_file_bytes, 8 << 20);
        assert_eq!(capture.ttl_secs, 900);
        assert_eq!(capture.default_window_secs, 900);
        assert_eq!(capture.max_span_secs, 604_800);
        assert_eq!(capture.deadline_secs, 15);
        assert_eq!(capture.source_max_bytes, 1 << 30);
        assert_eq!(capture.per_key_results, 64);
        assert_eq!(capture.per_key_bytes, 64 << 20);
        assert_eq!(capture.installation_results, 512);
        assert_eq!(capture.installation_bytes, 512 << 20);
        assert_eq!(capture.per_key_running, 1);
        assert_eq!(capture.installation_running, 2);
        assert_eq!(capture.queue_depth, 4);
        assert_eq!(capture.page_max_rows, 200);
        assert_eq!(capture.tombstone_secs, 86_400);
        let empty = capture_config("", "").unwrap();
        let default = CaptureConfig::default();
        assert_eq!(empty.capture.max_rows, default.max_rows);
        assert_eq!(empty.capture.tombstone_secs, default.tombstone_secs);
        assert_eq!(empty.capture.installation_bytes, default.installation_bytes);
    }

    #[test]
    fn capture_full_override_round_trips() {
        let config = capture_config(
            "",
            "max_rows = 1000\nmax_wire_bytes = \"1M\"\nmax_file_bytes = \"2M\"\n\
             ttl_secs = 60\ndefault_window_secs = 120\nmax_span_secs = 3600\n\
             deadline_secs = 5\nsource_max_bytes = \"256M\"\nper_key_results = 8\n\
             per_key_bytes = \"16M\"\ninstallation_results = 32\n\
             installation_bytes = \"128M\"\nper_key_running = 2\n\
             installation_running = 3\nqueue_depth = 6\npage_max_rows = 100\n\
             tombstone_secs = 7200",
        )
        .unwrap();
        let capture = &config.capture;
        assert_eq!(capture.max_rows, 1000);
        assert_eq!(capture.max_wire_bytes, 1 << 20);
        assert_eq!(capture.max_file_bytes, 2 << 20);
        assert_eq!(capture.ttl_secs, 60);
        assert_eq!(capture.default_window_secs, 120);
        assert_eq!(capture.max_span_secs, 3600);
        assert_eq!(capture.deadline_secs, 5);
        assert_eq!(capture.source_max_bytes, 256 << 20);
        assert_eq!(capture.per_key_results, 8);
        assert_eq!(capture.per_key_bytes, 16 << 20);
        assert_eq!(capture.installation_results, 32);
        assert_eq!(capture.installation_bytes, 128 << 20);
        assert_eq!(capture.per_key_running, 2);
        assert_eq!(capture.installation_running, 3);
        assert_eq!(capture.queue_depth, 6);
        assert_eq!(capture.page_max_rows, 100);
        assert_eq!(capture.tombstone_secs, 7200);
    }

    #[test]
    fn capture_refuses_unknown_keys() {
        assert!(capture_config("", "no_such_limit = 1").is_err());
    }

    #[test]
    fn capture_refuses_a_zero_value_naming_the_key() {
        for key in [
            "max_rows",
            "max_wire_bytes",
            "max_file_bytes",
            "ttl_secs",
            "default_window_secs",
            "max_span_secs",
            "deadline_secs",
            "source_max_bytes",
            "per_key_results",
            "per_key_bytes",
            "installation_results",
            "installation_bytes",
            "per_key_running",
            "installation_running",
            "queue_depth",
            "page_max_rows",
            "tombstone_secs",
        ] {
            let err = capture_config("", &format!("{key} = 0"))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("capture.{key} must be > 0")),
                "{key}: {err}"
            );
        }
        let err = capture_config("", "max_file_bytes = \"0M\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("capture.max_file_bytes"), "{err}");
    }

    #[test]
    fn capture_refuses_a_per_key_limit_above_its_installation_limit() {
        for (capture, small, large) in [
            (
                "per_key_results = 513",
                "per_key_results",
                "installation_results",
            ),
            (
                "per_key_bytes = \"513M\"",
                "per_key_bytes",
                "installation_bytes",
            ),
            (
                "per_key_running = 3",
                "per_key_running",
                "installation_running",
            ),
            ("page_max_rows = 5001", "page_max_rows", "max_rows"),
            (
                "default_window_secs = 604801",
                "default_window_secs",
                "max_span_secs",
            ),
            (
                "max_wire_bytes = \"9M\"",
                "max_wire_bytes",
                "max_file_bytes",
            ),
        ] {
            let err = capture_config("", capture).unwrap_err().to_string();
            assert!(
                err.contains(&format!("capture.{small} must be <= capture.{large}")),
                "{capture}: {err}"
            );
        }
        capture_config("", "per_key_results = 512\nper_key_running = 2").unwrap();
    }

    #[test]
    fn capture_refuses_a_wire_limit_below_the_cell_read_minimum() {
        let err = capture_config("", "max_wire_bytes = \"3\"")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("capture.max_wire_bytes is 3, below the minimum of 4 bytes"),
            "{err}"
        );
        let config = capture_config("", "max_wire_bytes = \"4\"").unwrap();
        assert_eq!(config.capture.max_wire_bytes, CAPTURE_MAX_WIRE_BYTES_MIN);
    }

    fn investigation_config(capture: &str, investigation: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!(
            "[server]\n[data]\npath = \"/var/lib/trawl/data\"\n[capture]\n{capture}\n\
             [investigation]\n{investigation}\n"
        ))
    }

    #[test]
    fn investigation_defaults_apply_when_the_table_is_absent() {
        let config =
            Config::from_toml("[server]\n[data]\npath = \"/var/lib/trawl/data\"\n").unwrap();
        let investigation = &config.investigation;
        assert_eq!(investigation.evidence_idle_secs, 604_800);
        assert_eq!(investigation.evidence_ceiling_secs, 2_592_000);
        assert_eq!(investigation.evidence_per_investigation_bytes, 64 << 20);
        assert_eq!(investigation.evidence_installation_bytes, 1 << 30);
        let empty = investigation_config("", "").unwrap().investigation;
        let default = InvestigationConfig::default();
        for (absent, empty, default) in [
            (
                investigation.evidence_idle_secs,
                empty.evidence_idle_secs,
                default.evidence_idle_secs,
            ),
            (
                investigation.evidence_ceiling_secs,
                empty.evidence_ceiling_secs,
                default.evidence_ceiling_secs,
            ),
            (
                investigation.evidence_per_investigation_bytes,
                empty.evidence_per_investigation_bytes,
                default.evidence_per_investigation_bytes,
            ),
            (
                investigation.evidence_installation_bytes,
                empty.evidence_installation_bytes,
                default.evidence_installation_bytes,
            ),
        ] {
            assert_eq!(absent, empty);
            assert_eq!(absent, default);
        }
    }

    #[test]
    fn investigation_full_override_round_trips() {
        let investigation = investigation_config(
            "",
            "evidence_idle_secs = 3600\nevidence_ceiling_secs = 86400\n\
             evidence_per_investigation_bytes = \"16M\"\n\
             evidence_installation_bytes = \"256MiB\"",
        )
        .unwrap()
        .investigation;
        assert_eq!(investigation.evidence_idle_secs, 3600);
        assert_eq!(investigation.evidence_ceiling_secs, 86_400);
        assert_eq!(investigation.evidence_per_investigation_bytes, 16 << 20);
        assert_eq!(investigation.evidence_installation_bytes, 256 << 20);
    }

    #[test]
    fn investigation_refuses_unknown_keys() {
        let err = investigation_config("", "evidence_ttl_secs = 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("evidence_ttl_secs"), "{err}");
    }

    #[test]
    fn investigation_refuses_a_zero_value_naming_the_key() {
        for key in [
            "evidence_idle_secs",
            "evidence_ceiling_secs",
            "evidence_per_investigation_bytes",
            "evidence_installation_bytes",
        ] {
            let err = investigation_config("", &format!("{key} = 0"))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("investigation.{key} must be > 0")),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn investigation_refuses_limits_that_do_not_nest() {
        for (capture, investigation, small, large) in [
            (
                "",
                "evidence_idle_secs = 2592001",
                "investigation.evidence_idle_secs",
                "investigation.evidence_ceiling_secs",
            ),
            (
                "",
                "evidence_per_investigation_bytes = \"2GiB\"",
                "investigation.evidence_per_investigation_bytes",
                "investigation.evidence_installation_bytes",
            ),
            (
                "max_file_bytes = \"8MiB\"",
                "evidence_per_investigation_bytes = \"4MiB\"",
                "capture.max_file_bytes",
                "investigation.evidence_per_investigation_bytes",
            ),
        ] {
            let err = investigation_config(capture, investigation)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("{small} must be <= {large}")),
                "{investigation}: {err}"
            );
        }
        investigation_config(
            "max_file_bytes = \"8MiB\"",
            "evidence_idle_secs = 2592000\nevidence_per_investigation_bytes = \"8MiB\"\n\
             evidence_installation_bytes = \"8MiB\"",
        )
        .unwrap();
    }
}
