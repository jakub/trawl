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
//!
//! One setting has no default at all: `[web] public_origins`, the CSRF
//! allowlist (ADR-0016). Resolution fails and the proxy does not start
//! when neither the config file nor `FLEET_SESSION_PUBLIC_ORIGINS` states a
//! browser-visible origin, because every alternative is worse. Deriving one
//! from `bind_addr` guesses at what the browser's address bar says, and
//! treating an empty list as "allow everything" installs the vulnerability
//! this allowlist exists to close, silently.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use fleet_auth::{
    ENV_SESSION_AEAD_KEY, ENV_SESSION_COOKIE_DOMAIN, ENV_SESSION_COOKIE_SECURE,
    ENV_SESSION_PUBLIC_ORIGINS, KEY_LEN, PublicOrigins, PublicOriginsError, RuntimeCookieDomain,
    SessionKey, SessionRuntimeError, SessionRuntimeOverrides,
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
    /// The browser-visible origins cookie-authenticated requests may come
    /// from (ADR-0016).
    ///
    /// Non-`Option` and non-empty by construction: [`PublicOrigins`] has no
    /// way to build an empty list, so every holder of a `ResolvedConfig`
    /// knows the operator stated an origin and the guard is a plain
    /// membership test with no "unset means allow" branch to forget.
    pub public_origins: PublicOrigins,
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
        source: trawl_config::ConfigError,
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

    /// The configured browser-origin allowlist is empty or unusable.
    ///
    /// The wrapped error names the rule (empty list, which entry failed to
    /// parse, which two entries are the same origin); this variant appends
    /// the two doors that set it: an operator told the list is empty, on a
    /// machine where a package owns the config file, needs to know the
    /// environment can supply it too.
    #[error(
        "{source} (set it in `[web] public_origins` in config.toml, or in the \
         {ENV_SESSION_PUBLIC_ORIGINS} environment variable)"
    )]
    PublicOrigins {
        #[from]
        source: PublicOriginsError,
    },

    /// `FLEET_SESSION_PUBLIC_ORIGINS` was set but does not parse.
    ///
    /// Carried whole rather than re-worded: the shared runtime parser's
    /// message already names the variable, the failing entry's index and
    /// the rule that refused it, and a second wording here would be a
    /// second vocabulary for one failure.
    #[error(transparent)]
    SessionEnvOrigins(SessionRuntimeError),
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
        let config = Config::parse_toml(&contents).map_err(|e| ConfigError::ParseFile {
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
        // The environment REPLACES the file's list, never merges with it: a
        // merged allowlist would keep a stale config entry authorizing an
        // origin the operator believes they moved away from.
        let public_origins = match runtime.public_origins {
            Some(from_environment) => from_environment,
            None => PublicOrigins::parse(&web.public_origins)?,
        };
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
            public_origins,
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
    if let Some(from_environment) = &runtime.public_origins
        && !web.public_origins.is_empty()
    {
        // Both packaged deployments render one list into the config file
        // and hand the same list to the environment — the helm chart
        // injects the variable whenever the sidecar runs, so the
        // `config.raw` escape hatch stays covered — so warning on the
        // variable's mere presence fires on every default install and
        // teaches operators to scroll past the one line that says their
        // allowlist is not the one they wrote. Compare the two instead.
        // A configured list that does not parse counts as displaced: it
        // could never have been in force, and that is worth saying.
        let displaced = !PublicOrigins::parse(&web.public_origins)
            .is_ok_and(|configured| same_origins(&configured, from_environment));
        // The override's origins are printed because they parsed: each is
        // at most a serialized origin's worth of ASCII, and the point of
        // the line is telling the operator which allowlist is in force.
        // Their COUNT is its own field and the text is capped, because a
        // list long enough to bury the message is a list nobody reads.
        let (origins_count, origins) = summarize_origins(from_environment);
        if displaced {
            // The configured entries stay counted rather than printed:
            // this arm covers the list that failed to parse, whose text is
            // unvalidated and unbounded.
            tracing::warn!(
                event_type = "session_public_origins_override",
                env = ENV_SESSION_PUBLIC_ORIGINS,
                configured_entries = web.public_origins.len(),
                origins_count,
                origins = %origins,
                "the environment replaces the configured browser-origin allowlist"
            );
        } else {
            tracing::info!(
                event_type = "session_public_origins_override_matched",
                env = ENV_SESSION_PUBLIC_ORIGINS,
                origins_count,
                origins = %origins,
                "the environment restates the configured browser-origin allowlist"
            );
        }
    }
}

/// Whether two allowlists hold the same origins, order aside.
///
/// [`PublicOrigins::parse`] refuses two entries that normalize alike, so
/// neither list can repeat an origin and equal lengths plus one-way
/// containment is set equality. Both sides went through the same parser, so
/// `https://x:443` in the file and `https://x` in the environment are the
/// same origin here exactly as they are at the guard.
fn same_origins(left: &PublicOrigins, right: &PublicOrigins) -> bool {
    left.iter().count() == right.iter().count() && left.iter().all(|origin| right.contains(origin))
}

/// How many origins one diagnostic line names before it stops listing.
///
/// A deployment states one or two origins; eight is well past what anyone
/// reads off a startup line, and the list is operator-supplied with no
/// ceiling on its length, so the line needs one of its own.
const MAX_LOGGED_ORIGINS: usize = 8;

/// Summarize an allowlist for one log line: how many origins there are,
/// and the first few of them.
///
/// Two fields rather than one string, because the count is the fact an
/// operator checks (did my list load?) and the text is the sample that
/// tells them which list loaded. The text goes through `Display` on each
/// [`fleet_auth::Origin`] rather than the operator's own strings, so it
/// shows what the guard will actually compare against: `https://x:443` in
/// the file shows as `https://x`, which is what a browser sends.
pub fn summarize_origins(origins: &PublicOrigins) -> (usize, String) {
    let count = origins.iter().count();
    let mut text = origins
        .iter()
        .take(MAX_LOGGED_ORIGINS)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if let Some(hidden) = count.checked_sub(MAX_LOGGED_ORIGINS).filter(|n| *n > 0) {
        write!(text, ",… and {hidden} more").expect("writing to a String cannot fail");
    }
    (count, text)
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
            // Both origin failures already carry the variable name, the
            // entry index and the parser's rule, so they travel whole.
            error @ (SessionRuntimeError::InvalidOrigin { .. }
            | SessionRuntimeError::DuplicateOrigin { .. }) => Self::SessionEnvOrigins(error),
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

    /// The origin every fixture below states as the browser-visible one.
    const TEST_ORIGIN: &str = "https://trawl.example.com";

    #[test]
    fn file_load_errors_identify_paths_without_config_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trawld.toml");
        for (document, expected) in [
            (
                "[server]\n[data]\npath='/data'\n[web]\ncoastwatch_url='private-secret'",
                "web.coastwatch_url",
            ),
            (
                "[server]\n[data]\npath='/data'\n[web]\nsession_ttl_secs='private-secret'",
                "web.session_ttl_secs",
            ),
            (
                "[server]\n[data]\npath='/data'\n[web]\nupstream_url='private-secret",
                "invalid TOML syntax",
            ),
        ] {
            std::fs::write(&path, document).unwrap();
            let error = ResolvedConfig::load(&path).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!error.to_string().contains("private-secret"));
            assert!(!format!("{error:?}").contains("private-secret"));
        }
    }

    #[test]
    fn file_load_resolves_the_current_web_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trawld.toml");
        let key_path = directory.path().join("session.key");
        std::fs::write(&key_path, [0x42u8; KEY_LEN]).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"
[server]
http_addr = "127.0.0.1:5514"
[data]
path = "~/data"
[web]
public_origins = ["https://trawl.example.com"]
cookie_secret_path = "{}"
session_ttl_secs = 3600
"#,
                key_path.display()
            ),
        )
        .unwrap();
        let resolved = ResolvedConfig::load(&path).unwrap();
        assert_eq!(resolved.upstream_url, "https://127.0.0.1:5514");
        assert_eq!(resolved.session_ttl_secs, 3600);
        assert_eq!(
            resolved.cookie_key.to_base64url().as_str(),
            SessionKey::from_bytes([0x42; KEY_LEN])
                .to_base64url()
                .as_str()
        );
    }

    #[test]
    fn a_long_allowlist_is_summarized_by_count_and_a_capped_sample() {
        // Nothing bounds how many origins an operator states, so the
        // startup line states the number and shows a sample rather than
        // pasting the whole list into one field.
        let entries: Vec<String> = (0..20)
            .map(|i| format!("https://o{i}.example.com"))
            .collect();
        let origins = PublicOrigins::parse(&entries).expect("20 distinct origins parse");

        let (count, text) = summarize_origins(&origins);
        assert_eq!(count, 20, "the count is exact whatever the text shows");
        let (shown, tail) = text.rsplit_once(',').expect("the suffix follows a comma");
        assert_eq!(shown.split(',').count(), 8, "{text}");
        assert!(shown.starts_with("https://o0.example.com"), "{text}");
        assert!(shown.ends_with("https://o7.example.com"), "{text}");
        assert!(!text.contains("https://o8.example.com"), "{text}");
        assert_eq!(tail, "… and 12 more");
    }

    #[test]
    fn a_short_allowlist_is_shown_whole_with_no_suffix() {
        let origins =
            PublicOrigins::parse(["https://trawl.example.com:443", "http://localhost:8090"])
                .expect("two origins parse");
        let (count, text) = summarize_origins(&origins);
        assert_eq!(count, 2);
        // Normalized, so the line shows what the guard compares against.
        assert_eq!(text, "https://trawl.example.com,http://localhost:8090");
    }

    /// A `[web]` section carrying the one setting that has no default.
    ///
    /// `public_origins` is required (ADR-0016), so a bare
    /// `WebConfig::default()` no longer resolves. That refusal is the
    /// feature, and these fixtures state an origin the way a deployment
    /// must. Spelled as a function so `..configured_web()` leaves every
    /// other field at its `WebConfig` default.
    fn configured_web() -> WebConfig {
        WebConfig {
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..WebConfig::default()
        }
    }

    #[test]
    fn defaults_applied_when_web_section_carries_only_the_required_origin() {
        let web = configured_web();
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
            ..configured_web()
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
            ..configured_web()
        };
        let expected = SessionKey::from_bytes([0x42; KEY_LEN]);
        let encoded = expected.to_base64url();
        let runtime = SessionRuntimeOverrides::parse(
            Some(encoded.to_string()),
            None,
            Some("/".into()),
            None,
            None,
        )
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
            ..configured_web()
        };
        let runtime = SessionRuntimeOverrides::parse(None, None, None, None, None).unwrap();

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
            ..configured_web()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            Some(String::new()),
            Some("/".into()),
            Some("false".into()),
            None,
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
            ..configured_web()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            Some(String::new()),
            Some("/".into()),
            Some("true".into()),
            None,
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
            ..configured_web()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            Some(".new.example".into()),
            Some("/".into()),
            None,
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
            SessionRuntimeOverrides::parse(Some("not-base64".into()), None, None, None, None)
                .is_err()
        );
    }

    // -- the browser-origin allowlist (ADR-0016) ---------------------------

    #[test]
    fn an_empty_allowlist_refuses_to_resolve_and_names_both_doors() {
        // The loud half of ADR-0016: there is no derived default, so a
        // deployment that says nothing does not start. The message has to
        // name both places an operator can say it, because a packaged
        // install may own the config file while the environment is the
        // only thing the operator controls.
        let web = WebConfig::default();
        let runtime = SessionRuntimeOverrides::parse(None, None, None, None, None).unwrap();
        let error = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime)
            .expect_err("an empty public_origins list must refuse to resolve");

        assert!(matches!(
            error,
            ConfigError::PublicOrigins {
                source: PublicOriginsError::Empty
            }
        ));
        let message = error.to_string();
        assert!(message.contains("[web] public_origins"), "got: {message}");
        assert!(
            message.contains(ENV_SESSION_PUBLIC_ORIGINS),
            "got: {message}"
        );
    }

    #[test]
    fn a_bad_configured_entry_is_refused_at_its_own_index() {
        // The whole list is refused, never the working subset: a typo that
        // silently dropped one origin shows up much later as a mysterious
        // 403 for whoever browses to it.
        let web = WebConfig {
            public_origins: vec![
                TEST_ORIGIN.to_owned(),
                "https://trawl.example.com/app".to_owned(),
            ],
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(None, None, None, None, None).unwrap();
        let error = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime)
            .expect_err("a path is not an origin");

        assert!(matches!(
            error,
            ConfigError::PublicOrigins {
                source: PublicOriginsError::Entry { index: 1, .. }
            }
        ));
        let message = error.to_string();
        assert!(message.contains("entry 1"), "got: {message}");
    }

    #[test]
    fn a_configured_list_resolves_to_the_normalized_origins() {
        let web = WebConfig {
            // The default port is written out here and must normalize
            // away, because the browser will not send it.
            public_origins: vec!["https://trawl.example.com:443".to_owned()],
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(None, None, None, None, None).unwrap();
        let resolved = ResolvedConfig::from_parsed_with_runtime(&web, None, runtime).unwrap();

        let expected = fleet_auth::Origin::parse(TEST_ORIGIN).unwrap();
        assert!(resolved.public_origins.contains(&expected));
    }

    #[test]
    fn the_environment_replaces_the_configured_allowlist_and_says_so() {
        let web = WebConfig {
            public_origins: vec![TEST_ORIGIN.to_owned(), "http://localhost:8090".to_owned()],
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            None,
            None,
            None,
            Some("http://localhost:8081".to_owned()),
        )
        .unwrap();

        let (resolved, lines) =
            captured_resolution(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
        let resolved = resolved.unwrap();

        // Replacement, not a merge: the configured origins are gone.
        assert!(
            resolved
                .public_origins
                .contains(&fleet_auth::Origin::parse("http://localhost:8081").unwrap())
        );
        assert!(
            !resolved
                .public_origins
                .contains(&fleet_auth::Origin::parse(TEST_ORIGIN).unwrap()),
            "the environment replaces the file's list, it does not add to it"
        );

        let warning = lines
            .iter()
            .find(|line| line.contains("session_public_origins_override"))
            .unwrap_or_else(|| panic!("displacement must warn; got: {lines:?}"));
        assert!(warning.starts_with("WARN"), "got: {warning}");
        assert!(
            warning.contains(ENV_SESSION_PUBLIC_ORIGINS),
            "got: {warning}"
        );
        // The count of displaced entries, and the origins now in force.
        assert!(warning.contains("configured_entries=2"), "got: {warning}");
        assert!(
            warning.contains("origins=http://localhost:8081"),
            "got: {warning}"
        );
        // Never the operator's unparsed configured text.
        assert!(!warning.contains(TEST_ORIGIN), "got: {warning}");
    }

    #[test]
    fn an_environment_list_matching_the_file_is_noted_at_info_not_warned() {
        // What every default helm install does: the chart renders the list
        // into the TOML and hands the same list to the sidecar's
        // environment. Nothing is displaced, so the displacement warning
        // must stay quiet — otherwise it fires at every pod start and
        // stops meaning anything. Order differs and one entry writes out
        // the default port, both of which the parser normalizes away.
        let web = WebConfig {
            public_origins: vec![
                "http://localhost:8090".to_owned(),
                "https://trawl.example.com:443".to_owned(),
            ],
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            None,
            None,
            None,
            Some(format!("{TEST_ORIGIN},http://localhost:8090")),
        )
        .unwrap();

        let (resolved, lines) =
            captured_resolution(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
        assert!(resolved.is_ok());
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("WARN") && line.contains("public_origins")),
            "the lists match, nothing was displaced: {lines:?}"
        );
        let noted = lines
            .iter()
            .find(|line| line.contains("session_public_origins_override_matched"))
            .unwrap_or_else(|| panic!("the match itself is worth one line; got: {lines:?}"));
        assert!(noted.starts_with("INFO"), "got: {noted}");
        assert!(noted.contains("origins_count=2"), "got: {noted}");
    }

    #[test]
    fn an_unparseable_configured_list_is_a_displacement_and_warns() {
        // The file states an allowlist that could never have been in
        // force. The environment's list is what runs, which is exactly the
        // displacement the warning exists for — and the only hint the
        // operator gets that their config file is broken.
        let web = WebConfig {
            public_origins: vec!["https://trawl.example.com/app".to_owned()],
            ..WebConfig::default()
        };
        let runtime = SessionRuntimeOverrides::parse(
            None,
            None,
            None,
            None,
            Some("http://localhost:8081".to_owned()),
        )
        .unwrap();

        let (resolved, lines) =
            captured_resolution(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
        assert!(
            resolved.is_ok(),
            "the environment's list is valid, so startup proceeds"
        );

        let warning = lines
            .iter()
            .find(|line| line.contains("session_public_origins_override"))
            .unwrap_or_else(|| panic!("a broken configured list is displaced; got: {lines:?}"));
        assert!(warning.starts_with("WARN"), "got: {warning}");
        assert!(warning.contains("configured_entries=1"), "got: {warning}");
        // Still never the operator's unparsed text, least of all here.
        assert!(!warning.contains("/app"), "got: {warning}");
    }

    #[test]
    fn the_environment_alone_satisfies_the_requirement_without_warning() {
        // Nothing is displaced when the file says nothing, so this is the
        // normal dev-stack path and must be quiet.
        let web = WebConfig::default();
        let runtime = SessionRuntimeOverrides::parse(
            None,
            None,
            None,
            None,
            Some("http://localhost:8081".to_owned()),
        )
        .unwrap();

        let (resolved, lines) =
            captured_resolution(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
        assert!(resolved.is_ok());
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("session_public_origins_override")),
            "nothing was displaced: {lines:?}"
        );
    }

    #[test]
    fn a_bad_environment_entry_fails_startup_naming_the_variable_and_index() {
        // Driven through the parser and the error mapping rather than
        // `from_parsed`, which reads the real process environment: this
        // crate forbids `unsafe`, so a test cannot set a variable, and
        // these two steps are exactly what startup does with the value.
        let error = SessionRuntimeOverrides::parse(
            None,
            None,
            None,
            None,
            Some("http://localhost:8081,not-an-origin".to_owned()),
        )
        .map(|_| ())
        .map_err(ConfigError::from)
        .expect_err("a bad entry must fail startup, not be skipped");

        assert!(matches!(error, ConfigError::SessionEnvOrigins(_)));
        let message = error.to_string();
        assert!(
            message.contains(ENV_SESSION_PUBLIC_ORIGINS),
            "got: {message}"
        );
        assert!(message.contains("entry 1"), "got: {message}");
    }

    /// Collects one formatted `field=value` line per event.
    ///
    /// Same shape as fleet-auth's guard-log capture: the warning is the
    /// deliverable here, so the test reads what was recorded rather than
    /// trusting that the code meant to record it.
    #[derive(Clone, Default)]
    struct CaptureLayer {
        lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct FieldWriter<'a>(&'a mut String);

    impl tracing::field::Visit for FieldWriter<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={value} ", field.name());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    impl<S> tracing_subscriber::Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut line = format!("{} ", event.metadata().level());
            event.record(&mut FieldWriter(&mut line));
            self.lines.lock().expect("capture mutex").push(line);
        }
    }

    /// Serializes the capture tests against each other.
    ///
    /// `tracing` caches each callsite's `Interest` process-wide and
    /// rebuilds that cache when a subscriber registers or dies. Two
    /// capture tests running at once can leave the warning's callsite
    /// cached as "never interested" for the thread about to emit, so the
    /// event vanishes and the test reads "it did not warn", the exact
    /// failure it exists to catch, arriving at random. Poisoning is
    /// ignored on purpose: one panicking test must not cascade into the
    /// others.
    static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Resolve configuration under a capturing subscriber, returning the
    /// result and every line it logged.
    fn captured_resolution<T>(resolve: impl FnOnce() -> T) -> (T, Vec<String>) {
        use tracing_subscriber::layer::SubscriberExt as _;

        let _serialized = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let capture = CaptureLayer::default();
        let lines = std::sync::Arc::clone(&capture.lines);
        let subscriber = tracing_subscriber::registry().with(capture);
        let _guard = tracing::subscriber::set_default(subscriber);
        let outcome = resolve();
        let recorded = lines.lock().expect("capture mutex").clone();
        (outcome, recorded)
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
        let web = configured_web();
        let srv = ServerConfig {
            http_addr: "127.0.0.1:8080".into(),
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        assert_eq!(resolved.upstream_url, "https://127.0.0.1:8080");
    }

    #[test]
    fn upstream_url_rewrites_wildcard_bind() {
        let web = configured_web();
        let srv = ServerConfig {
            http_addr: "0.0.0.0:5514".into(),
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        assert_eq!(resolved.upstream_url, "https://127.0.0.1:5514");
    }

    #[test]
    fn upstream_url_handles_ipv6_bracketed() {
        let web = configured_web();
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
        let web = configured_web();
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
            ..configured_web()
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
        let web = configured_web();
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
            ..configured_web()
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
        let resolved = ResolvedConfig::from_parsed(&configured_web(), None).unwrap();
        assert!(resolved.shared_domain.is_none());

        // Empty string == unset — lets an operator blank the value to
        // disable SSO without deleting the line.
        let web = WebConfig {
            shared_domain: Some(String::new()),
            ..configured_web()
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
            ..configured_web()
        };
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        let _ = resolved.cookie_key; // successfully loaded
    }
}
