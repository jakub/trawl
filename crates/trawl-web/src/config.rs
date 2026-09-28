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

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::net::SocketAddr;
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

/// Env var name overriding `[web] bind_addr`. Lets a listener move without
/// editing the `trawld.toml` shared with the daemon — `bin/dev` uses it to
/// bind an address the browser's hostname resolves to (remote dev over a
/// tailnet) or to shift off a port collision.
pub const ENV_BIND_ADDR: &str = "TRAWL_WEB_BIND_ADDR";

/// Env var name overriding `[web] upstream_ca_path`. Lets a process pin
/// trawld's certificate without editing the `trawld.toml` shared with the
/// daemon — `bin/dev` uses it to pin the certificate the development
/// trawld generates. An empty value counts as unset, as for
/// [`ENV_BIND_ADDR`], so the file's setting applies.
pub const ENV_UPSTREAM_CA_PATH: &str = "TRAWL_WEB_UPSTREAM_CA_PATH";

/// All runtime settings the proxy needs, with defaults applied.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub bind_addr: String,
    /// The `https` URL the proxy reaches trawld at. Resolution refused any
    /// other scheme and any user name or password, so the value is safe to
    /// log.
    pub upstream_url: String,
    pub session_ttl_secs: u64,
    pub allow_insecure_cookies: bool,
    pub upstream_tls: UpstreamTls,
    /// Where the connection to trawld goes when `[web]
    /// upstream_connect_addr` is set. `None`: the `upstream_url` host is
    /// resolved normally.
    pub upstream_connect: Option<UpstreamConnect>,
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

/// How the proxy's upstream client decides whether to trust trawld's
/// TLS certificate. Both modes verify the chain and the host name
/// (ADR-0048).
pub enum UpstreamTls {
    /// The platform trust store.
    System,
    /// Only the roots read from `[web] upstream_ca_path`. No platform or
    /// built-in root is trusted.
    PinnedCa {
        /// The pin file, tilde-expanded, kept so it can be read again.
        path: PathBuf,
        /// The certificates the file held when it was read, or `None` when
        /// it did not exist yet: trawld writes its generated certificate
        /// only on its first start, which may come after trawl-web's.
        roots: Option<PinnedRoots>,
    },
}

/// A digest of a pin file's contents and the trust anchors they parsed to.
///
/// The digest stays with the certificates so a later read of the file can
/// tell whether it changed since the client was built from it. Only
/// [`pinned_roots`] builds one.
pub struct PinnedRoots {
    digest: blake3::Hash,
    certificates: Vec<reqwest::Certificate>,
}

impl PinnedRoots {
    /// The trust anchors, in file order.
    #[must_use]
    pub fn certificates(&self) -> &[reqwest::Certificate] {
        &self.certificates
    }

    /// The BLAKE3 digest of the file contents the certificates came from.
    pub(crate) fn digest(&self) -> blake3::Hash {
        self.digest
    }
}

/// Shows the count only, like [`UpstreamTls`]'s `Debug`.
impl std::fmt::Debug for PinnedRoots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} certificates>", self.certificates.len())
    }
}

/// Hand-written so a pinned bundle shows its size, not its certificates.
impl std::fmt::Debug for UpstreamTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str("System"),
            Self::PinnedCa { path, roots } => {
                let mut pinned = f.debug_struct("PinnedCa");
                pinned.field("path", path);
                match roots {
                    Some(roots) => pinned.field("roots", roots),
                    None => pinned.field("roots", &format_args!("<pending>")),
                };
                pinned.finish()
            }
        }
    }
}

/// A fixed socket address for the connection to trawld, from `[web]
/// upstream_connect_addr` (ADR-0048).
///
/// The client dials `addr` whenever the upstream URL names `host`, and
/// never asks a resolver for it. TLS still verifies `host`, so a sidecar can
/// reach trawld over loopback and check that the certificate names the
/// host in `upstream_url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamConnect {
    /// The `upstream_url` host: a DNS name, never an IP literal.
    pub host: String,
    /// Where the connection goes. Its port equals the `upstream_url` port.
    pub addr: SocketAddr,
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

    /// `[web] upstream_url` cannot be used. The URL itself is never part of
    /// the message, since the rule it broke may be that it holds a
    /// password.
    #[error("`[web] upstream_url` {0}")]
    UpstreamUrl(UpstreamUrlError),

    /// `[web] upstream_connect_addr` cannot be used with this upstream.
    #[error("`[web] upstream_connect_addr` {0}")]
    UpstreamConnectAddr(ConnectAddrError),

    /// `[web] upstream_ca_path` cannot be used as a trust root.
    #[error("upstream CA file {path}: {reason}")]
    UpstreamCa { path: PathBuf, reason: String },
}

/// Why `[web] upstream_url` was refused. No variant carries the URL, or any
/// part of it an operator may have put a secret in.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamUrlError {
    /// The URL does not parse. Carries the parser's fixed reason, which
    /// never quotes its input.
    #[error("does not parse as a URL: {0}")]
    Unparseable(String),
    /// The URL carries a user name or a password.
    #[error(
        "carries a user name or password. trawl-web sends each signed-in user's own key \
         to trawld, so the URL must hold no credentials: remove the part before `@`"
    )]
    Userinfo,
    /// The URL is not `https`. Carries the scheme, which the parser has
    /// already limited to letters, digits, `+`, `-` and `.`.
    #[error(
        "uses the `{scheme}` scheme. trawl-web verifies trawld's certificate, \
         so the upstream must be https"
    )]
    NotHttps { scheme: String },
    /// The URL names no host.
    #[error("names no host")]
    NoHost,
}

/// Why `[web] upstream_connect_addr` was refused. The address is not
/// echoed; the operator has it in the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectAddrError {
    /// Not an IP address and port.
    #[error(
        "is not an IP address and port, such as `127.0.0.1:5514` or `[::1]:5514`. \
         A host name is not accepted"
    )]
    Malformed,
    /// Port 0 would let the client pick the port from the URL instead.
    #[error("needs a port other than 0")]
    PortZero,
    /// The upstream URL's host is an IP address, so there is no name for
    /// TLS to verify apart from the address the connection goes to.
    #[error(
        "needs an `upstream_url` whose host is a DNS name. TLS verifies that name \
         while the connection goes to this address, and the URL names an IP address"
    )]
    IpHost,
    /// The URL's port and the address's port differ.
    #[error(
        "has port {addr_port}, but `upstream_url` has port {url_port} \
         (443 when the URL states none). The client would ignore one of them, \
         so use the same port in both"
    )]
    PortMismatch { url_port: u16, addr_port: u16 },
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
        let checked_url = check_upstream_url(&upstream_url).map_err(ConfigError::UpstreamUrl)?;
        let upstream_connect = web
            .upstream_connect_addr
            .as_deref()
            .map(|addr| resolve_upstream_connect(&checked_url, addr))
            .transpose()
            .map_err(ConfigError::UpstreamConnectAddr)?;
        let upstream_tls = resolve_upstream_tls(
            resolve_upstream_ca_path(
                std::env::var_os(ENV_UPSTREAM_CA_PATH).as_deref(),
                web.upstream_ca_path.as_deref(),
            )?
            .as_deref(),
        )?;
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
                std::env::var_os(ENV_BIND_ADDR).as_deref(),
                web.bind_addr.as_deref(),
            )?,
            upstream_url,
            session_ttl_secs: web.session_ttl_secs.unwrap_or(DEFAULT_SESSION_TTL_SECS),
            allow_insecure_cookies,
            upstream_tls,
            upstream_connect,
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
        // injects the variable whenever the sidecar runs — so warning on
        // the variable's mere presence fires on every default install and
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

/// The text of the environment override `name`, whose raw value is
/// `value`. A value that is set but not UTF-8 is refused, naming the
/// variable and never the value: read as unset, it would silently hand the
/// choice back to the file.
fn env_override<'a>(name: &str, value: Option<&'a OsStr>) -> Result<Option<&'a str>, ConfigError> {
    value
        .map(|value| {
            value.to_str().ok_or_else(|| ConfigError::EnvUtf8 {
                name: name.to_owned(),
            })
        })
        .transpose()
}

/// Pick the listen address: [`ENV_BIND_ADDR`] first, then `[web] bind_addr`,
/// then [`DEFAULT_BIND_ADDR`]. Empty values count as unset at both levels.
///
/// Split from `from_parsed` so the precedence is testable without mutating
/// process env (forbidden under `unsafe_code = "forbid"`).
///
/// # Errors
/// Returns [`ConfigError::EnvUtf8`] when the environment value is not UTF-8.
fn resolve_bind_addr(
    env_value: Option<&OsStr>,
    configured: Option<&str>,
) -> Result<String, ConfigError> {
    let pick = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
    Ok(pick(env_override(ENV_BIND_ADDR, env_value)?)
        .or_else(|| pick(configured))
        .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_owned()))
}

/// Pick the CA pin: [`ENV_UPSTREAM_CA_PATH`] first, then `[web]
/// upstream_ca_path`. An empty environment value counts as unset, as for
/// [`ENV_BIND_ADDR`]. An empty configured path is passed on and refused by
/// [`resolve_upstream_tls`]: falling back to the platform roots would
/// change the trust mode on a blank value.
///
/// Split from `from_parsed` so the precedence is testable without mutating
/// process env (forbidden under `unsafe_code = "forbid"`).
///
/// # Errors
/// Returns [`ConfigError::EnvUtf8`] when the environment value is not
/// UTF-8. Read as unset, it would fall back to the file's pin or to the
/// platform roots.
fn resolve_upstream_ca_path(
    env_value: Option<&OsStr>,
    configured: Option<&Path>,
) -> Result<Option<PathBuf>, ConfigError> {
    Ok(env_override(ENV_UPSTREAM_CA_PATH, env_value)?
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| configured.map(Path::to_owned)))
}

/// Check the rules every upstream URL follows, whatever the trust mode
/// (ADR-0048).
///
/// - It parses, with the parser the client dials with.
/// - It carries no user name or password. reqwest would turn them into a
///   Basic `Authorization` header, and the URL goes into the startup log.
/// - It is `https`, since both trust modes verify trawld's certificate and
///   a plain `http` upstream would send every user's key in cleartext.
/// - It names a host.
///
/// No error quotes the URL.
///
/// # Errors
/// Returns the [`UpstreamUrlError`] naming the rule that refused.
pub fn check_upstream_url(upstream_url: &str) -> Result<reqwest::Url, UpstreamUrlError> {
    let url = reqwest::Url::parse(upstream_url)
        .map_err(|e| UpstreamUrlError::Unparseable(e.to_string()))?;
    // The parser decodes no percent-encoding here and drops an empty
    // password, so `user@`, `user:@`, `:pw@` and their percent-encoded
    // spellings all show up in one of these two. A bare `@` or `:@` holds
    // no credential, and the parser drops it from the URL the client dials.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UpstreamUrlError::Userinfo);
    }
    if url.scheme() != "https" {
        return Err(UpstreamUrlError::NotHttps {
            scheme: url.scheme().to_owned(),
        });
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(UpstreamUrlError::NoHost);
    }
    Ok(url)
}

/// Parse `[web] upstream_connect_addr` against the checked upstream URL
/// (ADR-0048).
///
/// - The value is an IP address and port, IPv6 in brackets. A name is
///   refused: resolving it would be the resolver answer this setting
///   exists to replace.
/// - The URL's host is a DNS name. TLS verifies that name while the
///   connection goes to `addr`; with an IP literal there is nothing to
///   verify apart from the address itself.
/// - The ports agree. The client dials the URL's port when the URL states
///   one and the address's port when it does not, so a mismatch would
///   silently drop one of them. The URL's port counts as 443 when it
///   states none, which also covers an explicit `:443` that the parser
///   normalizes away.
///
/// `url` has passed [`check_upstream_url`], so it is https and names a
/// host.
///
/// # Errors
/// Returns the [`ConnectAddrError`] naming the rule that refused.
pub fn resolve_upstream_connect(
    url: &reqwest::Url,
    addr: &str,
) -> Result<UpstreamConnect, ConnectAddrError> {
    let addr: SocketAddr = addr.parse().map_err(|_| ConnectAddrError::Malformed)?;
    if addr.port() == 0 {
        return Err(ConnectAddrError::PortZero);
    }
    // `domain` is `None` for an IPv4 or IPv6 literal.
    let Some(host) = url.domain() else {
        return Err(ConnectAddrError::IpHost);
    };
    let url_port = url.port_or_known_default().unwrap_or(443);
    if url_port != addr.port() {
        return Err(ConnectAddrError::PortMismatch {
            url_port,
            addr_port: addr.port(),
        });
    }
    Ok(UpstreamConnect {
        host: host.to_owned(),
        addr,
    })
}

/// Decide how the upstream client verifies trawld's certificate.
///
/// No `ca_path` means the platform roots. Otherwise the file, tilde
/// expanded, is read once:
///
/// - Absent (`NotFound`): the pin is pending, and one `upstream_ca_pending`
///   warning says so. trawld writes its generated certificate on its first
///   start, which may come after trawl-web's.
/// - Any other read error, a path that is not a regular file, a file larger
///   than `MAX_PIN_FILE_BYTES` (1 MiB), or contents that do not parse as a
///   certificate bundle: refused, so a broken pin stops startup instead of
///   failing every request later.
///
/// # Errors
/// Returns [`ConfigError::UpstreamCa`] naming the file and the reason.
pub fn resolve_upstream_tls(ca_path: Option<&Path>) -> Result<UpstreamTls, ConfigError> {
    let Some(path) = ca_path else {
        return Ok(UpstreamTls::System);
    };
    let path = PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).into_owned());
    let refuse = |reason: &str| ConfigError::UpstreamCa {
        path: path.clone(),
        reason: reason.to_owned(),
    };
    if path.as_os_str().is_empty() {
        return Err(refuse(
            "the path is empty. Name trawld's CA file, or remove the setting to trust the platform roots",
        ));
    }
    let roots = match read_pin_file(&path) {
        Ok(pem) => Some(pinned_roots(&pem).map_err(refuse)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                event_type = "upstream_ca_pending",
                path = %path.display(),
                "the pinned upstream CA file does not exist yet; trawl-web cannot reach trawld until it does"
            );
            None
        }
        Err(e) => return Err(refuse(&e.to_string())),
    };
    Ok(UpstreamTls::PinnedCa { path, roots })
}

/// The largest pin file trawl-web reads, in bytes: 1 MiB.
///
/// A pin names trawld's CA: its one generated certificate is under 1 KiB,
/// and even a whole public root bundle is a few hundred KiB. A bigger file
/// is not a CA bundle, and reading it whole, at startup or on every
/// re-read, would let whoever can write the path make trawl-web allocate
/// the file's size, past a container's memory limit.
pub(crate) const MAX_PIN_FILE_BYTES: u64 = 1024 * 1024;

/// Read the pin file at `path`, refusing anything but a regular file of at
/// most [`MAX_PIN_FILE_BYTES`].
///
/// The open never waits: on unix it passes `O_NONBLOCK`, so a FIFO with
/// no writer opens at once instead of blocking until one appears. The
/// type and size checks run on the opened handle, so the path cannot be
/// swapped between check and read. A FIFO, directory or device is refused
/// with `InvalidInput`. A file over the cap is refused with `FileTooLarge`
/// before any of it is read, and the read itself stops one byte past the
/// cap, so a file that grows after the size check is refused too and never
/// read whole. Reading a regular file can still stall, on a network
/// volume for one, so callers on the async runtime run this on the
/// blocking pool. The one reader for a pin file, at startup and on any
/// later read.
pub(crate) fn read_pin_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path is not a regular file",
        ));
    }
    let too_large = || {
        std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!(
                "the file is larger than {} MiB, too large for a CA bundle",
                MAX_PIN_FILE_BYTES / (1024 * 1024)
            ),
        )
    };
    if metadata.len() > MAX_PIN_FILE_BYTES {
        return Err(too_large());
    }
    let mut pem = Vec::new();
    file.take(MAX_PIN_FILE_BYTES + 1).read_to_end(&mut pem)?;
    if pem.len() as u64 > MAX_PIN_FILE_BYTES {
        return Err(too_large());
    }
    Ok(pem)
}

/// Parse a PEM bundle into trust anchors, refusing one that yields none.
///
/// Every certificate goes through the same root-store check the TLS stack
/// applies when the client is built, so a bundle that passes here cannot
/// fail there with a bare "builder error". The one parser for a pin file,
/// at startup and on any later read.
pub(crate) fn pinned_roots(pem: &[u8]) -> Result<PinnedRoots, &'static str> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject as _;

    let ders = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "the file is not valid PEM")?;
    if ders.is_empty() {
        return Err("the file holds no PEM certificate");
    }
    let mut store = rustls::RootCertStore::empty();
    let certificates = ders
        .iter()
        .map(|der| {
            store
                .add(der.clone())
                .map_err(|_| "a certificate in the file does not parse")?;
            reqwest::Certificate::from_der(der)
                .map_err(|_| "a certificate in the file does not parse")
        })
        .collect::<Result<_, _>>()?;
    Ok(PinnedRoots {
        digest: blake3::hash(pem),
        certificates,
    })
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
    use std::ffi::OsString;

    use super::*;
    use crate::test_support::captured_logs;

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
            captured_logs(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
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
            captured_logs(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
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
            captured_logs(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
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
            captured_logs(|| ResolvedConfig::from_parsed_with_runtime(&web, None, runtime));
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

    #[test]
    fn bind_addr_precedence_env_then_config_then_default() {
        let env = |value: &str| Some(OsString::from(value));
        let resolve = |env_value: Option<OsString>, configured| {
            resolve_bind_addr(env_value.as_deref(), configured).unwrap()
        };
        assert_eq!(
            resolve(env("100.87.180.64:8090"), Some("127.0.0.1:9000")),
            "100.87.180.64:8090"
        );
        assert_eq!(resolve(None, Some("127.0.0.1:9000")), "127.0.0.1:9000");
        assert_eq!(resolve(None, None), DEFAULT_BIND_ADDR);
        // Exported-but-blank is a shell accident, not a bind request — at
        // either level.
        assert_eq!(resolve(env(""), Some("127.0.0.1:9000")), "127.0.0.1:9000");
        assert_eq!(resolve(env(""), Some("")), DEFAULT_BIND_ADDR);
    }

    /// An override that is set but not UTF-8 is refused, naming the
    /// variable and never its value. Read as unset, it would silently hand
    /// the choice back to the file: for the CA pin, possibly to the
    /// platform roots.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_override_is_refused_naming_the_variable() {
        use std::os::unix::ffi::OsStringExt;

        const SENTINEL: &str = "s3ntinel";
        let value = || {
            let mut bytes = format!("/run/{SENTINEL}").into_bytes();
            bytes.push(0xff);
            OsString::from_vec(bytes)
        };
        let ca_error = resolve_upstream_ca_path(
            Some(value().as_os_str()),
            Some(Path::new("/etc/trawl/ca.pem")),
        )
        .expect_err("a non-UTF-8 CA path override must refuse");
        let bind_error = resolve_bind_addr(Some(value().as_os_str()), Some("127.0.0.1:9000"))
            .expect_err("a non-UTF-8 bind override must refuse");
        for (error, name) in [
            (ca_error, ENV_UPSTREAM_CA_PATH),
            (bind_error, ENV_BIND_ADDR),
        ] {
            assert!(
                matches!(&error, ConfigError::EnvUtf8 { name: n } if n == name),
                "{error:?}"
            );
            let display = error.to_string();
            assert!(display.contains(name), "got: {display}");
            for rendered in [display, format!("{error:?}")] {
                assert!(!rendered.contains(SENTINEL), "{rendered}");
            }
        }
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

    // -- upstream URL rules and the pinned CA (ADR-0048) -------------------

    /// Resolve `web` with no runtime overrides and no `[server]`.
    fn resolve(web: &WebConfig) -> Result<ResolvedConfig, ConfigError> {
        let runtime = SessionRuntimeOverrides::parse(None, None, None, None, None).unwrap();
        ResolvedConfig::from_parsed_with_runtime(web, None, runtime)
    }

    #[test]
    fn upstream_requires_https_all_modes() {
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, test_ca_pem()).unwrap();
        for (upstream, scheme) in [
            ("http://127.0.0.1:5514", "http"),
            ("http://[::1]:5514", "http"),
            ("http://trawld:5514", "http"),
            ("ws://127.0.0.1:5514", "ws"),
            ("unix:/run/trawld.sock", "unix"),
        ] {
            // Platform roots, then a pinned CA: the rule is the same.
            for ca in [None, Some(ca_path.clone())] {
                let pinned = ca.is_some();
                let web = WebConfig {
                    upstream_url: Some(upstream.to_owned()),
                    upstream_ca_path: ca,
                    ..configured_web()
                };
                let error = resolve(&web).expect_err("a cleartext upstream must refuse");
                assert!(
                    matches!(
                        &error,
                        ConfigError::UpstreamUrl(UpstreamUrlError::NotHttps { scheme: s })
                            if s == scheme
                    ),
                    "{upstream} (pinned: {pinned}) gave {error:?}"
                );
                let message = error.to_string();
                assert!(message.contains("[web] upstream_url"), "got: {message}");
                assert!(message.contains(&format!("`{scheme}`")), "got: {message}");
                assert!(message.contains("https"), "got: {message}");
            }
        }

        // The derived default stays https and resolves under both modes,
        // for every shape of `[server] http_addr`.
        for http_addr in ["0.0.0.0:5514", "[::]:5514", "127.0.0.1:5514", "[::1]:5514"] {
            let srv = ServerConfig {
                http_addr: http_addr.into(),
                ..dummy_server()
            };
            for ca in [None, Some(ca_path.clone())] {
                let web = WebConfig {
                    upstream_ca_path: ca,
                    ..configured_web()
                };
                let runtime = SessionRuntimeOverrides::parse(None, None, None, None, None).unwrap();
                let resolved = ResolvedConfig::from_parsed_with_runtime(&web, Some(&srv), runtime)
                    .unwrap_or_else(|e| panic!("{http_addr} must derive a usable upstream: {e}"));
                assert!(
                    resolved.upstream_url.starts_with("https://"),
                    "{http_addr} derived {}",
                    resolved.upstream_url
                );
            }
        }
        let resolved = resolve(&configured_web()).expect("the fallback upstream resolves");
        assert_eq!(resolved.upstream_url, FALLBACK_UPSTREAM_URL);
        assert!(matches!(resolved.upstream_tls, UpstreamTls::System));
    }

    #[test]
    fn upstream_userinfo_refused_without_echo() {
        const SENTINEL: &str = "s3ntinel";
        // `%73` is `s`: the percent-encoded spelling of the same sentinel.
        const ENCODED: &str = "%73%33ntinel";
        for upstream in [
            format!("https://{SENTINEL}@trawld:5514"),
            format!("https://{SENTINEL}:@trawld:5514"),
            format!("https://:{SENTINEL}@trawld:5514"),
            format!("https://{SENTINEL}:{SENTINEL}@trawld:5514"),
            format!("https://{ENCODED}@trawld:5514"),
            format!("https://:{ENCODED}@trawld:5514"),
            format!("https://{SENTINEL}@127.0.0.1:5514"),
            // Refused as credentials before the scheme is judged, so the
            // http refusal cannot be the one that mentions them.
            format!("http://{SENTINEL}:{SENTINEL}@trawld:5514"),
        ] {
            let web = WebConfig {
                upstream_url: Some(upstream.clone()),
                ..configured_web()
            };
            let (outcome, lines) = captured_logs(|| resolve(&web));
            let error = outcome.expect_err("a URL with credentials must refuse");
            assert!(
                matches!(error, ConfigError::UpstreamUrl(UpstreamUrlError::Userinfo)),
                "{error:?}"
            );
            let display = error.to_string();
            let debug = format!("{error:?}");
            for rendered in [&display, &debug] {
                assert!(!rendered.contains(SENTINEL), "{upstream}: {rendered}");
                assert!(!rendered.contains(ENCODED), "{upstream}: {rendered}");
                assert!(!rendered.contains("trawld:5514"), "{upstream}: {rendered}");
            }
            assert!(display.contains("[web] upstream_url"), "got: {display}");
            assert!(
                lines.iter().all(|line| !line.contains(SENTINEL)),
                "{upstream}: {lines:?}"
            );
        }
    }

    #[test]
    fn an_upstream_url_that_does_not_parse_refuses_without_echo() {
        for upstream in ["", "not a url", "127.0.0.1:5514", "https://trawld:99999"] {
            let web = WebConfig {
                upstream_url: Some(upstream.to_owned()),
                ..configured_web()
            };
            let error = resolve(&web).expect_err("an unparseable upstream must refuse");
            assert!(
                matches!(
                    error,
                    ConfigError::UpstreamUrl(UpstreamUrlError::Unparseable(_))
                ),
                "{upstream:?} gave {error:?}"
            );
            if !upstream.is_empty() {
                assert!(!error.to_string().contains(upstream), "{error}");
                assert!(!format!("{error:?}").contains(upstream), "{error:?}");
            }
        }
    }

    #[test]
    fn a_connect_addr_resolves_for_a_dns_named_upstream() {
        for (upstream, addr, host, expected) in [
            (
                "https://trawl.test:5514",
                "127.0.0.1:5514",
                "trawl.test",
                "127.0.0.1:5514",
            ),
            (
                "https://trawl.test:5514",
                "[::1]:5514",
                "trawl.test",
                "[::1]:5514",
            ),
            // The parser lowercases the host, and the client matches the
            // lowercased name.
            (
                "https://Trawl.Lab.Example:5514",
                "127.0.0.1:5514",
                "trawl.lab.example",
                "127.0.0.1:5514",
            ),
            // No port in the URL, or `:443` written out: both are 443.
            (
                "https://trawl.test",
                "127.0.0.1:443",
                "trawl.test",
                "127.0.0.1:443",
            ),
            (
                "https://trawl.test:443",
                "127.0.0.1:443",
                "trawl.test",
                "127.0.0.1:443",
            ),
        ] {
            let web = WebConfig {
                upstream_url: Some(upstream.to_owned()),
                upstream_connect_addr: Some(addr.to_owned()),
                ..configured_web()
            };
            let resolved = resolve(&web).unwrap_or_else(|e| panic!("{upstream} via {addr}: {e}"));
            assert_eq!(
                resolved.upstream_connect,
                Some(UpstreamConnect {
                    host: host.to_owned(),
                    addr: expected.parse().unwrap(),
                }),
                "{upstream} via {addr}"
            );
        }
        // Unset: the host resolves normally.
        let web = WebConfig {
            upstream_url: Some("https://trawl.test:5514".to_owned()),
            ..configured_web()
        };
        assert_eq!(resolve(&web).unwrap().upstream_connect, None);
    }

    #[test]
    fn a_connect_addr_refuses_a_name_a_zero_port_or_a_hidden_port_mismatch() {
        let url = check_upstream_url("https://trawl.test:5514").unwrap();
        for addr in [
            "",
            "localhost:5514",
            "trawl.test:5514",
            "127.0.0.1",
            "::1:5514",
        ] {
            assert_eq!(
                resolve_upstream_connect(&url, addr),
                Err(ConnectAddrError::Malformed),
                "{addr:?}"
            );
        }
        assert_eq!(
            resolve_upstream_connect(&url, "127.0.0.1:0"),
            Err(ConnectAddrError::PortZero)
        );
        // `:443` is normalized away by the parser, and the client would
        // then dial the address's port. It still counts as a mismatch.
        let default_port = check_upstream_url("https://trawl.test:443").unwrap();
        let error = resolve_upstream_connect(&default_port, "127.0.0.1:5514").unwrap_err();
        assert_eq!(
            error,
            ConnectAddrError::PortMismatch {
                url_port: 443,
                addr_port: 5514
            }
        );
        let message = ConfigError::UpstreamConnectAddr(error).to_string();
        assert!(
            message.starts_with("`[web] upstream_connect_addr`"),
            "got: {message}"
        );
        assert!(
            message.contains("5514") && message.contains("443"),
            "got: {message}"
        );
    }

    #[test]
    fn upstream_ca_path_precedence_env_then_config() {
        let configured = Path::new("/etc/trawl/ca.pem");
        let env = |value: &str| Some(OsString::from(value));
        let resolve = |env_value: Option<OsString>, configured| {
            resolve_upstream_ca_path(env_value.as_deref(), configured).unwrap()
        };
        assert_eq!(
            resolve(env("/run/dev/cert.pem"), Some(configured)),
            Some(PathBuf::from("/run/dev/cert.pem"))
        );
        assert_eq!(
            resolve(env("/run/dev/cert.pem"), None),
            Some(PathBuf::from("/run/dev/cert.pem"))
        );
        assert_eq!(resolve(None, Some(configured)), Some(configured.to_owned()));
        assert_eq!(resolve(None, None), None);
        // Exported-but-blank is a shell accident: the file's setting holds.
        assert_eq!(
            resolve(env(""), Some(configured)),
            Some(configured.to_owned())
        );
        assert_eq!(resolve(env(""), None), None);
    }

    #[test]
    fn a_pinned_ca_file_resolves_to_its_certificates() {
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, format!("{}{}", test_ca_pem(), test_ca_pem())).unwrap();
        let tls = resolve_upstream_tls(Some(&ca_path)).unwrap();
        assert!(
            matches!(
                &tls,
                UpstreamTls::PinnedCa { path, roots: Some(roots) }
                    if *path == ca_path && roots.certificates().len() == 2
            ),
            "{tls:?}"
        );
        assert_eq!(
            format!("{tls:?}"),
            format!("PinnedCa {{ path: {ca_path:?}, roots: <2 certificates> }}")
        );
    }

    #[test]
    fn a_missing_pinned_ca_file_is_pending_and_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        // The parent does not exist either, as before trawld's first start
        // creates its `tls` directory.
        let missing = dir.path().join("tls").join("cert.pem");
        let (tls, lines) = captured_logs(|| resolve_upstream_tls(Some(&missing)));
        let tls = tls.expect("a missing pin file is pending, not a refusal");
        assert!(
            matches!(&tls, UpstreamTls::PinnedCa { path, roots: None } if *path == missing),
            "{tls:?}"
        );
        assert!(format!("{tls:?}").contains("roots: <pending>"), "{tls:?}");
        let pending: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("upstream_ca_pending"))
            .collect();
        assert_eq!(pending.len(), 1, "{lines:?}");
        assert!(pending[0].starts_with("WARN"), "got: {}", pending[0]);
        assert!(
            pending[0].contains(&missing.display().to_string()),
            "got: {}",
            pending[0]
        );
    }

    #[test]
    fn a_pinned_ca_path_is_tilde_expanded_and_kept_whole() {
        let tls = resolve_upstream_tls(Some(Path::new("~/.trawl-c3-absent/tls/cert.pem"))).unwrap();
        let expected =
            PathBuf::from(shellexpand::tilde("~/.trawl-c3-absent/tls/cert.pem").as_ref());
        assert!(
            !expected.starts_with("~"),
            "no home directory to expand into"
        );
        assert!(
            matches!(&tls, UpstreamTls::PinnedCa { path, .. } if *path == expected),
            "{tls:?}"
        );
    }

    #[test]
    fn an_unusable_pinned_ca_file_refuses_at_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let key_only = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        let bad_der = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let bad_pem = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
        let good_then_bad = format!("{}{bad_der}", test_ca_pem());
        for (contents, reason) in [
            ("", "no PEM certificate"),
            ("not pem at all\n", "no PEM certificate"),
            (key_only, "no PEM certificate"),
            (bad_der, "does not parse"),
            (good_then_bad.as_str(), "does not parse"),
            (bad_pem, "not valid PEM"),
        ] {
            let ca_path = dir.path().join("ca.pem");
            std::fs::write(&ca_path, contents).unwrap();
            let error = resolve_upstream_tls(Some(&ca_path))
                .expect_err("an unusable bundle must stop startup");
            assert!(
                matches!(&error, ConfigError::UpstreamCa { reason: r, .. } if r.contains(reason)),
                "{contents:?} gave {error:?}"
            );
            assert!(error.to_string().contains("ca.pem"), "{error}");
        }

        // Only a file that does not exist is pending. One that exists but
        // cannot be read refuses: a directory, a path through a file, and
        // an empty path, which would otherwise read as "not found".
        let through_a_file = dir.path().join("ca.pem").join("cert.pem");
        for unreadable in [dir.path(), through_a_file.as_path(), Path::new("")] {
            let error = resolve_upstream_tls(Some(unreadable))
                .expect_err("an unreadable pin must stop startup");
            assert!(
                matches!(&error, ConfigError::UpstreamCa { .. }),
                "{unreadable:?} gave {error:?}"
            );
        }
    }

    /// A pin file over [`MAX_PIN_FILE_BYTES`] refuses before it is read,
    /// whatever it holds. The file is sparse, so the test allocates nothing.
    /// A file of exactly the cap still reads.
    #[test]
    fn an_oversized_pinned_ca_file_refuses_at_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, test_ca_pem()).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&ca_path)
            .unwrap();

        file.set_len(MAX_PIN_FILE_BYTES).unwrap();
        let tls = resolve_upstream_tls(Some(&ca_path)).expect("a file at the cap reads");
        assert!(
            matches!(tls, UpstreamTls::PinnedCa { roots: Some(_), .. }),
            "{tls:?}"
        );

        file.set_len(MAX_PIN_FILE_BYTES + 1).unwrap();
        let error = resolve_upstream_tls(Some(&ca_path))
            .expect_err("an oversized pin file must stop startup");
        assert!(
            matches!(&error, ConfigError::UpstreamCa { reason, .. } if reason.contains("larger than 1 MiB")),
            "{error:?}"
        );
        let error = read_pin_file(&ca_path).expect_err("the read refuses");
        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn a_config_file_carries_the_pinned_ca_into_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, test_ca_pem()).unwrap();
        let web = WebConfig {
            upstream_url: Some("https://trawld:5514".into()),
            upstream_ca_path: Some(ca_path),
            ..configured_web()
        };
        // Reads the real process environment for the CA override, which
        // the test runner does not set.
        let resolved = resolve(&web).unwrap();
        assert!(matches!(
            resolved.upstream_tls,
            UpstreamTls::PinnedCa { roots: Some(_), .. }
        ));
    }

    /// The Debian package pins the certificate trawld generates for itself.
    /// The path itself is guarded beside trawld's generator, in
    /// `trawl-server`'s `tls.rs`, where the file name is defined.
    #[test]
    fn the_debian_default_config_pins_trawld_generated_certificate() {
        let config = Config::parse_toml(include_str!("../../trawl-server/debian/trawld.toml"))
            .expect("the packaged trawld.toml parses");
        let upstream = config
            .web
            .upstream_url
            .clone()
            .unwrap_or_else(|| default_upstream_from_server(Some(&config.server)));
        check_upstream_url(&upstream).expect("the Debian upstream follows the URL rules");
        assert!(
            config.web.upstream_ca_path.is_some(),
            "the packaged trawld.toml sets [web] upstream_ca_path"
        );

        // With trawld's certificate at the pinned path, the pin resolves. The
        // certificate here is the shape trawld generates: self-signed, with
        // the loopback names as SANs.
        let dir = tempfile::tempdir().unwrap();
        let generated = dir.path().join("cert.pem");
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
        ])
        .unwrap()
        .cert;
        std::fs::write(&generated, cert.pem()).unwrap();
        assert!(
            matches!(
                resolve_upstream_tls(Some(&generated)),
                Ok(UpstreamTls::PinnedCa { roots: Some(roots), .. }) if roots.certificates().len() == 1
            ),
            "the Debian upstream {upstream} must accept a pin"
        );

        // Before trawld's first start writes it, trawl-web still starts,
        // with the pin pending.
        let missing = dir.path().join("tls").join("cert.pem");
        assert!(
            matches!(
                resolve_upstream_tls(Some(&missing)),
                Ok(UpstreamTls::PinnedCa { path, roots: None }) if path == missing
            ),
            "a missing generated certificate is pending"
        );
    }

    /// A self-signed CA certificate in PEM.
    fn test_ca_pem() -> String {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap()
            .pem()
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
