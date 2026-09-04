// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared configuration types for the trawl workspace.
//!
//! Lives in its own crate so consumers that only need the config types
//! (e.g. `trawl-web`, the browser-facing session proxy) don't have to
//! pull in `trawl-server`'s transitive deps, most loudly the `DuckDB`
//! engine chain: depending on `trawl-server` for `Config`/`WebConfig`
//! alone drags libduckdb-sys into every build of the proxy.
//!
//! `trawl-server` re-exports these types via `trawl_server::config::*`
//! for its own modules; other crates use `trawl_config::...` directly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub mod fs;

/// Top-level daemon configuration, loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
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
}

/// HTTPS listener settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address to bind the HTTPS listener (e.g. "127.0.0.1:8080").
    #[serde(default = "default_http_addr")]
    pub http_addr: String,

    /// Query execution timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Maximum concurrent queries (bounds the executor pool).
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

    /// Maximum concurrent HTTP requests (default: 256).
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,

    /// Graceful shutdown drain timeout in seconds (default: 30).
    #[serde(default = "default_shutdown_drain_secs")]
    pub shutdown_drain_secs: u64,

    /// Optional JSON log file. Written only when internal telemetry is off;
    /// with telemetry on the WAL layer takes that slot and this path is never
    /// opened.
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
}

impl ServerConfig {
    /// Resolve the HTTPS listen address: the `TRAWL_HTTP_ADDR` environment
    /// variable first, then `[server] http_addr` from the config file.
    /// Empty values count as unset.
    ///
    /// The env override exists so a listener can be moved without editing
    /// (or templating) the shared `trawld.toml` — `bin/dev` uses it to shift
    /// ports off a collision, and container images can repoint the bind
    /// without a config volume. [`Config::from_toml`] applies it at parse
    /// time, so trawld's own call sites read the already-resolved field;
    /// trawl-web deserializes the file itself and calls this directly when
    /// deriving its upstream URL.
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
        env_value
            .filter(|s| !s.is_empty())
            .map_or_else(|| configured.to_owned(), str::to_owned)
    }
}

/// Per-key rate limiting in requests per minute (ADR-0006).
///
/// Every API key gets an independent token bucket per route class:
/// `default_rpm` on the interactive API routes, `ingest_rpm` on
/// `/api/v1/ingest` for a key holding the `ingest` permission (any other key
/// on that route draws on `default_rpm`). Two ceilings, because a log shipper
/// flushing batches and a human running `DuckDB` scans need budgets an order
/// of magnitude apart: one number would either throttle ingest or hand every
/// interactive key the shipper-sized ceiling. These are the class defaults; a
/// role's `rate_rpm` in the fleet keystore overrides them for its keys.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    /// Requests/minute allowed per API key on the interactive API routes
    /// (default: 100). 0 = disabled.
    #[serde(default = "default_rate_limit_rpm")]
    pub default_rpm: u32,
    /// Requests/minute allowed per API key on `/api/v1/ingest`
    /// (default: 1000). 0 = disabled.
    #[serde(default = "default_ingest_rate_limit_rpm")]
    pub ingest_rpm: u32,
    /// Retired per-role knob (ADR-0006), kept as a deserialization sentinel
    /// so an old config's tuned value fails loudly at validation instead of
    /// being silently ignored.
    #[serde(default)]
    pub admin: Option<u32>,
    /// Retired per-role knob (sentinel).
    #[serde(default)]
    pub analyst: Option<u32>,
    /// Retired per-role knob (sentinel).
    #[serde(default)]
    pub reader: Option<u32>,
    /// Retired per-role knob (sentinel).
    #[serde(default)]
    pub ingest: Option<u32>,
}

/// Parquet data source settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DataConfig {
    /// Directory containing parquet files (e.g. "/var/lib/trawl/data").
    ///
    /// Accepts either a bare directory path or a glob pattern. Glob
    /// characters (`*`, `?`, `[`) are stripped to derive the base directory.
    pub path: String,
}

impl DataConfig {
    /// The base directory where parquet files live.
    pub fn base_dir(&self) -> PathBuf {
        if self.has_glob() {
            let path = Path::new(&self.path);
            let mut base = PathBuf::new();
            for component in path.components() {
                let s = component.as_os_str().to_string_lossy();
                if s.contains('*') || s.contains('?') || s.contains('[') {
                    break;
                }
                base.push(component);
            }
            base
        } else {
            PathBuf::from(&self.path)
        }
    }

    /// Return the glob pattern for `read_parquet()`.
    ///
    /// If the configured path is already a glob, returns it as-is.
    /// If it's a bare directory, appends `**/*.parquet`.
    pub fn parquet_glob(&self) -> String {
        if self.has_glob() {
            self.path.clone()
        } else {
            format!("{}/**/*.parquet", self.path.trim_end_matches('/'))
        }
    }

    fn has_glob(&self) -> bool {
        self.path.contains('*') || self.path.contains('?') || self.path.contains('[')
    }
}

/// Log ingestion pipeline settings.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestConfig {
    /// Whether the ingest endpoint is enabled.
    #[serde(default = "default_ingest_enabled")]
    pub enabled: bool,

    /// Maximum request body size for ingest (bytes). Default: 16 MB.
    /// Accepts human-readable sizes like `"16M"`, `"1G"`.
    #[serde(
        default = "default_ingest_max_body_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub max_body_bytes: usize,

    /// WAL directory path. Defaults to `{data.base_dir}/wal/`.
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

    /// Maximum number of events held in the hot buffer.
    /// Oldest batches are evicted when this limit is exceeded.
    /// Default: 100,000.
    #[serde(default = "default_hot_buffer_max_events")]
    pub hot_buffer_max_events: usize,

    /// Maximum estimated memory usage for the hot buffer in bytes.
    /// Default: 100 MB. Accepts human-readable sizes like `"100M"`, `"1G"`.
    #[serde(
        default = "default_hot_buffer_max_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub hot_buffer_max_bytes: usize,

    /// How often server stats (pool utilization, SSE connections, hot buffer
    /// metrics) are emitted as telemetry events (seconds). Default: 60.
    /// Lower values give more granular observability at the cost of telemetry
    /// volume. Set to 0 to disable.
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
    /// sizes like `"16M"`. Like `hot_buffer_max_bytes`, the charge is an
    /// estimate: serialized ndjson bytes plus the retained event maps
    /// (which hold roughly the same payload again) plus a fixed per-event
    /// overhead. Enforced as events arrive: over budget the oldest queued
    /// batches are shed first and then the incoming event itself, counted
    /// in `trawl_telemetry_events_dropped_total{reason="buffer_cap"}`. Must
    /// be positive: set `internal_telemetry = false` to turn self-telemetry
    /// off.
    #[serde(
        default = "default_telemetry_buffer_max_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub telemetry_buffer_max_bytes: usize,

    /// Maximum WAL files per compaction chunk. Larger backlogs are split
    /// into chunks of this size and merged incrementally. Default: 500.
    #[serde(default = "default_compaction_chunk_size")]
    pub compaction_chunk_size: usize,

    /// `DuckDB` memory limit for compaction connections. Forces spill-to-disk
    /// earlier rather than letting `DuckDB` use 80% of container RAM.
    /// Accepts `DuckDB` memory strings like `"2GB"`, `"512MB"`. Default: `"2GB"`.
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
    /// mappable wins (ADR-0013). Every source is read and left where it
    /// is, as an ordinary sender column.
    ///
    /// Entries take the bare-string shorthand (`"level"`) or the typed
    /// form (`{ field = "syslog_severity", dialect = "syslog" }`); the
    /// dialect governs numerics only. A producer profile's fixed sources
    /// prepend this list and are not configurable.
    ///
    /// Empty is legal and means "derive nothing". Default:
    /// [`DEFAULT_SEVERITY_FROM`] — a packaging contract, so the code
    /// default, the Debian example, the Helm chart and the configuration
    /// reference all state the same list.
    ///
    /// Semantics (bare names only, no post-fold duplicates, bounded
    /// length, known dialect) are validated boot-fatally by the server:
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

/// Expand a packaged default into the bare-shorthand entries it stands for.
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

/// Env names reserved for sibling directories under the data root.
pub const RESERVED_ENV_NAMES: &[&str] = &["wal", "scheduled"];

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

/// Default environment name for zero-config deployments.
fn default_env_name() -> String {
    "prod".to_string()
}

/// Data retention policy settings.
///
/// Two independent policies, both always-on with defaults. Age retention
/// deletes a date directory once it is older than the limit that applies
/// to its env, which is the env's own `[retention.env.<name>]` entry when
/// it has one and the global `max_age_days` otherwise. Disk-pressure
/// retention is install-wide: below `min_free_disk_bytes` free, it deletes
/// date directories one at a time until the volume is back over the
/// threshold, taking the directory that has used up the largest fraction
/// of its env's age limit first.
///
/// A 0 anywhere means "keep forever" for whatever it governs, never "the
/// whole task is off": a global 0 with `[retention.env.prod] max_age_days
/// = 30` still ages prod out, and an env whose own entry is 0 keeps its
/// data past the global limit. A directory that no age limit will ever
/// reach is still a disk-pressure candidate, ranked after everything that
/// expires.
///
/// Unlike the rest of the config, this section refuses keys it does not
/// know, and so does every `[retention.env.<name>]` table. Everywhere else
/// a typo costs a setting that stays at its default. Here it costs data:
/// `[retention.evn.prod] max_age_days = 365` would otherwise load, be
/// discarded, and leave prod ageing out at the global limit while the
/// operator reads their own config as keeping it a year.
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
    /// An env with no entry keeps `max_age_days`. Disk-pressure retention
    /// is install-wide and takes no override: it deletes to free space,
    /// and every env's data sits on the one filesystem.
    ///
    /// Keys are validated at load like `ingest.envs` entries, and a key
    /// naming an env that is not in `ingest.envs` is legal: de-listing an
    /// env stops new ingest for it while its directories stay on disk and
    /// still need an age policy.
    #[serde(default)]
    pub env: BTreeMap<String, EnvRetention>,
}

/// One env's retention override.
///
/// `max_age_days` is required: an empty `[retention.env.lab]` table is a
/// load error rather than a silent inherit or a silent 0, because the two
/// readings ("keep what the global says" and "keep forever") are opposite
/// answers and the operator wrote the table to say something.
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
    ///
    /// The one fallback lookup. Env names on disk and config keys are both
    /// held to `[a-z0-9_-]{1,32}`, so this is byte equality with no folding.
    #[must_use]
    pub fn max_age_days_for(&self, env: &str) -> u64 {
        self.env
            .get(env)
            .map_or(self.max_age_days, |e| e.max_age_days)
    }
}

/// Scheduler configuration for background query execution.
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// Enable the scheduler. When false, no scheduled queries run.
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,

    /// How often the scheduler checks for due schedules (seconds).
    #[serde(default = "default_scheduler_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// Maximum rows stored in a report run result.
    #[serde(default = "default_scheduler_report_max_rows")]
    pub report_max_rows: usize,

    /// Maximum runs to keep per schedule (retention).
    #[serde(default = "default_scheduler_max_runs_per_schedule")]
    pub max_runs_per_schedule: u64,

    /// Delete report runs older than this many days.
    #[serde(default = "default_scheduler_report_retention_days")]
    pub report_retention_days: u64,

    /// How many intervals of missed coverage a `since_last` report window
    /// may swallow in one catch-up run (ADR-0018 ruling 9).
    ///
    /// Missed runs coalesce into one window rather than backfilling one run
    /// each. A daemon down for a week would otherwise hand the next run a
    /// week-wide window and one enormous query, so a gap beyond this many
    /// intervals clamps the window forward and flags the run
    /// (`window_truncated` plus `trawl_scheduler_window_truncated_total`).
    /// It never wedges the schedule and never drops the gap silently.
    #[serde(default = "default_scheduler_max_catchup_intervals")]
    pub max_catchup_intervals: u32,
}

const DEFAULT_SCHEDULER_ENABLED: bool = true;
const DEFAULT_SCHEDULER_POLL_INTERVAL_SECS: u64 = 10;
const DEFAULT_SCHEDULER_REPORT_MAX_ROWS: usize = 10_000;
const DEFAULT_SCHEDULER_MAX_RUNS_PER_SCHEDULE: u64 = 100;
const DEFAULT_SCHEDULER_REPORT_RETENTION_DAYS: u64 = 30;
/// A day of missed coverage at the common hourly cadence.
pub const DEFAULT_SCHEDULER_MAX_CATCHUP_INTERVALS: u32 = 24;

fn default_scheduler_enabled() -> bool {
    DEFAULT_SCHEDULER_ENABLED
}
fn default_scheduler_poll_interval_secs() -> u64 {
    DEFAULT_SCHEDULER_POLL_INTERVAL_SECS
}
fn default_scheduler_report_max_rows() -> usize {
    DEFAULT_SCHEDULER_REPORT_MAX_ROWS
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
            report_max_rows: DEFAULT_SCHEDULER_REPORT_MAX_ROWS,
            max_runs_per_schedule: DEFAULT_SCHEDULER_MAX_RUNS_PER_SCHEDULE,
            report_retention_days: DEFAULT_SCHEDULER_REPORT_RETENTION_DAYS,
            max_catchup_intervals: DEFAULT_SCHEDULER_MAX_CATCHUP_INTERVALS,
        }
    }
}

/// Native syslog listener settings for receiving logs from network appliances.
#[derive(Debug, Clone, Deserialize)]
pub struct SyslogConfig {
    /// Enable the syslog listener. Default: false.
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

    /// Close a TCP connection after this many consecutive failed sends
    /// to the batcher (indicates sustained backpressure). Default: 100.
    #[serde(default = "default_syslog_consecutive_send_failures_limit")]
    pub consecutive_send_failures_limit: usize,

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
    /// meaningful APP-NAME (e.g. `UniFi` consoles).
    #[serde(default)]
    pub source_service_map: std::collections::HashMap<String, String>,

    /// Channel capacity for the event queue between listeners and batcher.
    /// Increase for high-volume syslog deployments. Default: 10,000.
    #[serde(default = "default_syslog_channel_capacity")]
    pub channel_capacity: usize,
}

const DEFAULT_SYSLOG_ADDR: &str = "0.0.0.0:1514";
const DEFAULT_SYSLOG_MAX_TCP_CONNECTIONS: usize = 256;
const DEFAULT_SYSLOG_BATCH_INTERVAL_MS: u64 = 500;
const DEFAULT_SYSLOG_BATCH_MAX_EVENTS: usize = 1000;
const DEFAULT_SYSLOG_TCP_IDLE_TIMEOUT_SECS: u64 = 60;
const DEFAULT_SYSLOG_MAX_EVENTS_PER_CONNECTION: usize = 100_000;
const DEFAULT_SYSLOG_CONSECUTIVE_SEND_FAILURES_LIMIT: usize = 100;
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

fn default_syslog_consecutive_send_failures_limit() -> usize {
    DEFAULT_SYSLOG_CONSECUTIVE_SEND_FAILURES_LIMIT
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
            consecutive_send_failures_limit: DEFAULT_SYSLOG_CONSECUTIVE_SEND_FAILURES_LIMIT,
            default_service: "syslog".to_owned(),
            allow_cidrs: Vec::new(),
            source_service_map: std::collections::HashMap::new(),
            channel_capacity: DEFAULT_SYSLOG_CHANNEL_CAPACITY,
        }
    }
}

// -- byte size deserializer --------------------------------------------------
// Accepts either a raw integer or a string with a unit suffix like
// "128K", "16M", "1G", "100MiB". All multipliers are binary
// (1024-based) because nobody means 1,000,000 when they write "1M" in a
// server config file.

/// Parse a byte size string like "128K", "16M", "1GiB" into a raw byte count.
fn parse_byte_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty byte size".into());
    }

    // Find where digits/dots end and the suffix begins.
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

    // Use integer math for whole numbers (the common case), float only
    // for decimals like "1.5G".
    if num_str.contains('.') {
        let num: f64 = num_str
            .parse()
            .map_err(|_| format!("invalid number in byte size: {s:?}"))?;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        Ok((num * multiplier as f64) as u64)
    } else {
        let num: u64 = num_str
            .parse()
            .map_err(|_| format!("invalid number in byte size: {s:?}"))?;
        Ok(num * multiplier)
    }
}

/// Deserialize a byte size as either a raw number or a string with unit suffix.
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

/// Same as [`deserialize_byte_size`] but returns `u64` (for retention config).
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

// -- default constants -------------------------------------------------------
// Public so another crate can name the same value the deserializer would
// supply: trawl-server's query tracker reads `DEFAULT_MAX_QUERY_HISTORY`, its
// ingest derivation policy reads `DEFAULT_SEVERITY_FROM`/`DEFAULT_TIME_FROM`.
// The `default_*` functions exist only because serde's
// `#[serde(default = "...")]` requires a function path.

/// Default HTTPS listen address.
pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8080";
/// Default query execution timeout (seconds).
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default maximum rows a query can return.
pub const DEFAULT_MAX_RESULT_ROWS: usize = 100_000;
/// Default maximum rows for export responses (1M).
pub const DEFAULT_MAX_EXPORT_ROWS: usize = 1_000_000;
/// Default maximum request body size (128 KB).
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 128 * 1024;
/// Default maximum concurrent HTTP requests.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 256;
/// Default graceful shutdown drain timeout (seconds).
pub const DEFAULT_SHUTDOWN_DRAIN_SECS: u64 = 30;
/// Default TLS certificate reload interval (seconds). 0 = disabled.
pub const DEFAULT_TLS_RELOAD_INTERVAL_SECS: u64 = 300;
/// Default schema cache TTL (seconds).
pub const DEFAULT_SCHEMA_CACHE_TTL_SECS: u64 = 60;
/// Default query history ring buffer capacity.
pub const DEFAULT_MAX_QUERY_HISTORY: usize = 1000;
/// Default ingest request body size limit (16 MB).
pub const DEFAULT_INGEST_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Default compaction interval (seconds).
pub const DEFAULT_COMPACTION_INTERVAL_SECS: u64 = 10;
/// Default internal telemetry (enabled).
pub const DEFAULT_INTERNAL_TELEMETRY: bool = true;
/// Default daily rollup (enabled).
pub const DEFAULT_DAILY_ROLLUP: bool = true;
/// Default compaction chunk size (WAL files per `read_json` call).
pub const DEFAULT_COMPACTION_CHUNK_SIZE: usize = 500;
/// Default `DuckDB` memory limit for compaction connections.
pub const DEFAULT_COMPACTION_MEMORY_LIMIT: &str = "2GB";
/// Default retention max age (days).
pub const DEFAULT_RETENTION_MAX_AGE_DAYS: u64 = 90;
/// Default retention minimum free disk space (bytes). 1 GiB.
pub const DEFAULT_RETENTION_MIN_FREE_DISK_BYTES: u64 = 1_073_741_824;
/// Default retention check interval (seconds). 1 hour.
pub const DEFAULT_RETENTION_INTERVAL_SECS: u64 = 3600;
/// Default per-key rate limit on the interactive API routes
/// (requests/minute). Sized for a human at a terminal, an order of magnitude
/// below `DEFAULT_INGEST_RATE_LIMIT_RPM` — an untuned key must not get a
/// shipper-sized query budget.
pub const DEFAULT_RATE_LIMIT_RPM: u32 = 100;
/// Default per-key rate limit on `/api/v1/ingest` (requests/minute).
/// Sized for shippers: vector flushes a batch per 1 MB / 5 s per source, and
/// several sources commonly share one ingest key.
pub const DEFAULT_INGEST_RATE_LIMIT_RPM: u32 = 1000;
/// Default maximum concurrent SSE connections.
pub const DEFAULT_MAX_SSE_CONNECTIONS: usize = 32;
/// Fallback CPU count when `available_parallelism()` fails.
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

/// Default hot buffer max events.
pub const DEFAULT_HOT_BUFFER_MAX_EVENTS: usize = 100_000;
/// Default hot buffer max bytes (100 MB).
pub const DEFAULT_HOT_BUFFER_MAX_BYTES: usize = 100 * 1024 * 1024;

fn default_hot_buffer_max_events() -> usize {
    DEFAULT_HOT_BUFFER_MAX_EVENTS
}

fn default_hot_buffer_max_bytes() -> usize {
    DEFAULT_HOT_BUFFER_MAX_BYTES
}

/// Default server stats emission interval (seconds).
pub const DEFAULT_STATS_INTERVAL_SECS: u64 = 60;
/// Default telemetry flush interval (seconds).
pub const DEFAULT_TELEMETRY_FLUSH_INTERVAL_SECS: u64 = 1;

fn default_stats_interval_secs() -> u64 {
    DEFAULT_STATS_INTERVAL_SECS
}

fn default_telemetry_flush_interval_secs() -> u64 {
    DEFAULT_TELEMETRY_FLUSH_INTERVAL_SECS
}

/// Default telemetry retry-queue memory cap (16 MiB, estimated charge).
pub const DEFAULT_TELEMETRY_BUFFER_MAX_BYTES: usize = 16 * 1024 * 1024;

fn default_telemetry_buffer_max_bytes() -> usize {
    DEFAULT_TELEMETRY_BUFFER_MAX_BYTES
}

/// Default key audit polling interval (seconds).
pub const DEFAULT_AUDIT_INTERVAL_SECS: u64 = 30;

/// Authentication settings.
///
/// API keys live in the fleet-auth Postgres keystore (`database_url`).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// Retired knob, kept only as a sentinel.
    ///
    /// App state (query history, saved queries, schedules) lives in the
    /// dedicated `trawl` postgres database (`[storage] database_url`,
    /// ADR-0004). serde has no `deny_unknown_fields` here, so without this
    /// field an old config's `db_path` would silently vanish; instead
    /// [`Config::validate`] rejects it with a message naming the migration.
    #[serde(default)]
    pub db_path: Option<PathBuf>,

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
            db_path: None,
            database_url: None,
            audit_interval_secs: default_audit_interval_secs(),
        }
    }
}

impl AuthConfig {
    /// Resolve the fleet keystore URL: `FLEET_DATABASE_URL` env var first,
    /// then `[auth] database_url` from the config file. Empty values count
    /// as unset.
    ///
    /// The bare `DATABASE_URL` is deliberately not consulted: that variable
    /// belongs to the sqlx test harness (`#[sqlx::test]` hardwires it), and a
    /// process-wide `DATABASE_URL` must never silently repoint trawld's
    /// keystore.
    ///
    /// # Errors
    /// Returns [`ConfigError::Validation`] when neither source is set.
    pub fn resolve_database_url(&self) -> Result<String, ConfigError> {
        Self::resolve_database_url_from(
            std::env::var("FLEET_DATABASE_URL").ok().as_deref(),
            self.database_url.as_deref(),
        )
    }

    /// Pure resolution core, split out for testability (mutating process
    /// env in tests is forbidden under `unsafe_code = "forbid"`).
    fn resolve_database_url_from(
        env_value: Option<&str>,
        configured: Option<&str>,
    ) -> Result<String, ConfigError> {
        let pick = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
        pick(env_value).or_else(|| pick(configured)).ok_or_else(|| {
            ConfigError::Validation(
                "fleet keystore URL required: set [auth] database_url in trawld.toml \
                 or the FLEET_DATABASE_URL environment variable"
                    .into(),
            )
        })
    }
}

/// Storage settings for trawl's own app-state database (query history,
/// saved queries, schedules, report runs — ADR-0004).
///
/// This is a dedicated `trawl` postgres database owned by trawl-server
/// (boot-time migrated, advisory-locked sole writer). Deliberately separate
/// from `[auth]`: the stores are app state, not auth, and neither URL falls
/// back to the other.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StorageConfig {
    /// Postgres URL of the dedicated `trawl` app-state database (e.g.
    /// `postgres://trawl:pass@host:5432/trawl`). The `TRAWL_DATABASE_URL`
    /// environment variable takes precedence. trawld refuses to start when
    /// neither is set.
    #[serde(default)]
    pub database_url: Option<String>,
}

impl StorageConfig {
    /// Resolve the app-state database URL: `TRAWL_DATABASE_URL` env var
    /// first, then `[storage] database_url` from the config file. Empty
    /// values count as unset. No fallback to the `[auth]` URL.
    ///
    /// # Errors
    /// Returns [`ConfigError::Validation`] when neither source is set.
    pub fn resolve_database_url(&self) -> Result<String, ConfigError> {
        Self::resolve_database_url_from(
            std::env::var("TRAWL_DATABASE_URL").ok().as_deref(),
            self.database_url.as_deref(),
        )
    }

    /// Pure resolution core, split out for testability (mutating process
    /// env in tests is forbidden under `unsafe_code = "forbid"`).
    fn resolve_database_url_from(
        env_value: Option<&str>,
        configured: Option<&str>,
    ) -> Result<String, ConfigError> {
        let pick = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
        pick(env_value).or_else(|| pick(configured)).ok_or_else(|| {
            ConfigError::Validation(
                "trawl app-state database URL required: set [storage] database_url in \
                 trawld.toml or the TRAWL_DATABASE_URL environment variable"
                    .into(),
            )
        })
    }
}

/// Browser-facing session proxy (`trawl-web`) settings.
///
/// Consumed by the `trawl-web` binary, which translates cookie-based browser
/// sessions into bearer-token requests against trawld. Every field is
/// optional so the proxy can supply its own defaults without coupling trawld
/// to the proxy's operational choices.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebConfig {
    /// Bind address for the proxy HTTP listener. Default: "127.0.0.1:8090".
    pub bind_addr: Option<String>,

    /// How the proxy reaches trawld. If unset, derived from `[server]`.
    pub upstream_url: Option<String>,

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
    ///
    /// This is the CSRF allowlist: `trawl-web` compares a request's whole
    /// `Origin` header (scheme, host and port) against these entries and
    /// consults no forwarding header, so an operator states the origin
    /// their browser shows rather than trusting whatever a proxy put in
    /// `Host`. There is deliberately no derived default. The proxy refuses
    /// to start with an empty list, because a guessed origin is either
    /// wrong (every browser request 403s) or right by accident.
    /// The two spellings of loopback are different origins to a browser,
    /// so `http://127.0.0.1:8090` and `http://localhost:8090` must both
    /// be listed if both are used.
    ///
    /// Kept as raw strings: this crate is trawld's dependency-light config
    /// surface and never links fleet-auth, so validation happens in
    /// `trawl-web` where the one origin parser lives. `#[serde(default)]`
    /// keeps a trawld-only config with no `[web]` section parsing.
    #[serde(default)]
    pub public_origins: Vec<String>,

    /// Parent domain for the shared `fleet_session` SSO cookie — the SSO
    /// knob shared by every fleet app so operator docs can say "set the same
    /// value in every app" (ADR-0004).
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
    num_cpus()
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

/// Default monitor refresh interval (1 second).
const DEFAULT_MONITOR_REFRESH_MS: u64 = 1000;

fn default_monitor_refresh_ms() -> u64 {
    DEFAULT_MONITOR_REFRESH_MS
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
            admin: None,
            analyst: None,
            reader: None,
            ingest: None,
        }
    }
}

/// Portable CPU count without pulling in the `num_cpus` crate.
fn num_cpus() -> usize {
    std::thread::available_parallelism().map_or(FALLBACK_CPU_COUNT, std::num::NonZero::get)
}

/// Expand a leading `~/` to `$HOME/`.
fn expand_tilde(path: &str) -> String {
    shellexpand::tilde(path).into_owned()
}

/// Reject leftover keys from a superseded config shape.
///
/// Package upgrades keep the operator's existing config (deb conffile
/// semantics, reused helm config maps) and serde silently ignores unknown
/// keys — so every removed knob has to fail loudly, naming the migration that
/// killed it and what replaces it, rather than quietly dropping tuned values.
///
/// `fields` pairs each removed key's fully-qualified name with whether the
/// parsed config still carries a value for it; only the present ones are
/// named in the error.
fn reject_removed_fields(
    fields: &[(&str, bool)],
    migration: &str,
    guidance: &str,
) -> Result<(), ConfigError> {
    let present: Vec<&str> = fields
        .iter()
        .filter(|(_, is_set)| *is_set)
        .map(|&(name, _)| name)
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    Err(ConfigError::Validation(format!(
        "{} removed in {migration}: {guidance}",
        present.join(", "),
    )))
}

/// Validate the one safety budget whose zero value is deliberately invalid.
///
/// Other caps use zero as an explicit off switch, but an unbounded telemetry
/// buffer can grow indefinitely behind a wedged WAL write. Self-telemetry has
/// its own boolean off switch, so zero has no valid interpretation.
fn validate_telemetry_buffer_max_bytes(bytes: usize) -> Result<(), ConfigError> {
    if bytes == 0 {
        return Err(ConfigError::Validation(
            "ingest.telemetry_buffer_max_bytes must be a positive byte count; set \
             ingest.internal_telemetry = false to disable internal telemetry"
                .into(),
        ));
    }
    Ok(())
}

/// Validate the catch-up ceiling, the other budget whose zero is invalid.
///
/// A catch-up window is measured in whole schedule intervals, so a ceiling
/// of zero would clamp every `since_last` window to nothing and produce
/// empty reports forever. Unlike the caps that use zero as an off switch,
/// "do not clamp" is spelled with a large number here, not with none.
fn validate_max_catchup_intervals(intervals: u32) -> Result<(), ConfigError> {
    if intervals == 0 {
        return Err(ConfigError::Validation(
            "scheduler.max_catchup_intervals must be >= 1 (it is a count of whole \
             schedule intervals a since_last window may cover in one catch-up run)"
                .into(),
        ));
    }
    Ok(())
}

impl Config {
    /// Parse configuration from a TOML string.
    ///
    /// Resolves paths (tilde expansion) and validates, same as [`from_file`].
    pub fn from_toml(contents: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(contents).map_err(|e| ConfigError::Parse {
            path: PathBuf::from("<inline>"),
            source: e,
        })?;
        config.resolve_paths();
        // Applied before validation so an env-supplied address is held to the
        // same checks as a configured one, and so every downstream reader of
        // `server.http_addr` (bind, log line, dashboard snapshot) sees one
        // resolved value instead of each remembering to consult the env.
        config.server.http_addr = config.server.resolve_http_addr();
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from a TOML file.
    ///
    /// All paths in the config are resolved (tilde-expanded) after parsing.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::Io {
            path: path.as_ref().to_owned(),
            source: e,
        })?;
        Self::from_toml(&contents).map_err(|e| match e {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: path.as_ref().to_owned(),
                source,
            },
            other => other,
        })
    }

    /// Expand `~` to `$HOME` in all path fields.
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
    }

    /// Resolve the WAL directory: explicit config value or `{data.base_dir}/wal/`.
    pub fn wal_dir(&self) -> PathBuf {
        self.ingest
            .wal_dir
            .clone()
            .unwrap_or_else(|| self.data.base_dir().join("wal"))
    }

    /// State directory — parent of the data directory.
    ///
    /// Used as the base for auto-generated TLS certs (`{state_dir}/tls/`).
    /// For the deb package this resolves to `/var/lib/trawl/`.
    pub fn state_dir(&self) -> PathBuf {
        self.data
            .base_dir()
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    /// Return warnings about potentially dangerous configuration.
    ///
    /// Called after tracing is initialized so these can be logged.
    pub fn warnings(&self) -> Vec<String> {
        let mut warns = Vec::new();
        if self.server.tls_cert_path.is_none() {
            warns.push(
                "no TLS certificate configured — using auto-generated self-signed certificate"
                    .into(),
            );
        }
        if self.ingest.internal_telemetry && self.server.log_file.is_some() {
            warns.push(
                "log_file is deprecated when internal_telemetry is enabled — \
                 server events now flow through the ingest pipeline as service:trawld"
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

    /// Whether internal telemetry is effectively enabled.
    ///
    /// Requires both `ingest.enabled` and `ingest.internal_telemetry` to be true.
    pub fn internal_telemetry_enabled(&self) -> bool {
        self.ingest.enabled && self.ingest.internal_telemetry
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.data.path.is_empty() {
            return Err(ConfigError::Validation("data.path cannot be empty".into()));
        }

        reject_removed_fields(
            &[("auth.db_path", self.auth.db_path.is_some())],
            "the ADR-0004 slice-3 migration",
            "query history, saved queries, and schedules now live in the dedicated trawl \
             postgres database. Remove db_path from [auth], configure [storage] database_url \
             (or TRAWL_DATABASE_URL), and see the fleet-auth cutover runbook. The old sqlite \
             file is not imported — recreate saved queries and schedules",
        )?;

        let rl = &self.server.rate_limit;
        reject_removed_fields(
            &[
                ("server.rate_limit.admin", rl.admin.is_some()),
                ("server.rate_limit.analyst", rl.analyst.is_some()),
                ("server.rate_limit.reader", rl.reader.is_some()),
                ("server.rate_limit.ingest", rl.ingest.is_some()),
            ],
            "ADR-0006 slice 0",
            &format!(
                "rate limiting is now per API key, not per role. Replace the per-role keys \
                 with default_rpm for the interactive API routes (requests/minute per key, \
                 0 disables; default {DEFAULT_RATE_LIMIT_RPM}) and ingest_rpm for \
                 /api/v1/ingest (default {DEFAULT_INGEST_RATE_LIMIT_RPM}). Per-role \
                 class-of-service returns in slice 1 as a role rate_rpm attribute"
            ),
        )?;

        if self.server.max_concurrent_queries == 0 {
            return Err(ConfigError::Validation(
                "server.max_concurrent_queries must be > 0".into(),
            ));
        }

        validate_telemetry_buffer_max_bytes(self.ingest.telemetry_buffer_max_bytes)?;
        validate_max_catchup_intervals(self.scheduler.max_catchup_intervals)?;

        if self.server.tls_cert_path.is_some() != self.server.tls_key_path.is_some() {
            return Err(ConfigError::Validation(
                "tls_cert_path and tls_key_path must both be set or both omitted".into(),
            ));
        }

        self.validate_ingest_env_names()?;
        self.validate_retention_env_keys()?;

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

    /// Env allowlist (ADR-0009): validated at load, refuse to start
    /// otherwise — env is a path segment and the charset is the whole
    /// injectivity argument.
    fn validate_ingest_env_names(&self) -> Result<(), ConfigError> {
        for env in &self.ingest.envs {
            if !is_valid_env_name(env) {
                return Err(ConfigError::Validation(format!(
                    "ingest.envs entry {env:?} is not a valid env name \
                     (must match [a-z0-9_-]{{1,32}})"
                )));
            }
            if RESERVED_ENV_NAMES.contains(&env.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "ingest.envs entry {env:?} is reserved — `wal/` and \
                     `scheduled/` live alongside env directories under the \
                     data root"
                )));
            }
        }
        if !is_valid_env_name(&self.ingest.default_env) {
            return Err(ConfigError::Validation(format!(
                "ingest.default_env {:?} is not a valid env name \
                 (must match [a-z0-9_-]{{1,32}})",
                self.ingest.default_env
            )));
        }
        if RESERVED_ENV_NAMES.contains(&self.ingest.default_env.as_str()) {
            return Err(ConfigError::Validation(format!(
                "ingest.default_env {:?} is reserved — `wal/` and \
                 `scheduled/` live alongside env directories under the data \
                 root",
                self.ingest.default_env
            )));
        }
        if !self.ingest.envs.is_empty() && !self.ingest.envs.contains(&self.ingest.default_env) {
            return Err(ConfigError::Validation(format!(
                "ingest.default_env {:?} must be a member of ingest.envs \
                 ({:?})",
                self.ingest.default_env, self.ingest.envs
            )));
        }
        Ok(())
    }

    /// Per-env retention keys are env names too: they are matched against
    /// directory names under the data root, so they carry the same charset
    /// as `ingest.envs` entries and are boot-fatal in the same way.
    ///
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
        let invalid = |field: String, value: &String| {
            ConfigError::Validation(format!(
                "{field} {value:?} is not a valid service name \
                 (1-{MAX_SERVICE_NAME_LEN} chars of [A-Za-z0-9._-], not \
                 dot-leading)"
            ))
        };

        if !is_valid_service_name(&self.syslog.default_service) {
            return Err(invalid(
                "syslog.default_service".to_owned(),
                &self.syslog.default_service,
            ));
        }
        for (ip, service) in &self.syslog.source_service_map {
            if !is_valid_service_name(service) {
                return Err(invalid(
                    format!("syslog.source_service_map entry {ip:?} ="),
                    service,
                ));
            }
        }

        Ok(())
    }
}

/// Configuration loading errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid TOML in {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("config validation error: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[server]

[data]
path = "/var/lib/trawl/data/**/*.parquet"

[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.http_addr, "127.0.0.1:8080");
        assert_eq!(config.server.timeout_secs, 30);
        assert!(config.server.max_concurrent_queries > 0);
        assert_eq!(config.data.path, "/var/lib/trawl/data/**/*.parquet");
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
path = "/data/**/*.parquet"

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
    fn validation_rejects_leftover_db_path() {
        // Deb conffile upgrades preserve old trawld.toml files, so a leftover
        // db_path must be a loud error naming the migration, never a silent
        // ignore.
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
db_path = "/var/lib/trawl/store.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("auth.db_path removed"), "got: {err}");
        assert!(err.contains("slice-3"), "got: {err}");
        assert!(err.contains("[storage]"), "got: {err}");
        assert!(err.contains("runbook"), "got: {err}");
    }

    #[test]
    fn validation_rejects_zero_concurrency() {
        let toml = r#"
[server]
max_concurrent_queries = 0
[data]
path = "/data/*.parquet"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_concurrent_queries"));
    }

    #[test]
    fn parse_tls_config() {
        let toml = r#"
[server]
tls_cert_path = "/etc/trawl/cert.pem"
tls_key_path = "/etc/trawl/key.pem"
[data]
path = "/data/*.parquet"
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
path = "/data/*.parquet"
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
path = "/data/*.parquet"
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
path = "/data/*.parquet"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let warns = config.warnings();
        assert!(warns.is_empty());
    }

    #[test]
    fn cors_defaults_to_empty() {
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.server.cors_allowed_origins.is_empty());
    }

    #[test]
    fn base_dir_strips_glob() {
        let data = DataConfig {
            path: "/var/lib/trawl/data/**/*.parquet".into(),
        };
        assert_eq!(data.base_dir(), std::path::Path::new("/var/lib/trawl/data"));
    }

    #[test]
    fn base_dir_bare_directory() {
        let data = DataConfig {
            path: "/var/lib/trawl/data".into(),
        };
        assert_eq!(data.base_dir(), std::path::Path::new("/var/lib/trawl/data"));
    }

    #[test]
    fn parquet_glob_from_directory() {
        let data = DataConfig {
            path: "/var/lib/trawl/data".into(),
        };
        assert_eq!(data.parquet_glob(), "/var/lib/trawl/data/**/*.parquet");
    }

    #[test]
    fn parquet_glob_passthrough_existing_glob() {
        let data = DataConfig {
            path: "/data/**/*.parquet".into(),
        };
        assert_eq!(data.parquet_glob(), "/data/**/*.parquet");
    }

    #[test]
    fn parquet_glob_strips_trailing_slash() {
        let data = DataConfig {
            path: "/var/lib/trawl/data/".into(),
        };
        assert_eq!(data.parquet_glob(), "/var/lib/trawl/data/**/*.parquet");
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
path = "/data/*.parquet"
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
path = "/data/*.parquet"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        // Interactive keys default to 100/min, not the shipper-sized 1000:
        // an untuned key must not get an ingest-sized query budget.
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
path = "/data/*.parquet"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.rate_limit.default_rpm, 250);
        assert_eq!(config.server.rate_limit.ingest_rpm, 5000);
        config.validate().unwrap();
    }

    #[test]
    fn validation_rejects_legacy_rate_limit_fields() {
        // Deb conffile upgrades preserve old trawld.toml files, so a leftover
        // per-role key must be a loud error naming the migration, never a
        // silent ignore of the operator's tuned quotas.
        for legacy_key in ["admin", "analyst", "reader", "ingest"] {
            let toml = format!(
                r#"
[server]
[server.rate_limit]
{legacy_key} = 100
[data]
path = "/data/*.parquet"
[auth]
"#
            );
            let config: Config = toml::from_str(&toml).unwrap();
            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains(legacy_key), "got: {err}");
            assert!(err.contains("removed"), "got: {err}");
            assert!(err.contains("default_rpm"), "got: {err}");
            assert!(err.contains("ingest_rpm"), "got: {err}");
            assert!(err.contains("ADR-0006"), "got: {err}");
        }
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
            "config validation error: ingest.telemetry_buffer_max_bytes must be a positive byte \
             count; set ingest.internal_telemetry = false to disable internal telemetry"
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
    fn scheduler_max_catchup_intervals_zero_is_boot_fatal() {
        let err = Config::from_toml(
            r#"
[server]
[data]
path = "/data"
[auth]
[scheduler]
max_catchup_intervals = 0
"#,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "config validation error: scheduler.max_catchup_intervals must be >= 1 (it is a \
             count of whole schedule intervals a since_last window may cover in one catch-up run)"
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
    fn internal_telemetry_warns_log_file_deprecated() {
        let toml = r#"
[server]
log_file = "/var/log/trawld.log"
[data]
path = "/data"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let warns = config.warnings();
        assert!(warns.iter().any(|w| w.contains("log_file is deprecated")));
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
        assert_eq!(config.retention.min_free_disk_bytes, 1_073_741_824);
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

    // -- byte size deserializer tests ----------------------------------------

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

    // -- interval field tests ------------------------------------------------

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

    #[test]
    fn stats_interval_disabled() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
stats_interval_secs = 0
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ingest.stats_interval_secs, 0);
    }

    #[test]
    fn web_section_optional_preserves_backcompat() {
        // A trawld.toml with no [web] section must still parse; the proxy
        // supplies its own defaults.
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.web.bind_addr.is_none());
        assert!(config.web.upstream_url.is_none());
        assert!(config.web.cookie_secret_path.is_none());
        assert!(config.web.cookie_secret_env.is_none());
        assert!(config.web.session_ttl_secs.is_none());
        assert!(!config.web.allow_insecure_cookies);
        // The proxy's own boot refusal is what turns an empty list into an
        // error; trawld must still parse the file without one.
        assert!(config.web.public_origins.is_empty());
    }

    // -- fleet-auth keystore: [auth] database_url (ADR-0004) -----------------

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

        // Empty env values are unset — a CI secret that fails to inject must
        // not shadow the configured value.
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

        // Empty values on both sides are equally unset.
        let err = AuthConfig::resolve_database_url_from(Some(""), Some("")).unwrap_err();
        assert!(err.to_string().contains("database_url"));
    }

    #[test]
    fn validation_accepts_auth_without_db_path() {
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
    fn legacy_auth_cache_ttl_key_still_parses() {
        // auth_cache_ttl_secs is not a config key; a trawld.toml still
        // carrying it must keep parsing (serde ignores unknown fields).
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
auth_cache_ttl_secs = 300
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.auth.audit_interval_secs, 30);
    }

    #[test]
    fn retired_web_upstream_key_is_ignored_by_serde_default() {
        // WebConfig intentionally has no deny_unknown_fields attribute, so
        // deployed config files carrying a removed optional knob keep parsing.
        let removed_key = "coastwatch_url";
        let toml = format!(
            r#"
[server]
[data]
path = "/data"
[auth]
[web]
{removed_key} = "https://retired.invalid"
"#
        );

        let config: Config = toml::from_str(&toml).unwrap();
        assert!(config.web.shared_domain.is_none());
    }

    #[test]
    fn web_public_origins_parse_as_written() {
        // This crate stores the entries verbatim; trawl-web is where they
        // reach the origin parser, so the only contract here is that the
        // list survives TOML in order and unmodified.
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

    // -- [storage] database_url (ADR-0004) ------------------------------------

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
        // Old configs without [storage] must still parse; resolution is what
        // fails loudly (at boot), not deserialization.
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

        // Empty env values are unset — a secret that fails to inject must not
        // shadow the configured value.
        let url = StorageConfig::resolve_database_url_from(Some(""), Some("postgres://toml/trawl"))
            .unwrap();
        assert_eq!(url, "postgres://toml/trawl");
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
        // The stores are app state, not auth: a configured [auth] database_url
        // must not leak into storage resolution. The resolver's signature
        // admits no auth input; this pins the end-to-end behaviour on a config
        // carrying only the auth URL.
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
database_url = "postgres://fleet:fleet@localhost:5433/fleet"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.storage.database_url.is_none());
        // With TRAWL_DATABASE_URL unset in the environment this must error,
        // never borrow the auth URL. (CI never sets TRAWL_DATABASE_URL.)
        if std::env::var("TRAWL_DATABASE_URL").is_err() {
            let err = config.storage.resolve_database_url().unwrap_err();
            assert!(err.to_string().contains("[storage]"), "got: {err}");
        }
    }

    // -- [server] http_addr env override: TRAWL_HTTP_ADDR --------------------

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
        // An exported-but-blank var is a shell accident (`FOO= cmd`), not a
        // request to bind the empty string.
        assert_eq!(
            ServerConfig::resolve_http_addr_from(Some(""), "127.0.0.1:5514"),
            "127.0.0.1:5514"
        );
    }

    // -- [auth] env contract: FLEET_DATABASE_URL, not DATABASE_URL -----------

    #[test]
    fn auth_resolve_ignores_bare_database_url() {
        // DATABASE_URL is ceded to the sqlx test harness (#[sqlx::test]
        // hardwires it). trawld's [auth] resolution reads FLEET_DATABASE_URL
        // only. Canary: with a toml value configured and FLEET_DATABASE_URL
        // unset, resolution returns the toml value even when the process has
        // DATABASE_URL set (CI sets it for the whole test job — this test
        // fails there if the bare override ever creeps back in).
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
path = "/data/*.parquet"
[auth]
[web]
bind_addr = "0.0.0.0:8090"
upstream_url = "https://localhost:5514"
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
            config.web.cookie_secret_path.as_deref(),
            Some(std::path::Path::new("/etc/trawl/web.key"))
        );
        assert_eq!(config.web.session_ttl_secs, Some(3600));
        assert!(config.web.allow_insecure_cookies);
        // no shared_domain in this config → standalone mode
        assert!(config.web.shared_domain.is_none());
    }

    #[test]
    fn web_shared_domain_parses_from_toml() {
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
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
        // Absent → standalone mode (origin-scoped cookie, no Domain attr).
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
[web]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.web.shared_domain.is_none());
    }

    // -- [ingest] env allowlist (ADR-0009) --------------------------------

    fn config_with_ingest(ingest: &str) -> Config {
        let toml = format!(
            r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
[ingest]
{ingest}
"#
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn ingest_env_zero_config_defaults() {
        // Zero-config ingestion works out of the box: default_env fills a
        // missing env, and an omitted `envs` behaves as `[default_env]`.
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
        assert!(err.contains("dev"), "got: {err}");
    }

    #[test]
    fn validation_rejects_env_charset_violations() {
        // `[a-z0-9_-]{1,32}` — uppercase, dots, slashes, spaces, empty, and
        // over-length names are all path-placement hazards.
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
        // `wal/` (default wal_dir) and `scheduled/` (report runs) live under
        // the data root — an env with either name would collide with them.
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

    // -- [retention.env.<name>] overrides (#108) --------------------------

    fn config_with_retention(retention: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!(
            r#"
[server]
[data]
path = "/data/*.parquet"
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
        // "Inherit the global" and "keep forever" are opposite answers, so
        // an operator who wrote the table has to say which one they meant.
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
        // The whole point of denying unknown keys here: `evn` parses as a
        // perfectly valid table, and without the refusal the override is
        // silently discarded while prod ages out at the global limit.
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

    /// The helm chart renders `config.retention.envs` through `toJson`, so a
    /// values file's `1.9`, a `--set-string`'s `"1.9"`, a `"typo"` and a key
    /// like `prod] #` reach trawld typed and quoted rather than laundered by
    /// `int` into a one-day limit, a keep-forever 0, or an override for
    /// `prod`. This pins the other half of that contract: every one of
    /// those is a refused boot here.
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
        // And the quoted GOOD key is the same env as the bare spelling, so
        // the chart's quoting changes nothing for a valid name.
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

    /// Syslog service names reach WAL filenames verbatim (ADR-0009), so a
    /// path-escaping or unqueryable config value must refuse to start
    /// rather than silently write outside the data tree.
    #[test]
    fn syslog_service_config_is_validated_at_load() {
        let with_syslog = |syslog: &str| {
            Config::from_toml(&format!(
                r#"
[server]
[data]
path = "/data/*.parquet"
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
"192.168.1.1" = "Living Room AP"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("syslog.source_service_map"), "got: {err}");

        with_syslog(
            r#"
default_service = "syslog"

[syslog.source_service_map]
"192.168.1.1" = "unifi-gateway"
"#,
        )
        .expect("valid syslog service names must load");
    }

    // -- [ingest] derivation sources (ADR-0013) ---------------------------
    //
    // Shape only: this crate parses the TOML, trawl-server's
    // `ingest::producer::Derivation::resolve` owns every semantic rule
    // (bare names, `_time` presence, fold collisions, dialect vocabulary,
    // bounds) and is boot-fatal about them.

    #[test]
    fn derivation_sources_default_to_the_packaged_lists() {
        let config = config_with_ingest("");
        let names = |specs: &[DerivationSourceSpec]| -> Vec<String> {
            specs.iter().map(|s| s.field().to_owned()).collect()
        };
        assert_eq!(names(&config.ingest.severity_from), DEFAULT_SEVERITY_FROM);
        assert_eq!(names(&config.ingest.time_from), DEFAULT_TIME_FROM);
        // The shorthand carries no dialect at all — "unset", which the
        // server reads as otel, is distinct from an explicit "otel".
        assert!(
            config
                .ingest
                .severity_from
                .iter()
                .all(|s| s.dialect().is_none())
        );
        // The code default is what `IngestConfig::default()` holds, so a
        // programmatically-built config and a parsed empty one agree.
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
        // A silently-defaulted `dialct` would stop inverting a syslog feed
        // and say nothing, so an unknown key fails the whole parse.
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
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
path = "/data/*.parquet"
[auth]
[ingest]
time_from = [{ dialect = "otel" }]
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err().to_string();
        assert!(err.contains("time_from"), "got: {err}");
    }

    #[test]
    fn derivation_source_rejects_a_non_string_entry() {
        // Neither spelling matches a bare integer, so the untagged enum
        // fails rather than coercing it into a field name.
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
[ingest]
severity_from = [7]
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }
}
