// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Configuration loading for `trawl-web`.
//!
//! The proxy reads the SAME `config.toml` as trawld, looking at its
//! `[web]` section. Fields absent from that section fall back to sensible
//! defaults picked here so operators can drop the proxy into an existing
//! deployment without touching the daemon's config.
//!
//! When present, the common `FLEET_SESSION_*` runtime variables override only
//! their corresponding cookie settings. When absent, existing production
//! `[web]` key, domain, and secure-cookie configuration remains authoritative.

use std::path::{Path, PathBuf};

use fleet_auth::{
    ENV_SESSION_AEAD_KEY, ENV_SESSION_COOKIE_DOMAIN, ENV_SESSION_COOKIE_PATH,
    ENV_SESSION_COOKIE_SECURE, KEY_LEN, SessionKey,
};
use trawl_config::{Config, ServerConfig, WebConfig};
use zeroize::Zeroizing;

/// Default bind address for the proxy HTTP listener.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8090";

/// Fallback upstream URL used only if the loaded config exposes no
/// `[server].http_addr` AND `[web].upstream_url` is also unset — i.e.
/// nearly never. Matches the homelab/dev convention documented in
/// CLAUDE.md.
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
    pub coastwatch_url: Option<String>,
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
    /// With neither source, the existing ephemeral-key fallback is preserved.
    pub fn from_parsed(
        web: &WebConfig,
        server: Option<&ServerConfig>,
    ) -> Result<Self, ConfigError> {
        let runtime = SessionRuntimeOverrides::from_process_env()?;
        Self::from_parsed_with_runtime(web, server, runtime)
    }

    fn from_parsed_with_runtime(
        web: &WebConfig,
        server: Option<&ServerConfig>,
        runtime: SessionRuntimeOverrides,
    ) -> Result<Self, ConfigError> {
        let cookie_key = runtime.key.map_or_else(|| load_key(web), Ok)?;
        let upstream_url = web
            .upstream_url
            .clone()
            .unwrap_or_else(|| default_upstream_from_server(server));
        let allow_insecure_cookies = runtime
            .secure
            .map_or(web.allow_insecure_cookies, |secure| !secure);
        let shared_domain = match runtime.domain {
            RuntimeDomain::PreserveConfigured => {
                // Empty string == unset == standalone mode, so an operator can
                // "comment out" SSO by blanking the value.
                web.shared_domain.clone().filter(|s| !s.is_empty())
            }
            RuntimeDomain::HostOnly => None,
            RuntimeDomain::Explicit(domain) => Some(domain),
        };
        Ok(Self {
            bind_addr: resolve_bind_addr(
                std::env::var(ENV_BIND_ADDR).ok().as_deref(),
                web.bind_addr.as_deref(),
            ),
            upstream_url,
            coastwatch_url: web.coastwatch_url.clone(),
            session_ttl_secs: web.session_ttl_secs.unwrap_or(DEFAULT_SESSION_TTL_SECS),
            allow_insecure_cookies,
            insecure_upstream_tls: std::env::var(ENV_INSECURE_UPSTREAM)
                .is_ok_and(|v| !v.is_empty()),
            cookie_key,
            shared_domain,
        })
    }
}

/// Parsed common Fleet session environment.
///
/// `domain` deliberately has three states; see [`RuntimeDomain`].
#[derive(Debug)]
struct SessionRuntimeOverrides {
    key: Option<SessionKey>,
    domain: RuntimeDomain,
    secure: Option<bool>,
}

#[derive(Debug)]
enum RuntimeDomain {
    /// Runtime variable absent: preserve application configuration.
    PreserveConfigured,
    /// Runtime variable present and empty: force a host-only cookie.
    HostOnly,
    /// Runtime variable supplies an explicit `Domain=` value.
    Explicit(String),
}

impl SessionRuntimeOverrides {
    fn from_process_env() -> Result<Self, ConfigError> {
        Self::parse(
            read_optional_env(ENV_SESSION_AEAD_KEY)?,
            read_optional_env(ENV_SESSION_COOKIE_DOMAIN)?,
            read_optional_env(ENV_SESSION_COOKIE_PATH)?,
            read_optional_env(ENV_SESSION_COOKIE_SECURE)?,
        )
    }

    fn parse(
        key: Option<String>,
        domain: Option<String>,
        path: Option<String>,
        secure: Option<String>,
    ) -> Result<Self, ConfigError> {
        let key = key
            .map(Zeroizing::new)
            .map(|raw| {
                SessionKey::from_base64(raw.as_str()).map_err(|_| ConfigError::EnvKey {
                    name: ENV_SESSION_AEAD_KEY.to_owned(),
                })
            })
            .transpose()?;

        if let Some(path) = path
            && path != "/"
        {
            return Err(ConfigError::SessionEnvValue {
                name: ENV_SESSION_COOKIE_PATH,
                reason: "only the fleet-wide path `/` is supported",
            });
        }

        let secure = secure
            .map(|value| match value.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(ConfigError::SessionEnvValue {
                    name: ENV_SESSION_COOKIE_SECURE,
                    reason: "expected exactly `true` or `false`",
                }),
            })
            .transpose()?;

        let domain = match domain {
            None => RuntimeDomain::PreserveConfigured,
            Some(domain) if domain.is_empty() => RuntimeDomain::HostOnly,
            Some(domain) => {
                validate_runtime_cookie_domain(&domain)?;
                RuntimeDomain::Explicit(domain)
            }
        };

        Ok(Self {
            key,
            domain,
            secure,
        })
    }
}

fn read_optional_env(name: &'static str) -> Result<Option<String>, ConfigError> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::EnvUtf8 {
            name: name.to_owned(),
        }),
    }
}

/// Validate a runtime-provided cookie domain before it can reach a response
/// header. The leading dot accepted by existing production configuration is
/// permitted; the remaining value must be a conventional ASCII DNS name.
fn validate_runtime_cookie_domain(domain: &str) -> Result<(), ConfigError> {
    let bare = domain.strip_prefix('.').unwrap_or(domain);
    let valid = !bare.is_empty()
        && bare.len() <= 253
        && !bare.ends_with('.')
        && bare.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        });
    if valid {
        Ok(())
    } else {
        Err(ConfigError::SessionEnvValue {
            name: ENV_SESSION_COOKIE_DOMAIN,
            reason: "expected empty for host-only or an ASCII DNS domain",
        })
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
    // shape that's easy to set accidentally (e.g. env var from a
    // secrets provider unexpectedly overlaps with a path configured
    // in the TOML), so emit a loud warning with both identifiers.
    // Documented precedence in the field-level rustdoc on WebConfig.
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
        // the latter is an encoding issue. Mapping both to a single
        // "not valid UTF-8" error sent operators down the wrong path.
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
    fn invalid_fleet_session_runtime_values_fail_closed() {
        let bad_key = SessionRuntimeOverrides::parse(Some("not-base64".into()), None, None, None)
            .unwrap_err();
        assert!(matches!(bad_key, ConfigError::EnvKey { name } if name == ENV_SESSION_AEAD_KEY));

        for path in ["", "/app", "//"] {
            let err =
                SessionRuntimeOverrides::parse(None, None, Some(path.into()), None).unwrap_err();
            assert!(matches!(
                err,
                ConfigError::SessionEnvValue { name, .. } if name == ENV_SESSION_COOKIE_PATH
            ));
        }

        for secure in ["TRUE", "1", "", "yes"] {
            let err =
                SessionRuntimeOverrides::parse(None, None, None, Some(secure.into())).unwrap_err();
            assert!(matches!(
                err,
                ConfigError::SessionEnvValue { name, .. } if name == ENV_SESSION_COOKIE_SECURE
            ));
        }

        for domain in [
            ".",
            "https://fleet.example",
            "-fleet.example",
            "fleet..example",
            "fleet.example:8444",
            "fleet.example\r\nx-injected: yes",
        ] {
            let err =
                SessionRuntimeOverrides::parse(None, Some(domain.into()), None, None).unwrap_err();
            assert!(matches!(
                err,
                ConfigError::SessionEnvValue { name, .. } if name == ENV_SESSION_COOKIE_DOMAIN
            ));
        }
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

    /// Build a minimally populated `ServerConfig` for tests. Most fields
    /// aren't exercised by the upstream-URL derivation but the struct
    /// doesn't implement `Default` — see trawl-server's config module.
    fn dummy_server() -> ServerConfig {
        // `toml::from_str` with only mandatory fields gives us a fully
        // defaulted ServerConfig (serde defaults fill everything else).
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
