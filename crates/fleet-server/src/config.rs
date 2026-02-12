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
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            enabled: default_ingest_enabled(),
            max_body_bytes: default_ingest_max_body_bytes(),
            wal_dir: None,
            compaction_interval_secs: default_compaction_interval_secs(),
        }
    }
}

fn default_ingest_enabled() -> bool {
    true
}

fn default_ingest_max_body_bytes() -> usize {
    16 * 1024 * 1024 // 16 MB
}

fn default_compaction_interval_secs() -> u64 {
    10
}

/// Authentication database settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// Path to the `SQLite` auth database.
    pub db_path: PathBuf,
}

fn default_http_addr() -> String {
    "127.0.0.1:8080".to_owned()
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_max_concurrent_queries() -> usize {
    num_cpus()
}

fn default_max_result_rows() -> usize {
    100_000
}

fn default_max_request_body_bytes() -> usize {
    128 * 1024 // 128 KB
}

fn default_max_concurrent_requests() -> usize {
    256
}

fn default_shutdown_drain_secs() -> u64 {
    30
}

fn default_tls_reload_interval_secs() -> u64 {
    300 // 5 minutes
}

/// Portable CPU count without pulling in the `num_cpus` crate.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4)
}

/// Expand a leading `~/` to `$HOME/`.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    }
    path.to_owned()
}

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// All paths in the config are resolved (tilde-expanded) after parsing.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::Io {
            path: path.as_ref().to_owned(),
            source: e,
        })?;
        let mut config: Self = toml::from_str(&contents).map_err(|e| ConfigError::Parse {
            path: path.as_ref().to_owned(),
            source: e,
        })?;
        config.resolve_paths();
        config.validate()?;
        Ok(config)
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
        warns
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
}
