// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which server `trawl doctor` checks, with which key, under which trust
//! (ADR-0047).
//!
//! Two steps, kept apart on purpose:
//!
//! - [`select`] turns the command line into a [`Selection`], or refuses it.
//!   A refusal is a usage error (exit 2) that names the variable or flag and
//!   never its value. Nothing is read from disk here.
//! - [`resolve`] reads what the selection names, once, and produces the
//!   `connection.config` check. A source that is missing or unusable fails
//!   that check and leaves no [`Connection`], so no later check can contact
//!   anything.
//!
//! The rules that keep a key where the user bound it:
//!
//! - The ambient environment never selects anything: `TRAWL_URL`,
//!   `TRAWL_PROFILE`, `TRAWL_TOKEN` and `TRAWL_INSECURE` refuse the run when
//!   present, even empty.
//! - `--url` never reads `config.toml`, so a key saved for another server
//!   cannot follow it. Its key comes only from `--token-env` or
//!   `--token-file`.
//! - A profile's key comes only from that profile's own table, never from
//!   `[server].token`. Its trust (`ca_cert`, `insecure`) inherits as it does
//!   for every other command: an inherited pin only adds verification, and an
//!   inherited `insecure` only makes the TLS check fail.
//! - `-p trial` resolves through the trial's own rules (ADR-0045).
//! - A URL that carries userinfo is refused before anything is built: the
//!   HTTP client would turn it into an `Authorization: Basic` header.
//!
//! Nothing here calls `Config::load_token`, which falls back to
//! `[server].token`, and nothing renders the `Display` of a `ConfigError` or
//! `TrialError`: every reason is written here, from names alone.

use std::fmt;
use std::io::Read as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use clap::error::ErrorKind;
use trawl_api::doctor::{Check, Outcome, Target};
use trawl_client::TlsTrust;
use zeroize::Zeroizing;

use crate::config::{self, Config, ConfigError, ProfileConfig};
use crate::trial;

/// Id of the check this module produces.
pub const CONNECTION_CONFIG: &str = "connection.config";

/// A key file holds one key and a newline; anything larger is not a key.
const MAX_KEY_BYTES: u64 = 4096;

/// The command line as `trawl doctor` sees it, before selection.
///
/// `url`, `token`, `insecure` and `profile` are the global flags. clap
/// merges each with its environment variable, which is why [`select`]
/// refuses those variables before it trusts any of these.
#[derive(Debug, Default)]
pub struct Invocation {
    /// `--url`.
    pub url: Option<String>,
    /// Whether `--token` was given. Its value is never looked at.
    pub token: bool,
    /// `--insecure`.
    pub insecure: bool,
    /// `-p/--profile`.
    pub profile: Option<String>,
    /// `-c/--config`.
    pub config: Option<String>,
    /// `--token-env NAME`.
    pub token_env: Option<String>,
    /// `--token-file PATH`.
    pub token_file: Option<PathBuf>,
    /// `--web-url ORIGIN`.
    pub web_url: Option<String>,
}

/// A command line [`select`] refused: a usage error, exit 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The clap error kind it is reported as.
    pub kind: ErrorKind,
    /// The message. It names variables and flags, never their values.
    pub message: String,
}

impl Refusal {
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::ArgumentConflict,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidValue,
            message: message.into(),
        }
    }
}

/// What the command line selected. Nothing has been read yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The API target and where its key comes from.
    pub target: TargetSelection,
    /// The browser origin `--web-url` names, if any.
    pub web: Option<CheckedUrl>,
}

/// The one API target a doctor run checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSelection {
    /// `--url`: no config file, trust from system roots unless
    /// `--insecure`, and a key only from a source named on the command line.
    Url {
        url: CheckedUrl,
        insecure: bool,
        key: KeySource,
    },
    /// `--profile NAME` other than `trial`.
    Profile {
        name: String,
        config: Option<String>,
    },
    /// `-p trial` (ADR-0045).
    Trial { config: Option<String> },
}

/// Where a `--url` run reads its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// No key: `api.identity` is `not_configured`.
    None,
    /// `--token-env NAME`.
    Env(String),
    /// `--token-file PATH`.
    File(PathBuf),
}

/// `http` or `https`; nothing else is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// A URL that passed the doctor's shape rules: `http` or `https`, a host,
/// an optional port, and no userinfo.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckedUrl {
    scheme: Scheme,
    host: String,
    /// `scheme://host[:port]`, lowercased.
    origin: String,
    /// The origin plus any path, without a trailing `/`.
    base: String,
    /// Whether anything followed the authority besides a lone `/`.
    has_path: bool,
}

impl fmt::Debug for CheckedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedUrl")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl CheckedUrl {
    /// `scheme://host[:port]`.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The URL requests are built on: the origin plus any path prefix.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Whether the host is `localhost` or a loopback address literal.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        let host = self.host.as_str();
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            return v6.parse::<Ipv6Addr>().is_ok_and(|ip| ip.is_loopback());
        }
        host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
    }
}

/// Why a URL was not accepted. Neither variant carries any of the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlProblem {
    /// Not `http(s)://host[:port][/path]`.
    Malformed,
    /// It carries userinfo (`user@` or `user:password@`).
    Credentials,
}

/// Check a URL's shape.
///
/// The authority ends where the WHATWG parser ends it, at the first `/`,
/// `\`, `?` or `#`, and any `@` inside it is userinfo. That check comes
/// first, so a URL with credentials is always reported as such.
pub fn check_url(raw: &str) -> Result<CheckedUrl, UrlProblem> {
    let (scheme, rest) = raw.split_once("://").ok_or(UrlProblem::Malformed)?;
    let end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err(UrlProblem::Credentials);
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) || tail.contains('\\') {
        return Err(UrlProblem::Malformed);
    }
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        _ => return Err(UrlProblem::Malformed),
    };
    let host = host_of(authority).ok_or(UrlProblem::Malformed)?;
    let origin = format!("{}://{}", scheme.as_str(), authority.to_ascii_lowercase());
    let path = tail.trim_end_matches('/');
    Ok(CheckedUrl {
        scheme,
        host: host.to_ascii_lowercase(),
        base: format!("{origin}{path}"),
        origin,
        has_path: !path.is_empty(),
    })
}

/// The host of `host[:port]` or `[v6][:port]`, if the authority is one.
fn host_of(authority: &str) -> Option<&str> {
    let (host, port) = if authority.starts_with('[') {
        let close = authority.find(']')?;
        let (host, after) = authority.split_at(close + 1);
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':')?)),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if let Some(port) = port
        && (port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().is_err())
    {
        return None;
    }
    let valid = if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        v6.parse::<Ipv6Addr>().is_ok()
    } else {
        !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    };
    valid.then_some(host)
}

/// Turn the command line into a [`Selection`], or refuse it.
///
/// `refused_env` is every variable clap binds for `trawl doctor`, and
/// `env_present` answers whether one is set in this process, empty or not.
/// The first rule broken is the one reported.
pub fn select(
    invocation: &Invocation,
    refused_env: &[String],
    env_present: impl Fn(&str) -> bool,
) -> Result<Selection, Refusal> {
    if let Some(name) = refused_env.iter().find(|name| env_present(name)) {
        return Err(Refusal::conflict(format!(
            "{name} is set in the environment, and trawl doctor takes its target, key, and \
             trust only from the command line. Unset {name}, then name the target with \
             --url or --profile"
        )));
    }
    if invocation.token {
        return Err(Refusal::conflict(
            "--token cannot be used with trawl doctor, which never takes a key's value on the \
             command line. With --url, name where the key is with --token-env or --token-file; \
             or use a profile that holds it",
        ));
    }
    let web = invocation
        .web_url
        .as_deref()
        .map(check_web_url)
        .transpose()?;

    let target = match (&invocation.url, &invocation.profile) {
        (None, None) => {
            return Err(Refusal {
                kind: ErrorKind::MissingRequiredArgument,
                message: "trawl doctor needs one target: give --url or --profile".to_owned(),
            });
        }
        (Some(_), Some(_)) => {
            return Err(Refusal::conflict(
                "--url and --profile both select a target; trawl doctor checks one, so give one",
            ));
        }
        (Some(url), None) => select_url(invocation, url)?,
        (None, Some(profile)) => select_profile(invocation, profile)?,
    };
    Ok(Selection { target, web })
}

fn select_url(invocation: &Invocation, url: &str) -> Result<TargetSelection, Refusal> {
    if invocation.config.is_some() {
        return Err(Refusal::conflict(
            "-c cannot be used with --url: --url never reads a config file. To check a saved \
             connection, use --profile",
        ));
    }
    let url = check_url(url).map_err(|problem| match problem {
        UrlProblem::Credentials => Refusal::invalid(
            "--url carries credentials (a user or password before the host), which would be \
             sent to the server. Remove them, and name the key with --token-env or --token-file",
        ),
        UrlProblem::Malformed => Refusal::invalid(
            "--url must be an http or https URL with a host, such as https://HOST:PORT",
        ),
    })?;
    let key = match (&invocation.token_env, &invocation.token_file) {
        (Some(_), Some(_)) => {
            return Err(Refusal::conflict(
                "--token-env and --token-file both name a key; give one",
            ));
        }
        (Some(name), None) => KeySource::Env(name.clone()),
        (None, Some(path)) => KeySource::File(path.clone()),
        (None, None) => KeySource::None,
    };
    Ok(TargetSelection::Url {
        url,
        insecure: invocation.insecure,
        key,
    })
}

fn select_profile(invocation: &Invocation, profile: &str) -> Result<TargetSelection, Refusal> {
    let bound = |flag: &str| {
        Refusal::conflict(format!(
            "--profile binds the connection's URL, key, and trust, so {flag} cannot be used \
             with it. Change the profile, or check the server with --url instead"
        ))
    };
    if invocation.insecure {
        return Err(bound("--insecure"));
    }
    if invocation.token_env.is_some() {
        return Err(bound("--token-env"));
    }
    if invocation.token_file.is_some() {
        return Err(bound("--token-file"));
    }
    let config = invocation.config.clone();
    Ok(if profile == trial::PROFILE {
        TargetSelection::Trial { config }
    } else {
        TargetSelection::Profile {
            name: profile.to_owned(),
            config,
        }
    })
}

fn check_web_url(raw: &str) -> Result<CheckedUrl, Refusal> {
    let url = check_url(raw).map_err(|problem| match problem {
        UrlProblem::Credentials => Refusal::invalid(
            "--web-url carries credentials (a user or password before the host), which would \
             be sent to that origin. Remove them",
        ),
        UrlProblem::Malformed => Refusal::invalid(
            "--web-url must be an http or https origin with a host, such as https://HOST:PORT",
        ),
    })?;
    if url.has_path {
        return Err(Refusal::invalid(
            "--web-url must be an origin: a scheme, a host, and a port, with no path, query, \
             or fragment",
        ));
    }
    if url.scheme == Scheme::Http && !url.is_loopback() {
        return Err(Refusal::invalid(
            "--web-url must use https unless its host is loopback (localhost, 127.0.0.1, or \
             [::1])",
        ));
    }
    Ok(url)
}

/// An API key the selection named, read once.
pub struct Key {
    secret: Zeroizing<String>,
    source: String,
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl Key {
    /// The key itself. It goes only into the request that proves identity.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Where the key came from, named without its value.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

/// A connection that resolved: everything the API checks need.
#[derive(Debug)]
pub struct Connection {
    /// The API URL.
    pub url: CheckedUrl,
    /// How the server's certificate is verified.
    pub trust: TlsTrust,
    /// The key, when one was selected.
    pub key: Option<Key>,
}

/// The outcome of [`resolve`].
#[derive(Debug)]
pub struct Resolution {
    /// What the report names as its target.
    pub target: Target,
    /// The `connection.config` check.
    pub check: Check,
    /// Present only when `connection.config` is complete.
    pub connection: Option<Connection>,
}

/// Why `connection.config` failed, all of it written here.
#[derive(Debug)]
struct Failure {
    source: String,
    reason: String,
    next_action: String,
}

impl Failure {
    fn new(source: impl Into<String>, reason: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            reason: reason.into(),
            next_action: next.into(),
        }
    }
}

/// Read what the selection names and build the `connection.config` check.
pub fn resolve(selection: &TargetSelection) -> Resolution {
    let (source, resolved) = match selection {
        TargetSelection::Url { url, insecure, key } => {
            ("--url flag".to_owned(), resolve_url(url, *insecure, key))
        }
        TargetSelection::Profile { name, config } => {
            let path = config_path(config.as_deref());
            (
                format!("CLI profile `{name}` in {path}"),
                resolve_profile(name, config.as_deref(), &path),
            )
        }
        TargetSelection::Trial { config } => (
            "trial profile (-p trial)".to_owned(),
            resolve_trial(config.as_deref()),
        ),
    };
    match resolved {
        Ok((connection, trust_detail)) => {
            let key = connection
                .key
                .as_ref()
                .map_or("none selected", |key| key.source.as_str());
            let check = Check {
                id: CONNECTION_CONFIG.to_owned(),
                outcome: Outcome::Complete,
                reason: None,
                detail: Some(format!("trust: {trust_detail}; key: {key}")),
                source: Some(source.clone()),
                blocked_by: None,
                next_action: None,
            };
            Resolution {
                target: Target {
                    origin: Some(connection.url.origin.clone()),
                    source,
                },
                check,
                connection: Some(connection),
            }
        }
        Err(failure) => Resolution {
            target: Target {
                origin: None,
                source,
            },
            check: Check {
                id: CONNECTION_CONFIG.to_owned(),
                outcome: Outcome::Failed,
                reason: Some(failure.reason),
                detail: None,
                source: Some(failure.source),
                blocked_by: None,
                next_action: Some(failure.next_action),
            },
            connection: None,
        },
    }
}

/// The config path as the user named it, for messages.
fn config_path(config: Option<&str>) -> String {
    config.unwrap_or(config::DEFAULT_CONFIG_PATH).to_owned()
}

/// A resolved connection and the trust mode, named.
type Resolved = Result<(Connection, String), Failure>;

fn resolve_url(url: &CheckedUrl, insecure: bool, key: &KeySource) -> Resolved {
    let key = match key {
        KeySource::None => None,
        KeySource::Env(name) => Some(key_from_env(name)?),
        KeySource::File(path) => Some(key_from_file(path)?),
    };
    let (trust, detail) = if insecure {
        (
            TlsTrust::AcceptInvalid,
            "certificate verification off (--insecure)",
        )
    } else {
        (TlsTrust::System, "system roots")
    };
    Ok((
        Connection {
            url: url.clone(),
            trust,
            key,
        },
        detail.to_owned(),
    ))
}

fn key_from_env(name: &str) -> Result<Key, Failure> {
    // The variable's name is not echoed: a key pasted in its place would be.
    const SOURCE: &str = "key from --token-env";
    let fail = |what: &str| {
        Failure::new(
            SOURCE,
            format!("the variable --token-env names {what}"),
            "export the key in that variable, or name another with --token-env",
        )
    };
    let value = match std::env::var(name) {
        Ok(value) => Zeroizing::new(value),
        Err(std::env::VarError::NotPresent) => return Err(fail("is not set")),
        Err(std::env::VarError::NotUnicode(_)) => return Err(fail("is not valid UTF-8")),
    };
    key_from_text(&value, SOURCE).ok_or_else(|| fail("is empty"))
}

fn key_from_file(path: &std::path::Path) -> Result<Key, Failure> {
    // The path is not echoed, for the same reason as the variable's name.
    const SOURCE: &str = "key from --token-file";
    let fail = |what: &str| {
        Failure::new(
            SOURCE,
            format!("the file --token-file names {what}"),
            "write the key to that file, or name another with --token-file",
        )
    };
    // Non-blocking, so a FIFO cannot stall the open; the regular-file check
    // on the opened handle then refuses it.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(fail("does not exist"));
        }
        Err(_) => return Err(fail("cannot be read")),
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(fail("is not a regular file")),
        Err(_) => return Err(fail("cannot be read")),
    }
    let mut bytes = Zeroizing::new(Vec::new());
    if file
        .take(MAX_KEY_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Err(fail("cannot be read"));
    }
    if bytes.len() as u64 > MAX_KEY_BYTES {
        return Err(fail("is larger than an API key"));
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Err(fail("is not valid UTF-8"));
    };
    key_from_text(text, SOURCE).ok_or_else(|| fail("is empty"))
}

/// A trimmed, non-empty key.
fn key_from_text(text: &str, source: &str) -> Option<Key> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| Key {
        secret: Zeroizing::new(trimmed.to_owned()),
        source: source.to_owned(),
    })
}

/// The `connection.config` failure for a config file that did not load.
fn config_failure(err: &ConfigError, path: &str, profile: &str) -> Failure {
    let source = format!("config file {path}");
    match err {
        ConfigError::Missing { .. } => Failure::new(
            source,
            "the config file does not exist",
            format!("create it with a [profiles.{profile}] table, or name another file with -c"),
        ),
        ConfigError::Parse { .. } => Failure::new(
            source,
            "the config file is not valid TOML for trawl",
            "fix the file's syntax",
        ),
        _ => Failure::new(
            source,
            "the config file cannot be read",
            "check the file's permissions",
        ),
    }
}

fn resolve_profile(name: &str, config: Option<&str>, path: &str) -> Resolved {
    let mut cfg = Config::load_required(config).map_err(|e| config_failure(&e, path, name))?;
    let table = format!("[profiles.{name}]");
    let source = format!("{table} in {path}");
    let Some(profile) = cfg.profiles.get(name).cloned() else {
        let mut names: Vec<&str> = cfg.profiles.keys().map(String::as_str).collect();
        names.sort_unstable();
        let next = if names.is_empty() {
            format!("add a {table} table with url and ca_cert")
        } else {
            format!("add a {table} table, or pick one of: {}", names.join(", "))
        };
        return Err(Failure::new(
            format!("config file {path}"),
            format!("the config file has no {table}"),
            next,
        ));
    };
    let Some(ref raw_url) = profile.url else {
        return Err(Failure::new(
            source,
            "the profile sets no url",
            format!("add url = \"https://HOST:PORT\" to {table}"),
        ));
    };
    let url = check_url(raw_url).map_err(|problem| match problem {
        UrlProblem::Credentials => Failure::new(
            format!("url in {table} in {path}"),
            "URL carries credentials",
            format!(
                "remove the user and password from the url in {table}; keep the key in its token"
            ),
        ),
        UrlProblem::Malformed => Failure::new(
            format!("url in {table} in {path}"),
            "the url is not an http or https URL with a host",
            format!("set url = \"https://HOST:PORT\" in {table}"),
        ),
    })?;
    // The key comes from this table alone: `apply_profile` would fill it
    // from `[server].token`, a key saved for whatever server that is.
    let key = match profile.token {
        None => None,
        Some(ref token) => Some(
            key_from_text(token, &format!("token in {table}")).ok_or_else(|| {
                Failure::new(
                    format!("token in {table} in {path}"),
                    "the profile's token is empty",
                    format!("set the key as token in {table}, or remove the token line"),
                )
            })?,
        ),
    };

    let trust_source = trust_source(&cfg, &profile, &table);
    if cfg.apply_profile(name).is_err() {
        // Unreachable: the profile was found above in this same value.
        return Err(Failure::new(
            source,
            "the profile cannot be applied",
            "check the profile",
        ));
    }
    let trust = cfg.tls_trust().map_err(|e| {
        let (reason, next) = match e {
            ConfigError::CaCertWithInsecure => (
                "ca_cert and insecure are both on",
                "remove insecure; ca_cert already verifies the server",
            ),
            ConfigError::CaCertNotAbsolute { .. } => (
                "ca_cert is not an absolute path",
                "give ca_cert as an absolute path or one starting with ~",
            ),
            ConfigError::CaCertEmpty { .. } => (
                "ca_cert is empty",
                "point ca_cert at the server's CA PEM file",
            ),
            _ => (
                "ca_cert cannot be read",
                "check that ca_cert names a readable PEM file",
            ),
        };
        Failure::new(format!("{trust_source} in {path}"), reason, next)
    })?;
    let detail = match trust {
        TlsTrust::System => "system roots".to_owned(),
        TlsTrust::PinnedCa(_) => format!("pinned CA from {trust_source}"),
        TlsTrust::AcceptInvalid => format!("certificate verification off ({trust_source})"),
    };
    Ok((Connection { url, trust, key }, detail))
}

/// Name the table each trust field comes from, after inheritance.
fn trust_source(cfg: &Config, profile: &ProfileConfig, table: &str) -> String {
    let ca_cert = match profile.ca_cert {
        Some(ref path) if path.as_os_str().is_empty() => None,
        Some(_) => Some(format!("ca_cert in {table}")),
        None => cfg
            .server
            .ca_cert
            .as_ref()
            .map(|_| "ca_cert in [server]".to_owned()),
    };
    let insecure = match profile.insecure {
        Some(true) => Some(format!("insecure in {table}")),
        Some(false) => None,
        None => cfg
            .server
            .insecure
            .then(|| "insecure in [server]".to_owned()),
    };
    match (ca_cert, insecure) {
        (Some(ca), Some(insecure)) => format!("{ca} and {insecure}"),
        (Some(one), None) | (None, Some(one)) => one,
        (None, None) => table.to_owned(),
    }
}

fn resolve_trial(config: Option<&str>) -> Resolved {
    let path = config_path(config);
    let mut cfg = Config::load(config).map_err(|e| config_failure(&e, &path, trial::PROFILE))?;
    let paths = trial::paths::TrialPaths::from_env().map_err(|e| trial_failure(&e, &path))?;
    // The environment and the flags that would move the trial were refused
    // by `select`; they are observed again here all the same.
    let overrides = trial::profile::Overrides::observe(false, false);
    trial::profile::apply(&mut cfg, overrides, &path, &paths)
        .map_err(|e| trial_failure(&e, &path))?;
    let Some(token) = cfg.server.token.as_deref() else {
        return Err(Failure::new(
            "the trial's operator key",
            "the trial has no operator key yet",
            "finish the trial with `trawl trial up`",
        ));
    };
    let key = key_from_text(token, "the trial's operator key");
    let url = check_url(&cfg.server.url).map_err(|_| {
        Failure::new(
            "the trial's recorded API port",
            "the trial's API address is not usable",
            "delete the trial with `trawl trial down` and create it again",
        )
    })?;
    let trust = cfg.tls_trust().map_err(|_| {
        Failure::new(
            "the trial's certificate",
            "the trial's certificate cannot be pinned",
            "finish the trial with `trawl trial up`",
        )
    })?;
    Ok((
        Connection { url, trust, key },
        "pinned CA from the trial's certificate".to_owned(),
    ))
}

/// The `connection.config` failure for `-p trial`.
fn trial_failure(err: &trial::TrialError, config: &str) -> Failure {
    use trial::TrialError as E;
    match err {
        E::ProfileInConfig { .. } => Failure::new(
            format!("config file {config}"),
            "the config file defines [profiles.trial], and `trial` is reserved for `trawl trial`",
            "rename that profile",
        ),
        E::NoTrial { .. } | E::NotCreated { .. } => Failure::new(
            "the trial state directory",
            "there is no trial on this machine",
            "start one with `trawl trial up`",
        ),
        E::TrialNotReady { missing, .. } => Failure::new(
            "the trial state directory",
            format!("the trial has no {missing} yet"),
            "finish the trial with `trawl trial up`",
        ),
        E::NoStateHome => Failure::new(
            "XDG_STATE_HOME and HOME",
            "the trial state directory cannot be placed: neither is an absolute path",
            "set HOME to your home directory",
        ),
        _ => Failure::new(
            "the trial state directory",
            "the trial state was refused as unsafe or unreadable",
            "run `trawl trial status` to see why",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: [&str; 4] = [
        "TRAWL_URL",
        "TRAWL_TOKEN",
        "TRAWL_INSECURE",
        "TRAWL_PROFILE",
    ];

    fn env_names() -> Vec<String> {
        ENV.iter().map(|&n| n.to_owned()).collect()
    }

    fn url(raw: &str) -> Invocation {
        Invocation {
            url: Some(raw.to_owned()),
            ..Invocation::default()
        }
    }

    fn profile(name: &str) -> Invocation {
        Invocation {
            profile: Some(name.to_owned()),
            ..Invocation::default()
        }
    }

    fn refused(invocation: &Invocation) -> Refusal {
        select(invocation, &env_names(), |_| false).expect_err("must be refused")
    }

    #[test]
    fn each_ambient_variable_is_refused_by_name() {
        for name in ENV {
            let err = select(&url("https://h:1"), &env_names(), |n| n == name).unwrap_err();
            assert_eq!(err.kind, ErrorKind::ArgumentConflict);
            assert!(err.message.starts_with(name), "{name}: {}", err.message);
        }
    }

    #[test]
    fn exactly_one_target() {
        let neither = refused(&Invocation::default());
        assert_eq!(neither.kind, ErrorKind::MissingRequiredArgument);
        let both = refused(&Invocation {
            url: Some("https://h:1".to_owned()),
            profile: Some("prod".to_owned()),
            ..Invocation::default()
        });
        assert!(both.message.contains("--url and --profile"), "{both:?}");
    }

    #[test]
    fn token_flag_is_always_refused() {
        for mut invocation in [url("https://h:1"), profile("prod"), profile("trial")] {
            invocation.token = true;
            assert!(refused(&invocation).message.starts_with("--token "));
        }
    }

    type SetFlag = fn(&mut Invocation);

    #[test]
    fn a_profile_refuses_flags_that_would_change_it() {
        let cases: [(&str, SetFlag); 3] = [
            ("--insecure", |i| i.insecure = true),
            ("--token-env", |i| i.token_env = Some("KEY".to_owned())),
            ("--token-file", |i| i.token_file = Some("/k".into())),
        ];
        for name in ["prod", "trial"] {
            for (flag, set) in cases {
                let mut invocation = profile(name);
                set(&mut invocation);
                let err = refused(&invocation);
                assert!(err.message.contains(flag), "{name} {flag}: {err:?}");
            }
        }
    }

    #[test]
    fn url_refuses_a_config_file_and_two_key_sources() {
        let mut with_config = url("https://h:1");
        with_config.config = Some("/etc/trawl/config.toml".to_owned());
        assert!(refused(&with_config).message.starts_with("-c "));

        let mut both = url("https://h:1");
        both.token_env = Some("KEY".to_owned());
        both.token_file = Some("/k".into());
        assert!(
            refused(&both)
                .message
                .contains("--token-env and --token-file")
        );
    }

    #[test]
    fn url_selection_carries_only_the_named_key_source() {
        let selection = select(&url("https://h:1"), &env_names(), |_| false).unwrap();
        assert_eq!(
            selection.target,
            TargetSelection::Url {
                url: check_url("https://h:1").unwrap(),
                insecure: false,
                key: KeySource::None,
            }
        );
        // With no key source, resolution holds no key, and it had no config
        // file to fall back on.
        let resolution = resolve(&selection.target);
        let connection = resolution.connection.expect("resolves");
        assert!(connection.key.is_none());
        assert_eq!(connection.trust, TlsTrust::System);
        assert_eq!(resolution.check.outcome, Outcome::Complete);
    }

    #[test]
    fn trial_is_its_own_selection() {
        let selection = select(&profile("trial"), &env_names(), |_| false).unwrap();
        assert_eq!(selection.target, TargetSelection::Trial { config: None });
    }

    #[test]
    fn urls_with_userinfo_are_refused_without_echo() {
        for raw in [
            "https://alice@h:1",
            "https://alice:s3cret@h:1",
            "HTTPS://alice:s3cret@h:1/path",
            "https://alice:s3cret@h:1\\x",
            "https://al ice@h:1",
        ] {
            assert_eq!(
                check_url(raw).unwrap_err(),
                UrlProblem::Credentials,
                "{raw}"
            );
            let err = refused(&url(raw));
            assert!(err.message.starts_with("--url carries credentials"));
            assert!(!err.message.contains("alice") && !err.message.contains("s3cret"));

            let mut web = url("https://h:1");
            web.web_url = Some(raw.to_owned());
            let err = refused(&web);
            assert!(err.message.starts_with("--web-url carries credentials"));
            assert!(!err.message.contains("alice") && !err.message.contains("s3cret"));
        }
        // An @ after the authority is a path, not userinfo.
        assert!(check_url("https://h:1/a@b").is_ok());
    }

    #[test]
    fn url_shapes() {
        for bad in [
            "",
            "h:1",
            "https:h",
            "ftp://h",
            "https://",
            "https://h:",
            "https://h:99999",
            "https://h:1x",
            "https://[zz]:1",
            "https://h h",
            "https://h\n",
        ] {
            assert_eq!(
                check_url(bad).unwrap_err(),
                UrlProblem::Malformed,
                "{bad:?}"
            );
        }
        let ok = check_url("HTTPS://Trawl.Example:5514/prefix/").unwrap();
        assert_eq!(ok.origin(), "https://trawl.example:5514");
        assert_eq!(ok.base(), "https://trawl.example:5514/prefix");
        assert_eq!(ok.scheme(), Scheme::Https);
        assert_eq!(
            check_url("http://[::1]:80").unwrap().origin(),
            "http://[::1]:80"
        );
    }

    #[test]
    fn web_url_is_https_or_loopback_http_and_an_origin() {
        let web = |raw: &str| {
            let mut invocation = url("https://h:1");
            invocation.web_url = Some(raw.to_owned());
            select(&invocation, &env_names(), |_| false)
        };
        for ok in [
            "https://trawl.example",
            "https://trawl.example:8443/",
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://127.1.2.3",
            "http://[::1]:8080",
        ] {
            assert!(web(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://trawl.example",
            "http://10.0.0.1:8080",
            "https://trawl.example/login",
            "https://trawl.example?x",
        ] {
            let err = web(bad).unwrap_err();
            assert_eq!(err.kind, ErrorKind::InvalidValue, "{bad}");
            assert!(err.message.starts_with("--web-url"), "{bad}: {err:?}");
        }
    }

    fn write_config(toml: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let path = path.to_str().unwrap().to_owned();
        (dir, path)
    }

    fn resolve_profile_in(name: &str, path: &str) -> Resolution {
        resolve(&TargetSelection::Profile {
            name: name.to_owned(),
            config: Some(path.to_owned()),
        })
    }

    #[test]
    fn a_profile_key_never_comes_from_server() {
        let (_dir, path) = write_config(
            "[server]\nurl = \"https://base:5514\"\ntoken = \"flt_server\"\n\n\
             [profiles.lab]\nurl = \"https://lab:5514\"\n\n\
             [profiles.own]\nurl = \"https://own:5514\"\ntoken = \" flt_own \"\n",
        );
        let lab = resolve_profile_in("lab", &path);
        let connection = lab.connection.expect("lab resolves");
        assert!(
            connection.key.is_none(),
            "[server].token must not be inherited"
        );
        assert_eq!(connection.url.origin(), "https://lab:5514");
        assert_eq!(lab.target.origin.as_deref(), Some("https://lab:5514"));

        let own = resolve_profile_in("own", &path);
        let key = own.connection.expect("own resolves").key.expect("own key");
        assert_eq!(key.secret(), "flt_own");
        assert_eq!(key.source(), "token in [profiles.own]");
        assert!(
            !format!("{key:?}").contains("flt_own"),
            "Debug hides the key"
        );
        let detail = own.check.detail.unwrap();
        assert!(!detail.contains("flt_"), "{detail}");
    }

    #[test]
    fn a_profile_inherits_its_trust() {
        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("ca.pem");
        std::fs::write(
            &pem,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let (_cfg_dir, path) = write_config(&format!(
            "[server]\nca_cert = {:?}\n\n[profiles.lab]\nurl = \"https://lab:5514\"\n\n\
             [profiles.loose]\nurl = \"https://loose:5514\"\nca_cert = \"\"\ninsecure = true\n",
            pem.display().to_string()
        ));
        let lab = resolve_profile_in("lab", &path);
        assert!(matches!(
            lab.connection.unwrap().trust,
            TlsTrust::PinnedCa(_)
        ));
        assert!(lab.check.detail.unwrap().contains("ca_cert in [server]"));

        let loose = resolve_profile_in("loose", &path);
        assert_eq!(loose.connection.unwrap().trust, TlsTrust::AcceptInvalid);
        assert!(
            loose
                .check
                .detail
                .unwrap()
                .contains("insecure in [profiles.loose]")
        );
    }

    #[test]
    fn a_profile_that_does_not_resolve_fails_connection_config() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.toml");
        let (_d, path) = write_config(
            "[server]\nurl = \"https://base:5514\"\n\n[profiles.nourl]\ntoken = \"flt_x\"\n\n\
             [profiles.creds]\nurl = \"https://alice:s3cret@h:1\"\n\n\
             [profiles.blank]\nurl = \"https://h:1\"\ntoken = \"  \"\n\n\
             [profiles.both]\nurl = \"https://h:1\"\nca_cert = \"/nope.pem\"\ninsecure = true\n",
        );
        let cases = [
            ("prod", absent.to_str().unwrap(), "does not exist"),
            ("prod", path.as_str(), "no [profiles.prod]"),
            ("nourl", path.as_str(), "sets no url"),
            ("creds", path.as_str(), "URL carries credentials"),
            ("blank", path.as_str(), "token is empty"),
            ("both", path.as_str(), "ca_cert and insecure"),
        ];
        for (name, path, fragment) in cases {
            let resolution = resolve_profile_in(name, path);
            assert!(resolution.connection.is_none(), "{name}");
            assert!(resolution.target.origin.is_none(), "{name}");
            let check = resolution.check;
            assert_eq!(check.outcome, Outcome::Failed, "{name}");
            let reason = check.reason.unwrap();
            assert!(reason.contains(fragment), "{name}: {reason}");
            let all = format!("{reason} {:?} {:?}", check.source, check.next_action);
            for secret in ["alice", "s3cret", "flt_x"] {
                assert!(!all.contains(secret), "{name}: {all}");
            }
        }
    }

    #[test]
    fn a_key_file_is_read_once_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        std::fs::write(&file, "flt_file\n").unwrap();
        let key = key_from_file(&file).unwrap();
        assert_eq!(key.secret(), "flt_file");
        assert_eq!(key.source(), "key from --token-file");

        let fail = |path: &std::path::Path| key_from_file(path).unwrap_err().reason;
        assert!(fail(&dir.path().join("absent")).ends_with("does not exist"));
        assert!(fail(dir.path()).ends_with("is not a regular file"));
        std::fs::write(&file, " \n").unwrap();
        assert!(fail(&file).ends_with("is empty"));
        std::fs::write(&file, vec![b'a'; 5000]).unwrap();
        assert!(fail(&file).ends_with("larger than an API key"));

        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert!(
            fail(&fifo).ends_with("is not a regular file"),
            "a FIFO never stalls"
        );
    }
}
