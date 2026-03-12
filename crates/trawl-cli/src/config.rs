//! Config file parsing (`~/.config/trawl/config.toml`).
//!
//! Shared configuration for both CLI and TUI modes.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default server URL.
pub const DEFAULT_URL: &str = "https://localhost:5514";

/// Default config file path.
pub const DEFAULT_CONFIG_PATH: &str = "~/.config/trawl/config.toml";

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
    #[error("no API token found (set TRAWL_TOKEN, use --token, or add token to config)")]
    TokenNotFound,
    #[error("unknown profile '{name}' (available: {available})")]
    UnknownProfile { name: String, available: String },
}

/// Trawl configuration loaded from TOML file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Server connection settings.
    #[serde(default)]
    pub server: ServerConfig,

    /// Named profiles that overlay `[server]` settings.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub profiles: HashMap<String, ProfileConfig>,

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

    /// API token (inline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Accept self-signed TLS certificates.
    #[serde(default)]
    pub insecure: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            url: default_url(),
            token: None,
            insecure: false,
        }
    }
}

/// Named profile — overlays `[server]` settings when selected via `--profile`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// Server URL override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// API token override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// TLS insecure override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
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

    /// Timezone for timestamp display.
    ///
    /// - `"local"` (default) — system timezone via `chrono::Local`
    /// - `"UTC"` — no conversion
    /// - `"+HH:MM"` / `"-HH:MM"` — fixed offset
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            enable_mouse: true,
            auto_save_history: true,
            tab_width: default_tab_width(),
            timezone: default_timezone(),
        }
    }
}

/// Live tail configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailConfig {
    /// Maximum events to keep in memory.
    #[serde(default = "default_max_events")]
    pub max_events: usize,
}

impl Default for TailConfig {
    fn default() -> Self {
        Self {
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

    /// Apply a named profile's overrides to the server config.
    pub fn apply_profile(&mut self, name: &str) -> Result<(), ConfigError> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            let mut available: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
            available.sort_unstable();
            ConfigError::UnknownProfile {
                name: name.to_owned(),
                available: if available.is_empty() {
                    "none".to_owned()
                } else {
                    available.join(", ")
                },
            }
        })?;

        if let Some(ref url) = profile.url {
            self.server.url = url.clone();
        }
        if let Some(ref token) = profile.token {
            self.server.token = Some(token.clone());
        }
        if let Some(insecure) = profile.insecure {
            self.server.insecure = insecure;
        }

        Ok(())
    }

    /// Apply CLI argument overrides.
    pub fn apply_overrides(&mut self, url: Option<String>, insecure: bool) {
        if let Some(u) = url {
            self.server.url = u;
        }
        if insecure {
            self.server.insecure = true;
        }
    }

    /// Load the API token.
    ///
    /// Resolution order:
    /// 1. `direct_token` (from `--token` flag / `TRAWL_TOKEN` env)
    /// 2. Inline token from config (post-profile overlay)
    pub fn load_token(&self, direct_token: Option<&str>) -> Result<String, ConfigError> {
        // Direct token takes precedence (--token flag or TRAWL_TOKEN env).
        if let Some(token) = direct_token {
            return Ok(token.trim().to_owned());
        }

        // Fall back to inline config token.
        if let Some(ref token) = self.server.token {
            return Ok(token.trim().to_owned());
        }

        Err(ConfigError::TokenNotFound)
    }
}

// -- default value functions -------------------------------------------------

fn default_url() -> String {
    DEFAULT_URL.to_owned()
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

const fn default_max_events() -> usize {
    1000
}

fn default_timezone() -> String {
    "local".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let config = Config::default();
        assert_eq!(config.server.url, DEFAULT_URL);
        assert!(config.server.token.is_none());
        assert!(!config.server.insecure);
        assert!(config.profiles.is_empty());
        assert_eq!(config.ui.theme, "default");
        assert!(config.ui.enable_mouse);
        assert!(config.ui.auto_save_history);
        assert_eq!(config.ui.tab_width, 2);
        assert_eq!(config.tail.max_events, 1000);
    }

    #[test]
    fn apply_overrides_updates_config() {
        let mut config = Config::default();
        config.apply_overrides(Some("https://example.com:8443".into()), true);
        assert_eq!(config.server.url, "https://example.com:8443");
        assert!(config.server.insecure);
    }

    #[test]
    fn partial_overrides_preserve_defaults() {
        let mut config = Config::default();
        config.apply_overrides(Some("https://example.com".into()), false);
        assert_eq!(config.server.url, "https://example.com");
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
        assert!(config.server.token.is_none());
        assert_eq!(config.ui.theme, "default");
    }

    #[test]
    fn direct_token_overrides_config_token() {
        let mut config = Config::default();
        config.server.token = Some("config-token".into());
        let token = config.load_token(Some("direct-token")).unwrap();
        assert_eq!(token, "direct-token");
    }

    #[test]
    fn config_token_used_when_no_direct_token() {
        let mut config = Config::default();
        config.server.token = Some("config-token".into());
        let token = config.load_token(None).unwrap();
        assert_eq!(token, "config-token");
    }

    #[test]
    fn config_token_is_trimmed() {
        let mut config = Config::default();
        config.server.token = Some("  spaced-token  ".into());
        let token = config.load_token(None).unwrap();
        assert_eq!(token, "spaced-token");
    }

    #[test]
    fn no_token_returns_error() {
        let config = Config::default();
        assert!(config.load_token(None).is_err());
    }

    #[test]
    fn profile_overlays_server_config() {
        let toml_str = r#"
[server]
url = "https://prod:5514"
token = "prod-token"

[profiles.dev]
url = "https://localhost:5514"
token = "dev-token"
insecure = true
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.apply_profile("dev").unwrap();
        assert_eq!(config.server.url, "https://localhost:5514");
        assert_eq!(config.server.token.as_deref(), Some("dev-token"));
        assert!(config.server.insecure);
    }

    #[test]
    fn partial_profile_preserves_base() {
        let toml_str = r#"
[server]
url = "https://prod:5514"
token = "prod-token"
insecure = false

[profiles.staging]
url = "https://staging:5514"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.apply_profile("staging").unwrap();
        assert_eq!(config.server.url, "https://staging:5514");
        assert_eq!(config.server.token.as_deref(), Some("prod-token"));
        assert!(!config.server.insecure);
    }

    #[test]
    fn profile_insecure_override() {
        let toml_str = r"
[server]
insecure = true

[profiles.prod]
insecure = false
";
        let mut config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.server.insecure);
        config.apply_profile("prod").unwrap();
        assert!(!config.server.insecure);
    }

    #[test]
    fn unknown_profile_returns_error() {
        let toml_str = r#"
[profiles.dev]
url = "https://localhost:5514"

[profiles.staging]
url = "https://staging:5514"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        let err = config.apply_profile("prod").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("prod"),
            "error should mention the requested profile"
        );
        assert!(msg.contains("dev"), "error should list available profiles");
        assert!(
            msg.contains("staging"),
            "error should list available profiles"
        );
    }

    #[test]
    fn unknown_profile_with_no_profiles() {
        let mut config = Config::default();
        let err = config.apply_profile("dev").unwrap_err();
        assert!(err.to_string().contains("none"));
    }

    #[test]
    fn profile_token_overrides_base_token() {
        let toml_str = r#"
[server]
token = "base-token"

[profiles.other]
token = "other-token"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.apply_profile("other").unwrap();
        let token = config.load_token(None).unwrap();
        assert_eq!(token, "other-token");
    }

    #[test]
    fn profiles_toml_round_trips() {
        let toml_str = r#"
[server]
url = "https://prod:5514"
token = "prod-token"

[profiles.dev]
url = "https://localhost:5514"
token = "dev-token"
insecure = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let serialized = toml::to_string(&config).unwrap();
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.profiles.len(), 1);
        let dev = &reparsed.profiles["dev"];
        assert_eq!(dev.url.as_deref(), Some("https://localhost:5514"));
        assert_eq!(dev.token.as_deref(), Some("dev-token"));
        assert_eq!(dev.insecure, Some(true));
    }
}
