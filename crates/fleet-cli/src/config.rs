//! Config file parsing (`~/.config/fleet/config.toml`).
//!
//! Shared configuration for both CLI and TUI modes.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default server URL.
pub const DEFAULT_URL: &str = "https://localhost:5514";

/// Default token file path.
pub const DEFAULT_TOKEN_FILE: &str = "~/.config/fleet/token";

/// Default config file path.
pub const DEFAULT_CONFIG_PATH: &str = "~/.config/fleet/config.toml";

/// Configuration error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("no API token found (set FLEET_TOKEN, use --token, or create {DEFAULT_TOKEN_FILE})")]
    TokenNotFound,
}

/// Fleet configuration loaded from TOML file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Server connection settings.
    #[serde(default)]
    pub server: ServerConfig,

    /// UI preferences.
    #[serde(default)]
    pub ui: UiConfig,

    /// Live tail settings.
    #[serde(default)]
    pub tail: TailConfig,
}

/// Server connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Server URL.
    #[serde(default = "default_url")]
    pub url: String,

    /// Path to API token file.
    #[serde(default = "default_token_file")]
    pub token_file: String,

    /// Accept self-signed TLS certificates.
    #[serde(default)]
    pub insecure: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            url: default_url(),
            token_file: default_token_file(),
            insecure: false,
        }
    }
}

/// UI preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Theme name or path to custom theme file.
    #[serde(default = "default_theme")]
    pub theme: String,

    /// Enable mouse support.
    #[serde(default = "default_true")]
    pub enable_mouse: bool,

    /// Auto-save query history.
    #[serde(default = "default_true")]
    pub auto_save_history: bool,

    /// Tab width in spaces.
    #[serde(default = "default_tab_width")]
    pub tab_width: usize,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            enable_mouse: true,
            auto_save_history: true,
            tab_width: default_tab_width(),
        }
    }
}

/// Live tail configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailConfig {
    /// Default interval between queries (seconds).
    #[serde(default = "default_tail_interval")]
    pub default_interval_secs: u64,

    /// Maximum events to keep in memory.
    #[serde(default = "default_max_events")]
    pub max_events: usize,
}

impl Default for TailConfig {
    fn default() -> Self {
        Self {
            default_interval_secs: default_tail_interval(),
            max_events: default_max_events(),
        }
    }
}

impl Config {
    /// Load config from file, using defaults if the file doesn't exist.
    pub fn load(path: Option<&str>) -> Result<Self, ConfigError> {
        let config_path = path.unwrap_or(DEFAULT_CONFIG_PATH);
        let expanded = shellexpand::tilde(config_path);
        let path = Path::new(expanded.as_ref());

        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        })?;

        let config: Self = toml::from_str(&contents).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;

        Ok(config)
    }

    /// Apply CLI argument overrides.
    pub fn apply_overrides(
        &mut self,
        url: Option<String>,
        token_file: Option<String>,
        insecure: bool,
    ) {
        if let Some(u) = url {
            self.server.url = u;
        }
        if let Some(t) = token_file {
            self.server.token_file = t;
        }
        if insecure {
            self.server.insecure = true;
        }
    }

    /// Apply environment variable overrides.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(url) = std::env::var("FLEET_URL") {
            self.server.url = url;
        }
        if let Ok(token) = std::env::var("FLEET_TOKEN_FILE") {
            self.server.token_file = token;
        }
    }

    /// Get the expanded token file path.
    pub fn token_path(&self) -> PathBuf {
        let expanded = shellexpand::tilde(&self.server.token_file);
        PathBuf::from(expanded.as_ref())
    }

    /// Load the API token.
    ///
    /// Resolution order:
    /// 1. `direct_token` (from `--token` flag / `FLEET_TOKEN` env)
    /// 2. Token file (from `--token-file` flag / config / default)
    pub fn load_token(&self, direct_token: Option<&str>) -> Result<String, ConfigError> {
        // Direct token takes precedence (--token flag or FLEET_TOKEN env).
        if let Some(token) = direct_token {
            return Ok(token.trim().to_owned());
        }

        // Fall back to token file.
        let path = self.token_path();
        let token = std::fs::read_to_string(&path).map_err(|_| ConfigError::TokenNotFound)?;
        Ok(token.trim().to_owned())
    }
}

// -- default value functions -------------------------------------------------

fn default_url() -> String {
    DEFAULT_URL.to_owned()
}

fn default_token_file() -> String {
    DEFAULT_TOKEN_FILE.to_owned()
}

fn default_theme() -> String {
    "default".to_owned()
}

const fn default_true() -> bool {
    true
}

const fn default_tab_width() -> usize {
    2
}

const fn default_tail_interval() -> u64 {
    5
}

const fn default_max_events() -> usize {
    1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let config = Config::default();
        assert_eq!(config.server.url, DEFAULT_URL);
        assert_eq!(config.server.token_file, DEFAULT_TOKEN_FILE);
        assert!(!config.server.insecure);
        assert_eq!(config.ui.theme, "default");
        assert!(config.ui.enable_mouse);
        assert!(config.ui.auto_save_history);
        assert_eq!(config.ui.tab_width, 2);
        assert_eq!(config.tail.default_interval_secs, 5);
        assert_eq!(config.tail.max_events, 1000);
    }

    #[test]
    fn apply_overrides_updates_config() {
        let mut config = Config::default();
        config.apply_overrides(
            Some("https://example.com:8443".into()),
            Some("/tmp/token".into()),
            true,
        );
        assert_eq!(config.server.url, "https://example.com:8443");
        assert_eq!(config.server.token_file, "/tmp/token");
        assert!(config.server.insecure);
    }

    #[test]
    fn partial_overrides_preserve_defaults() {
        let mut config = Config::default();
        config.apply_overrides(Some("https://example.com".into()), None, false);
        assert_eq!(config.server.url, "https://example.com");
        assert_eq!(config.server.token_file, DEFAULT_TOKEN_FILE);
        assert!(!config.server.insecure);
    }

    #[test]
    fn toml_serialization_round_trips() {
        let config = Config::default();
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.server.url, parsed.server.url);
        assert_eq!(config.ui.theme, parsed.ui.theme);
    }

    #[test]
    fn minimal_toml_deserializes_with_defaults() {
        let toml_str = r#"
[server]
url = "https://custom:9000"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.url, "https://custom:9000");
        assert_eq!(config.server.token_file, DEFAULT_TOKEN_FILE);
        assert_eq!(config.ui.theme, "default");
    }

    #[test]
    fn direct_token_takes_precedence() {
        let config = Config::default();
        let token = config.load_token(Some("direct-token-value")).unwrap();
        assert_eq!(token, "direct-token-value");
    }

    #[test]
    fn direct_token_is_trimmed() {
        let config = Config::default();
        let token = config.load_token(Some("  spaced-token  ")).unwrap();
        assert_eq!(token, "spaced-token");
    }
}
