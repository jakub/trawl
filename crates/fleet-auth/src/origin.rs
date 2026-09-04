// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Strict origin parsing and the configured public-origin allowlist
//! (ADR-0016).
//!
//! The shipped CSRF guard compared the `Origin` header's *host* against the
//! request's `Host` header. That admits same-host cross-scheme and
//! cross-port forgery, and it makes the verdict depend on a header a proxy
//! rewrites. ADR-0016 replaces it: a present `Origin` is compared whole
//! (scheme, host, effective port) against origins the operator states in
//! configuration, and no forwarding header is ever consulted.
//!
//! Comparing origins whole only works if both sides normalize identically,
//! so this module owns the ONE parser both the config list and the header
//! go through, plus the only comparison anyone performs: the derived
//! `PartialEq` on [`Origin`]. There is no `eq_ignore_ascii_case` anywhere
//! else, because two spellings of the same origin must be one value, not
//! two values plus a comparison rule.
//!
//! # Why hand-written, and not the `url` crate
//!
//! `url` is permissive in the wrong direction for a security decision: it
//! is built to make a browser's messy input navigable, so it accepts and
//! silently repairs shapes that must be refused here, and it drags IDNA
//! (ICU tables) into a feature that is otherwise wasm-clean and
//! dependency-light. This parser refuses instead of repairing. A shape it
//! does not recognize is an error, never a guess, and a non-ASCII host is
//! refused with a pointer at punycode rather than transformed under an
//! algorithm the operator's config file never states.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// Longest `Origin` header this crate will look at, in bytes.
///
/// A legal serialized origin cannot exceed 268 bytes (`https` + `://` + a
/// 253-byte DNS name + the root dot + `:65535`), so 512 is generous. The point is to
/// refuse before parsing: the header is attacker-controlled, and an
/// unbounded input has no business reaching a loop, a `String`, or a log
/// field. Config entries go through the same door for the same reason —
/// one parser means one length rule.
pub const MAX_ORIGIN_HEADER_BYTES: usize = 512;

/// Longest DNS host this parser accepts, in bytes (RFC 1035 §2.3.4 minus
/// the root label's length byte). Anything longer cannot resolve, so it
/// cannot be a deployment's public origin.
const MAX_DNS_HOST_BYTES: usize = 253;

/// Longest single DNS label, in bytes (RFC 1035 §2.3.4).
const MAX_DNS_LABEL_BYTES: usize = 63;

/// The scheme half of an origin. Only the two schemes a browser can send a
/// cookie-authenticated request from are representable — `ws`, `file`,
/// `chrome-extension` and friends are not schemes trawl-web serves, so
/// admitting them would only widen the allowlist's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OriginScheme {
    /// Plain HTTP. The packaged loopback bind and the dev stack use it.
    Http,
    /// HTTPS. Every deployment reachable from another machine should.
    Https,
}

impl OriginScheme {
    /// The canonical lowercase spelling, which is what [`Origin`]'s
    /// `Display` writes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    /// The port a browser omits from the serialized origin. `https://x`
    /// and `https://x:443` are the same origin, so the parser stores the
    /// effective port and `Display` drops it again.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }

    /// Scheme matching is ASCII-case-insensitive (RFC 3986 §3.1), so
    /// `HTTPS://x` is `https://x`. Everything else, including an empty
    /// scheme, is [`OriginParseError::UnsupportedScheme`].
    fn parse(text: &str) -> Result<Self, OriginParseError> {
        if text.eq_ignore_ascii_case("http") {
            Ok(Self::Http)
        } else if text.eq_ignore_ascii_case("https") {
            Ok(Self::Https)
        } else {
            Err(OriginParseError::UnsupportedScheme)
        }
    }
}

/// The host half of an origin, kept as three distinct shapes so that
/// normalization is the type's job rather than a comparison rule.
///
/// Private because the shapes must never be comparable across families:
/// `::ffff:127.0.0.1` and `127.0.0.1` route to the same machine but are
/// different origins to a browser, and a consumer holding this enum could
/// be tempted to write that equivalence. `Dns` is stored lowercased and
/// the two address forms canonicalize through `std::net`, so the derived
/// `PartialEq` is exact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum OriginHost {
    /// A DNS name, ASCII and already lowercased.
    Dns(String),
    /// A dotted-quad IPv4 literal.
    V4(Ipv4Addr),
    /// A bracketed IPv6 literal, stored unbracketed.
    V6(Ipv6Addr),
}

impl OriginHost {
    /// Read the host text under the family its brackets declare.
    ///
    /// A bracketed host is an IPv6 literal or nothing — no fallback to a
    /// DNS name, because `[evil.example.com]` is not a host a browser can
    /// produce. A bare host is an IPv4 literal if `std::net` says so, and
    /// a DNS name otherwise.
    fn parse(text: HostText<'_>) -> Result<Self, OriginParseError> {
        match text {
            HostText::Bracketed(inside) => {
                // A zone id (`fe80::1%eth0`) is meaningful only on the host
                // that owns the interface; a browser never sends one, and
                // `Ipv6Addr` cannot represent it. Refuse it by name so the
                // rule is stated rather than inherited from std.
                if inside.contains('%') {
                    return Err(OriginParseError::InvalidHost);
                }
                Ipv6Addr::from_str(inside)
                    .map(Self::V6)
                    .map_err(|_| OriginParseError::InvalidHost)
            }
            HostText::Bare(bare) => {
                if bare.is_empty() {
                    return Err(OriginParseError::EmptyHost);
                }
                if !bare.is_ascii() {
                    return Err(OriginParseError::NonAsciiHost);
                }
                if let Ok(addr) = Ipv4Addr::from_str(bare) {
                    return Ok(Self::V4(addr));
                }
                parse_dns_host(bare).map(Self::Dns)
            }
        }
    }
}

/// Host text as it appeared in the authority, carrying whether it was
/// bracketed. RFC 3986 makes brackets the only way to write an IPv6
/// literal, so the bracket is grammar rather than decoration: it decides
/// which parser the host text gets, and an unbracketed `::1` is refused
/// instead of being guessed at.
#[derive(Debug, Clone, Copy)]
enum HostText<'a> {
    Bracketed(&'a str),
    Bare(&'a str),
}

/// One serialized origin, normalized.
///
/// Fields are private and [`Origin::parse`] is the only constructor, so
/// every `Origin` in the process has been through the same rules and two
/// spellings of one origin are one value. That is the whole design: the
/// CSRF verdict is `allowed.contains(&parsed)`, a derived equality, with
/// no case folding, no port defaulting and no scheme special-casing at the
/// call site to get wrong.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    scheme: OriginScheme,
    host: OriginHost,
    port: u16,
}

impl Origin {
    /// Parse a serialized origin: `scheme "://" host [":" port]`, and
    /// nothing else.
    ///
    /// This is the one door for both the operator's `public_origins`
    /// entries and the browser's `Origin` header. If the two used
    /// different parsers, an entry that normalized one way in the config
    /// and another way on the wire would be a silent allowlist miss (or,
    /// worse, a silent hit).
    ///
    /// The posture is refuse, never repair. There is no path normalization
    /// because an origin has no path; no userinfo stripping because an
    /// origin has no userinfo; no IDNA because the config file states
    /// ASCII. Anything outside the grammar is an error.
    ///
    /// # Errors
    ///
    /// Returns the [`OriginParseError`] naming the rule that refused the
    /// input. The error never carries the input, so it is safe to log.
    pub fn parse(input: &str) -> Result<Self, OriginParseError> {
        // Length first: everything below this line allocates, loops or
        // both, and the input can be an attacker's header.
        if input.len() > MAX_ORIGIN_HEADER_BYTES {
            return Err(OriginParseError::TooLong);
        }
        let (scheme, authority) = input
            .split_once("://")
            .ok_or(OriginParseError::NotSerializedOrigin)?;
        let scheme = OriginScheme::parse(scheme)?;
        reject_non_authority_bytes(authority)?;
        let (host_text, port_text) = split_authority(authority)?;
        let host = OriginHost::parse(host_text)?;
        let port = match port_text {
            Some(text) => parse_port(text)?,
            None => scheme.default_port(),
        };
        Ok(Self { scheme, host, port })
    }
}

/// Refuse every byte that cannot appear in an origin's authority.
///
/// `/ \ ? # @` are the delimiters that start a path, query, fragment or
/// userinfo, so their presence means the input is a URL, not an origin. A
/// permissive parser would strip them; stripping is exactly how
/// `https://trawl.example.com@evil.example.com` gets read as trawl. The
/// backslash is in the set because it is the classic parser-differential
/// separator: browsers treat it as `/`, naive parsers as an ordinary
/// character.
///
/// Bytes at or below `0x20` (space, TAB, CR, LF, NUL, ESC) and `0x7f` go
/// too. None of them can be in a real origin, and all of them are how a
/// header value becomes a log-injection or header-splitting primitive.
/// Bytes above `0x7f` are left to the host rules so that a non-ASCII host
/// gets the punycode hint instead of a generic refusal.
fn reject_non_authority_bytes(authority: &str) -> Result<(), OriginParseError> {
    for byte in authority.bytes() {
        if matches!(byte, b'/' | b'\\' | b'?' | b'#' | b'@') || byte <= b' ' || byte == 0x7f {
            return Err(OriginParseError::NotSerializedOrigin);
        }
    }
    Ok(())
}

/// Split `host[:port]`, keeping the bracket that decides how the host is
/// read.
///
/// The unbracketed branch refuses a second colon rather than guessing: in
/// `::1:8090` the last group and the port are indistinguishable, and RFC
/// 3986 already says the answer is that an IPv6 literal must be bracketed.
fn split_authority(authority: &str) -> Result<(HostText<'_>, Option<&str>), OriginParseError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or(OriginParseError::InvalidHost)?;
        let (inside, tail) = (&rest[..end], &rest[end + 1..]);
        let port = match tail {
            "" => None,
            // Anything between `]` and the port other than `:` means the
            // authority is not `host[:port]` at all.
            _ => Some(
                tail.strip_prefix(':')
                    .ok_or(OriginParseError::InvalidHost)?,
            ),
        };
        return Ok((HostText::Bracketed(inside), port));
    }
    match authority.split_once(':') {
        None => Ok((HostText::Bare(authority), None)),
        Some((host, port)) if !port.contains(':') => Ok((HostText::Bare(host), Some(port))),
        Some(_) => Err(OriginParseError::InvalidHost),
    }
}

/// Parse a port: decimal ASCII digits only, `1..=65535`.
///
/// Leading zeros normalize (`:0443` is `:443`) because they name the same
/// number and a browser would send neither spelling as a distinct origin.
/// Everything else is refused rather than coerced: `+443`, `-1`, `0x1bb`
/// and full-width digits all parse "successfully" under some rule
/// somewhere, and every one of those rules is a way for two parsers to
/// disagree about which origin this is. Port 0 is refused because nothing
/// listens there, so no browser can have sent it.
fn parse_port(text: &str) -> Result<u16, OriginParseError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(OriginParseError::InvalidPort);
    }
    let significant = text.trim_start_matches('0');
    if significant.is_empty() || significant.len() > 5 {
        return Err(OriginParseError::InvalidPort);
    }
    significant
        .parse::<u16>()
        .map_err(|_| OriginParseError::InvalidPort)
}

/// Validate a DNS host and return it lowercased.
///
/// The rules are RFC 1123's hostname rules, applied strictly: labels of
/// `[A-Za-z0-9-]` that neither start nor end with `-`, no empty label,
/// each label at most 63 bytes and the whole name at most 253. Case is
/// accepted and folded away on the way out, because DNS is
/// case-insensitive and an origin has one spelling. `xn--` punycode labels
/// satisfy all of this already, which is why no IDNA step is needed to
/// state an internationalized origin.
///
/// One trailing dot is accepted, and kept. It is the DNS root label, and a
/// browser preserves it: `new URL("https://trawl.example.com./").origin`
/// is `https://trawl.example.com.`, so a deployment reached at that URL
/// sends that origin, and refusing it here would leave that deployment
/// unable to authorize itself at all. It is kept rather than folded away
/// because folding is repair, and repair is what this parser does not do:
/// the dotted and dotless names are two origins here exactly as they are
/// two origins to the browser, and an operator who serves both states
/// both. Exactly one dot, though. `example.com..` leaves an empty final
/// label and a leading dot leaves an empty first one, and both stay
/// refused.
///
/// The final rule is the interesting one, and it is WHATWG's rather than
/// RFC 1123's: a host that *ends in a number* must be an IPv4 address or
/// nothing. Per the spec's ends-in-a-number checker
/// (<https://url.spec.whatwg.org/#ends-in-a-number-checker>) the last
/// label is a number when it is all ASCII decimal digits, or when it
/// matches `0[xX][0-9a-fA-F]*` — the empty hex part included, because
/// `0x` parses as the number 0 there. Such a label reaches this function
/// only after `Ipv4Addr::from_str` has already refused the whole host, so
/// a number-final host arriving here is exactly a text this parser and a
/// browser read differently, and that is the shape a bypass is built
/// from. A browser reads `http://0x7f000001:8090` and
/// `http://127.0.0.0x1:8090` as IPv4 and serializes both as
/// `http://127.0.0.1:8090`; admitting either as a DNS name would give the
/// operator an allowlist entry that boots fine and can never match, and
/// would let one browser origin be spelled two ways in one list, where
/// the derived equality this module rests on sees two values. The same
/// rule keeps `010.1.1.1` (octal `8.1.1.1` to a browser), `127.1` and the
/// integer form `2130706433` out. The root dot is split off before this
/// test, so what it looks at is the last NON-EMPTY label: `127.0.0.1.`
/// still ends in a number and is still refused, which is the right answer,
/// because a browser reads that URL as IPv4 and sends `http://127.0.0.1`.
fn parse_dns_host(text: &str) -> Result<String, OriginParseError> {
    // The root dot belongs to no label. The walk below runs over the
    // labels without it; the value returned keeps it.
    let labels = text.strip_suffix('.').unwrap_or(text);
    if labels.len() > MAX_DNS_HOST_BYTES {
        return Err(OriginParseError::InvalidHost);
    }
    let mut last_label_is_number = false;
    for label in labels.split('.') {
        if label.is_empty() || label.len() > MAX_DNS_LABEL_BYTES {
            return Err(OriginParseError::InvalidHost);
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(OriginParseError::InvalidHost);
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(OriginParseError::InvalidHost);
        }
        last_label_is_number = label_is_number(label);
    }
    if last_label_is_number {
        return Err(OriginParseError::InvalidHost);
    }
    Ok(text.to_ascii_lowercase())
}

/// WHATWG's "is this label a number" test, the half of the ends-in-a-number
/// checker that looks at one label
/// (<https://url.spec.whatwg.org/#ends-in-a-number-checker>).
///
/// A label is a number when it is all ASCII decimal digits (which covers
/// the spec's octal `0…` form), or when it opens `0x`/`0X` and the rest is
/// hex digits or empty. The hex half is why this is a function and not an
/// `is_ascii_digit` call: `0x7f000001` is alphanumeric, so every DNS label
/// rule above accepts it, and only this test catches that a browser would
/// have sent `127.0.0.1` instead.
fn label_is_number(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if label.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    match label.as_bytes() {
        [b'0', b'x' | b'X', hex @ ..] => hex.iter().all(u8::is_ascii_hexdigit),
        _ => false,
    }
}

impl FromStr for Origin {
    type Err = OriginParseError;

    /// Same rules as [`Origin::parse`]; exists so config deserialization
    /// and `"…".parse::<Origin>()` cannot reach a second implementation.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for Origin {
    /// The canonical serialization, which is what a browser would have
    /// sent: lowercase scheme and host, default port omitted. Round-trips
    /// through [`Origin::parse`], so it is also the form the rejection log
    /// prints — bounded by construction and free of anything the attacker
    /// typed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme.as_str(), self.host)?;
        if self.port != self.scheme.default_port() {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

impl fmt::Display for OriginHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dns(name) => f.write_str(name),
            Self::V4(addr) => write!(f, "{addr}"),
            Self::V6(addr) => write!(f, "[{addr}]"),
        }
    }
}

/// Why an origin was refused.
///
/// `Copy` and input-free on purpose: these values end up in a rejection
/// log written from an attacker-reachable path, so a variant that carried
/// the offending text would be a log-injection primitive with a
/// pre-attached severity. The variant names the rule; the operator's own
/// config errors ([`PublicOriginsError::Entry`]) are the one place the
/// text appears, and that text is the operator's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OriginParseError {
    /// Longer than [`MAX_ORIGIN_HEADER_BYTES`]; refused before parsing.
    #[error("origin exceeds {MAX_ORIGIN_HEADER_BYTES} bytes")]
    TooLong,

    /// Not `scheme://host[:port]` — no `://`, or the authority carried a
    /// path, query, fragment, userinfo, whitespace or a control byte. The
    /// opaque `null` origin and comma- or space-joined lists land here.
    #[error("not a serialized origin (expected scheme://host[:port])")]
    NotSerializedOrigin,

    /// A scheme other than `http` or `https`.
    #[error("unsupported scheme (only http and https are accepted)")]
    UnsupportedScheme,

    /// `scheme://` with no host, or `scheme://:port`.
    #[error("empty host")]
    EmptyHost,

    /// A non-ASCII host. Deliberately not IDNA-normalized: state the
    /// punycode (`xn--…`) form the browser actually sends.
    #[error("non-ASCII host (state the punycode xn-- form the browser sends)")]
    NonAsciiHost,

    /// The host is neither a valid DNS name nor a valid IP literal.
    #[error("invalid host")]
    InvalidHost,

    /// The port is not a decimal number in `1..=65535`.
    #[error("invalid port (expected a decimal number in 1..=65535)")]
    InvalidPort,
}

impl OriginParseError {
    /// The stable, closed-vocabulary label this refusal logs as.
    ///
    /// A rejection log field must have a bounded value set or it is a
    /// metrics-cardinality and log-injection hazard. These strings are
    /// `&'static str` for exactly that reason, and they are the reason
    /// half of [`rejection_log_fields`].
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::TooLong => "too_long",
            Self::NotSerializedOrigin => "not_serialized_origin",
            Self::UnsupportedScheme => "unsupported_scheme",
            Self::EmptyHost => "empty_host",
            Self::NonAsciiHost => "non_ascii_host",
            Self::InvalidHost => "invalid_host",
            Self::InvalidPort => "invalid_port",
        }
    }
}

/// The deployment's browser-visible origins, non-empty by construction.
///
/// ADR-0016 refuses to boot the web surface without one, so the type
/// carries that: there is no `PublicOrigins::default()`, no `new()`, and
/// no way to build an empty one. A caller holding this value knows the
/// operator stated something, which is what lets the guard be a plain
/// `contains` with no "empty means allow everything" branch to forget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOrigins(Vec<Origin>);

/// Why a configured origin list was refused at startup.
///
/// Unlike [`OriginParseError`], these carry text: the input is the
/// operator's own config file, the reader is the operator, and an error
/// that will not say which entry is wrong wastes their time. The entry is
/// printed with `Debug` so a control character or terminal escape in the
/// config lands as `\u{1b}` rather than as an escape sequence in the
/// operator's terminal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicOriginsError {
    /// No entries at all. ADR-0016's deliberate loud upgrade: state the
    /// browser-visible origin once, or the web surface does not start.
    #[error(
        "[web] public_origins is empty: state the browser-visible origin(s), \
         e.g. public_origins = [\"https://trawl.example.com\"]"
    )]
    Empty,

    /// One entry did not parse. The whole list is refused, never the
    /// working subset: a typo that silently dropped one origin would show
    /// up as a mysterious 403 for whoever browses to it.
    #[error("[web] public_origins entry {index} ({entry:?}) is not a valid origin: {source}")]
    Entry {
        /// Zero-based position in the configured list.
        index: usize,
        /// The entry as the operator wrote it.
        entry: String,
        /// The rule that refused it.
        #[source]
        source: OriginParseError,
    },

    /// Two entries normalize to the same origin. Not fatal in effect, but
    /// it always means the operator believes those two spellings differ
    /// (`https://x` and `https://x:443`), and the next thing they will do
    /// is add a third that does not.
    #[error(
        "[web] public_origins entries {first} and {second} are the same origin after \
         normalization"
    )]
    Duplicate {
        /// Position of the first spelling.
        first: usize,
        /// Position of the duplicate spelling.
        second: usize,
    },
}

impl PublicOrigins {
    /// Parse and validate a configured origin list.
    ///
    /// Takes anything string-like so the TOML list, the comma-split
    /// environment override and tests all reach the same rules. Every
    /// entry goes through [`Origin::parse`] — the same function the
    /// header goes through — because an allowlist normalized by a
    /// different parser than the request is not an allowlist.
    ///
    /// # Errors
    ///
    /// [`PublicOriginsError::Empty`] for no entries,
    /// [`PublicOriginsError::Entry`] for the first entry that does not
    /// parse, and [`PublicOriginsError::Duplicate`] when two entries
    /// normalize identically.
    pub fn parse<I, S>(entries: I) -> Result<Self, PublicOriginsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut origins: Vec<Origin> = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            let entry = entry.as_ref();
            let origin = Origin::parse(entry).map_err(|source| PublicOriginsError::Entry {
                index,
                entry: entry.to_string(),
                source,
            })?;
            // Linear scan: a configured list is a handful of entries, and
            // the position of the first spelling is what makes the error
            // actionable.
            if let Some(first) = origins.iter().position(|seen| *seen == origin) {
                return Err(PublicOriginsError::Duplicate {
                    first,
                    second: index,
                });
            }
            origins.push(origin);
        }
        if origins.is_empty() {
            return Err(PublicOriginsError::Empty);
        }
        Ok(Self(origins))
    }

    /// Whether a parsed origin is allowed. This is the entire policy: a
    /// derived equality against a list the operator wrote.
    #[must_use]
    pub fn contains(&self, origin: &Origin) -> bool {
        self.0.contains(origin)
    }

    /// Iterate the configured origins, for the startup log and for
    /// operator-facing diagnostics.
    pub fn iter(&self) -> std::slice::Iter<'_, Origin> {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a PublicOrigins {
    type Item = &'a Origin;
    type IntoIter = std::slice::Iter<'a, Origin>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Reason logged when a request carries more than one `Origin` field.
///
/// Part of the same closed vocabulary as [`OriginParseError::reason`] and
/// the `not_allowed`/`absent` arms of [`rejection_log_fields`]: every value
/// the guard can put in the `reason` field is a `&'static str` chosen here,
/// so the field's value set is bounded no matter what a client sends.
pub const REASON_MULTIPLE_HEADERS: &str = "multiple_origin_headers";

/// Reason logged when the single `Origin` field is not valid UTF-8.
///
/// A header value is bytes; `Origin` is ASCII. Anything else cannot be
/// parsed, so there is no normalized text to log and the refusal names the
/// encoding rather than the shape.
pub const REASON_NON_UTF8: &str = "non_utf8_origin";

/// Build the bounded fields for a cross-origin rejection log line.
///
/// One vocabulary, one place. The reason is a `&'static str` from a closed
/// set; the origin text is `Some` only when the header actually parsed, in
/// which case it is [`Origin`]'s canonical form — at most 268 bytes drawn
/// from `[a-z0-9.:/\[\]-]`, so no CRLF, no ANSI escape, no control byte
/// and no attacker-chosen length can reach the log. When the header did
/// not parse there is no text at all, because the only text available
/// would be the attacker's.
///
/// Keeping the normalized text (rather than logging nothing) is what makes
/// a misconfigured `public_origins` debuggable: the operator sees the
/// origin their browser actually sent, in the exact spelling they need to
/// paste into the config.
#[must_use]
pub fn rejection_log_fields(origin_header: Option<&str>) -> (&'static str, Option<String>) {
    match origin_header {
        // An absent Origin is allowed, so this arm is defensive rather than
        // reachable from the guard. It exists because a function that is
        // total over its input cannot be called wrongly.
        None => ("absent", None),
        Some(raw) => match Origin::parse(raw) {
            // Well-formed and simply not on the list: the one case where
            // there is safe text to log, and the case an operator debugging
            // their own config actually hits.
            Ok(origin) => ("not_allowed", Some(origin.to_string())),
            Err(err) => (err.reason(), None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use OriginParseError::{
        EmptyHost, InvalidHost, InvalidPort, NonAsciiHost, NotSerializedOrigin, TooLong,
        UnsupportedScheme,
    };

    /// The allowlist the issue's hostile table is written against: a TLS
    /// deployment plus both spellings of the packaged loopback bind.
    fn allowlist() -> PublicOrigins {
        PublicOrigins::parse([
            "https://trawl.example.com",
            "http://localhost:8090",
            "http://[::1]:8090",
        ])
        .expect("fixture allowlist parses")
    }

    fn parsed(text: &str) -> Origin {
        Origin::parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
    }

    /// The verdict a request carrying `header` gets: parse, then match.
    /// This is exactly what `check_origin` will do in M2.
    fn allowed(header: &str) -> bool {
        Origin::parse(header).is_ok_and(|o| allowlist().contains(&o))
    }

    fn error(text: &str) -> OriginParseError {
        Origin::parse(text).expect_err(&format!("{text:?} should be refused"))
    }

    // -- the hostile table (issue #92 acceptance criterion 1) -------------

    #[test]
    fn configured_origins_match_in_every_equivalent_spelling() {
        assert!(allowed("https://trawl.example.com"));
        // Default port written out: same origin to a browser, so same here.
        assert!(allowed("https://trawl.example.com:443"));
        // Scheme and host are ASCII-case-insensitive.
        assert!(allowed("HTTPS://TRAWL.Example.COM"));
        assert!(allowed("http://localhost:8090"));
        assert!(allowed("http://[::1]:8090"));
        // Uncompressed IPv6 is the same address.
        assert!(allowed("http://[0:0:0:0:0:0:0:1]:8090"));
        // Uppercase hex in an IPv6 literal is the same address.
        assert!(allowed("HTTP://[::0001]:8090"));
        // A leading zero in a port digit string is not a different port.
        assert!(allowed("http://localhost:08090"));
    }

    #[test]
    fn cross_scheme_cross_port_and_neighbouring_hosts_are_rejected() {
        // The defect ADR-0016 closes: same host, other scheme.
        assert!(!allowed("http://trawl.example.com"));
        // The other half of it: same host and scheme, other port.
        assert!(!allowed("https://trawl.example.com:8443"));
        // http://localhost:8090 is configured; port 80 is not.
        assert!(!allowed("http://localhost"));
        // A sibling fleet app shares the SSO cookie, never mutation rights.
        assert!(!allowed("https://coastwatch.example.com"));
        // Subdomain and suffix forgery.
        assert!(!allowed("https://evil.trawl.example.com"));
        assert!(!allowed("https://eviltrawl.example.com"));
        // Bare parent domain.
        assert!(!allowed("https://example.com"));
        // Another loopback address entirely.
        assert!(!allowed("http://[::2]:8090"));
        assert!(!allowed("http://127.0.0.1:8090"));
    }

    #[test]
    fn opaque_wildcard_and_smuggled_origins_are_refused() {
        // The opaque origin a sandboxed iframe or data: URL sends.
        assert_eq!(error("null"), NotSerializedOrigin);
        // Wildcards are not a shape this allowlist speaks.
        assert_eq!(error("*"), NotSerializedOrigin);
        assert_eq!(error("https://*.example.com"), InvalidHost);
        assert_eq!(error("https://*"), InvalidHost);
        // Credentials, path, query, fragment: an origin has none of them.
        assert_eq!(
            error("https://user:pass@trawl.example.com"),
            NotSerializedOrigin
        );
        assert_eq!(
            error("https://trawl.example.com@evil.example.com"),
            NotSerializedOrigin
        );
        assert_eq!(
            error("https://evil.example.com/trawl.example.com"),
            NotSerializedOrigin
        );
        assert_eq!(error("https://trawl.example.com?a=b"), NotSerializedOrigin);
        assert_eq!(error("https://trawl.example.com#f"), NotSerializedOrigin);
        // A trailing slash is a path, and browsers do not send one.
        assert_eq!(error("https://trawl.example.com/"), NotSerializedOrigin);
        // Backslash: the classic parser-differential separator.
        assert_eq!(
            error("https://trawl.example.com\\@evil.example.com"),
            NotSerializedOrigin
        );
        // Lists, however they are joined.
        // Split at the FIRST `://`, so the second entry lands in the
        // authority and is refused there.
        assert_eq!(
            error("https://a.example.com https://b.example.com"),
            NotSerializedOrigin
        );
        assert_eq!(
            error("https://a.example.com,https://b.example.com"),
            NotSerializedOrigin
        );
        assert_eq!(error("https://a.example.com,b.example.com"), InvalidHost);
        // Whitespace and control bytes anywhere in the authority.
        assert_eq!(error("https://trawl.example.com "), NotSerializedOrigin);
        assert_eq!(error("https://trawl.example.com\r\n"), NotSerializedOrigin);
        assert_eq!(error("https://trawl.example\t.com"), NotSerializedOrigin);
        assert_eq!(error("https://trawl.example.com\0"), NotSerializedOrigin);
        assert_eq!(error(" https://trawl.example.com"), UnsupportedScheme);
        // Neither half on its own is an origin.
        assert_eq!(error(""), NotSerializedOrigin);
        assert_eq!(error("trawl.example.com"), NotSerializedOrigin);
        assert_eq!(error("https://"), EmptyHost);
        assert_eq!(error("https://:8090"), EmptyHost);
        // Schemes trawl-web does not serve.
        assert_eq!(error("ws://trawl.example.com"), UnsupportedScheme);
        assert_eq!(error("file://trawl.example.com"), UnsupportedScheme);
        assert_eq!(error("://trawl.example.com"), UnsupportedScheme);
        assert_eq!(error("javascript://trawl.example.com"), UnsupportedScheme);
    }

    #[test]
    fn ip_literals_normalize_within_a_family_and_never_across_one() {
        // Compressed and expanded spellings are one value.
        assert_eq!(
            parsed("http://[::1]:8090"),
            parsed("http://[0:0:0:0:0:0:0:1]:8090")
        );
        // IPv4-mapped IPv6 is a different origin from the IPv4 address it
        // maps, even though both reach the same socket. A browser treats
        // them as different origins, so the allowlist must too.
        assert_ne!(
            parsed("http://[::ffff:127.0.0.1]:8090"),
            parsed("http://127.0.0.1:8090")
        );
        // ...but the two spellings of the mapped address are one value.
        assert_eq!(
            parsed("http://[::ffff:127.0.0.1]:8090"),
            parsed("http://[::ffff:7f00:1]:8090")
        );
        // RFC 3986 requires the brackets; an unbracketed literal is refused
        // rather than guessed at (is `::1:8090` a port or a group?).
        assert_eq!(error("http://::1:8090"), InvalidHost);
        assert_eq!(error("http://::1"), InvalidHost);
        // Zone ids are a host-local concept with no meaning to a browser.
        assert_eq!(error("http://[fe80::1%eth0]:8090"), InvalidHost);
        assert_eq!(error("http://[fe80::1%25eth0]:8090"), InvalidHost);
        // Malformed brackets.
        assert_eq!(error("http://[::1:8090"), InvalidHost);
        assert_eq!(error("http://[]:8090"), InvalidHost);
        assert_eq!(error("http://[::1]8090"), InvalidHost);
        assert_eq!(error("http://[not:an:address]:8090"), InvalidHost);
        // A dotted quad is an IPv4 literal, canonically rendered.
        assert_eq!(
            parsed("http://127.0.0.1:8090").to_string(),
            "http://127.0.0.1:8090"
        );
    }

    #[test]
    fn host_charset_rules() {
        // Non-ASCII is refused with the punycode pointer, never IDNA-mapped.
        assert_eq!(error("https://tráwl.example.com"), NonAsciiHost);
        assert_eq!(error("https://例え.example.com"), NonAsciiHost);
        // The punycode form itself is an ordinary ASCII DNS name.
        assert!(Origin::parse("https://xn--80ak6aa92e.example.com").is_ok());
        // A trailing dot is the DNS root label, and a browser keeps it in
        // the origin it serializes, so it parses and stays its own
        // spelling. Refusing it would leave an install reached at
        // `https://trawl.example.com./` unable to state its own origin.
        assert!(Origin::parse("https://trawl.example.com.").is_ok());
        assert_ne!(
            parsed("https://trawl.example.com."),
            parsed("https://trawl.example.com")
        );
        assert_eq!(
            parsed("https://trawl.example.com.").to_string(),
            "https://trawl.example.com."
        );
        // Case still folds, and the dot survives the fold.
        assert_eq!(
            parsed("HTTPS://TRAWL.Example.COM."),
            parsed("https://trawl.example.com.")
        );
        // Exactly one dot, and only at the end.
        assert_eq!(error("https://example.com.."), InvalidHost);
        assert_eq!(error("https://trawl..example.com"), InvalidHost);
        assert_eq!(error("https://.example.com"), InvalidHost);
        assert_eq!(error("https://.trawl.example.com"), InvalidHost);
        assert_eq!(error("https://."), InvalidHost);
        // The root dot does not excuse a host from the ends-in-a-number
        // rule: a browser reads this URL as IPv4 and sends 127.0.0.1.
        assert_eq!(error("http://127.0.0.1."), InvalidHost);
        assert_eq!(error("http://0x7f000001."), InvalidHost);
        // Underscore is legal in DNS data but not in a hostname.
        assert_eq!(error("https://trawl_example.com"), InvalidHost);
        // Percent-escapes are a URL concept; an origin host is not escaped.
        assert_eq!(error("https://trawl%2Eexample.com"), InvalidHost);
        assert_eq!(error("https://trawl%00.example.com"), InvalidHost);
        // Hyphen may not lead or trail a label.
        assert_eq!(error("https://-trawl.example.com"), InvalidHost);
        assert_eq!(error("https://trawl-.example.com"), InvalidHost);
        // A host that ends in a number is an IPv4 address or nothing:
        // this is what keeps `010.1.1.1` (octal to a browser, a DNS name to
        // a naive parser) and `2130706433` out of the allowlist.
        assert_eq!(error("http://010.1.1.1"), InvalidHost);
        assert_eq!(error("http://127.1"), InvalidHost);
        assert_eq!(error("http://2130706433"), InvalidHost);
        assert_eq!(error("http://0x7f.0.0.1"), InvalidHost);
        assert_eq!(error("http://1.2.3.4.5"), InvalidHost);
        // The hexadecimal spellings a browser also reads as IPv4. Each of
        // these serializes as `http://127.0.0.1:8090` on the wire, so
        // admitting one as a DNS name would be an allowlist entry that can
        // never match and a second spelling of an origin already in the
        // list.
        assert_eq!(error("http://0x7f000001:8090"), InvalidHost);
        assert_eq!(error("http://127.0.0.0x1:8090"), InvalidHost);
        assert_eq!(error("http://0X7F.0.0.1"), InvalidHost);
        // `0x` with no hex digits is the number 0 to the spec's parser.
        assert_eq!(error("http://0x:80"), InvalidHost);
        // A label that merely opens with digit-ish text is not a number,
        // so an ordinary name keeps working.
        assert!(Origin::parse("http://x0a.example").is_ok());
        assert!(Origin::parse("http://0x7f.example").is_ok());
        // Single-label names are fine — `localhost` is one.
        assert!(Origin::parse("http://localhost:8090").is_ok());
        assert!(Origin::parse("http://trawl-01:5514").is_ok());
        // Label and host length ceilings.
        let long_label = "a".repeat(MAX_DNS_LABEL_BYTES + 1);
        assert_eq!(
            error(&format!("https://{long_label}.example.com")),
            InvalidHost
        );
        let long_host = std::iter::repeat_n("abcdefgh", 32)
            .collect::<Vec<_>>()
            .join(".");
        assert!(long_host.len() > MAX_DNS_HOST_BYTES && long_host.len() < 512);
        assert_eq!(error(&format!("https://{long_host}")), InvalidHost);
    }

    #[test]
    fn port_rules() {
        // Leading zeros normalize numerically, they do not make a new port.
        assert_eq!(
            parsed("https://trawl.example.com:0443"),
            parsed("https://trawl.example.com")
        );
        assert_eq!(parsed("http://localhost:00080"), parsed("http://localhost"));
        // Every non-default port is its own origin.
        assert_ne!(
            parsed("https://trawl.example.com:8443"),
            parsed("https://trawl.example.com")
        );
        assert_ne!(
            parsed("http://localhost:8090"),
            parsed("http://localhost:8091")
        );
        // Refusals.
        assert_eq!(error("https://trawl.example.com:"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:+443"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:-1"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:0x1bb"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:44 3"), NotSerializedOrigin);
        assert_eq!(error("https://trawl.example.com:0"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:00"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:65536"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:065536"), InvalidPort);
        assert_eq!(error("https://trawl.example.com:99999999999"), InvalidPort);
        // Full-width digits are digits to a human, not to this parser.
        assert_eq!(error("https://trawl.example.com:４４３"), InvalidPort);
        // The extremes are fine.
        assert!(Origin::parse("https://trawl.example.com:1").is_ok());
        assert!(Origin::parse("https://trawl.example.com:65535").is_ok());
    }

    #[test]
    fn oversized_input_is_refused_before_parsing() {
        let long = format!("https://{}.example.com", "a".repeat(600));
        assert!(long.len() > MAX_ORIGIN_HEADER_BYTES);
        assert_eq!(error(&long), TooLong);
        // Exactly at the ceiling the length rule stands down and the host
        // rules take over, so the refusal names the real problem.
        let at_limit = format!("https://{}", "a".repeat(MAX_ORIGIN_HEADER_BYTES - 8));
        assert_eq!(at_limit.len(), MAX_ORIGIN_HEADER_BYTES);
        assert_eq!(error(&at_limit), InvalidHost);
        // A valid origin is nowhere near the ceiling.
        assert!("https://trawl.example.com".len() < MAX_ORIGIN_HEADER_BYTES);
    }

    // -- canonical form ---------------------------------------------------

    #[test]
    fn display_is_canonical_and_round_trips() {
        for (input, canonical) in [
            ("https://trawl.example.com", "https://trawl.example.com"),
            ("https://trawl.example.com:443", "https://trawl.example.com"),
            (
                "HTTPS://TRAWL.Example.COM:0443",
                "https://trawl.example.com",
            ),
            (
                "https://trawl.example.com:8443",
                "https://trawl.example.com:8443",
            ),
            ("http://localhost", "http://localhost"),
            ("http://localhost:80", "http://localhost"),
            ("http://localhost:8090", "http://localhost:8090"),
            ("HTTP://[0:0:0:0:0:0:0:1]:8090", "http://[::1]:8090"),
            ("http://[::1]:80", "http://[::1]"),
            ("http://[::FFFF:127.0.0.1]", "http://[::ffff:127.0.0.1]"),
            ("http://127.0.0.1:8090", "http://127.0.0.1:8090"),
            // The root dot round-trips, dot included.
            ("https://trawl.example.com.", "https://trawl.example.com."),
            (
                "HTTPS://TRAWL.Example.COM.:443",
                "https://trawl.example.com.",
            ),
        ] {
            let origin = parsed(input);
            assert_eq!(origin.to_string(), canonical, "canonical form of {input:?}");
            assert_eq!(
                parsed(&origin.to_string()),
                origin,
                "round trip of {input:?}"
            );
        }
    }

    #[test]
    fn from_str_is_the_same_door_as_parse() {
        assert_eq!(
            "https://trawl.example.com".parse::<Origin>(),
            Origin::parse("https://trawl.example.com")
        );
        assert_eq!("null".parse::<Origin>(), Err(NotSerializedOrigin));
    }

    // -- rejection log fields --------------------------------------------

    /// The ceiling on the logged origin text: `https` + `://` + a 253-byte
    /// DNS host + the root dot + `:65535`.
    const MAX_LOG_TEXT_BYTES: usize = 5 + 3 + MAX_DNS_HOST_BYTES + 1 + 6;

    /// Every reason [`rejection_log_fields`] can produce. A log field
    /// whose value set is not closed is a cardinality and injection
    /// hazard, so the test states the set rather than sampling it.
    const REASONS: &[&str] = &[
        "absent",
        "not_allowed",
        "too_long",
        "not_serialized_origin",
        "unsupported_scheme",
        "empty_host",
        "non_ascii_host",
        "invalid_host",
        "invalid_port",
    ];

    /// `[a-z0-9.:/\[\]-]{1,268}` — the only bytes a canonical origin can
    /// contain. No CR, no LF, no ESC, no NUL, no attacker-chosen length.
    fn is_bounded_log_text(text: &str) -> bool {
        !text.is_empty()
            && text.len() <= MAX_LOG_TEXT_BYTES
            && text.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'.' | b'-' | b':' | b'/' | b'[' | b']')
            })
    }

    #[test]
    fn rejection_log_fields_never_echo_a_hostile_header() {
        let ten_kib = format!("https://{}.example.com", "a".repeat(10 * 1024));
        let hostile = [
            ten_kib.as_str(),
            "https://evil.example.com\r\nX-Injected: 1",
            "https://evil.example.com\u{1b}[31mRED",
            "https://evil.example.com\u{7f}\u{1}\u{2}",
            "https://evil.example.com\0",
            "null",
            "{\"origin\":\"https://evil.example.com\"}",
            "*",
            "https://*.example.com",
            "",
        ];
        for raw in hostile {
            let (reason, text) = rejection_log_fields(Some(raw));
            assert!(
                text.is_none(),
                "{raw:?} does not parse, so nothing about it may be logged (got {text:?})"
            );
            // The reason is from the closed vocabulary, never derived text.
            assert!(
                REASONS.contains(&reason),
                "reason outside the vocabulary: {reason:?}"
            );
        }
    }

    #[test]
    fn rejection_log_fields_report_the_normalized_origin_when_it_parsed() {
        // A well-formed but disallowed origin: the operator needs to see
        // the exact spelling to paste into public_origins.
        let (reason, text) = rejection_log_fields(Some("HTTPS://Evil.Example.COM:443"));
        assert_eq!(reason, "not_allowed");
        let text = text.expect("a parsed origin is logged");
        assert_eq!(text, "https://evil.example.com");
        assert!(is_bounded_log_text(&text));
        // The longest legal origin there is — a 253-byte host, rooted with
        // the trailing dot, on the longest scheme with the longest port —
        // still fits the bound, so the bound is a fact about the type and
        // not about this sample.
        let label = "a".repeat(MAX_DNS_LABEL_BYTES);
        let host = format!("{label}.{label}.{label}.{}", "b".repeat(61));
        assert_eq!(host.len(), MAX_DNS_HOST_BYTES);
        let text = rejection_log_fields(Some(&format!("https://{host}.:65535")))
            .1
            .expect("the longest legal origin parses");
        assert_eq!(text.len(), MAX_LOG_TEXT_BYTES);
        assert!(is_bounded_log_text(&text));
    }

    #[test]
    fn rejection_reasons_are_a_closed_vocabulary() {
        assert_eq!(rejection_log_fields(None), ("absent", None));
        for (raw, reason) in [
            ("https://trawl.example.com", "not_allowed"),
            ("null", "not_serialized_origin"),
            ("ws://trawl.example.com", "unsupported_scheme"),
            ("https://", "empty_host"),
            ("https://tráwl.example.com", "non_ascii_host"),
            ("https://trawl_example.com", "invalid_host"),
            ("https://trawl.example.com:0", "invalid_port"),
        ] {
            assert_eq!(
                rejection_log_fields(Some(raw)).0,
                reason,
                "reason for {raw:?}"
            );
            assert!(REASONS.contains(&reason));
        }
        assert_eq!(
            rejection_log_fields(Some(&"x".repeat(MAX_ORIGIN_HEADER_BYTES + 1))).0,
            "too_long"
        );
    }

    // -- PublicOrigins ----------------------------------------------------

    #[test]
    fn empty_list_names_the_configuration_knob() {
        let err = PublicOrigins::parse(Vec::<String>::new()).expect_err("empty is refused");
        assert_eq!(err, PublicOriginsError::Empty);
        let rendered = err.to_string();
        assert!(rendered.contains("[web] public_origins"), "{rendered}");
        // The message has to be actionable on its own: an operator reading
        // it in a systemd log needs the shape, not just the knob name.
        assert!(rendered.contains("https://"), "{rendered}");
    }

    #[test]
    fn a_bad_entry_names_its_index_and_the_operators_text() {
        let err = PublicOrigins::parse([
            "https://trawl.example.com",
            "http://localhost:8090",
            "trawl.example.com",
        ])
        .expect_err("entry 2 has no scheme");
        assert_eq!(
            err,
            PublicOriginsError::Entry {
                index: 2,
                entry: "trawl.example.com".to_string(),
                source: NotSerializedOrigin,
            }
        );
        let rendered = err.to_string();
        assert!(rendered.contains("entry 2"), "{rendered}");
        assert!(rendered.contains("trawl.example.com"), "{rendered}");
        // One bad entry refuses the whole list, never the working subset.
        assert!(PublicOrigins::parse(["https://good.example.com", "nope"]).is_err());
        // Control bytes in the operator's text are escaped, not replayed.
        let err = PublicOrigins::parse(["https://evil.example.com\u{1b}[31m"])
            .expect_err("control byte is refused");
        assert!(
            !err.to_string().contains('\u{1b}'),
            "escape reached the message"
        );
    }

    #[test]
    fn entries_that_normalize_identically_are_refused() {
        let err = PublicOrigins::parse([
            "https://trawl.example.com",
            "http://localhost:8090",
            "HTTPS://Trawl.Example.com:443",
        ])
        .expect_err("entries 0 and 2 are one origin");
        assert_eq!(
            err,
            PublicOriginsError::Duplicate {
                first: 0,
                second: 2
            }
        );
        assert!(err.to_string().contains("public_origins"));
    }

    #[test]
    fn a_valid_list_contains_exactly_what_was_configured() {
        let list = allowlist();
        assert_eq!(list.iter().count(), 3);
        assert!(list.contains(&parsed("https://trawl.example.com:443")));
        assert!(!list.contains(&parsed("http://trawl.example.com")));
        // The reference IntoIterator is the same three origins.
        let rendered: Vec<String> = (&list).into_iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            [
                "https://trawl.example.com",
                "http://localhost:8090",
                "http://[::1]:8090"
            ]
        );
    }
}
