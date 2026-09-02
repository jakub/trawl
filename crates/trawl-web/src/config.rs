// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Configuration loading for `trawl-web`.
//!
//! The proxy reads the same `config.toml` as trawld, looking at its
//! `[web]` section. Fields absent from that section fall back to defaults
//! picked here so operators can drop the proxy into an existing deployment
//! without touching the daemon's config.
//!
//! When present, the common `FLEET_SESSION_*` runtime variables override only
//! their corresponding cookie settings. When absent, existing production
//! `[web]` key, domain, and secure-cookie configuration remains authoritative.

use std::path::{Path, PathBuf};

use fleet_auth::{
    ENV_SESSION_AEAD_KEY, ENV_SESSION_COOKIE_DOMAIN, ENV_SESSION_COOKIE_SECURE, KEY_LEN,
    RuntimeCookieDomain, SessionKey, SessionRuntimeError, SessionRuntimeOverrides,
};
use trawl_config::{Config, ServerConfig, WebConfig};

/// Default bind address for the proxy HTTP listener.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8090";

/// Upstream URL used when `[web].upstream_url` is unset and the caller
/// supplied no [`ServerConfig`] to derive one from. `[server].http_addr`
/// carries a serde default, so a config loaded from disk always derives its
/// own upstream and never reaches this. The port is the one both packaged
/// deployments give trawld, not `trawl_config::DEFAULT_HTTP_ADDR`.
pub const FALLBACK_UPSTREAM_URL: &str = "https://127.0.0.1:5514";

/// Default session TTL in seconds (24h).
pub const DEFAULT_SESSION_TTL_SECS: u64 = 86_400;

/// Env var name that, when set to a non-empty value, makes the proxy's
/// upstream HTTP client accept self-signed / invalid TLS certificates.
/// Dev-only escape hatch; MUST NOT be set in production.
pub const ENV_INSECURE_UPSTREAM: &str = "TRAWL_WEB_INSECURE_UPSTREAM";

/// Env var name overriding `[web] bind_addr`. Lets a listener move without
/// editing the `trawld.toml` shared with the daemon — `bin/dev` uses it to
/// bind an address the browser's hostname resolves to (remote dev over a
/// tailnet) or to shift off a port collision.
pub const ENV_BIND_ADDR: &str = "TRAWL_WEB_BIND_ADDR";

/// All runtime settings the proxy needs, with defaults applied.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub bind_addr: String,
    pub upstream_url: String,
    pub session_ttl_secs: u64,
    pub allow_insecure_cookies: bool,
    pub insecure_upstream_tls: bool,
    pub cookie_key: SessionKey,
    /// Parent domain for the shared `fleet_session` SSO cookie
    /// (`Domain=` attribute). `None` (unset or empty in config) means
    /// standalone mode — origin-scoped cookie.
    pub shared_domain: Option<String>,
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

    #[error("env var {name} is referenced by config but not set in the environment")]
    EnvMissing { name: String },

    #[error("env var {name} is set but not valid UTF-8")]
    EnvUtf8 { name: String },

    #[error("env var {name} holds an AEAD key that isn't valid base64 or wrong length")]
    EnvKey { name: String },

    #[error("cookie secret file error: {0}")]
    KeyFile(String),

    #[error("env var {name} has an invalid Fleet session value: {reason}")]
    SessionEnvValue {
        name: &'static str,
        reason: &'static str,
    },

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
        Self::from_parsed(&config.web, Some(&config.server))
    }

    /// Resolve from an already-parsed `WebConfig`. `server` is used only
    /// to derive the default upstream URL when `web.upstream_url` is
    /// absent — pass `None` in tests that don't need that resolution.
    ///
    /// # Errors
    /// Returns a [`ConfigError`] when a common Fleet runtime override is
    /// invalid or an explicitly configured production key cannot be loaded.
    /// Returns [`ConfigError::NoKey`] when neither `FLEET_SESSION_AEAD_KEY`
    /// nor a `[web]` cookie-secret source is configured, since cookies would
    /// not survive a restart.
    pub fn from_parsed(
        web: &WebConfig,
        server: Option<&ServerConfig>,
    ) -> Result<Self, ConfigError> {
        let runtime = SessionRuntimeOverrides::from_process_env().map_err(ConfigError::from)?;
        Self::from_parsed_with_runtime(web, server, runtime)
    }

    fn from_parsed_with_runtime(
        web: &WebConfig,
        server: Option<&ServerConfig>,
        runtime: SessionRuntimeOverrides,
    ) -> Result<Self, ConfigError> {
        warn_on_runtime_override(web, &runtime);
        let cookie_key = runtime.key.map_or_else(|| load_key(web), Ok)?;
        let upstream_url = web
            .upstream_url
            .clone()
            .unwrap_or_else(|| default_upstream_from_server(server));
        let allow_insecure_cookies = runtime
            .secure
            .map_or(web.allow_insecure_cookies, |secure| !secure);
        let shared_domain = match runtime.domain {
            RuntimeCookieDomain::PreserveConfigured => {
                // Empty string == unset == standalone mode, so an operator can
                // "comment out" SSO by blanking the value.
                web.shared_domain.clone().filter(|s| !s.is_empty())
            }
            RuntimeCookieDomain::HostOnly => None,
            RuntimeCookieDomain::Explicit(domain) => Some(domain),
        };
        Ok(Self {
            bind_addr: resolve_bind_addr(
                std::env::var(ENV_BIND_ADDR).ok().as_deref(),
                web.bind_addr.as_deref(),
            ),
            upstream_url,
            session_ttl_secs: web.session_ttl_secs.unwrap_or(DEFAULT_SESSION_TTL_SECS),
            allow_insecure_cookies,
            insecure_upstream_tls: std::env::var(ENV_INSECURE_UPSTREAM)
                .is_ok_and(|v| !v.is_empty()),
            cookie_key,
            shared_domain,
        })
    }
}

/// Announce every runtime override that displaces deployed configuration.
///
/// The `FLEET_SESSION_*` variables exist for `fleet-dev`, but this is the
/// production binary and it reads them unconditionally. Silently clearing
/// `Secure` or swapping the cookie key out from under a configured deployment
/// is exactly the accident `load_key` already warns about for the far less
/// dangerous env-versus-path ambiguity.
fn warn_on_runtime_override(web: &WebConfig, runtime: &SessionRuntimeOverrides) {
    if runtime.key.is_some()
        && (web.cookie_secret_env.is_some() || web.cookie_secret_path.is_some())
    {
        tracing::warn!(
            event_type = "session_key_runtime_override",
            env = ENV_SESSION_AEAD_KEY,
            cookie_secret_env = ?web.cookie_secret_env,
            cookie_secret_path = ?web.cookie_secret_path,
            "FLEET_SESSION_AEAD_KEY overrides the configured cookie secret"
        );
    }
    if runtime.secure == Some(false) && !web.allow_insecure_cookies {
        tracing::warn!(
            event_type = "session_cookie_secure_downgraded",
            env = ENV_SESSION_COOKIE_SECURE,
            "the environment is clearing Secure on the session cookie"
        );
    }
    if !matches!(runtime.domain, RuntimeCookieDomain::PreserveConfigured)
        && web.shared_domain.is_some()
    {
        tracing::warn!(
            event_type = "session_cookie_domain_override",
            env = ENV_SESSION_COOKIE_DOMAIN,
            configured = ?web.shared_domain,
            "the environment overrides the configured shared cookie domain"
        );
    }
}

/// Map the shared fleet-auth runtime parser's errors onto [`ConfigError`],
/// so an operator sees one config-error vocabulary whatever produced it.
impl From<SessionRuntimeError> for ConfigError {
    fn from(error: SessionRuntimeError) -> Self {
        match error {
            SessionRuntimeError::NotUnicode { name } => Self::EnvUtf8 {
                name: name.to_owned(),
            },
            SessionRuntimeError::InvalidKey { name } => Self::EnvKey {
                name: name.to_owned(),
            },
            SessionRuntimeError::InvalidValue { name, reason } => {
                Self::SessionEnvValue { name, reason }
            }
        }
    }
}

/// Pick the listen address: [`ENV_BIND_ADDR`] first, then `[web] bind_addr`,
/// then [`DEFAULT_BIND_ADDR`]. Empty values count as unset at both levels.
///
/// Split from `from_parsed` so the precedence is testable without mutating
/// process env (forbidden under `unsafe_code = "forbid"`).
fn resolve_bind_addr(env_value: Option<&str>, configured: Option<&str>) -> String {
    let pick = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
    pick(env_value)
        .or_else(|| pick(configured))
        .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_owned())
}

/// Build the default upstream URL from the trawld `[server].http_addr`.
///
/// Trawld always speaks HTTPS (it auto-generates a self-signed cert if
/// none is configured), so we always produce an `https://` URL.
/// Wildcard bind addresses (`0.0.0.0`, `::`, `[::]`) are rewritten to
/// loopback — the proxy reaches trawld on the same host, never across
/// the wire. IPv6 literals are wrapped in brackets per RFC 3986.
///
/// Reads the address through [`ServerConfig::resolve_http_addr`] rather than
/// the raw field: trawld honours `TRAWL_HTTP_ADDR`, and a proxy still aiming
/// at the file's port would talk to nothing.
fn default_upstream_from_server(server: Option<&ServerConfig>) -> String {
    let Some(srv) = server else {
        return FALLBACK_UPSTREAM_URL.to_owned();
    };

    let resolved = srv.resolve_http_addr();
    let addr = resolved.trim();
    let (host, port_suffix) = split_addr(addr);
    let is_ipv6 = host.contains(':');
    let host = match host {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    // RFC 3986 requires IPv6 literals in URLs to be bracketed.
    if is_ipv6 && host != "127.0.0.1" {
        format!("https://[{host}]{port_suffix}")
    } else {
        format!("https://{host}{port_suffix}")
    }
}

/// Split "host:port" / "[ipv6]:port" / bare host into
/// (host-without-brackets, ":port" or "").
/// Does not validate; malformed input passes through unchanged so an
/// operator with an oddly-formatted addr gets an explanatory reqwest
/// error rather than a silent URL-building mistake.
fn split_addr(addr: &str) -> (&str, &str) {
    // IPv6 literal in brackets, e.g. "[::1]:5514". Strip the brackets
    // for the host component; the caller re-adds them when building the
    // URL.
    if let Some(rest) = addr.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let host = &rest[..end];
        let port = &rest[end + 1..];
        return (host, port);
    }
    // IPv4 or hostname with optional :port.
    addr.rsplit_once(':').map_or((addr, ""), |(host, port)| {
        (host, &addr[host.len()..host.len() + 1 + port.len()])
    })
}

fn load_key(web: &WebConfig) -> Result<SessionKey, ConfigError> {
    // Both sources set → env wins silently by policy. That's a config
    // shape that's easy to set accidentally (e.g. an env var from a
    // secrets provider unexpectedly overlaps with a path configured
    // in the TOML), so emit a loud warning naming both identifiers.
    if web.cookie_secret_env.is_some() && web.cookie_secret_path.is_some() {
        tracing::warn!(
            event_type = "session_key_ambiguous",
            cookie_secret_env = ?web.cookie_secret_env,
            cookie_secret_path = ?web.cookie_secret_path,
            "both cookie_secret_env and cookie_secret_path are set; env takes precedence"
        );
    }

    if let Some(ref env_name) = web.cookie_secret_env {
        // Distinguish "var unset" from "var set but not UTF-8": the
        // former is a config mistake (wrong name, forgotten export),
        // the latter is an encoding issue. One shared "not valid UTF-8"
        // error would point operators at the wrong problem.
        let raw = match std::env::var(env_name) {
            Ok(v) => v,
            Err(std::env::VarError::NotPresent) => {
                return Err(ConfigError::EnvMissing {
                    name: env_name.clone(),
                });
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::EnvUtf8 {
                    name: env_name.clone(),
                });
            }
        };
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
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        assert_eq!(resolved.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(resolved.upstream_url, FALLBACK_UPSTREAM_URL);
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
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        assert_eq!(resolved.bind_addr, "0.0.0.0:9091");
        assert_eq!(resolved.upstream_url, "https://trawld:5514");
        assert_eq!(resolved.session_ttl_secs, 3600);
        assert!(resolved.allow_insecure_cookies);
    }

    #[test]
    fn fleet_runtime_key_is_base64_parsed_and_wins_over_production_sources() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("production.key");
        std::fs::write(&key_path, [0x11; KEY_LEN]).unwrap();
        let web = WebConfig {
            cookie_secret_path: Some(key_path),
            // The canonical runtime key must win before this indirect
            // production source is inspected.
            cookie_secret_env: Some("TRAWL_TEST_MUST_NOT_BE_READ".into()),
            ..WebConfig::default()
        };
        let expected = SessionKey::from_bytes([0x42; KEY_LEN]);
        let encoded = expected.to_base64url();
        let runtime =
            SessionRuntimeOverrides::parse(Some(encoded.to_string()), None, Some("/".into()), None)
                .unwrap();

        let resolved = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime).unwrap();
        assert_eq!(
            resolved.cookie_key.to_base64url().as_str(),
            encoded.as_str()
        );
    }

    #[test]
    fn absent_fleet_runtime_values_preserve_production_cookie_config() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("production.key");
        std::fs::write(&key_path, [0x24; KEY_LEN]).unwrap();
        let web = WebConfig {
            cookie_secret_path: Some(key_path),
            allow_insecure_cookies: false,
            shared_domain: Some(".fleet.lab.ktle.net".into()),
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(None, None, None, None).unwrap();

        let resolved = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime).unwrap();
        assert!(!resolved.allow_insecure_cookies);
        assert_eq!(
            resolved.shared_domain.as_deref(),
            Some(".fleet.lab.ktle.net")
        );
        assert_eq!(
            resolved.cookie_key.to_base64url().as_str(),
            SessionKey::from_bytes([0x24; KEY_LEN])
                .to_base64url()
                .as_str()
        );
    }

    #[test]
    fn localhost_runtime_is_insecure_and_explicitly_host_only() {
        let web = WebConfig {
            // Prove that an empty runtime domain actively clears a production
            // parent-domain setting rather than behaving like "unset".
            shared_domain: Some(".fleet.lab.ktle.net".into()),
            allow_insecure_cookies: false,
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            Some(String::new()),
            Some("/".into()),
            Some("false".into()),
        )
        .unwrap();

        let resolved = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime).unwrap();
        assert!(resolved.allow_insecure_cookies);
        assert!(resolved.shared_domain.is_none());
    }

    #[test]
    fn tailscale_runtime_is_secure_and_host_only() {
        let web = WebConfig {
            shared_domain: Some(".fleet.lab.ktle.net".into()),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            Some(String::new()),
            Some("/".into()),
            Some("true".into()),
        )
        .unwrap();

        let resolved = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime).unwrap();
        assert!(!resolved.allow_insecure_cookies);
        assert!(resolved.shared_domain.is_none());
    }

    #[test]
    fn runtime_domain_can_explicitly_override_production_domain() {
        let web = WebConfig {
            shared_domain: Some(".old.example".into()),
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            Some(".new.example".into()),
            Some("/".into()),
            None,
        )
        .unwrap();

        let resolved = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime).unwrap();
        assert_eq!(resolved.shared_domain.as_deref(), Some(".new.example"));
    }

    #[test]
    fn runtime_parser_errors_keep_the_pre_hoist_config_error_surface() {
        // The exhaustive invalid-value cases live with the parser in
        // fleet-auth; this pins the mapping back onto ConfigError.
        let err = ConfigError::from(SessionRuntimeError::NotUnicode {
            name: ENV_SESSION_AEAD_KEY,
        });
        assert!(matches!(err, ConfigError::EnvUtf8 { name } if name == ENV_SESSION_AEAD_KEY));

        let err = ConfigError::from(SessionRuntimeError::InvalidKey {
            name: ENV_SESSION_AEAD_KEY,
        });
        assert!(matches!(err, ConfigError::EnvKey { name } if name == ENV_SESSION_AEAD_KEY));

        let err = ConfigError::from(SessionRuntimeError::InvalidValue {
            name: ENV_SESSION_COOKIE_DOMAIN,
            reason: "expected empty for host-only or an ASCII DNS domain",
        });
        assert!(matches!(
            err,
            ConfigError::SessionEnvValue { name, .. } if name == ENV_SESSION_COOKIE_DOMAIN
        ));

        // Fail-closed still holds end to end through the fleet-auth parser.
        assert!(
            SessionRuntimeOverrides::parse(Some("not-base64".into()), None, None, None).is_err()
        );
    }

    #[test]
    fn bind_addr_precedence_env_then_config_then_default() {
        assert_eq!(
            resolve_bind_addr(Some("100.87.180.64:8090"), Some("127.0.0.1:9000")),
            "100.87.180.64:8090"
        );
        assert_eq!(
            resolve_bind_addr(None, Some("127.0.0.1:9000")),
            "127.0.0.1:9000"
        );
        assert_eq!(resolve_bind_addr(None, None), DEFAULT_BIND_ADDR);
        // Exported-but-blank is a shell accident, not a bind request — at
        // either level.
        assert_eq!(
            resolve_bind_addr(Some(""), Some("127.0.0.1:9000")),
            "127.0.0.1:9000"
        );
        assert_eq!(resolve_bind_addr(Some(""), Some("")), DEFAULT_BIND_ADDR);
    }

    #[test]
    fn upstream_url_derived_from_server_http_addr() {
        let web = WebConfig::default();
        let srv = ServerConfig {
            http_addr: "127.0.0.1:8080".into(),
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        assert_eq!(resolved.upstream_url, "https://127.0.0.1:8080");
    }

    #[test]
    fn upstream_url_rewrites_wildcard_bind() {
        let web = WebConfig::default();
        let srv = ServerConfig {
            http_addr: "0.0.0.0:5514".into(),
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        assert_eq!(resolved.upstream_url, "https://127.0.0.1:5514");
    }

    #[test]
    fn upstream_url_handles_ipv6_bracketed() {
        let web = WebConfig::default();
        let srv = ServerConfig {
            http_addr: "[::1]:5514".into(),
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        // RFC 3986 requires brackets around IPv6 in URLs.
        assert_eq!(resolved.upstream_url, "https://[::1]:5514");
    }

    #[test]
    fn upstream_url_rewrites_ipv6_wildcard() {
        let web = WebConfig::default();
        let srv = ServerConfig {
            http_addr: "[::]:5514".into(),
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        // `::` wildcard maps to loopback IPv4 — same-host reach, no need
        // for IPv6 at all.
        assert_eq!(resolved.upstream_url, "https://127.0.0.1:5514");
    }

    #[test]
    fn explicit_web_upstream_url_wins_over_server() {
        let web = WebConfig {
            upstream_url: Some("https://trawld.internal:9000".into()),
            ..WebConfig::default()
        };
        let srv = ServerConfig {
            http_addr: "127.0.0.1:8080".into(),
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        assert_eq!(resolved.upstream_url, "https://trawld.internal:9000");
    }

    #[test]
    fn upstream_url_falls_back_when_server_missing() {
        let web = WebConfig::default();
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        assert_eq!(resolved.upstream_url, FALLBACK_UPSTREAM_URL);
    }

    /// Build a `ServerConfig` for tests. The upstream-URL derivation reads
    /// only `http_addr`, but `ServerConfig` implements no `Default`, so it
    /// comes from a TOML parse where every other field takes its serde
    /// default.
    fn dummy_server() -> ServerConfig {
        toml::from_str::<ServerConfig>("http_addr = \"127.0.0.1:8080\"").unwrap()
    }

    #[test]
    fn shared_domain_resolves_when_set() {
        let web = WebConfig {
            shared_domain: Some(".fleet.lab.ktle.net".into()),
            ..WebConfig::default()
        };
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        assert_eq!(
            resolved.shared_domain.as_deref(),
            Some(".fleet.lab.ktle.net")
        );
    }

    #[test]
    fn shared_domain_empty_or_absent_is_none() {
        // Absent → standalone.
        let resolved = ResolvedConfig::from_parsed(&WebConfig::default(), None).unwrap();
        assert!(resolved.shared_domain.is_none());

        // Empty string == unset — lets an operator blank the value to
        // disable SSO without deleting the line.
        let web = WebConfig {
            shared_domain: Some(String::new()),
            ..WebConfig::default()
        };
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        assert!(resolved.shared_domain.is_none());
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
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        let _ = resolved.cookie_key; // successfully loaded
    }
}
