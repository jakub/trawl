// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pieces every connection policy is built from (ADR-0065).

mod ordinary;

use std::fmt;
use std::io::Read as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::OpenOptionsExt as _;

use zeroize::Zeroizing;

pub(crate) use self::ordinary::{Inputs, ordinary};
use crate::cli::ConnectionParams;
use crate::config::{Config, ConfigError};

const MAX_KEY_BYTES: u64 = 4096;

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

#[derive(Clone, PartialEq, Eq)]
pub struct CheckedUrl {
    scheme: Scheme,
    host: String,
    origin: String,
    base: String,
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
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    #[must_use]
    pub const fn has_path(&self) -> bool {
        self.has_path
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlProblem {
    Malformed,
    Credentials,
    QueryOrFragment,
}

/// The authority ends where the WHATWG parser ends it, at the first `/`,
/// `\`, `?` or `#`, and any `@` inside it is userinfo. That check comes
/// first, so a URL with credentials is always reported as such.
/// `%3F` and `%23` are refused because a proxy that decodes the path
/// would read them as the same delimiters.
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
    let encoded = tail.to_ascii_lowercase();
    if tail.contains(['?', '#']) || encoded.contains("%3f") || encoded.contains("%23") {
        return Err(UrlProblem::QueryOrFragment);
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

#[derive(Clone)]
pub(crate) struct ApiBase {
    url: url::Url,
}

impl fmt::Debug for ApiBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiBase")
            .field("origin", &self.url.origin().ascii_serialization())
            .finish_non_exhaustive()
    }
}

impl ApiBase {
    /// The shape check runs on the text as given, because the parser
    /// silently repairs some of what it refuses (a backslash becomes `/`,
    /// surrounding whitespace is dropped). The parser then reads the same
    /// text, never [`CheckedUrl::base`], which has already trimmed every
    /// trailing `/`.
    pub(crate) fn parse(raw: &str) -> Result<Self, UrlProblem> {
        check_url(raw)?;
        let url = url::Url::parse(raw).map_err(|_| UrlProblem::Malformed)?;
        Ok(Self { url })
    }

    fn identity(&self) -> (&str, Option<url::Host<&str>>, Option<u16>, &str) {
        let path = self.url.path();
        (
            self.url.scheme(),
            self.url.host(),
            self.url.port_or_known_default(),
            path.strip_suffix('/').unwrap_or(path),
        )
    }

    pub(crate) fn same_base(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }

    pub(crate) fn request_base(&self) -> String {
        let text = self.url.as_str();
        text.strip_suffix('/').unwrap_or(text).to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    CommandLine,
    Environment,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Sources {
    pub(crate) url: Option<Origin>,
    pub(crate) token: Option<Origin>,
}

impl Sources {
    /// `--url` and `--token` are global, and clap copies a global's value
    /// between a command and its subcommands keeping the higher source, so
    /// the top level reports a flag typed after the subcommand as typed.
    ///
    /// An unknown source fails closed in each direction. A token counts as
    /// typed only when clap says so: a typed token goes to any target, an
    /// exported one only to its binding. A URL counts as exported only when
    /// clap says so: an exported URL can bind an exported token, a typed
    /// one never does.
    pub(crate) fn from_matches(matches: &clap::ArgMatches) -> Self {
        use clap::parser::ValueSource;
        Self {
            url: matches.value_source("url").map(|source| match source {
                ValueSource::EnvVariable => Origin::Environment,
                _ => Origin::CommandLine,
            }),
            token: matches.value_source("token").map(|source| match source {
                ValueSource::CommandLine => Origin::CommandLine,
                _ => Origin::Environment,
            }),
        }
    }
}

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
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.secret
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
}

#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) source: String,
    pub(crate) reason: String,
    pub(crate) next_action: String,
}

impl Failure {
    pub(crate) fn new(
        source: impl Into<String>,
        reason: impl Into<String>,
        next: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            reason: reason.into(),
            next_action: next.into(),
        }
    }
}

pub(crate) fn key_from_file(path: &std::path::Path) -> Result<Key, Failure> {
    // The path is not echoed: a key pasted in its place would be.
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

pub(crate) fn key_from_text(text: &str, source: &str) -> Option<Key> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| Key {
        secret: Zeroizing::new(trimmed.to_owned()),
        source: source.to_owned(),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("{}", url_refusal(.input, *.problem))]
    Url { input: String, problem: UrlProblem },
    #[error("--token and --token-file both give a key; give one")]
    BothKeyFlags,
    #[error("{input}: {reason}; {next}")]
    Key {
        input: String,
        reason: String,
        next: String,
    },
    #[error(
        "the key from {credential} belongs to a different API base than the one {target} \
         selects (the two can differ by a path prefix alone), so it was not sent. Give the key \
         for this server with --token or --token-file"
    )]
    Binding { credential: String, target: String },
    #[error(
        "no API token found (set {token_env}, use --token or --token-file, or add token to config)"
    )]
    NoCredential { token_env: String },
    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl From<Failure> for ConnectionError {
    fn from(failure: Failure) -> Self {
        Self::Key {
            input: failure.source,
            reason: failure.reason,
            next: failure.next_action,
        }
    }
}

fn url_refusal(input: &str, problem: UrlProblem) -> String {
    match problem {
        UrlProblem::Credentials => format!(
            "{input} carries credentials (a user or password before the host), which would be \
             sent to the server. Remove them, and give the key with --token or --token-file, or \
             as token in the config file"
        ),
        UrlProblem::QueryOrFragment => format!(
            "{input} has a query or fragment (a ? or #), which would carry requests somewhere \
             other than the API. Give the API's base URL, such as https://HOST:PORT"
        ),
        UrlProblem::Malformed => format!(
            "{input} must be an http or https URL with a host and nothing after its path, such \
             as https://HOST:PORT"
        ),
    }
}

pub(crate) fn trial(cfg: &Config) -> Result<ConnectionParams, ConnectionError> {
    const KEY: &str = "the trial's operator key";
    let base = ApiBase::parse(&cfg.server.url).map_err(|problem| ConnectionError::Url {
        input: "the trial's recorded API address".to_owned(),
        problem,
    })?;
    let key = cfg
        .server
        .token
        .as_deref()
        .and_then(|token| key_from_text(token, KEY))
        .ok_or_else(|| ConnectionError::Key {
            input: KEY.to_owned(),
            reason: "the trial has no operator key yet".to_owned(),
            next: "finish the trial with `trawl trial up`".to_owned(),
        })?;
    Ok(ConnectionParams {
        url: base.request_base(),
        token: key.secret().to_owned(),
        trust: cfg.tls_trust()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(raw: &str) -> ApiBase {
        ApiBase::parse(raw).unwrap_or_else(|problem| panic!("{raw}: {problem:?}"))
    }

    #[test]
    fn connection_base_identity() {
        let equal = [
            ("https://h", "https://H:443/", "https://h"),
            ("http://h:80", "http://h", "http://h"),
            (
                "https://[::1]:5514",
                "https://[0:0:0:0:0:0:0:1]:5514",
                "https://[::1]:5514",
            ),
            ("https://h/trawl", "https://h/trawl/", "https://h/trawl"),
            ("HTTPS://h", "https://h", "https://h"),
            (
                "https://Trawl.Example:5514/prefix/",
                "https://trawl.example:5514/prefix",
                "https://trawl.example:5514/prefix",
            ),
        ];
        for (left, right, request) in equal {
            let (l, r) = (base(left), base(right));
            assert!(l.same_base(&r), "{left} must equal {right}");
            assert!(r.same_base(&l), "{right} must equal {left}");
            assert_eq!(l.request_base(), request, "{left}");
            assert_eq!(r.request_base(), request, "{right}");
        }

        let different = [
            ("https://h/a", "https://h/b"),
            ("https://h/Trawl", "https://h/trawl"),
            ("https://localhost", "https://127.0.0.1"),
            ("https://h", "https://h."),
            ("https://h:5514", "https://h:443"),
            ("https://h:5514", "https://h"),
            ("http://h", "https://h"),
            ("https://h/a", "https://h/%61"),
            ("https://h/~trawl", "https://h/%7etrawl"),
            ("https://h/trawl//", "https://h/trawl"),
            ("https://h", "https://h/trawl"),
        ];
        for (left, right) in different {
            let (l, r) = (base(left), base(right));
            assert!(!l.same_base(&r), "{left} must differ from {right}");
            assert!(!r.same_base(&l), "{right} must differ from {left}");
        }
    }

    #[test]
    fn connection_url_rejections() {
        use UrlProblem::{Credentials, Malformed, QueryOrFragment};
        let cases = [
            ("https://alice@h:1/s3cret", Credentials),
            ("https://alice:s3cret@h:1", Credentials),
            ("HTTPS://alice:s3cret@h:1/x?q", Credentials),
            ("https://h:1/x?s3cret=1", QueryOrFragment),
            ("https://h:1?s3cret", QueryOrFragment),
            ("https://h:1/x?", QueryOrFragment),
            ("https://h:1/x#s3cret", QueryOrFragment),
            ("https://h:1#", QueryOrFragment),
            ("https://h:1/x%3Fs3cret", QueryOrFragment),
            ("https://h:1/x%3fs3cret", QueryOrFragment),
            ("https://h:1/x%23s3cret", QueryOrFragment),
            ("https://h:1/s3cret\\x", Malformed),
            ("https://h:1\\s3cret", Malformed),
            ("https://h:1/s3 cret", Malformed),
            ("https://h:1/s3cret\t", Malformed),
            (" https://h:1/s3cret", Malformed),
            ("https://h:1/s3cret\n", Malformed),
            ("ftp://h:1/s3cret", Malformed),
            ("file:///s3cret", Malformed),
            ("h:1/s3cret", Malformed),
            ("https://", Malformed),
            ("", Malformed),
        ];
        for (raw, expected) in cases {
            let problem = ApiBase::parse(raw).expect_err(raw);
            assert_eq!(problem, expected, "{raw:?}");
            let shown = format!("{problem:?}");
            assert!(
                !shown.contains("s3cret") && !shown.contains("h:1"),
                "{raw:?}: {shown}"
            );
        }
        let prefixed = base("https://h:1/s3cretprefix/");
        let shown = format!("{prefixed:?}");
        assert!(shown.contains("https://h:1"), "{shown}");
        assert!(
            !shown.contains("s3cretprefix"),
            "Debug shows the origin only"
        );
    }

    #[test]
    fn connection_key_file_sources() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        let fail = |path: &std::path::Path| {
            let failure = key_from_file(path).expect_err("must fail");
            let all = format!(
                "{} {} {}",
                failure.source, failure.reason, failure.next_action
            );
            assert!(
                !all.contains(&*dir.path().to_string_lossy()),
                "the path is never echoed: {all}"
            );
            assert_eq!(failure.source, "key from --token-file");
            failure.reason
        };

        std::fs::write(&file, "flt_file\n").unwrap();
        let key = key_from_file(&file).unwrap();
        assert_eq!(key.secret(), "flt_file");
        assert_eq!(key.source(), "key from --token-file");
        assert!(!format!("{key:?}").contains("flt_file"), "Debug hides it");

        std::fs::write(&file, "").unwrap();
        assert!(fail(&file).ends_with("is empty"));
        std::fs::write(&file, " \n\t\n").unwrap();
        assert!(fail(&file).ends_with("is empty"));

        let exact = "k".repeat(4096);
        std::fs::write(&file, &exact).unwrap();
        assert_eq!(key_from_file(&file).unwrap().secret(), exact);
        std::fs::write(&file, format!("{}\n", "k".repeat(4095))).unwrap();
        assert_eq!(key_from_file(&file).unwrap().secret().len(), 4095);
        std::fs::write(&file, "k".repeat(4097)).unwrap();
        assert!(fail(&file).ends_with("is larger than an API key"));

        std::fs::write(&file, [b'k', 0xff, 0xfe]).unwrap();
        assert!(fail(&file).ends_with("is not valid UTF-8"));

        assert!(fail(&dir.path().join("absent")).ends_with("does not exist"));
        assert!(fail(dir.path()).ends_with("is not a regular file"));

        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert!(
            fail(&fifo).ends_with("is not a regular file"),
            "a FIFO never stalls the open"
        );

        // Root reads a mode-000 file anyway; the row means nothing there.
        if !nix::unistd::geteuid().is_root() {
            use std::os::unix::fs::PermissionsExt as _;
            let locked = dir.path().join("locked");
            std::fs::write(&locked, "flt_locked\n").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert!(fail(&locked).ends_with("cannot be read"));
        }
    }
}
