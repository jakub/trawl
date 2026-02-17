//! Server configuration loading and validation.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level daemon configuration, loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub data: DataConfig,
    pub auth: AuthConfig,
    #[serde(default)]
    pub ingest: IngestConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
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
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,

    /// Maximum concurrent HTTP requests (default: 256).
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,

    /// Graceful shutdown drain timeout in seconds (default: 30).
    #[serde(default = "default_shutdown_drain_secs")]
    pub shutdown_drain_secs: u64,

    /// Optional log file path. When set, logs are written to both stdout and this file.
    pub log_file: Option<PathBuf>,

    /// Path to TLS certificate (PEM). If omitted, a self-signed cert is auto-generated.
    pub tls_cert_path: Option<PathBuf>,

    /// Path to TLS private key (PEM). If omitted, a self-signed key is auto-generated.
    pub tls_key_path: Option<PathBuf>,

    /// How often to check cert files for changes, in seconds (default: 300).
    /// Set to 0 to disable automatic cert reload.
    #[serde(default = "default_tls_reload_interval_secs")]
    pub tls_reload_interval_secs: u64,

    /// Allowed CORS origins (e.g. `["https://fleet.example.com"]`).
    /// Empty list (default) means no CORS headers are sent, so the browser's
    /// same-origin policy blocks all cross-origin requests.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    /// Schema cache TTL in seconds (default: 60). Controls how long
    /// `GET /schema` results are cached before re-introspecting.
    #[serde(default = "default_schema_cache_ttl_secs")]
    pub schema_cache_ttl_secs: u64,

    /// Maximum number of completed queries kept in the history ring buffer
    /// (default: 1000). Visible via `GET /queries` (admin only).
    #[serde(default = "default_max_query_history")]
    pub max_query_history: usize,

    /// Maximum concurrent SSE streaming connections (default: 32).
    /// Returns 429 when exceeded to prevent resource exhaustion.
    #[serde(default = "default_max_sse_connections")]
    pub max_sse_connections: usize,

    /// Per-role rate limiting (requests per minute). 0 = disabled.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}

/// Per-role rate limits in requests per minute.
///
/// Each role gets an independent rate limiter keyed by API key prefix.
/// Set a value to 0 to disable rate limiting for that role.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    /// Admin rate limit (requests/minute). Default: 100.
    #[serde(default = "default_rate_admin")]
    pub admin: u32,
    /// Analyst rate limit (requests/minute). Default: 60.
    #[serde(default = "default_rate_analyst")]
    pub analyst: u32,
    /// Reader rate limit (requests/minute). Default: 30.
    #[serde(default = "default_rate_reader")]
    pub reader: u32,
    /// Ingest rate limit (requests/minute). Default: 1000.
    #[serde(default = "default_rate_ingest")]
    pub ingest: u32,
}

/// Parquet data source settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DataConfig {
    /// Directory containing parquet files (e.g. "/var/lib/fleet/data").
    ///
    /// Accepts either a bare directory path or a glob pattern for backwards
    /// compatibility. If the path contains glob characters (`*`, `?`, `[`),
    /// they are stripped to derive the base directory.
    pub path: String,
}

impl DataConfig {
    /// The base directory where parquet files live.
    pub fn base_dir(&self) -> PathBuf {
        if self.has_glob() {
            // Legacy glob path — strip glob components.
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

    /// Whether the configured path contains glob characters.
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
    #[serde(default = "default_ingest_max_body_bytes")]
    pub max_body_bytes: usize,

    /// WAL directory path. Defaults to `{data.base_dir}/wal/`.
    pub wal_dir: Option<PathBuf>,

    /// How often the compaction task runs (seconds). Default: 10.
    #[serde(default = "default_compaction_interval_secs")]
    pub compaction_interval_secs: u64,

    /// Write internal server events to the ingest pipeline as `service:fleetd`.
    /// Enables querying server telemetry via the fleet DSL for dashboards
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
    /// Default: 100 MB.
    #[serde(default = "default_hot_buffer_max_bytes")]
    pub hot_buffer_max_bytes: usize,
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
        }
    }
}

/// Data retention policy settings.
///
/// Both policies are always-on with sensible defaults. Set either to 0
/// to disable that specific policy. If both are 0, the retention task
/// spawns but performs no deletions.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionConfig {
    /// Delete date directories older than this many days. 0 = disabled.
    #[serde(default = "default_retention_max_age_days")]
    pub max_age_days: u64,

    /// If free disk space drops below this many bytes, delete oldest
    /// data first regardless of age. 0 = disabled.
    #[serde(default = "default_retention_min_free_disk_bytes")]
    pub min_free_disk_bytes: u64,

    /// How often the retention task runs (seconds).
    #[serde(default = "default_retention_interval_secs")]
    pub retention_interval_secs: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_age_days: DEFAULT_RETENTION_MAX_AGE_DAYS,
            min_free_disk_bytes: DEFAULT_RETENTION_MIN_FREE_DISK_BYTES,
            retention_interval_secs: DEFAULT_RETENTION_INTERVAL_SECS,
        }
    }
}

// -- default constants -------------------------------------------------------
// Centralized so they can be referenced from other modules (e.g. http.rs
// fallback) and grepped easily. The `default_*` functions exist only because
// serde's `#[serde(default = "...")]` requires a function path.

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
/// Default retention max age (days).
pub const DEFAULT_RETENTION_MAX_AGE_DAYS: u64 = 90;
/// Default retention minimum free disk space (bytes). 1 GiB.
pub const DEFAULT_RETENTION_MIN_FREE_DISK_BYTES: u64 = 1_073_741_824;
/// Default retention check interval (seconds). 1 hour.
pub const DEFAULT_RETENTION_INTERVAL_SECS: u64 = 3600;
/// Default admin rate limit (requests/minute).
pub const DEFAULT_RATE_ADMIN: u32 = 100;
/// Default analyst rate limit (requests/minute).
pub const DEFAULT_RATE_ANALYST: u32 = 60;
/// Default reader rate limit (requests/minute).
pub const DEFAULT_RATE_READER: u32 = 30;
/// Default ingest rate limit (requests/minute).
pub const DEFAULT_RATE_INGEST: u32 = 1000;
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

fn default_event_bus_capacity() -> usize {
    crate::bus::DEFAULT_EVENT_BUS_CAPACITY
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

/// Default key audit polling interval (seconds).
pub const DEFAULT_AUDIT_INTERVAL_SECS: u64 = 30;

/// Default auth cache TTL (seconds).
pub const DEFAULT_AUTH_CACHE_TTL_SECS: u64 = 300;

fn default_auth_cache_ttl_secs() -> u64 {
    DEFAULT_AUTH_CACHE_TTL_SECS
}

/// Authentication database settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// Path to the `SQLite` auth database.
    pub db_path: PathBuf,

    /// How often to poll the auth database for key changes (seconds).
    /// Detects keys created/revoked by fleet-admin and emits audit events.
    /// Set to 0 to disable. Default: 30.
    #[serde(default = "default_audit_interval_secs")]
    pub audit_interval_secs: u64,

    /// TTL for the in-memory auth token cache (seconds). Verified tokens
    /// skip argon2id on cache hits. Revoked keys stay valid for up to
    /// this duration. Set to 0 to disable caching. Default: 300 (5 min).
    #[serde(default = "default_auth_cache_ttl_secs")]
    pub auth_cache_ttl_secs: u64,
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

fn default_max_sse_connections() -> usize {
    DEFAULT_MAX_SSE_CONNECTIONS
}

fn default_rate_admin() -> u32 {
    DEFAULT_RATE_ADMIN
}

fn default_rate_analyst() -> u32 {
    DEFAULT_RATE_ANALYST
}

fn default_rate_reader() -> u32 {
    DEFAULT_RATE_READER
}

fn default_rate_ingest() -> u32 {
    DEFAULT_RATE_INGEST
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
            admin: DEFAULT_RATE_ADMIN,
            analyst: DEFAULT_RATE_ANALYST,
            reader: DEFAULT_RATE_READER,
            ingest: DEFAULT_RATE_INGEST,
        }
    }
}

/// Portable CPU count without pulling in the `num_cpus` crate.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(FALLBACK_CPU_COUNT)
}

/// Expand a leading `~/` to `$HOME/`.
fn expand_tilde(path: &str) -> String {
    shellexpand::tilde(path).into_owned()
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
        self.auth.db_path = PathBuf::from(expand_tilde(&self.auth.db_path.to_string_lossy()));
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
    }

    /// Resolve the WAL directory: explicit config value or `{data.base_dir}/wal/`.
    pub fn wal_dir(&self) -> PathBuf {
        self.ingest
            .wal_dir
            .clone()
            .unwrap_or_else(|| self.data.base_dir().join("wal"))
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
                 server events now flow through the ingest pipeline as service:fleetd"
                    .into(),
            );
        }
        if self.ingest.internal_telemetry && !self.ingest.enabled {
            warns.push(
                "internal_telemetry requires ingest to be enabled — telemetry disabled".into(),
            );
        }
        warns
    }

    /// Whether internal telemetry is effectively enabled.
    ///
    /// Requires both `ingest.enabled` and `ingest.internal_telemetry` to be true.
    pub fn internal_telemetry_enabled(&self) -> bool {
        self.ingest.enabled && self.ingest.internal_telemetry
    }

    /// Validate configuration values.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.data.path.is_empty() {
            return Err(ConfigError::Validation("data.path cannot be empty".into()));
        }

        if self.auth.db_path.as_os_str().is_empty() {
            return Err(ConfigError::Validation(
                "auth.db_path cannot be empty".into(),
            ));
        }

        if self.server.max_concurrent_queries == 0 {
            return Err(ConfigError::Validation(
                "server.max_concurrent_queries must be > 0".into(),
            ));
        }

        if self.server.tls_cert_path.is_some() != self.server.tls_key_path.is_some() {
            return Err(ConfigError::Validation(
                "tls_cert_path and tls_key_path must both be set or both omitted".into(),
            ));
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
path = "/var/lib/fleet/data/**/*.parquet"

[auth]
db_path = "/var/lib/fleet/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.http_addr, "127.0.0.1:8080");
        assert_eq!(config.server.timeout_secs, 30);
        assert!(config.server.max_concurrent_queries > 0);
        assert_eq!(config.data.path, "/var/lib/fleet/data/**/*.parquet");
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
log_file = "/var/log/fleetd.log"

[data]
path = "/data/**/*.parquet"

[auth]
db_path = "~/.fleet/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.http_addr, "0.0.0.0:9090");
        assert_eq!(config.server.timeout_secs, 60);
        assert_eq!(config.server.max_concurrent_queries, 8);
        assert_eq!(
            config.server.log_file.as_deref(),
            Some(std::path::Path::new("/var/log/fleetd.log"))
        );
    }

    #[test]
    fn validation_rejects_empty_data_path() {
        let toml = r#"
[server]
[data]
path = ""
[auth]
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("data.path"));
    }

    #[test]
    fn validation_rejects_empty_auth_path() {
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
db_path = ""
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("auth.db_path"));
    }

    #[test]
    fn validation_rejects_zero_concurrency() {
        let toml = r#"
[server]
max_concurrent_queries = 0
[data]
path = "/data/*.parquet"
[auth]
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_concurrent_queries"));
    }

    #[test]
    fn parse_tls_config() {
        let toml = r#"
[server]
tls_cert_path = "/etc/fleet/cert.pem"
tls_key_path = "/etc/fleet/key.pem"
[data]
path = "/data/*.parquet"
[auth]
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.server.tls_cert_path.as_deref(),
            Some(std::path::Path::new("/etc/fleet/cert.pem"))
        );
        assert_eq!(
            config.server.tls_key_path.as_deref(),
            Some(std::path::Path::new("/etc/fleet/key.pem"))
        );
    }

    #[test]
    fn validation_rejects_partial_tls_config() {
        let toml = r#"
[server]
tls_cert_path = "/etc/fleet/cert.pem"
[data]
path = "/data/*.parquet"
[auth]
db_path = "/tmp/auth.db"
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
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let warns = config.warnings();
        assert!(warns.iter().any(|w| w.contains("self-signed")));
    }

    #[test]
    fn no_warning_when_tls_cert_configured() {
        let toml = r#"
[server]
tls_cert_path = "/etc/fleet/cert.pem"
tls_key_path = "/etc/fleet/key.pem"
[data]
path = "/data/*.parquet"
[auth]
db_path = "/tmp/auth.db"
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
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.server.cors_allowed_origins.is_empty());
    }

    #[test]
    fn base_dir_strips_glob() {
        let data = DataConfig {
            path: "/var/lib/fleet/data/**/*.parquet".into(),
        };
        assert_eq!(data.base_dir(), std::path::Path::new("/var/lib/fleet/data"));
    }

    #[test]
    fn base_dir_bare_directory() {
        let data = DataConfig {
            path: "/var/lib/fleet/data".into(),
        };
        assert_eq!(data.base_dir(), std::path::Path::new("/var/lib/fleet/data"));
    }

    #[test]
    fn parquet_glob_from_directory() {
        let data = DataConfig {
            path: "/var/lib/fleet/data".into(),
        };
        assert_eq!(data.parquet_glob(), "/var/lib/fleet/data/**/*.parquet");
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
            path: "/var/lib/fleet/data/".into(),
        };
        assert_eq!(data.parquet_glob(), "/var/lib/fleet/data/**/*.parquet");
    }

    #[test]
    fn ingest_defaults_when_omitted() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
db_path = "/tmp/auth.db"
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
cors_allowed_origins = ["https://fleet.example.com", "https://admin.example.com"]
[data]
path = "/data/*.parquet"
[auth]
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.cors_allowed_origins.len(), 2);
        assert_eq!(
            config.server.cors_allowed_origins[0],
            "https://fleet.example.com"
        );
    }

    #[test]
    fn rate_limit_defaults() {
        let toml = r#"
[server]
[data]
path = "/data/*.parquet"
[auth]
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.rate_limit.admin, 100);
        assert_eq!(config.server.rate_limit.analyst, 60);
        assert_eq!(config.server.rate_limit.reader, 30);
        assert_eq!(config.server.rate_limit.ingest, 1000);
    }

    #[test]
    fn rate_limit_custom_values() {
        let toml = r#"
[server]
[server.rate_limit]
admin = 200
analyst = 0
reader = 10
ingest = 500
[data]
path = "/data/*.parquet"
[auth]
db_path = "/tmp/auth.db"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.server.rate_limit.admin, 200);
        assert_eq!(config.server.rate_limit.analyst, 0);
        assert_eq!(config.server.rate_limit.reader, 10);
        assert_eq!(config.server.rate_limit.ingest, 500);
    }

    #[test]
    fn internal_telemetry_defaults_to_true() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
db_path = "/tmp/auth.db"
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
db_path = "/tmp/auth.db"
[ingest]
internal_telemetry = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.ingest.internal_telemetry);
        assert!(!config.internal_telemetry_enabled());
    }

    #[test]
    fn internal_telemetry_requires_ingest_enabled() {
        let toml = r#"
[server]
[data]
path = "/data"
[auth]
db_path = "/tmp/auth.db"
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
log_file = "/var/log/fleetd.log"
[data]
path = "/data"
[auth]
db_path = "/tmp/auth.db"
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
db_path = "/tmp/auth.db"
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
db_path = "/tmp/auth.db"
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
db_path = "/tmp/auth.db"
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
db_path = "/tmp/auth.db"
[retention]
max_age_days = 0
min_free_disk_bytes = 0
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.retention.max_age_days, 0);
        assert_eq!(config.retention.min_free_disk_bytes, 0);
    }
}
