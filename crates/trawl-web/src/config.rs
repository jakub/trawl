// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Configuration loading for `trawl-web`.
//!
//! The proxy reads the SAME `config.toml` as trawld, looking at its
//! `[web]` section. Fields absent from that section fall back to sensible
//! defaults picked here so operators can drop the proxy into an existing
//! deployment without touching the daemon's config.

use std::path::{Path, PathBuf};

use trawl_server::config::{Config, WebConfig};

use crate::session::{KEY_LEN, SessionKey};

/// Default bind address for the proxy HTTP listener.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8090";

/// Default upstream URL. Matches the `dev` profile's trawld.
pub const DEFAULT_UPSTREAM_URL: &str = "https://127.0.0.1:5514";

/// Default session TTL in seconds (24h).
pub const DEFAULT_SESSION_TTL_SECS: u64 = 86_400;

/// Env var name that, when set to a non-empty value, makes the proxy's
/// upstream HTTP client accept self-signed / invalid TLS certificates.
/// Dev-only escape hatch; MUST NOT be set in production.
pub const ENV_INSECURE_UPSTREAM: &str = "TRAWL_WEB_INSECURE_UPSTREAM";

/// All runtime settings the proxy needs, with defaults applied.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub bind_addr: String,
    pub upstream_url: String,
    pub session_ttl_secs: u64,
    pub allow_insecure_cookies: bool,
    pub insecure_upstream_tls: bool,
    pub cookie_key: SessionKey,
}

/// Errors while loading or validating proxy configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse config file {path}: {source}")]
    ParseFile {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("env var {name} is set but not valid UTF-8")]
    EnvUtf8 { name: String },

    #[error("env var {name} holds an AEAD key that isn't valid base64 or wrong length")]
    EnvKey { name: String },

    #[error("cookie secret file error: {0}")]
    KeyFile(String),

    #[error(
        "no cookie secret configured: set `web.cookie_secret_path` or `web.cookie_secret_env` in config.toml (a random key will otherwise be generated on every startup, invalidating sessions)"
    )]
    NoKey,
}

impl ResolvedConfig {
    /// Load and resolve settings from a config file path.
    ///
    /// # Errors
    /// Returns `ConfigError` variants for read/parse failures or unusable
    /// cookie-key configuration.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path).map_err(|e| ConfigError::ReadFile {
            path: path.to_owned(),
            source: e,
        })?;
        let config: Config = toml::from_str(&contents).map_err(|e| ConfigError::ParseFile {
            path: path.to_owned(),
            source: e,
        })?;
        Self::from_parsed(&config.web)
    }

    /// Resolve from an already-parsed `WebConfig`. Useful in tests.
    ///
    /// # Errors
    /// Returns `ConfigError::NoKey` if neither `cookie_secret_path` nor
    /// `cookie_secret_env` is set (cookies would not survive restart).
    pub fn from_parsed(web: &WebConfig) -> Result<Self, ConfigError> {
        let cookie_key = load_key(web)?;
        Ok(Self {
            bind_addr: web
                .bind_addr
                .clone()
                .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_owned()),
            upstream_url: web
                .upstream_url
                .clone()
                .unwrap_or_else(|| DEFAULT_UPSTREAM_URL.to_owned()),
            session_ttl_secs: web.session_ttl_secs.unwrap_or(DEFAULT_SESSION_TTL_SECS),
            allow_insecure_cookies: web.allow_insecure_cookies,
            insecure_upstream_tls: std::env::var(ENV_INSECURE_UPSTREAM)
                .is_ok_and(|v| !v.is_empty()),
            cookie_key,
        })
    }
}

fn load_key(web: &WebConfig) -> Result<SessionKey, ConfigError> {
    if let Some(ref env_name) = web.cookie_secret_env {
        let raw = std::env::var(env_name).map_err(|_| ConfigError::EnvUtf8 {
            name: env_name.clone(),
        })?;
        return SessionKey::from_base64(&raw).map_err(|_| ConfigError::EnvKey {
            name: env_name.clone(),
        });
    }

    if let Some(ref path) = web.cookie_secret_path {
        let expanded = shellexpand::tilde(&path.to_string_lossy()).into_owned();
        return SessionKey::from_file(Path::new(&expanded))
            .map_err(|e| ConfigError::KeyFile(e.to_string()));
    }

    // No source configured — generate one but loudly warn. Sessions won't
    // survive a proxy restart, so this is only reasonable for dev.
    tracing::warn!(
        event_type = "session_key_ephemeral",
        key_len = KEY_LEN,
        "no cookie_secret configured; generated an ephemeral key. sessions will not survive restart."
    );
    Ok(SessionKey::generate())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_applied_when_web_section_empty() {
        let web = WebConfig::default();
        // Will generate ephemeral key (prints a warning) — that's fine in tests.
        let resolved = ResolvedConfig::from_parsed(&web).unwrap();
        assert_eq!(resolved.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(resolved.upstream_url, DEFAULT_UPSTREAM_URL);
        assert_eq!(resolved.session_ttl_secs, DEFAULT_SESSION_TTL_SECS);
        assert!(!resolved.allow_insecure_cookies);
    }

    #[test]
    fn explicit_fields_win() {
        let web = WebConfig {
            bind_addr: Some("0.0.0.0:9091".into()),
            upstream_url: Some("https://trawld:5514".into()),
            session_ttl_secs: Some(3600),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        let resolved = ResolvedConfig::from_parsed(&web).unwrap();
        assert_eq!(resolved.bind_addr, "0.0.0.0:9091");
        assert_eq!(resolved.upstream_url, "https://trawld:5514");
        assert_eq!(resolved.session_ttl_secs, 3600);
        assert!(resolved.allow_insecure_cookies);
    }

    #[test]
    fn key_from_file_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("web.key");
        std::fs::write(&key_path, [0x42u8; KEY_LEN]).unwrap();

        let web = WebConfig {
            cookie_secret_path: Some(key_path),
            ..WebConfig::default()
        };
        let resolved = ResolvedConfig::from_parsed(&web).unwrap();
        let _ = resolved.cookie_key; // successfully loaded
    }
}
