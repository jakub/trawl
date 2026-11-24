// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resolution runs in two steps, so `trawl-web --doctor` can inspect a
//! configuration without the side effects of starting on it:
//! [`Sources::resolve`] is pure, and [`ResolvedConfig::from_sources`] is
//! startup's step.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fleet_auth::{
    ENV_SESSION_AEAD_KEY, ENV_SESSION_COOKIE_DOMAIN, ENV_SESSION_COOKIE_PATH,
    ENV_SESSION_COOKIE_SECURE, ENV_SESSION_PUBLIC_ORIGINS, KEY_LEN, PublicOrigins,
    PublicOriginsError, RuntimeCookieDomain, SessionKey, SessionRuntimeError,
    SessionRuntimeOverrides,
};
use trawl_config::{Config, ServerConfig, WebConfig};
use zeroize::Zeroizing;

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8090";

/// `[server].http_addr` carries a serde default, so a config loaded from disk
/// never reaches this. The port is the one both packaged deployments give
/// trawld, not `trawl_config::DEFAULT_HTTP_ADDR`.
pub const FALLBACK_UPSTREAM_URL: &str = "https://127.0.0.1:5514";

pub const DEFAULT_SESSION_TTL_SECS: u64 = 86_400;

pub const ENV_BIND_ADDR: &str = "TRAWL_WEB_BIND_ADDR";

/// An empty value counts as unset, as for [`ENV_BIND_ADDR`], so the file's
/// setting applies.
pub const ENV_UPSTREAM_CA_PATH: &str = "TRAWL_WEB_UPSTREAM_CA_PATH";

/// Env var name overriding trawld's `[server] http_addr`, which
/// [`ServerConfig::resolve_http_addr`] reads. trawl-web derives its default
/// upstream from the same address, so it reads the variable too.
pub const ENV_HTTP_ADDR: &str = "TRAWL_HTTP_ADDR";

#[derive(Debug)]
pub struct ResolvedConfig {
    pub bind_addr: String,
    /// The `https` URL the proxy reaches trawld at. Resolution refused any
    /// other scheme, any user name or password, and any query or fragment,
    /// so the value is safe to log.
    pub upstream_url: String,
    pub session_ttl_secs: u64,
    pub allow_insecure_cookies: bool,
    pub upstream_tls: UpstreamTls,
    pub upstream_connect: Option<UpstreamConnect>,
    pub cookie_key: SessionKey,
    /// Parent domain for the shared `fleet_session` SSO cookie
    /// (`Domain=` attribute). `None` (unset or empty in config) means
    /// standalone mode — origin-scoped cookie.
    pub shared_domain: Option<String>,
    /// The browser-visible origins cookie-authenticated requests may come
    /// from (ADR-0016).
    pub public_origins: PublicOrigins,
    pub limits: ProxyLimits,
}

/// The bounds the proxy holds each request to (ADR-0054).
/// The body deadline, the slack over trawld's deadline and the body budget
/// are constants; a test shrinks them through this struct, and no key sets
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyLimits {
    pub request_body_bytes: usize,
    pub body_budget_bytes: usize,
    pub body_read_timeout: Duration,
    /// The response-head deadline, also the `TCP_USER_TIMEOUT` of every
    /// connection the proxy opens to trawld (`upstream::proxy_client_builder`).
    pub upstream_head_timeout: Duration,
}

impl ProxyLimits {
    pub const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

    pub const UPSTREAM_HEAD_SLACK: Duration = Duration::from_secs(10);

    #[must_use]
    pub fn from_server(server: Option<&ServerConfig>) -> Self {
        let timeout_secs = server.map_or(trawl_config::DEFAULT_TIMEOUT_SECS, |server| {
            server.timeout_secs
        });
        Self {
            request_body_bytes: server
                .map_or(trawl_config::DEFAULT_MAX_REQUEST_BODY_BYTES, |server| {
                    server.max_request_body_bytes
                }),
            body_budget_bytes: crate::body_budget::PROXY_BODY_BUDGET,
            body_read_timeout: Self::BODY_READ_TIMEOUT,
            upstream_head_timeout: Duration::from_secs(timeout_secs)
                .saturating_add(Self::UPSTREAM_HEAD_SLACK),
        }
    }
}

/// Both modes verify the chain and the host name (ADR-0048).
pub enum UpstreamTls {
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

/// The digest stays with the certificates so a later read of the file can
/// tell whether it changed since the client was built from it.
pub struct PinnedRoots {
    digest: blake3::Hash,
    certificates: Vec<reqwest::Certificate>,
}

impl PinnedRoots {
    #[must_use]
    pub fn certificates(&self) -> &[reqwest::Certificate] {
        &self.certificates
    }

    pub(crate) fn digest(&self) -> blake3::Hash {
        self.digest
    }
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamConnect {
    /// The `upstream_url` host: a DNS name, never an IP literal.
    pub host: String,
    /// Where the connection goes. Its port equals the `upstream_url` port.
    pub addr: SocketAddr,
}

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
        "{source} (set it in `[web] public_origins` in config.toml, or in the \
         {ENV_SESSION_PUBLIC_ORIGINS} environment variable)"
    )]
    PublicOrigins {
        #[from]
        source: PublicOriginsError,
    },

    #[error(transparent)]
    SessionEnvOrigins(SessionRuntimeError),

    /// `[web] upstream_url` cannot be used. The URL itself is never part of
    /// the message, since the rule it broke may be that it holds a
    /// password.
    #[error("`[web] upstream_url` {0}")]
    UpstreamUrl(UpstreamUrlError),

    #[error("`[web] upstream_connect_addr` {0}")]
    UpstreamConnectAddr(ConnectAddrError),

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
    #[error(
        "carries a user name or password. trawl-web sends each signed-in user's own key \
         to trawld, so the URL must hold no credentials: remove the part before `@`"
    )]
    Userinfo,
    #[error(
        "carries a query or fragment. trawl-web appends each request's path and query \
         to the URL, so it must end at the path: remove the part from `?` or `#`"
    )]
    QueryOrFragment,
    /// The URL is not `https`. Carries the scheme, which the parser has
    /// already limited to letters, digits, `+`, `-` and `.`.
    #[error(
        "uses the `{scheme}` scheme. trawl-web verifies trawld's certificate, \
         so the upstream must be https"
    )]
    NotHttps { scheme: String },
    #[error("names no host")]
    NoHost,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectAddrError {
    #[error(
        "is not an IP address and port, such as `127.0.0.1:5514` or `[::1]:5514`. \
         A host name is not accepted"
    )]
    Malformed,
    /// Port 0 would let the client pick the port from the URL instead.
    #[error("needs a port other than 0")]
    PortZero,
    #[error(
        "needs an `upstream_url` whose host is a DNS name. TLS verifies that name \
         while the connection goes to this address, and the URL names an IP address"
    )]
    IpHost,
    #[error(
        "has port {addr_port}, but `upstream_url` has port {url_port} \
         (443 when the URL states none). The client would ignore one of them, \
         so use the same port in both"
    )]
    PortMismatch { url_port: u16, addr_port: u16 },
}

impl ResolvedConfig {
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

    pub fn from_parsed(
        web: &WebConfig,
        server: Option<&ServerConfig>,
    ) -> Result<Self, ConfigError> {
        Self::startup(web, server, RuntimeEnv::from_process(), expand_tilde)
    }

    fn startup(
        web: &WebConfig,
        server: Option<&ServerConfig>,
        env: RuntimeEnv,
        expand: impl FnMut(&Path) -> PathBuf,
    ) -> Result<Self, ConfigError> {
        let runtime = RuntimeParts::from_whole_parse(env)?;
        Self::from_sources_with(Sources::resolve(web, server, runtime), expand)
    }

    /// Resolve with `runtime` standing in for the `FLEET_SESSION_*`
    /// variables, which a test cannot set. The other variables are read
    /// from the process, as startup reads them. The whole parse does not
    /// report whether `FLEET_SESSION_COOKIE_PATH` was set, and nothing here
    /// uses it, so it counts as unset.
    #[cfg(test)]
    fn from_parsed_with_runtime(
        web: &WebConfig,
        server: Option<&ServerConfig>,
        runtime: SessionRuntimeOverrides,
    ) -> Result<Self, ConfigError> {
        let (_, host) = RuntimeEnv::from_process().split();
        let runtime = RuntimeParts::from_overrides(runtime, false, host);
        Self::from_sources(Sources::resolve(web, server, runtime))
    }

    /// A `~` in the key file's path and in the pin's path is expanded by
    /// [`expand_tilde`] at the step that reads that file, and nowhere else, so
    /// the home directory is looked up only once every earlier step has passed.
    ///
    /// The doctor never calls this.
    pub fn from_sources(sources: Sources) -> Result<Self, ConfigError> {
        Self::from_sources_with(sources, expand_tilde)
    }

    fn from_sources_with(
        sources: Sources,
        mut expand: impl FnMut(&Path) -> PathBuf,
    ) -> Result<Self, ConfigError> {
        warn_on_runtime_override(&sources);
        let Sources {
            public_origins,
            cookie_key,
            cookie_settings,
            upstream,
            bind_addr,
            limits,
            configured,
        } = sources;
        let public_origins = public_origins?.origins;
        let cookie_key = cookie_key?;
        if !matches!(cookie_key, KeySource::FleetEnv(_)) {
            warn_on_ambiguous_key(&configured);
        }
        let cookie_key = match cookie_key {
            KeySource::FleetEnv(key) => key,
            KeySource::ConfigEnv { name } => key_from_env(&name)?,
            KeySource::File { path } => SessionKey::from_file(&expand(&path))
                .map_err(|e| ConfigError::KeyFile(e.to_string()))?,
            KeySource::None => ephemeral_key(),
        };
        let upstream = upstream?;
        let upstream_tls = match upstream.trust {
            TrustSource::System => UpstreamTls::System,
            TrustSource::Pinned { path, .. } => load_pin(refuse_empty_pin(expand(&path))?)?,
        };
        let cookie_settings = cookie_settings?;
        let bind_addr = bind_addr?;
        Ok(Self {
            bind_addr: bind_addr.addr,
            upstream_url: upstream.url,
            session_ttl_secs: cookie_settings.ttl_secs,
            allow_insecure_cookies: !cookie_settings.secure,
            upstream_tls,
            upstream_connect: upstream.connect,
            cookie_key,
            shared_domain: cookie_settings.shared_domain,
            public_origins,
            limits,
        })
    }
}

#[derive(Default)]
pub struct RuntimeEnv {
    pub aead_key: Option<OsString>,
    pub cookie_domain: Option<OsString>,
    pub cookie_path: Option<OsString>,
    pub cookie_secure: Option<OsString>,
    pub public_origins: Option<OsString>,
    pub bind_addr: Option<OsString>,
    pub upstream_ca_path: Option<OsString>,
    pub http_addr: Option<OsString>,
}

impl RuntimeEnv {
    /// The `cookie_secret_env` variable is not among them: its name comes
    /// from the file, and [`key_from_env`] reads it only when the key is
    /// actually loaded. Nor is `HOME`: `~` expands where each file is
    /// read, as [`ResolvedConfig::from_sources`] describes.
    #[must_use]
    pub fn from_process() -> Self {
        let read = std::env::var_os;
        Self {
            aead_key: read(ENV_SESSION_AEAD_KEY),
            cookie_domain: read(ENV_SESSION_COOKIE_DOMAIN),
            cookie_path: read(ENV_SESSION_COOKIE_PATH),
            cookie_secure: read(ENV_SESSION_COOKIE_SECURE),
            public_origins: read(ENV_SESSION_PUBLIC_ORIGINS),
            bind_addr: read(ENV_BIND_ADDR),
            upstream_ca_path: read(ENV_UPSTREAM_CA_PATH),
            http_addr: read(ENV_HTTP_ADDR),
        }
    }

    /// The five `FLEET_SESSION_*` values in the order fleet-auth reads and
    /// parses them (key, domain, path, secure, origins), and the rest.
    fn split(self) -> ([Option<OsString>; 5], HostEnv) {
        (
            [
                self.aead_key,
                self.cookie_domain,
                self.cookie_path,
                self.cookie_secure,
                self.public_origins,
            ],
            HostEnv {
                bind_addr: self.bind_addr,
                upstream_ca_path: self.upstream_ca_path,
                http_addr: self.http_addr,
            },
        )
    }
}

/// Names which variables are set, never a value.
impl std::fmt::Debug for RuntimeEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = [
            (ENV_SESSION_AEAD_KEY, &self.aead_key),
            (ENV_SESSION_COOKIE_DOMAIN, &self.cookie_domain),
            (ENV_SESSION_COOKIE_PATH, &self.cookie_path),
            (ENV_SESSION_COOKIE_SECURE, &self.cookie_secure),
            (ENV_SESSION_PUBLIC_ORIGINS, &self.public_origins),
            (ENV_BIND_ADDR, &self.bind_addr),
            (ENV_UPSTREAM_CA_PATH, &self.upstream_ca_path),
            (ENV_HTTP_ADDR, &self.http_addr),
        ];
        f.debug_struct("RuntimeEnv")
            .field(
                "set",
                &set.iter()
                    .filter(|(_, value)| value.is_some())
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

struct HostEnv {
    bind_addr: Option<OsString>,
    upstream_ca_path: Option<OsString>,
    http_addr: Option<OsString>,
}

/// May block: with `HOME` unset or empty, the user database can be a
/// network service. Call it only at the step that reads the file.
pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).into_owned())
}

/// The field types are those of [`SessionRuntimeOverrides`], each wrapped
/// in its own `Result`, so one invalid variable fails only the component
/// it feeds.
pub struct RuntimeParts {
    key: Result<Option<SessionKey>, ConfigError>,
    domain: Result<RuntimeCookieDomain, ConfigError>,
    /// Whether [`ENV_SESSION_COOKIE_PATH`] is set. Its only accepted value
    /// is `/`, the path trawl-web always uses.
    path_set: Result<bool, ConfigError>,
    secure: Result<Option<bool>, ConfigError>,
    public_origins: Result<Option<PublicOrigins>, ConfigError>,
    host: HostEnv,
}

impl RuntimeParts {
    #[must_use]
    pub fn from_process_env() -> Self {
        Self::from_each_variable(RuntimeEnv::from_process())
    }

    /// Parse each `FLEET_SESSION_*` variable in `env` on its own, through
    /// the shared fleet-auth parser with every other variable absent.
    ///
    /// Each variable gets exactly the result the whole parse would give
    /// it. What differs from [`Self::from_whole_parse`] is only that an
    /// invalid variable does not stop the others from being judged.
    #[must_use]
    pub fn from_each_variable(env: RuntimeEnv) -> Self {
        let ([key, domain, path, secure, origins], host) = env.split();
        let parse_one = |name, value: Option<OsString>, slot: usize| {
            value
                .map(|value| {
                    let value = value
                        .into_string()
                        .map_err(|_| SessionRuntimeError::NotUnicode { name })?;
                    let mut args: [Option<String>; 5] = Default::default();
                    args[slot] = Some(value);
                    let [key, domain, path, secure, origins] = args;
                    SessionRuntimeOverrides::parse(key, domain, path, secure, origins)
                })
                .transpose()
                .map_err(ConfigError::from)
        };
        Self {
            key: parse_one(ENV_SESSION_AEAD_KEY, key, 0).map(|o| o.and_then(|o| o.key)),
            domain: parse_one(ENV_SESSION_COOKIE_DOMAIN, domain, 1)
                .map(|o| o.map_or(RuntimeCookieDomain::PreserveConfigured, |o| o.domain)),
            path_set: parse_one(ENV_SESSION_COOKIE_PATH, path, 2).map(|o| o.is_some()),
            secure: parse_one(ENV_SESSION_COOKIE_SECURE, secure, 3)
                .map(|o| o.and_then(|o| o.secure)),
            public_origins: parse_one(ENV_SESSION_PUBLIC_ORIGINS, origins, 4)
                .map(|o| o.and_then(|o| o.public_origins)),
            host,
        }
    }

    /// Parse the `FLEET_SESSION_*` variables in `env` together, as
    /// fleet-auth's `SessionRuntimeOverrides::from_process_env` does:
    /// startup's observation.
    ///
    /// A value that is not UTF-8 refuses first, in the order key, domain,
    /// path, secure, origins; then the first value that does not parse, in
    /// the parser's order.
    pub fn from_whole_parse(env: RuntimeEnv) -> Result<Self, ConfigError> {
        let ([key, domain, path, secure, origins], host) = env.split();
        let path_set = path.is_some();
        let text = |name, value: Option<OsString>| {
            value
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| SessionRuntimeError::NotUnicode { name })
                })
                .transpose()
        };
        let overrides = SessionRuntimeOverrides::parse(
            text(ENV_SESSION_AEAD_KEY, key)?,
            text(ENV_SESSION_COOKIE_DOMAIN, domain)?,
            text(ENV_SESSION_COOKIE_PATH, path)?,
            text(ENV_SESSION_COOKIE_SECURE, secure)?,
            text(ENV_SESSION_PUBLIC_ORIGINS, origins)?,
        )?;
        Ok(Self::from_overrides(overrides, path_set, host))
    }

    fn from_overrides(overrides: SessionRuntimeOverrides, path_set: bool, host: HostEnv) -> Self {
        Self {
            key: Ok(overrides.key),
            domain: Ok(overrides.domain),
            path_set: Ok(path_set),
            secure: Ok(overrides.secure),
            public_origins: Ok(overrides.public_origins),
            host,
        }
    }
}

/// Says which parts are set and which failed, never a value.
impl std::fmt::Debug for RuntimeParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn part<T>(result: &Result<T, ConfigError>, set: impl FnOnce(&T) -> bool) -> &'static str {
            match result {
                Ok(value) if set(value) => "set",
                Ok(_) => "unset",
                Err(_) => "invalid",
            }
        }
        f.debug_struct("RuntimeParts")
            .field("key", &part(&self.key, Option::is_some))
            .field(
                "domain",
                &part(&self.domain, |d| {
                    !matches!(d, RuntimeCookieDomain::PreserveConfigured)
                }),
            )
            .field("path", &part(&self.path_set, |set| *set))
            .field("secure", &part(&self.secure, Option::is_some))
            .field(
                "public_origins",
                &part(&self.public_origins, Option::is_some),
            )
            .field("bind_addr", &self.host.bind_addr.is_some())
            .field("upstream_ca_path", &self.host.upstream_ca_path.is_some())
            .field("http_addr", &self.host.http_addr.is_some())
            .finish()
    }
}

/// Every variant is safe to print: environment names are this crate's or
/// fleet-auth's constants, never operator text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    File,
    Environment(&'static str),
    /// The upstream URL, derived from `[server] http_addr` in the config
    /// file, or from that setting's default when the file leaves it out.
    DerivedFromFile,
    /// The upstream URL, derived from trawld's listen-address override,
    /// the named variable ([`ENV_HTTP_ADDR`]).
    DerivedFromEnvironment(&'static str),
    Default,
}

pub struct Sources {
    pub public_origins: Result<Origins, ConfigError>,
    /// The selected cookie key source. Only an environment key is already
    /// loaded; the others are selections that nothing has read yet.
    pub cookie_key: Result<KeySource, ConfigError>,
    pub cookie_settings: Result<CookieSettings, ConfigError>,
    /// The upstream URL, connect address and trust source. The pinned CA
    /// file is named, not read.
    pub upstream: Result<UpstreamPlan, ConfigError>,
    pub bind_addr: Result<BindAddr, ConfigError>,
    pub limits: ProxyLimits,
    configured: Configured,
}

impl Sources {
    /// Pure: no file is read, nothing is logged, no key is generated, and
    /// neither the `cookie_secret_env` variable nor the home directory is
    /// looked up. A selected source that is invalid fails its component with no
    /// fallback to the next source.
    #[must_use]
    pub fn resolve(web: &WebConfig, server: Option<&ServerConfig>, runtime: RuntimeParts) -> Self {
        let RuntimeParts {
            key,
            domain,
            path_set,
            secure,
            public_origins,
            host,
        } = runtime;
        Self {
            public_origins: resolve_origins(web, public_origins),
            cookie_key: resolve_key_source(web, key),
            cookie_settings: resolve_cookie_settings(web, path_set, secure, domain),
            upstream: resolve_upstream(web, server, &host),
            bind_addr: select_bind_addr(host.bind_addr.as_deref(), web.bind_addr.as_deref())
                .map(|(addr, from)| BindAddr { addr, from }),
            limits: ProxyLimits::from_server(server),
            configured: Configured {
                cookie_secret_env: web.cookie_secret_env.clone(),
                cookie_secret_path: web.cookie_secret_path.clone(),
                allow_insecure_cookies: web.allow_insecure_cookies,
                shared_domain: web.shared_domain.clone(),
                public_origins: web.public_origins.clone(),
            },
        }
    }
}

/// Shows each component's source and shape: no URL, host, origin, domain,
/// variable name or key.
impl std::fmt::Debug for Sources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// A component's `Debug`, or `Err` with the error left out: its
        /// text can quote configured values.
        struct Part<'a, T>(&'a Result<T, ConfigError>);
        impl<T: std::fmt::Debug> std::fmt::Debug for Part<'_, T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self.0 {
                    Ok(value) => value.fmt(f),
                    Err(_) => f.write_str("Err(<config error>)"),
                }
            }
        }
        f.debug_struct("Sources")
            .field("public_origins", &Part(&self.public_origins))
            .field("cookie_key", &Part(&self.cookie_key))
            .field("cookie_settings", &Part(&self.cookie_settings))
            .field("upstream", &Part(&self.upstream))
            .field("bind_addr", &Part(&self.bind_addr))
            .finish_non_exhaustive()
    }
}

struct Configured {
    cookie_secret_env: Option<String>,
    cookie_secret_path: Option<PathBuf>,
    allow_insecure_cookies: bool,
    shared_domain: Option<String>,
    public_origins: Vec<String>,
}

pub struct Origins {
    pub origins: PublicOrigins,
    pub from: OriginsFrom,
}

/// Shows the count and source, never an origin.
impl std::fmt::Debug for Origins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Origins")
            .field("count", &self.origins.iter().count())
            .field("from", &self.from)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginsFrom {
    File,
    /// [`ENV_SESSION_PUBLIC_ORIGINS`], which replaces the file's list
    /// whenever it is set, even to the same origins.
    Environment {
        /// How many entries the file's list held. Replaced by presence,
        /// not by a difference in content; 0 when the file lists none.
        replaced_file_entries: usize,
    },
}

pub enum KeySource {
    FleetEnv(SessionKey),
    /// The variable `[web] cookie_secret_env` names; not read yet
    /// ([`key_from_env`] reads it). `name` is operator text: an operator
    /// may have pasted the key itself here, so it is not printed as is.
    ConfigEnv {
        name: String,
    },
    File {
        /// The path as written: a `~` in it expands where the file is read.
        path: PathBuf,
    },
    /// No source: startup generates an ephemeral key, so sessions end
    /// with the process.
    None,
}

/// Names the kind only: no key, variable name or path.
impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::FleetEnv(_) => "FleetEnv",
            Self::ConfigEnv { .. } => "ConfigEnv",
            Self::File { .. } => "File",
            Self::None => "None",
        })
    }
}

pub struct CookieSettings {
    pub secure: bool,
    pub secure_from: SettingSource,
    /// The shared SSO domain (`Domain=`), or `None` for a host-only
    /// cookie. Operator text: not for output.
    pub shared_domain: Option<String>,
    pub domain_from: SettingSource,
    /// [`ENV_SESSION_COOKIE_PATH`] or the default. Either way the path is
    /// `/`, the only one Fleet sessions accept.
    pub path_from: SettingSource,
    pub ttl_secs: u64,
    pub ttl_from: SettingSource,
}

/// Shows whether a shared domain is set, never the domain.
impl std::fmt::Debug for CookieSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieSettings")
            .field("secure", &self.secure)
            .field("secure_from", &self.secure_from)
            .field("shared_domain", &self.shared_domain.is_some())
            .field("domain_from", &self.domain_from)
            .field("path_from", &self.path_from)
            .field("ttl_secs", &self.ttl_secs)
            .field("ttl_from", &self.ttl_from)
            .finish()
    }
}

pub struct UpstreamPlan {
    /// The URL as configured or derived, which passed
    /// [`check_upstream_url`]. The value is not for output.
    pub url: String,
    pub checked_url: reqwest::Url,
    pub url_from: SettingSource,
    pub connect: Option<UpstreamConnect>,
    pub trust: TrustSource,
}

/// Shows sources only: no URL, host or address.
impl std::fmt::Debug for UpstreamPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamPlan")
            .field("url_from", &self.url_from)
            .field("connect", &self.connect.is_some())
            .field("trust", &self.trust)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum TrustSource {
    System,
    Pinned {
        /// The path as written: a `~` in it expands where the file is read.
        path: PathBuf,
        from: SettingSource,
    },
}

/// Shows the mode and source, not the path.
impl std::fmt::Debug for TrustSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str("System"),
            Self::Pinned { from, .. } => f.debug_struct("Pinned").field("from", from).finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BindAddr {
    /// The address, unparsed, as startup binds it. Operator text: not for
    /// output.
    pub addr: String,
    pub from: SettingSource,
}

/// Shows the source only, not the address.
impl std::fmt::Debug for BindAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindAddr")
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

/// The allowlist: the environment's when [`ENV_SESSION_PUBLIC_ORIGINS`] is
/// set, whether or not it parses, and the file's otherwise.
fn resolve_origins(
    web: &WebConfig,
    from_environment: Result<Option<PublicOrigins>, ConfigError>,
) -> Result<Origins, ConfigError> {
    // The environment REPLACES the file's list, never merges with it: a
    // merged allowlist would keep a stale config entry authorizing an
    // origin the operator believes they moved away from.
    match from_environment? {
        Some(origins) => Ok(Origins {
            origins,
            from: OriginsFrom::Environment {
                replaced_file_entries: web.public_origins.len(),
            },
        }),
        None => Ok(Origins {
            origins: PublicOrigins::parse(&web.public_origins)?,
            from: OriginsFrom::File,
        }),
    }
}

/// Select the key source in startup's precedence. An invalid
/// [`ENV_SESSION_AEAD_KEY`] fails the component rather than falling back.
fn resolve_key_source(
    web: &WebConfig,
    from_environment: Result<Option<SessionKey>, ConfigError>,
) -> Result<KeySource, ConfigError> {
    if let Some(key) = from_environment? {
        return Ok(KeySource::FleetEnv(key));
    }
    // Both configured sources set: the variable wins, and startup warns.
    Ok(if let Some(name) = &web.cookie_secret_env {
        KeySource::ConfigEnv { name: name.clone() }
    } else if let Some(path) = &web.cookie_secret_path {
        KeySource::File { path: path.clone() }
    } else {
        KeySource::None
    })
}

/// The cookie's attributes: each `FLEET_SESSION_*` variable, when set,
/// overrides only its own setting. An invalid one fails the component, in
/// the parser's order: path, secure, domain.
fn resolve_cookie_settings(
    web: &WebConfig,
    path_set: Result<bool, ConfigError>,
    secure: Result<Option<bool>, ConfigError>,
    domain: Result<RuntimeCookieDomain, ConfigError>,
) -> Result<CookieSettings, ConfigError> {
    let path_from = if path_set? {
        SettingSource::Environment(ENV_SESSION_COOKIE_PATH)
    } else {
        SettingSource::Default
    };
    let (secure, secure_from) = match secure? {
        Some(secure) => (
            secure,
            SettingSource::Environment(ENV_SESSION_COOKIE_SECURE),
        ),
        None if web.allow_insecure_cookies => (false, SettingSource::File),
        None => (true, SettingSource::Default),
    };
    let (shared_domain, domain_from) = match domain? {
        RuntimeCookieDomain::PreserveConfigured => {
            // Empty string == unset == standalone mode, so an operator can
            // "comment out" SSO by blanking the value.
            match web.shared_domain.clone().filter(|s| !s.is_empty()) {
                Some(domain) => (Some(domain), SettingSource::File),
                None => (None, SettingSource::Default),
            }
        }
        RuntimeCookieDomain::HostOnly => {
            (None, SettingSource::Environment(ENV_SESSION_COOKIE_DOMAIN))
        }
        RuntimeCookieDomain::Explicit(domain) => (
            Some(domain),
            SettingSource::Environment(ENV_SESSION_COOKIE_DOMAIN),
        ),
    };
    let (ttl_secs, ttl_from) = match web.session_ttl_secs {
        Some(ttl) => (ttl, SettingSource::File),
        None => (DEFAULT_SESSION_TTL_SECS, SettingSource::Default),
    };
    Ok(CookieSettings {
        secure,
        secure_from,
        shared_domain,
        domain_from,
        path_from,
        ttl_secs,
        ttl_from,
    })
}

fn resolve_upstream(
    web: &WebConfig,
    server: Option<&ServerConfig>,
    host: &HostEnv,
) -> Result<UpstreamPlan, ConfigError> {
    let (url, url_from) = match &web.upstream_url {
        Some(url) => (url.clone(), SettingSource::File),
        None => derive_upstream(server, host.http_addr.as_deref()),
    };
    let checked_url = check_upstream_url(&url).map_err(ConfigError::UpstreamUrl)?;
    let connect = web
        .upstream_connect_addr
        .as_deref()
        .map(|addr| resolve_upstream_connect(&checked_url, addr))
        .transpose()
        .map_err(ConfigError::UpstreamConnectAddr)?;
    let trust = match select_upstream_ca_path(
        host.upstream_ca_path.as_deref(),
        web.upstream_ca_path.as_deref(),
    )? {
        None => TrustSource::System,
        Some((path, from)) => TrustSource::Pinned {
            path: refuse_empty_pin(path)?,
            from,
        },
    };
    Ok(UpstreamPlan {
        url,
        checked_url,
        url_from,
        connect,
        trust,
    })
}

/// The `FLEET_SESSION_*` variables exist for `fleet-dev`, but this is the
/// production binary and it reads them unconditionally.
fn warn_on_runtime_override(sources: &Sources) {
    let configured = &sources.configured;
    if matches!(sources.cookie_key, Ok(KeySource::FleetEnv(_)))
        && (configured.cookie_secret_env.is_some() || configured.cookie_secret_path.is_some())
    {
        tracing::warn!(
            event_type = "session_key_runtime_override",
            env = ENV_SESSION_AEAD_KEY,
            cookie_secret_env = ?configured.cookie_secret_env,
            cookie_secret_path = ?configured.cookie_secret_path,
            "FLEET_SESSION_AEAD_KEY overrides the configured cookie secret"
        );
    }
    if let Ok(settings) = &sources.cookie_settings {
        if matches!(settings.secure_from, SettingSource::Environment(_))
            && !settings.secure
            && !configured.allow_insecure_cookies
        {
            tracing::warn!(
                event_type = "session_cookie_secure_downgraded",
                env = ENV_SESSION_COOKIE_SECURE,
                "the environment is clearing Secure on the session cookie"
            );
        }
        if matches!(settings.domain_from, SettingSource::Environment(_))
            && configured.shared_domain.is_some()
        {
            tracing::warn!(
                event_type = "session_cookie_domain_override",
                env = ENV_SESSION_COOKIE_DOMAIN,
                configured = ?configured.shared_domain,
                "the environment overrides the configured shared cookie domain"
            );
        }
    }
    if let Ok(Origins {
        origins: from_environment,
        from: OriginsFrom::Environment { .. },
    }) = &sources.public_origins
        && !configured.public_origins.is_empty()
    {
        // Both packaged deployments render one list into the config file
        // and hand the same list to the environment, so warning on
        // the variable's mere presence fires on every default install.
        // A configured list that does not parse counts as displaced.
        let displaced = !PublicOrigins::parse(&configured.public_origins)
            .is_ok_and(|configured| same_origins(&configured, from_environment));
        // The override's origins are printed because they parsed: each is
        // at most a serialized origin's worth of ASCII.
        let (origins_count, origins) = summarize_origins(from_environment);
        if displaced {
            // The configured entries stay counted rather than printed:
            // this arm covers the list that failed to parse, whose text is
            // unvalidated and unbounded.
            tracing::warn!(
                event_type = "session_public_origins_override",
                env = ENV_SESSION_PUBLIC_ORIGINS,
                configured_entries = configured.public_origins.len(),
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

/// [`PublicOrigins::parse`] refuses two entries that normalize alike, so
/// neither list can repeat an origin and equal lengths plus one-way
/// containment is set equality.
fn same_origins(left: &PublicOrigins, right: &PublicOrigins) -> bool {
    left.iter().count() == right.iter().count() && left.iter().all(|origin| right.contains(origin))
}

const MAX_LOGGED_ORIGINS: usize = 8;

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

/// A value that is set but not UTF-8 is refused, naming the
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

fn select_bind_addr(
    env_value: Option<&OsStr>,
    configured: Option<&str>,
) -> Result<(String, SettingSource), ConfigError> {
    let pick = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
    if let Some(addr) = pick(env_override(ENV_BIND_ADDR, env_value)?) {
        return Ok((addr, SettingSource::Environment(ENV_BIND_ADDR)));
    }
    Ok(match pick(configured) {
        Some(addr) => (addr, SettingSource::File),
        None => (DEFAULT_BIND_ADDR.to_owned(), SettingSource::Default),
    })
}

#[cfg(test)]
fn resolve_bind_addr(
    env_value: Option<&OsStr>,
    configured: Option<&str>,
) -> Result<String, ConfigError> {
    select_bind_addr(env_value, configured).map(|(addr, _)| addr)
}

fn select_upstream_ca_path(
    env_value: Option<&OsStr>,
    configured: Option<&Path>,
) -> Result<Option<(PathBuf, SettingSource)>, ConfigError> {
    if let Some(path) = env_override(ENV_UPSTREAM_CA_PATH, env_value)?.filter(|v| !v.is_empty()) {
        return Ok(Some((
            PathBuf::from(path),
            SettingSource::Environment(ENV_UPSTREAM_CA_PATH),
        )));
    }
    Ok(configured.map(|path| (path.to_owned(), SettingSource::File)))
}

#[cfg(test)]
fn resolve_upstream_ca_path(
    env_value: Option<&OsStr>,
    configured: Option<&Path>,
) -> Result<Option<PathBuf>, ConfigError> {
    select_upstream_ca_path(env_value, configured).map(|picked| picked.map(|(path, _)| path))
}

/// Check the rules every upstream URL follows, whatever the trust mode
/// (ADR-0048).
/// - It carries no user name or password. reqwest would turn them into a
///   Basic `Authorization` header, and the URL goes into the startup log.
/// - It carries no query or fragment. Each request's path and query are
///   appended to the URL as text, which would land them inside the query
///   or fragment, and a query may hold a token the startup log would show.
/// - It is `https`: a plain `http` upstream would send every user's key in
///   cleartext.
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
    // The parser keeps an empty `?` or `#` as an empty query or fragment,
    // so those are refused too.
    if url.query().is_some() || url.fragment().is_some() {
        return Err(UpstreamUrlError::QueryOrFragment);
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
/// (ADR-0048). A name is refused: resolving it would be the resolver
/// answer this setting exists to replace. The client dials the URL's port
/// when the URL states one and the address's port when it does not, so a
/// mismatch would silently drop one of them. The URL's port counts as 443
/// when it states none, which also covers an explicit `:443` that the
/// parser normalizes away.
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

#[cfg(test)]
pub(crate) fn resolve_upstream_tls(ca_path: Option<&Path>) -> Result<UpstreamTls, ConfigError> {
    match ca_path {
        None => Ok(UpstreamTls::System),
        Some(path) => load_pin(refuse_empty_pin(expand_tilde(path))?),
    }
}

/// Falling back to the platform roots would change the trust mode on a
/// blank value, and reading it would report a missing file.
fn refuse_empty_pin(path: PathBuf) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::UpstreamCa {
            path,
            reason: "the path is empty. Name trawld's CA file, or remove the setting to trust the platform roots"
                .to_owned(),
        });
    }
    Ok(path)
}

fn load_pin(path: PathBuf) -> Result<UpstreamTls, ConfigError> {
    let refuse = |reason: &str| ConfigError::UpstreamCa {
        path: path.clone(),
        reason: reason.to_owned(),
    };
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

pub(crate) const MAX_PIN_FILE_BYTES: u64 = 1024 * 1024;

/// The open never waits: on unix it passes `O_NONBLOCK`, so a FIFO with
/// no writer opens at once instead of blocking until one appears. The
/// type and size checks run on the opened handle, so the path cannot be
/// swapped between check and read. The read stops one byte past the
/// cap, so a file that grows after the size check is refused too.
/// Reading a regular file can still stall, on a network volume for one, so
/// callers on the async runtime run this on the blocking pool.
pub(crate) fn read_capped_file(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;

    let file = open_capped(path, cap)?;
    let mut contents = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > cap {
        return Err(too_large(cap));
    }
    Ok(contents)
}

/// [`read_capped_file`] for a secret: the bytes land in one [`Zeroizing`]
/// buffer from the first byte, and every path out of the read scrubs it.
///
/// The buffer is allocated once, `cap + 1` bytes, before the first read,
/// and never grown, so no reallocation leaves a copy behind; the reads go
/// straight into it, with no intermediate buffer. An error part way
/// through, or a file past the cap, drops the buffer here, zeroized. It
/// suits a small cap, such as a key's: the whole buffer is allocated even
/// for a short file.
pub(crate) fn read_capped_secret(path: &Path, cap: u64) -> std::io::Result<Zeroizing<Vec<u8>>> {
    use std::io::Read as _;

    let mut file = open_capped(path, cap)?;
    let size = usize::try_from(cap.saturating_add(1)).map_err(|_| too_large(cap))?;
    let mut buffer = Zeroizing::new(vec![0; size]);
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if filled as u64 > cap {
        return Err(too_large(cap));
    }
    buffer.truncate(filled);
    Ok(buffer)
}

fn open_capped(path: &Path, cap: u64) -> std::io::Result<std::fs::File> {
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
    if metadata.len() > cap {
        return Err(too_large(cap));
    }
    Ok(file)
}

fn too_large(cap: u64) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::FileTooLarge,
        format!("the file is larger than {cap} bytes"),
    )
}

pub(crate) fn read_pin_file(path: &Path) -> std::io::Result<Vec<u8>> {
    read_capped_file(path, MAX_PIN_FILE_BYTES).map_err(|e| {
        if e.kind() == std::io::ErrorKind::FileTooLarge {
            std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!(
                    "the file is larger than {} MiB, too large for a CA bundle",
                    MAX_PIN_FILE_BYTES / (1024 * 1024)
                ),
            )
        } else {
            e
        }
    })
}

/// Every certificate goes through the same root-store check the TLS stack
/// applies when the client is built, so a bundle that passes here cannot
/// fail there with a bare "builder error".
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

fn derive_upstream(
    server: Option<&ServerConfig>,
    http_addr_env: Option<&OsStr>,
) -> (String, SettingSource) {
    let Some(srv) = server else {
        return (FALLBACK_UPSTREAM_URL.to_owned(), SettingSource::Default);
    };
    let (resolved, from) =
        match ServerConfig::http_addr_override(http_addr_env.and_then(OsStr::to_str)) {
            Some(addr) => (addr, SettingSource::DerivedFromEnvironment(ENV_HTTP_ADDR)),
            None => (srv.http_addr.as_str(), SettingSource::DerivedFromFile),
        };
    let addr = resolved.trim();
    let (host, port_suffix) = split_addr(addr);
    let is_ipv6 = host.contains(':');
    let host = match host {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    let url = if is_ipv6 && host != "127.0.0.1" {
        format!("https://[{host}]{port_suffix}")
    } else {
        format!("https://{host}{port_suffix}")
    };
    (url, from)
}

#[cfg(test)]
fn default_upstream_from_server(server: Option<&ServerConfig>) -> String {
    derive_upstream(server, std::env::var_os(ENV_HTTP_ADDR).as_deref()).0
}

/// Does not validate; malformed input passes through unchanged so an
/// operator with an oddly-formatted addr gets an explanatory reqwest
/// error rather than a silent URL-building mistake.
fn split_addr(addr: &str) -> (&str, &str) {
    if let Some(rest) = addr.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let host = &rest[..end];
        let port = &rest[end + 1..];
        return (host, port);
    }
    addr.rsplit_once(':').map_or((addr, ""), |(host, port)| {
        (host, &addr[host.len()..host.len() + 1 + port.len()])
    })
}

/// Reads that one variable from the process. Logs nothing and generates
/// nothing, so the doctor calls it as startup does.
pub(crate) fn key_from_env(name: &str) -> Result<SessionKey, ConfigError> {
    let raw = match std::env::var(name) {
        Ok(v) => Zeroizing::new(v),
        Err(std::env::VarError::NotPresent) => {
            return Err(ConfigError::EnvMissing {
                name: name.to_owned(),
            });
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(ConfigError::EnvUtf8 {
                name: name.to_owned(),
            });
        }
    };
    SessionKey::from_base64(&raw).map_err(|_| ConfigError::EnvKey {
        name: name.to_owned(),
    })
}

fn warn_on_ambiguous_key(configured: &Configured) {
    if configured.cookie_secret_env.is_some() && configured.cookie_secret_path.is_some() {
        tracing::warn!(
            event_type = "session_key_ambiguous",
            cookie_secret_env = ?configured.cookie_secret_env,
            cookie_secret_path = ?configured.cookie_secret_path,
            "both cookie_secret_env and cookie_secret_path are set; env takes precedence"
        );
    }
}

fn ephemeral_key() -> SessionKey {
    tracing::warn!(
        event_type = "session_key_ephemeral",
        key_len = KEY_LEN,
        "no cookie_secret configured; generated an ephemeral key. sessions will not survive restart."
    );
    SessionKey::generate()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::test_support::captured_logs;

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
        assert_eq!(text, "https://trawl.example.com,http://localhost:8090");
    }

    fn configured_web() -> WebConfig {
        WebConfig {
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..WebConfig::default()
        }
    }

    #[test]
    fn defaults_applied_when_web_section_carries_only_the_required_origin() {
        let web = configured_web();
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

        assert!(
            SessionRuntimeOverrides::parse(Some("not-base64".into()), None, None, None, None)
                .is_err()
        );
    }

    #[test]
    fn an_empty_allowlist_refuses_to_resolve_and_names_both_doors() {
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
        assert!(warning.contains("configured_entries=2"), "got: {warning}");
        assert!(
            warning.contains("origins=http://localhost:8081"),
            "got: {warning}"
        );
        assert!(!warning.contains(TEST_ORIGIN), "got: {warning}");
    }

    #[test]
    fn an_environment_list_matching_the_file_is_noted_at_info_not_warned() {
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
        assert!(!warning.contains("/app"), "got: {warning}");
    }

    #[test]
    fn the_environment_alone_satisfies_the_requirement_without_warning() {
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
        assert_eq!(resolve(env(""), Some("127.0.0.1:9000")), "127.0.0.1:9000");
        assert_eq!(resolve(env(""), Some("")), DEFAULT_BIND_ADDR);
    }

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
    fn proxy_limits_follow_the_server_section() {
        let web = configured_web();
        let srv = ServerConfig {
            timeout_secs: 7,
            max_request_body_bytes: 99,
            ..dummy_server()
        };
        let resolved = ResolvedConfig::from_parsed(&web, Some(&srv)).unwrap();
        assert_eq!(resolved.limits.request_body_bytes, 99);
        assert_eq!(
            resolved.limits.upstream_head_timeout,
            Duration::from_secs(17)
        );
        assert_eq!(resolved.limits.body_read_timeout, Duration::from_secs(30));

        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        assert_eq!(
            resolved.limits.request_body_bytes,
            trawl_config::DEFAULT_MAX_REQUEST_BODY_BYTES
        );
        assert_eq!(
            resolved.limits.upstream_head_timeout,
            Duration::from_secs(40)
        );
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

    /// `ServerConfig` implements no `Default`, so it comes from a TOML parse
    /// where every other field takes its serde default.
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
        let resolved = ResolvedConfig::from_parsed(&configured_web(), None).unwrap();
        assert!(resolved.shared_domain.is_none());

        let web = WebConfig {
            shared_domain: Some(String::new()),
            ..configured_web()
        };
        let resolved = ResolvedConfig::from_parsed(&web, None).unwrap();
        assert!(resolved.shared_domain.is_none());
    }

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
    fn upstream_query_or_fragment_refused_without_echo() {
        const SENTINEL: &str = "s3ntinel";
        for upstream in [
            format!("https://trawld:5514/?token={SENTINEL}"),
            format!("https://trawld:5514?{SENTINEL}"),
            format!("https://trawld:5514/#{SENTINEL}"),
            format!("https://trawld:5514/prefix?{SENTINEL}#{SENTINEL}"),
            // Empty, but kept by the parser: appending a request path after
            // them would still land in the query or the fragment.
            "https://trawld:5514/?".to_owned(),
            "https://trawld:5514/#".to_owned(),
            // Refused before the scheme is judged, like credentials.
            format!("http://trawld:5514/?{SENTINEL}"),
        ] {
            let web = WebConfig {
                upstream_url: Some(upstream.clone()),
                ..configured_web()
            };
            let (outcome, lines) = captured_logs(|| resolve(&web));
            let error = outcome.expect_err("a URL with a query or fragment must refuse");
            assert!(
                matches!(
                    error,
                    ConfigError::UpstreamUrl(UpstreamUrlError::QueryOrFragment)
                ),
                "{upstream}: {error:?}"
            );
            let display = error.to_string();
            for rendered in [&display, &format!("{error:?}")] {
                assert!(!rendered.contains(SENTINEL), "{upstream}: {rendered}");
                assert!(!rendered.contains("trawld:5514"), "{upstream}: {rendered}");
            }
            assert!(display.contains("[web] upstream_url"), "got: {display}");
            assert!(
                lines.iter().all(|line| !line.contains(SENTINEL)),
                "{upstream}: {lines:?}"
            );
        }

        for upstream in ["https://trawld:5514/", "https://trawld:5514/prefix"] {
            let web = WebConfig {
                upstream_url: Some(upstream.to_owned()),
                ..configured_web()
            };
            let resolved = resolve(&web).expect("a path without a query resolves");
            assert_eq!(resolved.upstream_url, upstream);
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
            (
                "https://Trawl.Lab.Example:5514",
                "127.0.0.1:5514",
                "trawl.lab.example",
                "127.0.0.1:5514",
            ),
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

    /// The file is sparse, so the test allocates nothing.
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
        let resolved = resolve(&web).unwrap();
        assert!(matches!(
            resolved.upstream_tls,
            UpstreamTls::PinnedCa { roots: Some(_), .. }
        ));
    }

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

        let missing = dir.path().join("tls").join("cert.pem");
        assert!(
            matches!(
                resolve_upstream_tls(Some(&missing)),
                Ok(UpstreamTls::PinnedCa { path, roots: None }) if path == missing
            ),
            "a missing generated certificate is pending"
        );
    }

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
        let _ = resolved.cookie_key;
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "fills the Option fields of RuntimeEnv"
    )]
    fn os(value: &str) -> Option<OsString> {
        Some(OsString::from(value))
    }

    /// A comparable rendering of every component, values included.
    fn summary(sources: &Sources) -> Vec<String> {
        fn part<T>(result: &Result<T, ConfigError>, show: impl FnOnce(&T) -> String) -> String {
            match result {
                Ok(value) => show(value),
                Err(error) => format!("err: {error}"),
            }
        }
        vec![
            part(&sources.public_origins, |o| {
                format!("{:?} {}", o.from, summarize_origins(&o.origins).1)
            }),
            part(&sources.cookie_key, |key| match key {
                KeySource::FleetEnv(key) => format!("fleet {}", key.to_base64url().as_str()),
                KeySource::ConfigEnv { name } => format!("env {name}"),
                KeySource::File { path } => format!("file {}", path.display()),
                KeySource::None => "none".to_owned(),
            }),
            part(&sources.cookie_settings, |s| {
                format!(
                    "{} {:?} {:?} {:?} {:?} {} {:?}",
                    s.secure,
                    s.secure_from,
                    s.shared_domain,
                    s.domain_from,
                    s.path_from,
                    s.ttl_secs,
                    s.ttl_from
                )
            }),
            part(&sources.upstream, |u| {
                let pin = match &u.trust {
                    TrustSource::Pinned { path, .. } => path.display().to_string(),
                    TrustSource::System => String::new(),
                };
                format!(
                    "{} {} {:?} {:?} {:?} {pin}",
                    u.url, u.checked_url, u.url_from, u.connect, u.trust
                )
            }),
            part(&sources.bind_addr, |b| format!("{} {:?}", b.addr, b.from)),
        ]
    }

    /// The key file and the pin below do not exist, the `cookie_secret_env`
    /// variable is not set, and every environment override that startup warns
    /// about is present, yet no line is logged.
    #[test]
    #[expect(clippy::too_many_lines, reason = "one assertion group per selection")]
    fn sources_name_every_selection() {
        let dir = tempfile::tempdir().unwrap();
        let missing_key = dir.path().join("absent").join("web.cookie");
        let missing_pin = dir.path().join("absent").join("cert.pem");
        let fleet_key = SessionKey::from_bytes([0x42; KEY_LEN]).to_base64url();
        let resolve = |web: &WebConfig, server: Option<&ServerConfig>, env: RuntimeEnv| {
            let (sources, lines) = captured_logs(|| {
                Sources::resolve(web, server, RuntimeParts::from_each_variable(env))
            });
            assert!(lines.is_empty(), "resolution logged: {lines:?}");
            sources
        };
        let srv = ServerConfig {
            http_addr: "127.0.0.1:8080".into(),
            ..dummy_server()
        };

        let both = WebConfig {
            cookie_secret_env: Some("TRAWL_TEST_C2_NEVER_SET".into()),
            cookie_secret_path: Some(missing_key.clone()),
            ..configured_web()
        };
        let sources = resolve(
            &both,
            None,
            RuntimeEnv {
                aead_key: os(fleet_key.as_str()),
                ..RuntimeEnv::default()
            },
        );
        assert!(matches!(&sources.cookie_key, Ok(KeySource::FleetEnv(key))
                if key.to_base64url().as_str() == fleet_key.as_str()));
        let sources = resolve(
            &both,
            None,
            RuntimeEnv {
                aead_key: os("not-base64"),
                ..RuntimeEnv::default()
            },
        );
        assert!(
            matches!(&sources.cookie_key, Err(ConfigError::EnvKey { name })
                if name == ENV_SESSION_AEAD_KEY),
            "{sources:?}"
        );
        let sources = resolve(&both, None, RuntimeEnv::default());
        assert!(
            matches!(&sources.cookie_key, Ok(KeySource::ConfigEnv { name })
                if name == "TRAWL_TEST_C2_NEVER_SET")
        );
        let path_only = WebConfig {
            cookie_secret_path: Some(missing_key.clone()),
            ..configured_web()
        };
        let sources = resolve(&path_only, None, RuntimeEnv::default());
        assert!(
            matches!(&sources.cookie_key, Ok(KeySource::File { path }) if *path == missing_key)
        );
        let tilde = WebConfig {
            cookie_secret_path: Some("~/.trawl-c2-absent/web.cookie".into()),
            ..configured_web()
        };
        let sources = resolve(&tilde, None, RuntimeEnv::default());
        assert!(matches!(&sources.cookie_key, Ok(KeySource::File { path })
                if *path == Path::new("~/.trawl-c2-absent/web.cookie")));
        let sources = resolve(&configured_web(), None, RuntimeEnv::default());
        assert!(matches!(sources.cookie_key, Ok(KeySource::None)));

        let two = WebConfig {
            public_origins: vec![TEST_ORIGIN.to_owned(), "http://localhost:8090".to_owned()],
            ..WebConfig::default()
        };
        let origins_env = |value: &str| RuntimeEnv {
            public_origins: os(value),
            ..RuntimeEnv::default()
        };
        let from = |sources: &Sources| {
            sources
                .public_origins
                .as_ref()
                .map(|o| (o.from, o.origins.iter().count()))
                .map_err(ToString::to_string)
        };
        assert_eq!(
            from(&resolve(&two, None, RuntimeEnv::default())),
            Ok((OriginsFrom::File, 2))
        );
        assert_eq!(
            from(&resolve(&two, None, origins_env("http://localhost:8081"))),
            Ok((
                OriginsFrom::Environment {
                    replaced_file_entries: 2
                },
                1
            ))
        );
        assert_eq!(
            from(&resolve(&configured_web(), None, origins_env(TEST_ORIGIN))),
            Ok((
                OriginsFrom::Environment {
                    replaced_file_entries: 1
                },
                1
            ))
        );
        assert_eq!(
            from(&resolve(
                &WebConfig::default(),
                None,
                origins_env("http://localhost:8081")
            )),
            Ok((
                OriginsFrom::Environment {
                    replaced_file_entries: 0
                },
                1
            ))
        );
        let sources = resolve(&two, None, origins_env("not-an-origin"));
        assert!(
            matches!(
                sources.public_origins,
                Err(ConfigError::SessionEnvOrigins(_))
            ),
            "{sources:?}"
        );
        let sources = resolve(&WebConfig::default(), None, RuntimeEnv::default());
        assert!(matches!(
            sources.public_origins,
            Err(ConfigError::PublicOrigins {
                source: PublicOriginsError::Empty
            })
        ));

        let settings = |sources: Sources| sources.cookie_settings.unwrap();
        let s = settings(resolve(&configured_web(), None, RuntimeEnv::default()));
        assert!(s.secure);
        assert_eq!(s.secure_from, SettingSource::Default);
        assert_eq!(s.shared_domain, None);
        assert_eq!(s.domain_from, SettingSource::Default);
        assert_eq!(s.path_from, SettingSource::Default);
        assert_eq!(
            (s.ttl_secs, s.ttl_from),
            (DEFAULT_SESSION_TTL_SECS, SettingSource::Default)
        );
        let from_file = WebConfig {
            allow_insecure_cookies: true,
            shared_domain: Some(".fleet.example".into()),
            session_ttl_secs: Some(3600),
            ..configured_web()
        };
        let s = settings(resolve(&from_file, None, RuntimeEnv::default()));
        assert!(!s.secure);
        assert_eq!(s.secure_from, SettingSource::File);
        assert_eq!(s.shared_domain.as_deref(), Some(".fleet.example"));
        assert_eq!(s.domain_from, SettingSource::File);
        assert_eq!((s.ttl_secs, s.ttl_from), (3600, SettingSource::File));
        let blank_domain = WebConfig {
            shared_domain: Some(String::new()),
            ..configured_web()
        };
        let s = settings(resolve(&blank_domain, None, RuntimeEnv::default()));
        assert_eq!(
            (s.shared_domain, s.domain_from),
            (None, SettingSource::Default)
        );
        let s = settings(resolve(
            &from_file,
            None,
            RuntimeEnv {
                cookie_secure: os("true"),
                cookie_domain: os(""),
                cookie_path: os("/"),
                ..RuntimeEnv::default()
            },
        ));
        assert!(s.secure);
        assert_eq!(
            s.secure_from,
            SettingSource::Environment(ENV_SESSION_COOKIE_SECURE)
        );
        assert_eq!(s.shared_domain, None, "an empty variable forces host-only");
        assert_eq!(
            s.domain_from,
            SettingSource::Environment(ENV_SESSION_COOKIE_DOMAIN)
        );
        assert_eq!(
            s.path_from,
            SettingSource::Environment(ENV_SESSION_COOKIE_PATH)
        );
        let s = settings(resolve(
            &from_file,
            None,
            RuntimeEnv {
                cookie_domain: os(".new.example"),
                ..RuntimeEnv::default()
            },
        ));
        assert_eq!(s.shared_domain.as_deref(), Some(".new.example"));
        let sources = resolve(
            &configured_web(),
            None,
            RuntimeEnv {
                cookie_path: os("/app"),
                ..RuntimeEnv::default()
            },
        );
        assert!(
            matches!(&sources.cookie_settings, Err(ConfigError::SessionEnvValue { name, .. })
                if *name == ENV_SESSION_COOKIE_PATH),
            "{sources:?}"
        );

        let url_from = |sources: &Sources| {
            let upstream = sources.upstream.as_ref().unwrap();
            (upstream.url.clone(), upstream.url_from)
        };
        let explicit = WebConfig {
            upstream_url: Some("https://trawld.internal:9000".into()),
            ..configured_web()
        };
        assert_eq!(
            url_from(&resolve(&explicit, Some(&srv), RuntimeEnv::default())),
            (
                "https://trawld.internal:9000".to_owned(),
                SettingSource::File
            )
        );
        let http_addr = |value: &str| RuntimeEnv {
            http_addr: os(value),
            ..RuntimeEnv::default()
        };
        assert_eq!(
            url_from(&resolve(
                &configured_web(),
                Some(&srv),
                http_addr("0.0.0.0:9999")
            )),
            (
                "https://127.0.0.1:9999".to_owned(),
                SettingSource::DerivedFromEnvironment(ENV_HTTP_ADDR)
            )
        );
        assert_eq!(
            url_from(&resolve(&configured_web(), Some(&srv), http_addr(""))),
            (
                "https://127.0.0.1:8080".to_owned(),
                SettingSource::DerivedFromFile
            )
        );
        assert_eq!(
            url_from(&resolve(&configured_web(), None, http_addr("0.0.0.0:9999"))),
            (FALLBACK_UPSTREAM_URL.to_owned(), SettingSource::Default)
        );

        let trust = |sources: Sources| sources.upstream.map(|u| u.trust);
        let pinned_in_file = WebConfig {
            upstream_ca_path: Some(missing_pin.clone()),
            ..configured_web()
        };
        assert_eq!(
            trust(resolve(&pinned_in_file, None, RuntimeEnv::default())).unwrap(),
            TrustSource::Pinned {
                path: missing_pin.clone(),
                from: SettingSource::File
            }
        );
        let other_pin = dir.path().join("other.pem");
        let ca_env = |value: &OsStr| RuntimeEnv {
            upstream_ca_path: Some(value.to_owned()),
            ..RuntimeEnv::default()
        };
        assert_eq!(
            trust(resolve(
                &pinned_in_file,
                None,
                ca_env(other_pin.as_os_str())
            ))
            .unwrap(),
            TrustSource::Pinned {
                path: other_pin,
                from: SettingSource::Environment(ENV_UPSTREAM_CA_PATH)
            }
        );
        assert_eq!(
            trust(resolve(&pinned_in_file, None, ca_env(OsStr::new("")))).unwrap(),
            TrustSource::Pinned {
                path: missing_pin,
                from: SettingSource::File
            }
        );
        assert_eq!(
            trust(resolve(&configured_web(), None, RuntimeEnv::default())).unwrap(),
            TrustSource::System
        );
        let empty_pin = WebConfig {
            upstream_ca_path: Some(PathBuf::new()),
            ..configured_web()
        };
        assert!(matches!(
            trust(resolve(&empty_pin, None, RuntimeEnv::default())),
            Err(ConfigError::UpstreamCa { .. })
        ));
        let connected = WebConfig {
            upstream_url: Some("https://trawl.test:5514".into()),
            upstream_connect_addr: Some("127.0.0.1:5514".into()),
            ..configured_web()
        };
        let sources = resolve(&connected, None, RuntimeEnv::default());
        assert_eq!(
            sources.upstream.unwrap().connect,
            Some(UpstreamConnect {
                host: "trawl.test".to_owned(),
                addr: "127.0.0.1:5514".parse().unwrap(),
            })
        );

        let bind = |web: &WebConfig, env: RuntimeEnv| resolve(web, None, env).bind_addr.unwrap();
        let bound = WebConfig {
            bind_addr: Some("127.0.0.1:9000".into()),
            ..configured_web()
        };
        assert_eq!(
            bind(
                &bound,
                RuntimeEnv {
                    bind_addr: os("0.0.0.0:9091"),
                    ..RuntimeEnv::default()
                }
            ),
            BindAddr {
                addr: "0.0.0.0:9091".into(),
                from: SettingSource::Environment(ENV_BIND_ADDR)
            }
        );
        assert_eq!(
            bind(&bound, RuntimeEnv::default()),
            BindAddr {
                addr: "127.0.0.1:9000".into(),
                from: SettingSource::File
            }
        );
        assert_eq!(
            bind(&configured_web(), RuntimeEnv::default()),
            BindAddr {
                addr: DEFAULT_BIND_ADDR.into(),
                from: SettingSource::Default
            }
        );
    }

    #[test]
    fn sources_debug_shows_no_configured_value() {
        const SENTINEL: &str = "s3ntinel";
        let fleet_key = SessionKey::from_bytes([0x42; KEY_LEN]).to_base64url();
        let web = WebConfig {
            public_origins: vec![format!("https://{SENTINEL}.example")],
            upstream_url: Some(format!("https://{SENTINEL}.internal:5514")),
            upstream_connect_addr: Some("127.0.0.9:5514".into()),
            upstream_ca_path: Some(format!("/{SENTINEL}/ca.pem").into()),
            shared_domain: Some(format!(".{SENTINEL}.example")),
            cookie_secret_env: Some(SENTINEL.into()),
            bind_addr: Some(format!("{SENTINEL}.bind:9000")),
            ..WebConfig::default()
        };
        let derived = WebConfig {
            upstream_url: None,
            upstream_connect_addr: None,
            cookie_secret_env: None,
            cookie_secret_path: Some(format!("/{SENTINEL}/web.cookie").into()),
            ..web.clone()
        };
        let server = ServerConfig {
            http_addr: format!("{SENTINEL}.srv:5514"),
            ..dummy_server()
        };
        let cases = [
            (&web, None, RuntimeEnv::default()),
            (
                &web,
                None,
                RuntimeEnv {
                    aead_key: os(fleet_key.as_str()),
                    ..RuntimeEnv::default()
                },
            ),
            (
                &web,
                None,
                RuntimeEnv {
                    public_origins: os(&format!("https://{SENTINEL}.example,{SENTINEL}")),
                    ..RuntimeEnv::default()
                },
            ),
            (
                &web,
                None,
                RuntimeEnv {
                    bind_addr: os(&format!("{SENTINEL}.env:9091")),
                    upstream_ca_path: os(&format!("/{SENTINEL}/env-ca.pem")),
                    ..RuntimeEnv::default()
                },
            ),
            (&derived, Some(&server), RuntimeEnv::default()),
            (
                &derived,
                Some(&server),
                RuntimeEnv {
                    http_addr: os(&format!("{SENTINEL}.env:5514")),
                    ..RuntimeEnv::default()
                },
            ),
        ];
        for (web, server, env) in cases {
            let env_debug = format!("{env:?}");
            let parts = RuntimeParts::from_each_variable(env);
            let parts_debug = format!("{parts:?}");
            let sources = Sources::resolve(web, server, parts);
            assert!(sources.bind_addr.is_ok(), "{sources:?}");
            assert!(sources.upstream.is_ok(), "{sources:?}");
            for rendered in [env_debug, parts_debug, format!("{sources:?}")] {
                assert!(!rendered.contains(SENTINEL), "{rendered}");
                assert!(!rendered.contains(fleet_key.as_str()), "{rendered}");
                assert!(!rendered.contains("127.0.0.9"), "{rendered}");
            }
        }
    }

    #[test]
    fn resolve_keeps_tilde_paths_as_written() {
        let web = WebConfig {
            cookie_secret_path: Some("~/web.cookie".into()),
            ..configured_web()
        };
        let sources = Sources::resolve(
            &web,
            None,
            RuntimeParts::from_each_variable(RuntimeEnv {
                upstream_ca_path: os("~/tls/cert.pem"),
                ..RuntimeEnv::default()
            }),
        );
        assert!(
            matches!(&sources.cookie_key, Ok(KeySource::File { path })
                if *path == Path::new("~/web.cookie")),
            "{sources:?}"
        );
        assert!(
            matches!(&sources.upstream, Ok(UpstreamPlan {
                    trust: TrustSource::Pinned { path, .. },
                    ..
                }) if *path == Path::new("~/tls/cert.pem")),
            "{sources:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one table row per error startup returns"
    )]
    fn startup_expands_tilde_only_where_each_file_is_read() {
        use std::os::unix::ffi::OsStringExt as _;

        struct Case {
            name: String,
            web: WebConfig,
            env: RuntimeEnv,
            error: String,
            /// The paths expanded before it, in order.
            expanded: Vec<&'static str>,
        }
        /// A `FLEET_SESSION_*` variable, the environment holding a value in
        /// it alone, and a value it refuses.
        type Fleet = (
            &'static str,
            fn(Option<OsString>) -> RuntimeEnv,
            &'static str,
        );
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("cookie.key"), [0x24; KEY_LEN]).unwrap();
        std::fs::write(home.path().join("ca.pem"), test_ca_pem()).unwrap();
        let home_dir = home.path().to_str().unwrap().to_owned();
        let startup = |web: &WebConfig, env: RuntimeEnv| {
            let mut asked = Vec::new();
            let result = ResolvedConfig::startup(web, None, env, |path: &Path| {
                asked.push(path.to_owned());
                let text = path.to_string_lossy();
                PathBuf::from(
                    shellexpand::tilde_with_context(&text, || Some(home_dir.as_str())).into_owned(),
                )
            });
            (result, asked)
        };
        let tilde_paths = WebConfig {
            cookie_secret_path: Some("~/cookie.key".into()),
            upstream_ca_path: Some("~/ca.pem".into()),
            ..configured_web()
        };
        let not_utf8 = || Some(OsString::from_vec(vec![b'/', 0xff]));

        let mut cases = vec![
            Case {
                name: "an invalid origin, before the key file".into(),
                web: WebConfig {
                    public_origins: vec!["not-an-origin".into()],
                    ..tilde_paths.clone()
                },
                env: RuntimeEnv::default(),
                error: String::new(),
                expanded: vec![],
            },
            Case {
                name: "a missing cookie_secret_env variable, before the pin".into(),
                web: WebConfig {
                    cookie_secret_env: Some("TRAWL_TEST_C2_NEVER_SET".into()),
                    ..tilde_paths.clone()
                },
                env: RuntimeEnv::default(),
                error: ConfigError::EnvMissing {
                    name: "TRAWL_TEST_C2_NEVER_SET".into(),
                }
                .to_string(),
                expanded: vec![],
            },
            Case {
                name: "an upstream URL that is not https, after the key file".into(),
                web: WebConfig {
                    upstream_url: Some("http://trawld:5514".into()),
                    ..tilde_paths.clone()
                },
                env: RuntimeEnv::default(),
                error: String::new(),
                expanded: vec!["~/cookie.key"],
            },
            Case {
                name: "a connect address that is not an address, before the pin".into(),
                web: WebConfig {
                    upstream_url: Some("https://trawld:5514".into()),
                    upstream_connect_addr: Some("trawld".into()),
                    cookie_secret_path: None,
                    ..tilde_paths.clone()
                },
                env: RuntimeEnv::default(),
                error: String::new(),
                expanded: vec![],
            },
            Case {
                name: "a CA path variable that is not UTF-8, before the pin".into(),
                web: tilde_paths.clone(),
                env: RuntimeEnv {
                    upstream_ca_path: not_utf8(),
                    ..RuntimeEnv::default()
                },
                error: ConfigError::EnvUtf8 {
                    name: ENV_UPSTREAM_CA_PATH.into(),
                }
                .to_string(),
                expanded: vec!["~/cookie.key"],
            },
            Case {
                name: "a listen address that is not UTF-8, after both files".into(),
                web: tilde_paths.clone(),
                env: RuntimeEnv {
                    bind_addr: not_utf8(),
                    ..RuntimeEnv::default()
                },
                error: ConfigError::EnvUtf8 {
                    name: ENV_BIND_ADDR.into(),
                }
                .to_string(),
                expanded: vec!["~/cookie.key", "~/ca.pem"],
            },
        ];
        let fleet: [Fleet; 5] = [
            (
                ENV_SESSION_AEAD_KEY,
                |v| RuntimeEnv {
                    aead_key: v,
                    ..RuntimeEnv::default()
                },
                "not-base64",
            ),
            (
                ENV_SESSION_COOKIE_DOMAIN,
                |v| RuntimeEnv {
                    cookie_domain: v,
                    ..RuntimeEnv::default()
                },
                "bad domain",
            ),
            (
                ENV_SESSION_COOKIE_PATH,
                |v| RuntimeEnv {
                    cookie_path: v,
                    ..RuntimeEnv::default()
                },
                "/other",
            ),
            (
                ENV_SESSION_COOKIE_SECURE,
                |v| RuntimeEnv {
                    cookie_secure: v,
                    ..RuntimeEnv::default()
                },
                "maybe",
            ),
            (
                ENV_SESSION_PUBLIC_ORIGINS,
                |v| RuntimeEnv {
                    public_origins: v,
                    ..RuntimeEnv::default()
                },
                "not-an-origin",
            ),
        ];
        for (name, env, invalid) in fleet {
            for (how, value) in [("invalid", os(invalid)), ("not UTF-8", not_utf8())] {
                let error = RuntimeParts::from_whole_parse(env(value.clone()))
                    .map(|_| ())
                    .expect_err("the whole parse refuses")
                    .to_string();
                cases.push(Case {
                    name: format!("{name} {how}"),
                    web: tilde_paths.clone(),
                    env: env(value),
                    error,
                    expanded: vec![],
                });
            }
        }

        for case in cases {
            let (result, asked) = startup(&case.web, case.env);
            let error = result.map(|_| ()).expect_err(&case.name).to_string();
            let expected = if case.error.is_empty() {
                let sources = Sources::resolve(
                    &case.web,
                    None,
                    RuntimeParts::from_each_variable(RuntimeEnv::default()),
                );
                [
                    sources.public_origins.as_ref().err(),
                    sources.upstream.as_ref().err(),
                ]
                .into_iter()
                .flatten()
                .next()
                .unwrap_or_else(|| panic!("{}: no component error", case.name))
                .to_string()
            } else {
                case.error
            };
            assert_eq!(error, expected, "{}", case.name);
            let expanded: Vec<PathBuf> = case.expanded.iter().map(PathBuf::from).collect();
            assert_eq!(asked, expanded, "{}: {error}", case.name);
        }

        let (result, asked) = startup(&tilde_paths, RuntimeEnv::default());
        let resolved = result.expect("startup resolves");
        assert_eq!(
            asked,
            [PathBuf::from("~/cookie.key"), PathBuf::from("~/ca.pem")]
        );
        assert_eq!(
            resolved.cookie_key.to_base64url().as_str(),
            SessionKey::from_bytes([0x24; KEY_LEN])
                .to_base64url()
                .as_str()
        );
        assert!(
            matches!(&resolved.upstream_tls, UpstreamTls::PinnedCa { path, roots: Some(_) }
                if *path == home.path().join("ca.pem")),
            "{:?}",
            resolved.upstream_tls
        );

        let pin = "~/.trawl-c2-absent/tls/cert.pem";
        let expected = PathBuf::from(shellexpand::tilde(pin).as_ref());
        assert!(
            !expected.starts_with("~"),
            "no home directory to expand into"
        );
        assert_eq!(expand_tilde(Path::new(pin)), expected);
        let mut asked = Vec::new();
        let resolved = ResolvedConfig::startup(
            &WebConfig {
                upstream_ca_path: Some(pin.into()),
                ..configured_web()
            },
            None,
            RuntimeEnv::default(),
            |path: &Path| {
                asked.push(path.to_owned());
                expand_tilde(path)
            },
        )
        .expect("a missing pin is pending");
        assert_eq!(asked, [PathBuf::from(pin)]);
        assert!(
            matches!(&resolved.upstream_tls, UpstreamTls::PinnedCa { path, roots: None }
                if *path == expected),
            "{:?}",
            resolved.upstream_tls
        );
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one table row per case")]
    fn runtime_parts_agree_with_the_whole_parse() {
        type Observed = (
            Result<Option<String>, String>,
            Result<RuntimeCookieDomain, String>,
            Result<bool, String>,
            Result<Option<bool>, String>,
            Result<Option<Vec<String>>, String>,
        );
        fn observed(parts: &RuntimeParts) -> Observed {
            let err = |e: &ConfigError| e.to_string();
            (
                parts
                    .key
                    .as_ref()
                    .map(|k| k.as_ref().map(|k| k.to_base64url().to_string()))
                    .map_err(err),
                parts.domain.as_ref().cloned().map_err(err),
                parts.path_set.as_ref().copied().map_err(err),
                parts.secure.as_ref().copied().map_err(err),
                parts
                    .public_origins
                    .as_ref()
                    .map(|o| {
                        o.as_ref()
                            .map(|o| o.iter().map(ToString::to_string).collect())
                    })
                    .map_err(err),
            )
        }
        fn env(row: [Option<&str>; 5]) -> RuntimeEnv {
            let [
                aead_key,
                cookie_domain,
                cookie_path,
                cookie_secure,
                public_origins,
            ] = row.map(|value| value.map(OsString::from));
            RuntimeEnv {
                aead_key,
                cookie_domain,
                cookie_path,
                cookie_secure,
                public_origins,
                ..RuntimeEnv::default()
            }
        }

        let key = SessionKey::from_bytes([0x07; KEY_LEN]).to_base64url();
        let key = Some(key.as_str());
        let rows: [[Option<&str>; 5]; 13] = [
            [None; 5],
            [
                key,
                Some(""),
                Some("/"),
                Some("true"),
                Some("https://a.example"),
            ],
            [
                key,
                Some(".fleet.example"),
                None,
                Some("false"),
                Some("http://localhost:8090,https://b.example:443"),
            ],
            [Some("not-base64"), None, None, None, None],
            [None, Some("bad domain"), None, None, None],
            [None, None, Some("/app"), None, None],
            [None, None, None, Some("yes"), None],
            [None, None, None, None, Some("not-an-origin")],
            [None, None, None, None, Some("")],
            [
                None,
                None,
                None,
                None,
                Some("https://a.example,https://a.example:443"),
            ],
            [
                Some("not-base64"),
                Some("bad domain"),
                Some("/app"),
                Some("yes"),
                Some(""),
            ],
            [None, Some("bad domain"), Some("/app"), Some("yes"), None],
            [key, Some("bad domain"), None, Some("yes"), Some("x")],
        ];
        for row in rows {
            let each = RuntimeParts::from_each_variable(env(row));
            let seen = observed(&each);
            let whole = SessionRuntimeOverrides::parse(
                row[0].map(str::to_owned),
                row[1].map(str::to_owned),
                row[2].map(str::to_owned),
                row[3].map(str::to_owned),
                row[4].map(str::to_owned),
            );
            match whole {
                Ok(overrides) => {
                    let expected: Observed = (
                        Ok(overrides.key.as_ref().map(|k| k.to_base64url().to_string())),
                        Ok(overrides.domain.clone()),
                        Ok(row[2].is_some()),
                        Ok(overrides.secure),
                        Ok(overrides
                            .public_origins
                            .as_ref()
                            .map(|o| o.iter().map(ToString::to_string).collect())),
                    );
                    assert_eq!(seen, expected, "{row:?}");
                    let startup = RuntimeParts::from_whole_parse(env(row))
                        .unwrap_or_else(|e| panic!("{row:?}: {e}"));
                    assert_eq!(observed(&startup), expected, "{row:?}");
                }
                Err(error) => {
                    let expected = ConfigError::from(error).to_string();
                    let first = [
                        seen.0.as_ref().err(),
                        seen.2.as_ref().err(),
                        seen.3.as_ref().err(),
                        seen.1.as_ref().err(),
                        seen.4.as_ref().err(),
                    ]
                    .into_iter()
                    .flatten()
                    .next();
                    assert_eq!(first, Some(&expected), "{row:?}: {seen:?}");
                    let startup = RuntimeParts::from_whole_parse(env(row))
                        .expect_err("the whole parse refuses");
                    assert_eq!(startup.to_string(), expected, "{row:?}");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_fleet_variable_refuses_first_and_alone() {
        use std::os::unix::ffi::OsStringExt;

        let not_utf8 = || Some(OsString::from_vec(vec![b'/', 0xff]));
        let env = || RuntimeEnv {
            aead_key: os("not-base64"),
            cookie_domain: not_utf8(),
            cookie_secure: os("true"),
            ..RuntimeEnv::default()
        };
        let error = RuntimeParts::from_whole_parse(env()).expect_err("refuses");
        assert!(
            matches!(&error, ConfigError::EnvUtf8 { name } if name == ENV_SESSION_COOKIE_DOMAIN),
            "{error:?}"
        );
        let each = RuntimeParts::from_each_variable(env());
        assert!(matches!(&each.key, Err(ConfigError::EnvKey { .. })));
        assert!(
            matches!(&each.domain, Err(ConfigError::EnvUtf8 { name }) if name == ENV_SESSION_COOKIE_DOMAIN)
        );
        assert!(matches!(each.secure, Ok(Some(true))));
        assert!(matches!(each.path_set, Ok(false)));
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one resolver, compared end to end")]
    fn startup_and_doctor_share_resolve() {
        /// The environment for one case, given the Fleet key to use.
        type EnvFor = fn(&str) -> RuntimeEnv;

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("web.cookie");
        std::fs::write(&key_path, [0x24; KEY_LEN]).unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, test_ca_pem()).unwrap();
        let fleet_key = SessionKey::from_bytes([0x42; KEY_LEN]).to_base64url();
        let srv = ServerConfig {
            http_addr: "0.0.0.0:5514".into(),
            ..dummy_server()
        };
        let file = WebConfig {
            public_origins: vec![TEST_ORIGIN.to_owned()],
            cookie_secret_path: Some(key_path),
            shared_domain: Some(".fleet.example".into()),
            session_ttl_secs: Some(3600),
            upstream_url: Some("https://trawl.test:5514/prefix".into()),
            upstream_connect_addr: Some("127.0.0.1:5514".into()),
            upstream_ca_path: Some(ca_path.clone()),
            bind_addr: Some("127.0.0.1:9000".into()),
            ..WebConfig::default()
        };
        let cases: [(&str, WebConfig, EnvFor); 3] = [
            ("file only", file.clone(), |_| RuntimeEnv::default()),
            ("every override", file, |key| RuntimeEnv {
                aead_key: os(key),
                cookie_domain: os(""),
                cookie_path: os("/"),
                cookie_secure: os("false"),
                public_origins: os("http://localhost:8081"),
                bind_addr: os("0.0.0.0:9091"),
                upstream_ca_path: None,
                http_addr: os("127.0.0.1:6000"),
            }),
            ("derived upstream", configured_web(), |key| RuntimeEnv {
                aead_key: os(key),
                http_addr: os("[::1]:6000"),
                ..RuntimeEnv::default()
            }),
        ];
        for (case, web, env) in cases {
            let startup = Sources::resolve(
                &web,
                Some(&srv),
                RuntimeParts::from_whole_parse(env(fleet_key.as_str())).unwrap(),
            );
            let doctor = Sources::resolve(
                &web,
                Some(&srv),
                RuntimeParts::from_each_variable(env(fleet_key.as_str())),
            );
            let selected = summary(&startup);
            assert_eq!(selected, summary(&doctor), "{case}");

            let settings = startup.cookie_settings.as_ref().unwrap();
            let (secure, shared_domain, ttl_secs) = (
                settings.secure,
                settings.shared_domain.clone(),
                settings.ttl_secs,
            );
            let upstream = startup.upstream.as_ref().unwrap();
            let (url, connect, pinned) = (
                upstream.url.clone(),
                upstream.connect.clone(),
                matches!(upstream.trust, TrustSource::Pinned { .. }),
            );
            let bind = startup.bind_addr.as_ref().unwrap().addr.clone();
            let origins = startup.public_origins.as_ref().unwrap().origins.clone();
            let key = match startup.cookie_key.as_ref().unwrap() {
                KeySource::FleetEnv(_) => fleet_key.to_string(),
                KeySource::File { .. } => SessionKey::from_bytes([0x24; KEY_LEN])
                    .to_base64url()
                    .to_string(),
                other => panic!("{case}: {other:?}"),
            };

            let resolved =
                ResolvedConfig::from_sources(startup).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(resolved.bind_addr, bind, "{case}");
            assert_eq!(resolved.upstream_url, url, "{case}");
            assert_eq!(resolved.upstream_connect, connect, "{case}");
            assert_eq!(
                matches!(
                    resolved.upstream_tls,
                    UpstreamTls::PinnedCa { roots: Some(_), .. }
                ),
                pinned,
                "{case}"
            );
            assert_eq!(resolved.session_ttl_secs, ttl_secs, "{case}");
            assert_eq!(resolved.allow_insecure_cookies, !secure, "{case}");
            assert_eq!(resolved.shared_domain, shared_domain, "{case}");
            assert_eq!(resolved.public_origins, origins, "{case}");
            assert_eq!(resolved.cookie_key.to_base64url().as_str(), key, "{case}");
        }

        let broken = WebConfig {
            upstream_url: Some("http://trawld:5514".into()),
            bind_addr: Some("127.0.0.1:9000".into()),
            ..WebConfig::default()
        };
        let sources = Sources::resolve(
            &broken,
            None,
            RuntimeParts::from_whole_parse(RuntimeEnv::default()).unwrap(),
        );
        let upstream_error = sources
            .upstream
            .as_ref()
            .map(|_| ())
            .unwrap_err()
            .to_string();
        let origins_error = sources
            .public_origins
            .as_ref()
            .map(|_| ())
            .unwrap_err()
            .to_string();
        let error = ResolvedConfig::from_sources(sources).expect_err("both refuse");
        assert_eq!(error.to_string(), origins_error, "origins come first");
        assert_ne!(origins_error, upstream_error);

        let bad_key = || RuntimeEnv {
            aead_key: os("not-base64"),
            ..RuntimeEnv::default()
        };
        let startup = RuntimeParts::from_whole_parse(bad_key()).expect_err("startup refuses");
        let doctor = Sources::resolve(
            &configured_web(),
            None,
            RuntimeParts::from_each_variable(bad_key()),
        );
        assert_eq!(
            doctor
                .cookie_key
                .as_ref()
                .map(|_| ())
                .unwrap_err()
                .to_string(),
            startup.to_string()
        );
        assert!(doctor.public_origins.is_ok());
        assert!(doctor.cookie_settings.is_ok());
        assert!(doctor.upstream.is_ok());
        assert!(doctor.bind_addr.is_ok());
    }

    #[test]
    fn read_capped_file_refuses_past_its_cap_and_never_waits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.cookie");
        std::fs::write(&path, [0x42; KEY_LEN + 1]).unwrap();
        assert_eq!(
            read_capped_file(&path, KEY_LEN as u64 + 1).unwrap().len(),
            KEY_LEN + 1
        );
        let error = read_capped_file(&path, KEY_LEN as u64).expect_err("past the cap");
        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
        let error = read_capped_file(dir.path(), 1024).expect_err("a directory");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let error = read_capped_file(&dir.path().join("absent"), 1024).expect_err("a missing file");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        let secret = read_capped_secret(&path, KEY_LEN as u64 + 1).unwrap();
        assert_eq!(*secret, [0x42; KEY_LEN + 1]);
        assert_eq!(secret.capacity(), KEY_LEN + 2);
        let error = read_capped_secret(&path, KEY_LEN as u64).expect_err("past the cap");
        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
        let error = read_capped_secret(dir.path(), 1024).expect_err("a directory");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let error =
            read_capped_secret(&dir.path().join("absent"), 1024).expect_err("a missing file");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        #[cfg(unix)]
        {
            let fifo = dir.path().join("fifo");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("run mkfifo");
            assert!(status.success(), "mkfifo: {status}");
            let error = read_capped_file(&fifo, 1024).expect_err("a FIFO");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }
}
