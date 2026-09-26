// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Config file parsing (`~/.config/trawl/config.toml`).
//!
//! Shared configuration for both CLI and TUI modes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    #[error(
        "ca_cert and insecure are both on; insecure turns off the verification that ca_cert pins. \
         Remove one (insecure comes from --insecure, TRAWL_INSECURE, [server], or the profile)"
    )]
    CaCertWithInsecure,
    #[error("ca_cert must be an absolute path or start with ~: {path}")]
    CaCertNotAbsolute { path: String },
    #[error("failed to read ca_cert {path}: {source}")]
    CaCertRead {
        path: String,
        source: std::io::Error,
    },
    #[error("ca_cert {path} is empty")]
    CaCertEmpty { path: String },
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

    /// PEM file of the only CA roots to trust for this server. Absolute or
    /// `~`-prefixed; never combined with `insecure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,

    /// CA roots already read into memory, pinned in place of `ca_cert`.
    /// `-p trial` sets it from the bytes it read and checked once, so no
    /// later step re-opens a path that could have changed. Never read from
    /// or written to the config file.
    #[serde(skip)]
    pub pinned_ca: Option<Vec<u8>>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            url: default_url(),
            token: None,
            insecure: false,
            ca_cert: None,
            pinned_ca: None,
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

    /// CA bundle override. `""` clears an inherited `[server] ca_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
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
        if let Some(ref ca_cert) = profile.ca_cert {
            self.server.ca_cert = if ca_cert.as_os_str().is_empty() {
                None
            } else {
                Some(ca_cert.clone())
            };
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

    /// Resolve how the client trusts the server's certificate
    /// (post-profile, post-override).
    ///
    /// A `ca_cert` is read here, so a missing, unreadable, or empty file
    /// fails before any request. `ca_cert` beside an effective `insecure`
    /// is refused rather than letting either one silently win. In-memory
    /// `pinned_ca` bytes take the place of `ca_cert` and follow the same
    /// `insecure` rule.
    pub fn tls_trust(&self) -> Result<trawl_client::TlsTrust, ConfigError> {
        if let Some(ref pem) = self.server.pinned_ca {
            if self.server.insecure {
                return Err(ConfigError::CaCertWithInsecure);
            }
            return Ok(trawl_client::TlsTrust::PinnedCa(pem.clone()));
        }
        let Some(ref ca_cert) = self.server.ca_cert else {
            return Ok(if self.server.insecure {
                trawl_client::TlsTrust::AcceptInvalid
            } else {
                trawl_client::TlsTrust::System
            });
        };
        if self.server.insecure {
            return Err(ConfigError::CaCertWithInsecure);
        }
        let path = expand_ca_cert(ca_cert)?;
        let pem = std::fs::read(&path).map_err(|source| ConfigError::CaCertRead {
            path: path.display().to_string(),
            source,
        })?;
        if pem.iter().all(u8::is_ascii_whitespace) {
            return Err(ConfigError::CaCertEmpty {
                path: path.display().to_string(),
            });
        }
        Ok(trawl_client::TlsTrust::PinnedCa(pem))
    }

    /// Load the API token.
    ///
    /// Resolution order:
    /// 1. `direct_token` (from `--token` flag / `TRAWL_TOKEN` env)
    /// 2. Inline token from config (post-profile overlay)
    pub fn load_token(&self, direct_token: Option<&str>) -> Result<String, ConfigError> {
        if let Some(token) = direct_token {
            return Ok(token.trim().to_owned());
        }

        if let Some(ref token) = self.server.token {
            return Ok(token.trim().to_owned());
        }

        Err(ConfigError::TokenNotFound)
    }
}

/// Expand a leading `~` and require an absolute result. A relative path
/// would resolve against whatever directory `trawl` happens to run in.
fn expand_ca_cert(path: &Path) -> Result<PathBuf, ConfigError> {
    let expanded = match path.to_str() {
        Some(text) if text.starts_with('~') => PathBuf::from(shellexpand::tilde(text).as_ref()),
        _ => path.to_path_buf(),
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Err(ConfigError::CaCertNotAbsolute {
            path: path.display().to_string(),
        })
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

    // -- ca_cert -------------------------------------------------------------

    use trawl_client::TlsTrust;

    /// A config whose `[server] ca_cert` points at a real temp file.
    fn with_ca_file(extra: &str) -> (tempfile::TempDir, std::path::PathBuf, Config) {
        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("ca.pem");
        std::fs::write(
            &pem,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let toml_str = format!(
            "[server]\nca_cert = {:?}\n{extra}",
            pem.display().to_string()
        );
        let config: Config = toml::from_str(&toml_str).unwrap();
        (dir, pem, config)
    }

    #[test]
    fn ca_cert_parses_on_server_and_profile() {
        let toml_str = r#"
[server]
ca_cert = "/etc/trawl/ca.pem"

[profiles.lab]
ca_cert = "~/lab-ca.pem"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.server.ca_cert.as_deref(),
            Some(Path::new("/etc/trawl/ca.pem"))
        );
        assert_eq!(
            config.profiles["lab"].ca_cert.as_deref(),
            Some(Path::new("~/lab-ca.pem"))
        );
        let reparsed: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(reparsed.server.ca_cert, config.server.ca_cert);
        assert_eq!(
            reparsed.profiles["lab"].ca_cert,
            config.profiles["lab"].ca_cert
        );
    }

    #[test]
    fn profile_ca_cert_overlays_server() {
        let toml_str = r#"
[server]
ca_cert = "/etc/trawl/ca.pem"

[profiles.lab]
ca_cert = "/etc/trawl/lab-ca.pem"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.apply_profile("lab").unwrap();
        assert_eq!(
            config.server.ca_cert.as_deref(),
            Some(Path::new("/etc/trawl/lab-ca.pem"))
        );
    }

    #[test]
    fn profile_inherits_server_ca_cert() {
        let toml_str = r#"
[server]
ca_cert = "/etc/trawl/ca.pem"

[profiles.lab]
url = "https://lab:5514"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.apply_profile("lab").unwrap();
        assert_eq!(
            config.server.ca_cert.as_deref(),
            Some(Path::new("/etc/trawl/ca.pem"))
        );
    }

    #[test]
    fn empty_profile_ca_cert_clears_the_inherited_one() {
        let toml_str = r#"
[server]
ca_cert = "/etc/trawl/ca.pem"

[profiles.public]
ca_cert = ""
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.apply_profile("public").unwrap();
        assert_eq!(config.server.ca_cert, None);
        assert_eq!(config.tls_trust().unwrap(), TlsTrust::System);
    }

    #[test]
    fn relative_ca_cert_is_refused() {
        for path in ["ca.pem", "./certs/ca.pem", "~other/ca.pem", ""] {
            let mut config = Config::default();
            config.server.ca_cert = Some(path.into());
            let err = config.tls_trust().unwrap_err();
            assert!(
                matches!(err, ConfigError::CaCertNotAbsolute { .. }),
                "{path:?} gave {err:?}"
            );
        }
    }

    #[test]
    fn tilde_ca_cert_expands_to_home() {
        let expanded = expand_ca_cert(Path::new("~/trawl/ca.pem")).unwrap();
        assert!(expanded.is_absolute());
        assert!(expanded.ends_with("trawl/ca.pem"));
        assert!(!expanded.to_string_lossy().contains('~'));
    }

    #[test]
    fn ca_cert_with_insecure_is_refused() {
        // [server] insecure
        let (_dir, _, config) = with_ca_file("insecure = true\n");
        assert!(matches!(
            config.tls_trust(),
            Err(ConfigError::CaCertWithInsecure)
        ));

        // --insecure / TRAWL_INSECURE
        let (_dir, _, mut config) = with_ca_file("");
        config.apply_overrides(None, true);
        assert!(matches!(
            config.tls_trust(),
            Err(ConfigError::CaCertWithInsecure)
        ));

        // A profile's insecure beside an inherited ca_cert.
        let (_dir, _, mut config) = with_ca_file("[profiles.dev]\ninsecure = true\n");
        config.apply_profile("dev").unwrap();
        assert!(matches!(
            config.tls_trust(),
            Err(ConfigError::CaCertWithInsecure)
        ));
    }

    #[test]
    fn ca_cert_reads_the_bundle() {
        let (_dir, pem, config) = with_ca_file("");
        assert_eq!(
            config.tls_trust().unwrap(),
            TlsTrust::PinnedCa(std::fs::read(pem).unwrap())
        );
    }

    #[test]
    fn missing_or_empty_ca_cert_fails_before_any_request() {
        let (dir, pem, mut config) = with_ca_file("");
        std::fs::write(&pem, " \n").unwrap();
        assert!(matches!(
            config.tls_trust(),
            Err(ConfigError::CaCertEmpty { .. })
        ));

        config.server.ca_cert = Some(dir.path().join("absent.pem"));
        assert!(matches!(
            config.tls_trust(),
            Err(ConfigError::CaCertRead { .. })
        ));
    }

    /// In-memory roots are pinned as they are, with no file behind them,
    /// and refuse `insecure` exactly as `ca_cert` does.
    #[test]
    fn pinned_ca_bytes_pin_without_a_file() {
        let mut config = Config::default();
        config.server.pinned_ca = Some(b"pem bytes".to_vec());
        assert_eq!(
            config.tls_trust().unwrap(),
            TlsTrust::PinnedCa(b"pem bytes".to_vec())
        );
        let toml = toml::to_string(&config).unwrap();
        assert!(!toml.contains("pinned_ca"), "never serialized: {toml}");

        config.apply_overrides(None, true);
        assert!(matches!(
            config.tls_trust(),
            Err(ConfigError::CaCertWithInsecure)
        ));
    }

    #[test]
    fn without_ca_cert_insecure_picks_the_trust() {
        let mut config = Config::default();
        assert_eq!(config.tls_trust().unwrap(), TlsTrust::System);
        config.apply_overrides(None, true);
        assert_eq!(config.tls_trust().unwrap(), TlsTrust::AcceptInvalid);
    }
}
