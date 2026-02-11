//! Server configuration loading and validation.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level daemon configuration, loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub data: DataConfig,
    pub auth: AuthConfig,
}

/// HTTP listener settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address to bind the HTTP listener (e.g. "127.0.0.1:8080").
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
}

/// Parquet data source settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DataConfig {
    /// Glob path passed to `DuckDB` `read_parquet()` (e.g. "/var/lib/fleet/data/**/*.parquet").
    pub path: String,
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
}
