// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pieces every connection policy is built from (ADR-0065).
//!
//! Four policies decide where a key may go: ordinary commands, `trawl
//! doctor` (ADR-0047), `trawl mcp` (ADR-0058), and the `-p trial` overlay
//! (ADR-0045). Each has its own entrypoint. This module holds only what
//! they share, so no caller can pass a set of switches that turns a
//! stricter policy into a looser one:
//!
//! - [`check_url`], the URL shape rules: `http` or `https`, a host, and
//!   nothing after the path prefix. A URL with userinfo, a query, or a
//!   fragment is refused before anything is built from it.
//! - [`ApiBase`], a checked URL parsed by the parser the HTTP client uses,
//!   with the one equality that decides whether a key bound to one base may
//!   go to another.
//! - [`Key`], a key with the name of where it came from, and
//!   [`key_from_file`], the bounded reader for a key file.

use std::fmt;
use std::io::Read as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::OpenOptionsExt as _;

use zeroize::Zeroizing;

/// A key file holds one key and a newline; anything larger is not a key.
const MAX_KEY_BYTES: u64 = 4096;

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
/// an optional port, an optional path prefix, and no userinfo, query, or
/// fragment.
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

    /// Whether anything followed the authority besides a lone `/`.
    #[must_use]
    pub const fn has_path(&self) -> bool {
        self.has_path
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
    /// It has a query or a fragment (`?` or `#`, empty or not, or either
    /// percent-encoded in the path).
    QueryOrFragment,
}

/// Check a URL's shape.
///
/// The authority ends where the WHATWG parser ends it, at the first `/`,
/// `\`, `?` or `#`, and any `@` inside it is userinfo. That check comes
/// first, so a URL with credentials is always reported as such.
///
/// Request paths are appended to what remains, so anything after it but a
/// path prefix is refused: a `?` or `#` would carry every request path into
/// a query or a fragment. `%3F` and `%23` are refused as well, because a
/// proxy that decodes the path would read them as the same delimiters.
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

/// An API base: a URL that passed [`check_url`], parsed by the `url` crate,
/// the parser `reqwest` applies to every request URL the client builds.
///
/// Two bases are the same when they parse to the same scheme, host,
/// effective port, and path prefix. Parsing lowercases the scheme and a DNS
/// host and writes address literals one way; a scheme's default port is the
/// same as no port; one trailing `/` on the path is trimmed. Path case and
/// percent-encoding are kept, and nothing is resolved: `localhost` and
/// `127.0.0.1` are different bases, and so are `h` and `h.`. Requests are
/// built on [`ApiBase::request_base`], the same parsed URL the comparison
/// used.
#[derive(Clone)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the ordinary policy builds on it next")
)]
pub(crate) struct ApiBase {
    url: url::Url,
}

/// The origin only: a path prefix can name an installation behind a proxy,
/// and a refusal never prints a URL.
impl fmt::Debug for ApiBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiBase")
            .field("origin", &self.url.origin().ascii_serialization())
            .finish_non_exhaustive()
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the ordinary policy builds on it next")
)]
impl ApiBase {
    /// Check `raw`'s shape, then parse it as the client will.
    ///
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

    /// Scheme, host, effective port, and the path with one trailing `/`
    /// trimmed.
    fn identity(&self) -> (&str, Option<url::Host<&str>>, Option<u16>, &str) {
        let path = self.url.path();
        (
            self.url.scheme(),
            self.url.host(),
            self.url.port_or_known_default(),
            path.strip_suffix('/').unwrap_or(path),
        )
    }

    /// Whether a key bound to `self` may go to `other`.
    pub(crate) fn same_base(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }

    /// The URL requests are built on: the parsed URL with one trailing `/`
    /// trimmed. A checked URL has no query or fragment, so it ends with the
    /// path.
    pub(crate) fn request_base(&self) -> String {
        let text = self.url.as_str();
        text.strip_suffix('/').unwrap_or(text).to_owned()
    }
}

/// An API key, read once, with the name of where it came from.
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

/// Why a key or a connection could not be read, all of it written by trawl:
/// what was being read, what is wrong with it, and what to do next. None of
/// the three carries a value that was read.
#[derive(Debug)]
pub(crate) struct Failure {
    /// What was being read, named without its value.
    pub(crate) source: String,
    /// What is wrong with it.
    pub(crate) reason: String,
    /// What the user does next.
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

/// Read the key from the file `--token-file` names: a regular file of at
/// most [`MAX_KEY_BYTES`], UTF-8, trimmed, not empty.
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

/// A trimmed, non-empty key.
pub(crate) fn key_from_text(text: &str, source: &str) -> Option<Key> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| Key {
        secret: Zeroizing::new(trimmed.to_owned()),
        source: source.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(raw: &str) -> ApiBase {
        ApiBase::parse(raw).unwrap_or_else(|problem| panic!("{raw}: {problem:?}"))
    }

    /// ADR-0065's base equality, row by row, with the request base each
    /// equal pair sends to.
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
            // Percent-encoding is kept as written.
            ("https://h/a", "https://h/%61"),
            ("https://h/~trawl", "https://h/%7etrawl"),
            // One trailing `/` is trimmed, not every one.
            ("https://h/trawl//", "https://h/trawl"),
            // A prefix is a different base, not a narrower one.
            ("https://h", "https://h/trawl"),
        ];
        for (left, right) in different {
            let (l, r) = (base(left), base(right));
            assert!(!l.same_base(&r), "{left} must differ from {right}");
            assert!(!r.same_base(&l), "{right} must differ from {left}");
        }
    }

    /// Every shape `check_url` refuses is refused by [`ApiBase::parse`] too,
    /// before the parser can repair it, and the problem carries none of the
    /// URL.
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

    /// A key file is read once, bounded, and must be a non-empty UTF-8
    /// regular file. Every failure names the flag, never the path.
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
