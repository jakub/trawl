// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL contract (ADR-0027): one producer, one reader, both pure.
//!
//! The search page keeps its whole state in the address bar — `q` (the
//! executed DSL), `page`, `mode`, `f` (filters) and `r` (range). This
//! module owns every encode and decode of that state and nothing else:
//! no leptos, no `js_sys`, no `web_sys`, so `cargo nextest` on the host
//! exercises the same functions the browser runs. `state::query` keeps
//! only the router memos and the navigator closure on top.
//!
//! Two halves of the contract are worth naming here:
//!
//! - `q` and `r` are readable and stable. `r` is a quick label (`1h`) or
//!   `<from>..<to>` with both bounds in canonical UTC RFC 3339 (`Z`, no
//!   fraction), the right one optionally the literal `now`. No percent
//!   codec touches `r`: a query value is decoded exactly once, by
//!   [`query_params`], and the old `abs:<from>:<to>` form was split
//!   apart at the hour colon after that decode. `f` stays opaque
//!   and versioned because catalog names carry `,`, `=` and `&`.
//! - Decoding answers a [`Verdict`], not a default. A link whose
//!   structured state cannot be read is shown with a banner and does not
//!   run, so a bad `f` can no longer widen a query silently. Every
//!   decode is bounded before it allocates — a URL is attacker-controlled
//!   input to the SPA.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};

use trawl_core::parser::suggest::quote_dsl_field;

use crate::filter_codec::{self, PayloadError};
use crate::query_merge::{Filter, FilterOp, QUICK_RANGES, RangeSpec};

/// Rows per page for the snapshot results table. Lives here because the
/// page parameter's overflow check is part of the URL contract; `api`
/// re-exports it so request building keeps its existing path.
pub const PAGE_SIZE: usize = 50;

/// Widened once, so the offset check answers the same on a 64-bit host
/// test and a 32-bit wasm browser. A `usize` parse would make
/// `page=18446744073709551615` a parse failure in the browser and an
/// arithmetic overflow on the test host — two different contracts.
const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;

/// Largest offset a page may name. The request carries
/// `offset: Option<usize>` and `usize` is 32 bits in the browser, so an
/// offset past `u32::MAX` is one this client cannot ask for — and
/// pinning the ceiling here rather than at `usize::try_from` is what
/// keeps the host tests and the browser answering the same.
pub const MAX_OFFSET: u64 = u32::MAX as u64;

/// Largest raw `f` value read at all, checked before base64 or JSON
/// allocation.
pub const MAX_FILTER_PAYLOAD_BYTES: usize = 4096;
/// Largest number of filters one link may carry.
pub const MAX_FILTERS: usize = 32;
/// Largest field name, in bytes, inside a decoded filter.
pub const MAX_FILTER_FIELD_BYTES: usize = 255;
/// Largest value, in bytes, inside a decoded filter.
pub const MAX_FILTER_VALUE_BYTES: usize = 1024;

/// Largest raw `r` value read at all. The longest range this app writes
/// is 42 bytes (`<from>..<to>` with two canonical UTC instants) and the
/// `now` form is 24, so 64 leaves room to spell one and none at all to
/// hand a 10 KB string to a timestamp parser. Checked before the split,
/// so an oversized `r` is malformed by length and nothing else.
pub const MAX_RANGE_BYTES: usize = 64;

/// Characters a percent-encoded reserved set exercises, shared by the
/// native table test and the browser spec so both pin one literal.
/// Evidence, not app code: nothing in the browser build reads it.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub const RESERVED_SET: &str = " #&/:%'!~*()日本語😀";
/// [`RESERVED_SET`] as `percent_encode` renders it, which is also what
/// the browser's own `encodeURIComponent` renders.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub const RESERVED_SET_ENCODED: &str =
    "%20%23%26%2F%3A%25'!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80";

/// Whether a query string may be handed to an execution endpoint at all.
///
/// The search page blanks its effective query while the link cannot be
/// read (ADR-0027), and an empty query is not a narrow request: trawld's
/// emitter turns it into `SELECT *` with no WHERE, so a request carrying
/// it answers with the whole corpus while the page says the link was
/// refused. `/api/v1/export` is the one that mattered — it streams rows
/// straight to a file — so the door it goes through asks this first, and
/// so does the modal above it.
#[must_use]
pub fn is_executable(query: &str) -> bool {
    !query.trim().is_empty()
}

/// What a door that refused an empty query says. One sentence, no
/// mention of the URL: the modal can also be open on a page where
/// nothing has been typed yet.
pub const EMPTY_QUERY_REFUSAL: &str = "Nothing to export: the query is empty.";

/// Display mode for the search page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Paginated snapshot of a one-shot query.
    Snapshot,
    /// SSE-streamed raw events (or aggregation snapshots, depending on
    /// the query shape — resolved downstream).
    Live,
}

impl Mode {
    #[must_use]
    pub fn from_url_param(raw: Option<&str>) -> Self {
        match raw {
            Some("live") => Self::Live,
            _ => Self::Snapshot,
        }
    }

    fn as_param(self) -> Option<&'static str> {
        match self {
            Self::Snapshot => None,
            Self::Live => Some("live"),
        }
    }
}

/// Which URL parameter a verdict is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    Filters,
    Range,
    Page,
}

impl Param {
    /// The parameter as the address bar spells it.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Filters => "f",
            Self::Range => "r",
            Self::Page => "page",
        }
    }

    /// The parameter as the banner names it to a reader.
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Filters => "filters",
            Self::Range => "time range",
            Self::Page => "page",
        }
    }
}

/// Why a parameter could not be read. Diagnostic, not copy: the banner
/// names the parameter and echoes the raw value, because a reason like
/// "third filter's value is 1400 bytes" tells the reader of a shared
/// link nothing they can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `f` carries no `v1.` prefix — an old link, or not ours at all.
    UnversionedFilters,
    /// `f` is versioned but its base64 or JSON did not decode.
    UndecodableFilters,
    /// The raw `f` value is over [`MAX_FILTER_PAYLOAD_BYTES`].
    FilterPayloadTooLarge,
    /// Over [`MAX_FILTERS`] filters decoded.
    TooManyFilters,
    /// A field name is over [`MAX_FILTER_FIELD_BYTES`].
    FilterFieldTooLong,
    /// A value is over [`MAX_FILTER_VALUE_BYTES`].
    FilterValueTooLong,
    /// A record is not one this app writes: an operator other than
    /// include/exclude, or an empty field name.
    InvalidFilterRecord,
    /// A field name the DSL cannot spell, even backticked. It would be
    /// dropped from the emitted query, so the link would run something
    /// narrower than it claims.
    UnrenderableFilterField,
    /// `r` is neither a quick label nor a readable `<from>..<to>` pair.
    UnreadableRange,
    /// The raw `r` value is over [`MAX_RANGE_BYTES`], so it is not a
    /// range at all and never reaches the timestamp parser.
    RangeTooLong,
    /// `r`'s bounds are readable but `from` is after `to`.
    RangeReversed,
    /// `page * PAGE_SIZE` does not fit — the link claims a page that
    /// cannot be asked for.
    PageOffsetOverflow,
}

/// One parameter that could not be read, carrying as much of the raw
/// value as the banner will show and nothing more.
///
/// The value is attacker-controlled and unbounded — a 1 MB `f` is
/// refused before it is decoded, but the verdict that refuses it is
/// cloned into every memo that reads it, so keeping the whole string
/// would hand the address bar a megabyte of resident state per read. The
/// prefix is cut once, here, at the only place a `Malformed` is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    pub param: Param,
    /// The first [`RAW_DISPLAY_CHARS`] characters of the raw value. The
    /// rest is gone: `truncated_raw` is the only reader, and no caller
    /// has a use for text the banner will not print.
    raw_prefix: String,
    /// Whether anything was cut, so the display can say so without
    /// keeping the evidence.
    truncated: bool,
    pub reason: Reason,
}

/// How much of the raw value the banner shows — and, since ADR-0027's
/// review, how much of it is retained at all.
const RAW_DISPLAY_CHARS: usize = 120;

impl Malformed {
    fn new(param: Param, raw: &str, reason: Reason) -> Self {
        Self {
            param,
            // Characters, not bytes: the cut is a display bound, and a
            // byte cut could split a multibyte name in half.
            raw_prefix: raw.chars().take(RAW_DISPLAY_CHARS).collect(),
            truncated: raw.chars().nth(RAW_DISPLAY_CHARS).is_some(),
            reason,
        }
    }

    /// The retained prefix, with an ellipsis when anything was cut.
    #[must_use]
    pub fn truncated_raw(&self) -> String {
        let mut out = self.raw_prefix.clone();
        if self.truncated {
            out.push('\u{2026}');
        }
        out
    }

    /// The banner's sentence, up to the raw value it introduces.
    #[must_use]
    pub fn message(&self) -> String {
        format!("This link's {} could not be read:", self.param.noun())
    }

    /// The one repair the banner offers, as its button reads.
    #[must_use]
    pub const fn repair_label(&self) -> &'static str {
        match self.param {
            Param::Filters => "Drop filters",
            Param::Range => "Use last 15 minutes",
            Param::Page => "Go to page 1",
        }
    }
}

/// What one URL parameter said: nothing, something readable, or a false
/// claim. Absent and Malformed both fall back to the parameter's default
/// for rendering; only Malformed stops the query from running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict<T> {
    Absent,
    Valid(T),
    Malformed(Malformed),
}

impl<T> Verdict<T> {
    /// The failure, when there is one.
    #[must_use]
    pub const fn malformed(&self) -> Option<&Malformed> {
        match self {
            Self::Malformed(m) => Some(m),
            Self::Absent | Self::Valid(_) => None,
        }
    }

    /// The value, when the parameter was present and readable.
    #[must_use]
    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Valid(v) => Some(v),
            Self::Absent | Self::Malformed(_) => None,
        }
    }
}

/// Percent-encode one URL query-parameter value exactly as the browser's
/// `encodeURIComponent` does: `A-Za-z0-9-_.!~*'()` pass through, every
/// other byte of the UTF-8 encoding becomes `%XX` with uppercase hex.
///
/// Hand-rolled rather than `js_sys`, so `build_search_url` is a pure
/// function the host can test; the browser spec submits [`RESERVED_SET`]
/// and compares against [`RESERVED_SET_ENCODED`] to prove the two agree.
#[must_use]
pub fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}

/// Read a raw query string into its `(name, value)` pairs, decoding each
/// one EXACTLY ONCE.
///
/// The reader takes `use_location().search`, the query string as the
/// address bar spells it, rather than the router's `ParamsMap`, whose
/// `insert` runs `decodeURIComponent` over a value
/// `UrlSearchParams` has already decoded. Two decodes eat a literal
/// percent: `?q=message%3D%2F100%2541%2F` is the query text
/// `message=/100%41/`, and the router handed it over as
/// `message=/100A/`. `f` is base64url and `r` is a closed timestamp
/// grammar, so neither can carry a `%` and neither ever noticed; `q` is
/// the user's own DSL and does. The URL is the document, so the decode
/// belongs here beside the encoder it inverts.
///
/// The rules are `application/x-www-form-urlencoded`'s, which is what
/// `URLSearchParams` implements: an optional leading `?`, then split on
/// `&`, then on the first `=` of each pair (a pair with no `=` is a name
/// with an empty value, an empty pair is dropped); in name and value
/// alike a `+` is a space, `%` followed by two hex digits is that byte,
/// and any other `%` is a literal `%`, so `%zz` reads back as `%zz`.
/// Bytes are assembled first and read as UTF-8 last, so `%E6%97%A5` is one
/// character and a truncated sequence is U+FFFD rather than a panic.
///
/// Pairs come back in the order the URL carries them, duplicates and
/// all: which duplicate wins is [`first_value`]'s rule, and `repair_url`
/// needs every pair to carry the link through untouched.
#[must_use]
pub fn query_params(raw_search: &str) -> Vec<(String, String)> {
    raw_search
        .strip_prefix('?')
        .unwrap_or(raw_search)
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (form_decode(name), form_decode(value)),
            None => (form_decode(pair), String::new()),
        })
        .collect()
}

/// Decode one `application/x-www-form-urlencoded` name or value.
fn form_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if let Some(hi) = bytes.get(i + 1).copied().and_then(hex_digit)
                    && let Some(lo) = bytes.get(i + 2).copied().and_then(hex_digit)
                {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    // Not an escape at all. The browser keeps the `%` as
                    // text and so do we, so `color=%zz` is `%zz`.
                    out.push(b'%');
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    // Lossy, and deliberately: a hand-typed `%E6%97` is half a character
    // and the banner it may end up in must be able to print it.
    String::from_utf8_lossy(&out).into_owned()
}

/// One ASCII hex digit's value, or `None` for anything else.
const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The FIRST value a query string gives a key, which is what
/// `URLSearchParams.get` answers.
///
/// A duplicated key has to resolve somehow, and the browser's own
/// accessor is the rule a reader can check for themselves with
/// `new URLSearchParams(location.search).get('r')`. (The router's
/// `ParamsMap::get` took the last, so `?r=1h&r=garbage` used to read as
/// the garbage.)
#[must_use]
pub fn first_value<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Build a `/search?q=...` URL. Omits elidable params (default mode,
/// zero page in live mode, default range, no filters) so the common case
/// stays readable.
#[must_use]
pub fn build_search_url(
    query: &str,
    page: usize,
    mode: Mode,
    filters: &[Filter],
    range: &RangeSpec,
) -> String {
    let mut url = format!("/search?q={}", percent_encode(query));
    if mode == Mode::Snapshot {
        let _ = write!(url, "&page={page}");
    }
    if let Some(m) = mode.as_param() {
        let _ = write!(url, "&mode={m}");
    }
    if !filters.is_empty() {
        // Admission is upstream: every producer asks `admit_filters`
        // before it navigates, and a filter set that came back out of
        // `decode_filters` was admitted by the same rules on the way in.
        // No assertion here — the only way to reach one would be an
        // in-app caller that skipped the door, and a debug build that
        // panics on a URL is worse than the banner.
        let _ = write!(url, "&f={}", encode_filters(filters));
    }
    if *range != RangeSpec::default() {
        let _ = write!(url, "&r={}", encode_range(range));
    }
    url
}

/// Rewrite a search URL, replacing ONLY the parameter the banner names
/// and carrying every other parameter through exactly as it arrived.
///
/// A one-parameter edit rather than a rebuild out of the decoded memos,
/// because a link can be wrong in more than one place at once.
/// `?q=service%3Dnginx&f=v1.!&r=garbage` used to lose its range the
/// moment the reader clicked "Drop filters": the rebuild wrote the
/// range memo, which had already fallen back to the default, and the
/// query quietly ran over the last 15 minutes with no banner and no
/// mention. Dropping `f` and nothing else leaves `r=garbage` in the
/// address bar, so the next verdict raises its own banner and offers
/// its own repair.
///
/// `params` is the query as the router hands it over: each value
/// decoded exactly once, in the order the URL carries them.
#[must_use]
pub fn repair_url<'a>(
    params: impl IntoIterator<Item = (&'a str, &'a str)>,
    which: Param,
) -> String {
    let mut url = String::from("/search");
    let mut sep = '?';
    let mut repaired = false;
    for (name, value) in params {
        if name == which.key() {
            // The repair. Filters and range have no default spelling in
            // the URL — an absent parameter IS the default — so the
            // repair is the removal. A page has one, and page 0 written
            // in place keeps the parameter where the reader saw it.
            if which == Param::Page && !repaired {
                let _ = write!(url, "{sep}page=0");
                sep = '&';
            }
            repaired = true;
            continue;
        }
        let _ = write!(
            url,
            "{sep}{}={}",
            percent_encode(name),
            carry_value(name, value)
        );
        sep = '&';
    }
    url
}

/// Write a carried-through value back the way the producer would have
/// written it.
///
/// `q` is percent-encoded, exactly as `build_search_url` encodes it.
/// Every other parameter gets the same rule with the colon left alone,
/// because `r`'s readable `<from>..<to>` spelling is built on colons and
/// a browser carries a colon in a query value as itself — so repairing
/// one parameter does not sprinkle `%3A` through the hour of another.
/// Everything else is encoded, which is how a `+` inside an unreadable
/// range survives as a `+` instead of arriving back as a space.
fn carry_value(name: &str, value: &str) -> String {
    if name == "q" {
        return percent_encode(value);
    }
    let mut out = String::with_capacity(value.len());
    for (i, part) in value.split(':').enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&percent_encode(part));
    }
    out
}

/// Encode filters as the opaque versioned payload. Its alphabet
/// (`v1.` + unpadded base64url) needs no percent codec.
#[must_use]
pub fn encode_filters(filters: &[Filter]) -> String {
    filter_codec::encode_payload(
        filters
            .iter()
            .map(|f| (f.op.prefix(), f.field.as_str(), f.value.as_str())),
    )
}

/// Every filter-set rule the DECODER and the producer share: how many
/// filters a link may carry, how long a field or a value may be, and
/// whether the field can be written into the DSL at all.
fn admit_records(filters: &[Filter]) -> Result<(), Reason> {
    if filters.len() > MAX_FILTERS {
        return Err(Reason::TooManyFilters);
    }
    for f in filters {
        if f.field.len() > MAX_FILTER_FIELD_BYTES {
            return Err(Reason::FilterFieldTooLong);
        }
        if f.value.len() > MAX_FILTER_VALUE_BYTES {
            return Err(Reason::FilterValueTooLong);
        }
        // `query_merge::format_filter` renders the field through this
        // same function and drops the clause when it answers `None`, so
        // admitting an unrenderable name here would mean a link whose
        // filter silently does nothing (ADR-0013 ruling 7).
        if quote_dsl_field(&f.field).is_none() {
            return Err(Reason::UnrenderableFilterField);
        }
    }
    Ok(())
}

/// Whether a filter set may be written into a link at all — the reader's
/// rules asked BEFORE the URL is built, so the app cannot navigate to a
/// link its own reader would refuse. The payload length is part of it,
/// which is why this is the producer's door and `decode_filters` checks
/// the raw value it was handed instead.
///
/// # Errors
/// The first rule the set breaks, for the caller to turn into copy.
pub fn admit_filters(filters: &[Filter]) -> Result<(), Reason> {
    admit_records(filters)?;
    if encode_filters(filters).len() > MAX_FILTER_PAYLOAD_BYTES {
        return Err(Reason::FilterPayloadTooLarge);
    }
    Ok(())
}

/// What a refused filter says to the person who clicked. One sentence,
/// naming the limit rather than the internal rule.
#[must_use]
pub const fn refusal_copy(reason: Reason) -> &'static str {
    match reason {
        Reason::TooManyFilters => "Can't add filter: too many filters (max 32)",
        Reason::FilterValueTooLong => "Can't add filter: value too long",
        Reason::FilterFieldTooLong => "Can't add filter: field name too long",
        Reason::UnrenderableFilterField => "Can't add filter: that field name can't be queried",
        _ => "Can't add filter: link would be too long",
    }
}

/// Read the `f` parameter. An empty value is a missing one; anything the
/// current codec cannot produce is malformed, including the pre-#85
/// plain-text form — that dialect's include links never survived the
/// browser's own decode, so reading them now would be inventing filters
/// nobody could have shared.
#[must_use]
pub fn decode_filters(raw: &str) -> Verdict<Vec<Filter>> {
    if raw.is_empty() {
        return Verdict::Absent;
    }
    let bad = |reason| Verdict::Malformed(Malformed::new(Param::Filters, raw, reason));
    if raw.len() > MAX_FILTER_PAYLOAD_BYTES {
        return bad(Reason::FilterPayloadTooLarge);
    }
    let parts = match filter_codec::decode_payload(raw) {
        Ok(parts) => parts,
        Err(PayloadError::NotVersioned) => return bad(Reason::UnversionedFilters),
        Err(PayloadError::Undecodable) => return bad(Reason::UndecodableFilters),
        Err(PayloadError::InvalidRecord) => return bad(Reason::InvalidFilterRecord),
    };
    let filters: Vec<Filter> = parts
        .into_iter()
        .map(|(op, field, value)| Filter {
            field,
            value,
            // `decode_payload` yields only `+` and `-`.
            op: if op == '+' {
                FilterOp::Include
            } else {
                FilterOp::Exclude
            },
        })
        .collect();
    // The producer's door, not just the per-record rules: a `Valid`
    // verdict has to imply that `build_search_url` can write this set
    // back. JSON is a looser language than this codec's own output, so a
    // payload inside the raw cap can still decode into filters whose
    // canonical re-encoding is over it, and the app would then navigate
    // into its own malformed banner.
    if let Err(reason) = admit_filters(&filters) {
        return bad(reason);
    }
    Verdict::Valid(filters)
}

/// Encode a range: the quick label, or `<from>..<to>` with both bounds
/// normalized to canonical UTC. `now` on the right stays `now`.
#[must_use]
pub fn encode_range(range: &RangeSpec) -> String {
    match range {
        RangeSpec::Quick(q) => (*q).to_string(),
        RangeSpec::Absolute { from, to } => {
            // Both constructors (the picker's Apply and `decode_range`)
            // already hand over canonical text; normalizing again is
            // what makes that a property of this function rather than a
            // habit of its callers.
            let f = normalize_instant(from).unwrap_or_else(|| from.clone());
            let t = if to == "now" {
                to.clone()
            } else {
                normalize_instant(to).unwrap_or_else(|| to.clone())
            };
            format!("{f}..{t}")
        }
    }
}

/// Read the `r` parameter. Empty is missing; anything over
/// [`MAX_RANGE_BYTES`] is malformed by length before it is looked at; a
/// known quick label is
/// itself; `<from>..<to>` needs canonical UTC bounds (`now` allowed on
/// the right only) in order. Everything else — including the old `abs:`
/// form — is malformed, with no special copy for it: it is one more
/// range the app cannot read.
#[must_use]
pub fn decode_range(raw: &str) -> Verdict<RangeSpec> {
    if raw.is_empty() {
        return Verdict::Absent;
    }
    let bad = |reason| Verdict::Malformed(Malformed::new(Param::Range, raw, reason));
    if raw.len() > MAX_RANGE_BYTES {
        return bad(Reason::RangeTooLong);
    }
    if let Some(q) = QUICK_RANGES.iter().find(|q| **q == raw) {
        return Verdict::Valid(RangeSpec::Quick(q));
    }
    let Some((from, to)) = raw.split_once("..") else {
        return bad(Reason::UnreadableRange);
    };
    let Some(from_at) = canonical_instant(from) else {
        return bad(Reason::UnreadableRange);
    };
    if to != "now" {
        let Some(to_at) = canonical_instant(to) else {
            return bad(Reason::UnreadableRange);
        };
        if from_at > to_at {
            return bad(Reason::RangeReversed);
        }
    }
    Verdict::Valid(RangeSpec::Absolute {
        from: from.to_string(),
        to: to.to_string(),
    })
}

/// The instant a bound names, if it is written in the one canonical form
/// this app writes: RFC 3339, UTC, `Z`, whole seconds. An offset, a
/// fraction, a lowercase `z` or surrounding whitespace are all readable
/// timestamps and none of them is the form we produce, so a link
/// carrying one is a link we did not write and do not run.
fn canonical_instant(raw: &str) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw).ok()?;
    let utc = parsed.with_timezone(&Utc);
    (utc.to_rfc3339_opts(SecondsFormat::Secs, true) == raw).then_some(utc)
}

/// Render any RFC 3339 instant in the canonical form the URL carries.
/// The date-range picker runs its two inputs through this on Apply, so
/// `2026-01-01T01:00:00+01:00` reaches the URL as
/// `2026-01-01T00:00:00Z`; `None` keeps the popover open with an error
/// rather than writing something the reader cannot read back.
#[must_use]
pub fn normalize_instant(raw: &str) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(raw.trim()).ok()?;
    Some(
        parsed
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

/// Read the `page` parameter.
///
/// A value that is not a number is a missing value and reads as page 0 —
/// the same answer a link with no `page` at all gets. A value that IS a
/// number but whose offset (`page * PAGE_SIZE`) cannot be represented is
/// a false claim: the link names a page that cannot be asked for, so it
/// is malformed rather than quietly served as page 0.
///
/// The parse and the multiplication are `u64` on every target. `usize`
/// is 32-bit in the browser and 64-bit on the test host, and a contract
/// whose answer depends on that is not a contract.
#[must_use]
pub fn parse_page(raw: &str) -> Verdict<usize> {
    let bad = || Verdict::Malformed(Malformed::new(Param::Page, raw, Reason::PageOffsetOverflow));
    let Ok(page) = raw.parse::<u64>() else {
        return Verdict::Valid(0);
    };
    let Some(offset) = page.checked_mul(PAGE_SIZE_U64) else {
        return bad();
    };
    if offset > MAX_OFFSET {
        return bad();
    }
    // The offset fits in 32 bits, so the page number fits any target.
    usize::try_from(page).map_or_else(|_| bad(), Verdict::Valid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Base64url, unpadded, as the codec writes it — spelled out here
    /// so a test can hand `decode_filters` a payload the codec would
    /// never produce.
    fn base64_url(bytes: &[u8]) -> String {
        use base64ct::{Base64UrlUnpadded, Encoding as _};
        Base64UrlUnpadded::encode_string(bytes)
    }

    fn inc(field: &str, value: &str) -> Filter {
        Filter {
            field: field.into(),
            value: value.into(),
            op: FilterOp::Include,
        }
    }

    // ---- the empty-query door --------------------------------------

    /// The sentinel the malformed gate produces. Whitespace counts as
    /// empty because the DSL reader trims, so `" "` reaches the emitter
    /// as the same `SELECT *` an empty string does.
    #[test]
    fn an_empty_or_blank_query_never_executes() {
        for raw in ["", " ", "\t", "\n", "  \t\n "] {
            assert!(!is_executable(raw), "{raw:?}");
        }
        for raw in ["*", "service=nginx", " service=nginx "] {
            assert!(is_executable(raw), "{raw:?}");
        }
    }

    // ---- percent encoder -------------------------------------------

    /// Whether a byte passes through unencoded, spelled out here so the
    /// table below is checked against the rule rather than the code.
    fn unreserved(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
    }

    #[test]
    fn every_byte_a_string_can_carry_encodes_as_the_browser_would() {
        // ASCII: one input per byte value, identity or %XX.
        for byte in 0u8..=0x7F {
            let input = (byte as char).to_string();
            let encoded = percent_encode(&input);
            if unreserved(byte) {
                assert_eq!(encoded, input, "byte {byte:#04x} must pass through");
            } else {
                assert_eq!(
                    encoded,
                    format!("%{byte:02X}"),
                    "byte {byte:#04x} must percent-encode"
                );
            }
        }

        // Non-ASCII bytes only ever reach this function as part of a
        // character's UTF-8 encoding, so the table walks characters and
        // records which byte values it covered. 0xC0, 0xC1 and 0xF5-0xFF
        // are the bytes valid UTF-8 never contains, so no `&str` input
        // can produce them and the coverage claim excludes exactly those.
        let mut seen = [false; 256];
        for cp in 0x80u32..=0x10_FFFF {
            let Some(ch) = char::from_u32(cp) else {
                continue;
            };
            let input = ch.to_string();
            let mut expected = String::new();
            for &b in input.as_bytes() {
                assert!(!unreserved(b), "byte {b:#04x} of {cp:#x} is ASCII");
                let _ = write!(expected, "%{b:02X}");
                seen[b as usize] = true;
            }
            assert_eq!(percent_encode(&input), expected, "U+{cp:04X}");
        }
        for byte in 0x80u8..=0xFF {
            let impossible = matches!(byte, 0xC0 | 0xC1 | 0xF5..=0xFF);
            assert_eq!(
                seen[byte as usize], !impossible,
                "byte {byte:#04x} coverage"
            );
        }
    }

    #[test]
    fn multibyte_strings_encode_per_utf8_byte() {
        assert_eq!(percent_encode("日本語"), "%E6%97%A5%E6%9C%AC%E8%AA%9E");
        assert_eq!(percent_encode("😀"), "%F0%9F%98%80");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode(""), "");
    }

    #[test]
    fn the_reserved_set_encodes_to_the_pinned_literal() {
        assert_eq!(percent_encode(RESERVED_SET), RESERVED_SET_ENCODED);
    }

    #[test]
    fn an_already_encoded_value_is_encoded_again() {
        // A `%` is not special on the way out: the value is text, and
        // the browser decodes exactly once on the way back in.
        assert_eq!(percent_encode("%41"), "%2541");
        assert_eq!(percent_encode("%zz"), "%25zz");
    }

    // ---- query string reader ---------------------------------------

    /// The defect this reader exists for. `leptos_router` decoded a
    /// query value twice, once in `UrlSearchParams` and once more in
    /// `ParamsMap`, so a literal percent in the DSL was eaten on the way
    /// in and the link ran a query nobody wrote.
    #[test]
    fn a_percent_in_the_query_text_survives_exactly_one_decode() {
        let params = query_params("q=message%3D%2F100%2541%2F&page=0");
        assert_eq!(first_value(&params, "q"), Some("message=/100%41/"));
        // What the second decode used to make of it.
        assert_ne!(first_value(&params, "q"), Some("message=/100A/"));
        // And the encoder is the inverse: this is the URL the app writes
        // for that query text.
        assert_eq!(
            build_search_url(
                "message=/100%41/",
                0,
                Mode::Snapshot,
                &[],
                &RangeSpec::default()
            ),
            "/search?q=message%3D%2F100%2541%2F&page=0"
        );
    }

    #[test]
    fn a_value_decodes_by_the_form_urlencoded_rules() {
        for (raw, expected) in [
            ("q=100%2541", "100%41"),
            ("q=a%2Bb", "a+b"),
            ("q=a+b", "a b"),
            ("q=100%25", "100%"),
            // Not an escape: the `%` is text, exactly as the browser
            // reads it.
            ("q=%zz", "%zz"),
            ("q=%", "%"),
            ("q=%4", "%4"),
            ("q=50%%2041", "50% 41"),
            ("q=%E6%97%A5", "\u{65e5}"),
            // Hex is case-insensitive.
            ("q=%e6%97%a5", "\u{65e5}"),
            ("q=%F0%9F%98%80", "\u{1f600}"),
            // Half a character: replaced, never a panic.
            ("q=%E6%97", "\u{fffd}"),
            ("q=%FF", "\u{fffd}"),
            ("q=", ""),
        ] {
            assert_eq!(
                first_value(&query_params(raw), "q"),
                Some(expected),
                "{raw}"
            );
        }
    }

    #[test]
    fn the_pair_split_follows_url_search_params() {
        // Nothing at all.
        assert_eq!(query_params(""), Vec::new());
        assert_eq!(query_params("?"), Vec::new());
        // A leading `?` is not part of the first name, and empty pairs
        // are dropped rather than becoming empty names.
        assert_eq!(
            query_params("?&q=x&&page=0&"),
            vec![("q".into(), "x".into()), ("page".into(), "0".into())]
        );
        // No `=` is a name with an empty value…
        assert_eq!(query_params("live"), vec![("live".into(), String::new())]);
        // …and only the FIRST `=` splits, so an `=` inside a DSL value
        // stays in the value.
        assert_eq!(
            query_params("q=service=nginx"),
            vec![("q".into(), "service=nginx".into())]
        );
        // Names decode too.
        assert_eq!(
            query_params("a%20b=1&c%2Bd=2"),
            vec![("a b".into(), "1".into()), ("c+d".into(), "2".into())]
        );
    }

    /// Duplicates: every pair is kept in arrival order, and the FIRST is
    /// the one a lookup answers with, as `URLSearchParams.get` does.
    #[test]
    fn a_duplicated_key_reads_as_its_first_value() {
        let params = query_params("r=1h&q=a&r=garbage&q=b");
        assert_eq!(
            params,
            vec![
                ("r".into(), "1h".into()),
                ("q".into(), "a".into()),
                ("r".into(), "garbage".into()),
                ("q".into(), "b".into()),
            ]
        );
        assert_eq!(first_value(&params, "r"), Some("1h"));
        assert_eq!(first_value(&params, "q"), Some("a"));
        assert_eq!(first_value(&params, "page"), None);
        // A repair drops EVERY copy of the parameter it names, so the
        // second `r` cannot come back as the first one's replacement.
        assert_eq!(
            repair_url(
                params.iter().map(|(n, v)| (n.as_str(), v.as_str())),
                Param::Range
            ),
            "/search?q=a&q=b"
        );
    }

    /// The whole read side over one raw search string: this is what
    /// `state::query::url_signals` wraps in memos and nothing more.
    #[test]
    fn a_raw_search_string_gives_the_verdicts_the_decoders_give() {
        let params = query_params("q=service%3Dnginx&page=0&f=v1.!&r=garbage&mode=live");
        assert_eq!(first_value(&params, "q"), Some("service=nginx"));
        assert_eq!(
            Mode::from_url_param(first_value(&params, "mode")),
            Mode::Live
        );
        assert_eq!(
            decode_filters(first_value(&params, "f").unwrap())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UndecodableFilters)
        );
        assert_eq!(
            decode_range(first_value(&params, "r").unwrap())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UnreadableRange)
        );
        assert_eq!(
            parse_page(first_value(&params, "page").unwrap()),
            Verdict::Valid(0)
        );
        // Filters first in the banner's precedence, and its repair edits
        // that one parameter of the link as it stands.
        assert_eq!(
            repair_url(
                params.iter().map(|(n, v)| (n.as_str(), v.as_str())),
                Param::Filters
            ),
            "/search?q=service%3Dnginx&page=0&r=garbage&mode=live"
        );

        // The pre-#85 plain-text filter dialect, as the address bar has
        // to spell it to reach the app at all.
        let legacy = query_params("q=service%3Dnginx&f=%2Bhost%3Dweb-01");
        assert_eq!(first_value(&legacy, "f"), Some("+host=web-01"));
        assert_eq!(
            decode_filters(first_value(&legacy, "f").unwrap())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UnversionedFilters)
        );

        // A link that reads: every parameter valid, page 2.
        let good = query_params(
            "q=service%3Dnginx&page=2\
             &f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0\
             &r=2026-01-01T00:00:00Z..now",
        );
        assert_eq!(
            decode_filters(first_value(&good, "f").unwrap()),
            Verdict::Valid(vec![inc("host", "web-01")])
        );
        assert_eq!(
            decode_range(first_value(&good, "r").unwrap()),
            Verdict::Valid(RangeSpec::Absolute {
                from: "2026-01-01T00:00:00Z".into(),
                to: "now".into(),
            })
        );
        assert_eq!(
            parse_page(first_value(&good, "page").unwrap()),
            Verdict::Valid(2)
        );

        // An unescaped `+` offset in `r` is a space by the time the app
        // sees it, which is why only the `Z` spelling is readable.
        let plus = query_params("r=2026-01-01T00:00:00+02:00..now");
        assert_eq!(
            first_value(&plus, "r"),
            Some("2026-01-01T00:00:00 02:00..now")
        );
        assert_eq!(
            decode_range(first_value(&plus, "r").unwrap())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UnreadableRange)
        );
    }

    // ---- build_search_url ------------------------------------------

    #[test]
    fn a_snapshot_url_carries_page_and_elides_the_default_range() {
        assert_eq!(
            build_search_url(
                "service=nginx",
                0,
                Mode::Snapshot,
                &[],
                &RangeSpec::default()
            ),
            "/search?q=service%3Dnginx&page=0"
        );
    }

    #[test]
    fn a_live_url_carries_mode_and_no_page() {
        assert_eq!(
            build_search_url("service=nginx", 3, Mode::Live, &[], &RangeSpec::default()),
            "/search?q=service%3Dnginx&mode=live"
        );
    }

    #[test]
    fn filters_and_a_non_default_range_are_written_raw() {
        let url = build_search_url(
            "service=nginx",
            2,
            Mode::Snapshot,
            &[inc("host", "web-01")],
            &RangeSpec::Absolute {
                from: "2026-01-01T00:00:00Z".into(),
                to: "now".into(),
            },
        );
        assert_eq!(
            url,
            "/search?q=service%3Dnginx&page=2\
             &f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0\
             &r=2026-01-01T00:00:00Z..now"
        );
        // The range is readable in the address bar: no percent codec
        // touches it, so `%` appears only in the query text's encoding.
        assert_eq!(url.matches('%').count(), 1);
    }

    #[test]
    fn a_quick_range_that_is_not_the_default_is_written() {
        assert_eq!(
            build_search_url("*", 0, Mode::Snapshot, &[], &RangeSpec::Quick("1h")),
            "/search?q=*&page=0&r=1h"
        );
    }

    // ---- filters ----------------------------------------------------

    #[test]
    fn filters_round_trip_through_the_versioned_payload() {
        let filters = vec![
            inc("host", "web-01"),
            Filter {
                field: "source".into(),
                value: "auth.log".into(),
                op: FilterOp::Exclude,
            },
        ];
        let encoded = encode_filters(&filters);
        assert_eq!(decode_filters(&encoded), Verdict::Valid(filters));
    }

    #[test]
    fn an_absent_or_empty_f_is_absent() {
        assert_eq!(decode_filters(""), Verdict::Absent);
    }

    #[test]
    fn the_legacy_plain_text_form_is_malformed() {
        for raw in ["+host=web-01", "-source=auth.log", "+host=web-01,-a=b"] {
            let v = decode_filters(raw);
            assert_eq!(
                v.malformed().map(|m| m.reason),
                Some(Reason::UnversionedFilters),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_malformed_versioned_payload_is_not_zero_filters() {
        for raw in ["v1.", "v1.!", "v1.bm90anNvbg"] {
            let v = decode_filters(raw);
            assert_eq!(
                v.malformed().map(|m| m.reason),
                Some(Reason::UndecodableFilters),
                "{raw}"
            );
        }
    }

    #[test]
    fn the_raw_payload_cap_is_checked_before_decoding() {
        // 4096 bytes is inside the cap, so the failure is the decode…
        let at_cap = format!("v1.{}", "A".repeat(MAX_FILTER_PAYLOAD_BYTES - 3));
        assert_eq!(at_cap.len(), MAX_FILTER_PAYLOAD_BYTES);
        assert_eq!(
            decode_filters(&at_cap).malformed().map(|m| m.reason),
            Some(Reason::UndecodableFilters)
        );
        // …and one byte over, the cap fires first.
        let over_cap = format!("{at_cap}A");
        assert_eq!(over_cap.len(), MAX_FILTER_PAYLOAD_BYTES + 1);
        assert_eq!(
            decode_filters(&over_cap).malformed().map(|m| m.reason),
            Some(Reason::FilterPayloadTooLarge)
        );
    }

    /// The adversarial shape ADR-0027's review found: serde's derived
    /// reader accepted positional arrays, which are dense enough that a
    /// payload inside the raw cap decodes into filters this app cannot
    /// write back. Both doors close it — the codec refuses the shape,
    /// and `decode_filters` measures the canonical re-encoding.
    #[test]
    fn a_dense_array_shaped_payload_inside_the_raw_cap_is_malformed() {
        let records: Vec<String> = (0..MAX_FILTERS)
            .map(|i| format!("[\"+\",\"f{i}\",\"{}\"]", "v".repeat(75)))
            .collect();
        let json = format!("[{}]", records.join(","));
        let payload = format!("v1.{}", base64_url(json.as_bytes()));
        // Inside the raw cap, so the length gate does not fire…
        assert_eq!(payload.len(), 3831);
        assert!(payload.len() <= MAX_FILTER_PAYLOAD_BYTES);
        // …while the same filters written the way this app writes them
        // are half a kilobyte past it.
        let equivalent: Vec<Filter> = (0..MAX_FILTERS)
            .map(|i| inc(&format!("f{i}"), &"v".repeat(75)))
            .collect();
        assert_eq!(encode_filters(&equivalent).len(), 4727);
        assert_eq!(
            admit_filters(&equivalent),
            Err(Reason::FilterPayloadTooLarge)
        );
        // The record shape is refused first, so this never reaches the
        // size door — and it is a banner either way, never filters.
        assert_eq!(
            decode_filters(&payload).malformed().map(|m| m.reason),
            Some(Reason::UndecodableFilters)
        );
    }

    #[test]
    fn the_filter_count_cap_is_inclusive() {
        let make = |n: usize| {
            let filters: Vec<Filter> = (0..n).map(|i| inc(&format!("f{i}"), "v")).collect();
            encode_filters(&filters)
        };
        let at_cap = make(MAX_FILTERS);
        assert!(at_cap.len() <= MAX_FILTER_PAYLOAD_BYTES);
        assert!(matches!(decode_filters(&at_cap), Verdict::Valid(f) if f.len() == MAX_FILTERS));
        let over_cap = make(MAX_FILTERS + 1);
        assert!(over_cap.len() <= MAX_FILTER_PAYLOAD_BYTES);
        assert_eq!(
            decode_filters(&over_cap).malformed().map(|m| m.reason),
            Some(Reason::TooManyFilters)
        );
    }

    /// A record the codec would never write fails the whole parameter,
    /// rather than being skipped into a narrower query than the link
    /// describes.
    #[test]
    fn an_unwritable_record_fails_the_whole_parameter() {
        for parts in [
            vec![('x', "host", "prod")],
            vec![('+', "", "prod")],
            vec![('+', "host", "web-01"), ('x', "source", "auth.log")],
        ] {
            let payload = filter_codec::encode_payload(parts.iter().copied());
            assert_eq!(
                decode_filters(&payload).malformed().map(|m| m.reason),
                Some(Reason::InvalidFilterRecord),
                "{parts:?}"
            );
        }
    }

    /// A field the DSL cannot spell would be dropped from the emitted
    /// query by `format_filter`, so the link would run something
    /// narrower than it claims. Refuse it at the door instead.
    #[test]
    fn a_field_the_dsl_cannot_spell_fails_the_whole_parameter() {
        for field in ["a\u{202e}b", "a`b\u{0}"] {
            let payload = encode_filters(&[inc(field, "x")]);
            assert_eq!(
                decode_filters(&payload).malformed().map(|m| m.reason),
                Some(Reason::UnrenderableFilterField),
                "{field:?}"
            );
            assert_eq!(
                admit_filters(&[inc(field, "x")]),
                Err(Reason::UnrenderableFilterField)
            );
        }
        // …while a name that needs backticks is perfectly fine.
        assert!(matches!(
            decode_filters(&encode_filters(&[inc("x-forwarded-for", "10.0.0.1")])),
            Verdict::Valid(_)
        ));
    }

    /// The producer asks the same questions the reader does, so the app
    /// cannot navigate to a link its own reader would refuse.
    #[test]
    fn the_producer_admits_exactly_what_the_reader_reads() {
        assert_eq!(admit_filters(&[]), Ok(()));
        let at_count: Vec<Filter> = (0..MAX_FILTERS)
            .map(|i| inc(&format!("f{i}"), "v"))
            .collect();
        assert_eq!(admit_filters(&at_count), Ok(()));
        assert!(matches!(
            decode_filters(&encode_filters(&at_count)),
            Verdict::Valid(_)
        ));

        let over_count: Vec<Filter> = (0..=MAX_FILTERS)
            .map(|i| inc(&format!("f{i}"), "v"))
            .collect();
        assert_eq!(admit_filters(&over_count), Err(Reason::TooManyFilters));
        assert_eq!(
            decode_filters(&encode_filters(&over_count))
                .malformed()
                .map(|m| m.reason),
            Some(Reason::TooManyFilters)
        );

        let field_over = vec![inc(&"a".repeat(MAX_FILTER_FIELD_BYTES + 1), "v")];
        assert_eq!(admit_filters(&field_over), Err(Reason::FilterFieldTooLong));
        let value_over = vec![inc("f", &"v".repeat(MAX_FILTER_VALUE_BYTES + 1))];
        assert_eq!(admit_filters(&value_over), Err(Reason::FilterValueTooLong));

        // A set that is legal filter by filter but whose payload is
        // longer than the reader will look at.
        let bulky: Vec<Filter> = (0..MAX_FILTERS)
            .map(|i| inc(&format!("f{i}"), &"v".repeat(MAX_FILTER_VALUE_BYTES)))
            .collect();
        assert_eq!(admit_records(&bulky), Ok(()));
        assert_eq!(admit_filters(&bulky), Err(Reason::FilterPayloadTooLarge));
        assert_eq!(
            decode_filters(&encode_filters(&bulky))
                .malformed()
                .map(|m| m.reason),
            Some(Reason::FilterPayloadTooLarge)
        );
    }

    #[test]
    fn a_refusal_names_the_limit_it_hit() {
        assert_eq!(
            refusal_copy(Reason::TooManyFilters),
            "Can't add filter: too many filters (max 32)"
        );
        assert_eq!(
            refusal_copy(Reason::FilterValueTooLong),
            "Can't add filter: value too long"
        );
        assert_eq!(
            refusal_copy(Reason::FilterFieldTooLong),
            "Can't add filter: field name too long"
        );
        assert_eq!(
            refusal_copy(Reason::UnrenderableFilterField),
            "Can't add filter: that field name can't be queried"
        );
        assert_eq!(
            refusal_copy(Reason::FilterPayloadTooLarge),
            "Can't add filter: link would be too long"
        );
    }

    #[test]
    fn the_field_and_value_caps_are_inclusive() {
        let field_at = encode_filters(&[inc(&"a".repeat(MAX_FILTER_FIELD_BYTES), "v")]);
        assert!(matches!(decode_filters(&field_at), Verdict::Valid(f) if f.len() == 1));
        let field_over = encode_filters(&[inc(&"a".repeat(MAX_FILTER_FIELD_BYTES + 1), "v")]);
        assert_eq!(
            decode_filters(&field_over).malformed().map(|m| m.reason),
            Some(Reason::FilterFieldTooLong)
        );

        let value_at = encode_filters(&[inc("f", &"v".repeat(MAX_FILTER_VALUE_BYTES))]);
        assert!(matches!(decode_filters(&value_at), Verdict::Valid(f) if f.len() == 1));
        let value_over = encode_filters(&[inc("f", &"v".repeat(MAX_FILTER_VALUE_BYTES + 1))]);
        assert_eq!(
            decode_filters(&value_over).malformed().map(|m| m.reason),
            Some(Reason::FilterValueTooLong)
        );
    }

    // ---- range -------------------------------------------------------

    #[test]
    fn every_quick_label_round_trips() {
        for q in QUICK_RANGES {
            let encoded = encode_range(&RangeSpec::Quick(q));
            assert_eq!(encoded, *q);
            assert_eq!(decode_range(&encoded), Verdict::Valid(RangeSpec::Quick(q)));
        }
    }

    #[test]
    fn an_absolute_range_round_trips_readably() {
        let range = RangeSpec::Absolute {
            from: "2026-01-01T00:00:00Z".into(),
            to: "2026-01-01T00:15:00Z".into(),
        };
        let encoded = encode_range(&range);
        assert_eq!(encoded, "2026-01-01T00:00:00Z..2026-01-01T00:15:00Z");
        assert!(!encoded.contains('%'));
        assert_eq!(decode_range(&encoded), Verdict::Valid(range));
    }

    #[test]
    fn now_is_allowed_on_the_right_only() {
        let to_now = RangeSpec::Absolute {
            from: "2026-01-01T00:00:00Z".into(),
            to: "now".into(),
        };
        assert_eq!(encode_range(&to_now), "2026-01-01T00:00:00Z..now");
        assert_eq!(
            decode_range("2026-01-01T00:00:00Z..now"),
            Verdict::Valid(to_now)
        );
        assert_eq!(
            decode_range("now..2026-01-01T00:00:00Z")
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UnreadableRange)
        );
        assert_eq!(
            decode_range("now..now").malformed().map(|m| m.reason),
            Some(Reason::UnreadableRange)
        );
    }

    #[test]
    fn a_reversed_range_is_malformed() {
        assert_eq!(
            decode_range("2026-01-02T00:00:00Z..2026-01-01T00:00:00Z")
                .malformed()
                .map(|m| m.reason),
            Some(Reason::RangeReversed)
        );
        // …while equal bounds are a legitimate empty window.
        assert!(matches!(
            decode_range("2026-01-01T00:00:00Z..2026-01-01T00:00:00Z"),
            Verdict::Valid(_)
        ));
    }

    #[test]
    fn only_the_canonical_instant_spelling_reads() {
        for raw in [
            "2026-01-01T00:00:00+02:00..now",
            "2026-01-01T00:00:00.000Z..now",
            "2026-01-01t00:00:00z..now",
            "2026-01-01T00:00:00Z ..now",
            "2026-01-01..now",
            "2026-01-01T00:00:00Z..2026-01-01T00:15:00+00:00",
        ] {
            assert_eq!(
                decode_range(raw).malformed().map(|m| m.reason),
                Some(Reason::UnreadableRange),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_offset_bearing_bound_is_normalized_on_write() {
        // What the picker hands over after `normalize_instant`, and what
        // a pasted URL carrying the same offset does NOT get.
        assert_eq!(
            normalize_instant("2026-01-01T01:00:00+01:00").as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            normalize_instant("2026-01-01T00:00:00.500Z").as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            normalize_instant("  2026-01-01T00:00:00Z  ").as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(normalize_instant("now"), None);
        assert_eq!(normalize_instant("2026-01-01"), None);
        assert_eq!(normalize_instant(""), None);
        assert_eq!(
            encode_range(&RangeSpec::Absolute {
                from: "2026-01-01T01:00:00+01:00".into(),
                to: "now".into(),
            }),
            "2026-01-01T00:00:00Z..now"
        );
    }

    #[test]
    fn the_retired_abs_form_is_just_unreadable() {
        for raw in [
            "abs:2026-01-01T00:00:00Z:2026-01-01T00:15:00Z",
            "abs:2026-01-01T00:00:00Z..2026-01-01T00:15:00Z",
            "garbage",
            "15",
            "..",
            "..2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z..",
        ] {
            assert_eq!(
                decode_range(raw).malformed().map(|m| m.reason),
                Some(Reason::UnreadableRange),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_empty_r_is_absent() {
        assert_eq!(decode_range(""), Verdict::Absent);
    }

    /// Length first, parser second: a 10 KB `r` is not a range, and
    /// handing it to a timestamp parser to find that out is work an
    /// address bar should not be able to ask for.
    #[test]
    fn an_oversized_range_never_reaches_the_parser() {
        let huge = "9".repeat(10_000);
        assert_eq!(
            decode_range(&huge).malformed().map(|m| m.reason),
            Some(Reason::RangeTooLong)
        );

        // The cap leaves room for the longest range this app writes…
        let longest = encode_range(&RangeSpec::Absolute {
            from: "2026-01-01T00:00:00Z".into(),
            to: "2026-12-31T23:59:59Z".into(),
        });
        assert_eq!(longest.len(), 42);
        assert!(longest.len() <= MAX_RANGE_BYTES);
        assert!(matches!(decode_range(&longest), Verdict::Valid(_)));

        // …and is inclusive: at the cap the value is read and found
        // unreadable, one byte over it is refused by length.
        assert_eq!(
            decode_range(&"x".repeat(MAX_RANGE_BYTES))
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UnreadableRange)
        );
        assert_eq!(
            decode_range(&"x".repeat(MAX_RANGE_BYTES + 1))
                .malformed()
                .map(|m| m.reason),
            Some(Reason::RangeTooLong)
        );
    }

    // ---- page --------------------------------------------------------

    #[test]
    fn an_unreadable_page_is_page_zero() {
        for raw in ["", "wat", "-1", "1.5", "0x10", " 7"] {
            assert_eq!(parse_page(raw), Verdict::Valid(0), "{raw}");
        }
    }

    #[test]
    fn a_readable_page_is_itself() {
        assert_eq!(parse_page("0"), Verdict::Valid(0));
        assert_eq!(parse_page("7"), Verdict::Valid(7));
    }

    #[test]
    fn a_page_past_the_parse_reads_as_zero_and_one_past_the_offset_is_malformed() {
        // Over u64: not a number at all, so it is a missing value.
        assert_eq!(parse_page("99999999999999999999999999"), Verdict::Valid(0));
        // Parses, but the offset does not fit: a false claim.
        for raw in ["18446744073709551615", "368934881474191033"] {
            assert_eq!(
                parse_page(raw).malformed().map(|m| m.reason),
                Some(Reason::PageOffsetOverflow),
                "{raw}"
            );
        }
    }

    /// The offset ceiling is 32 bits because the request's own `offset`
    /// is, so the answer cannot depend on which target ran the check.
    #[test]
    fn the_offset_ceiling_is_the_same_on_every_target() {
        let last = MAX_OFFSET / PAGE_SIZE_U64;
        assert_eq!(last, 85_899_345);
        assert_eq!(parse_page(&last.to_string()), Verdict::Valid(85_899_345));
        assert_eq!(
            parse_page(&(last + 1).to_string())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::PageOffsetOverflow)
        );
    }

    // ---- verdict surface --------------------------------------------

    #[test]
    fn the_banner_names_the_parameter_and_truncates_the_raw_value() {
        let long = "x".repeat(200);
        let m = Malformed::new(Param::Filters, &long, Reason::UndecodableFilters);
        assert_eq!(m.message(), "This link's filters could not be read:");
        assert_eq!(m.repair_label(), "Drop filters");
        assert_eq!(m.truncated_raw().chars().count(), RAW_DISPLAY_CHARS + 1);
        assert!(m.truncated_raw().ends_with('\u{2026}'));

        let short = Malformed::new(Param::Range, "garbage", Reason::UnreadableRange);
        assert_eq!(short.message(), "This link's time range could not be read:");
        assert_eq!(short.repair_label(), "Use last 15 minutes");
        assert_eq!(short.truncated_raw(), "garbage");

        // Cut on a char boundary, never inside a multibyte character.
        let wide = Malformed::new(Param::Page, &"日".repeat(200), Reason::PageOffsetOverflow);
        assert_eq!(wide.message(), "This link's page could not be read:");
        assert_eq!(wide.repair_label(), "Go to page 1");
        assert!(wide.truncated_raw().starts_with('日'));
        assert_eq!(wide.truncated_raw().chars().count(), RAW_DISPLAY_CHARS + 1);
    }

    /// The verdict is cloned into every memo that reads it, so what it
    /// KEEPS is the display window and not a byte more. A megabyte in
    /// the address bar must not become a megabyte of resident state.
    #[test]
    fn a_refused_value_is_not_retained_past_the_display_window() {
        let huge = format!("v1.{}", "A".repeat(1_000_000));
        let v = decode_filters(&huge);
        let m = v.malformed().expect("a 1 MB payload is malformed");
        assert_eq!(m.reason, Reason::FilterPayloadTooLarge);
        assert_eq!(m.raw_prefix.chars().count(), RAW_DISPLAY_CHARS);
        assert!(m.truncated);
        assert_eq!(m.truncated_raw().chars().count(), RAW_DISPLAY_CHARS + 1);

        // Same for the range, whose own gate is a length check.
        let long_range = "9".repeat(10_000);
        let r = decode_range(&long_range);
        let m = r.malformed().expect("a 10 KB range is malformed");
        assert_eq!(m.raw_prefix.chars().count(), RAW_DISPLAY_CHARS);

        // A value inside the window is kept whole, with no ellipsis.
        let short = Malformed::new(Param::Range, "garbage", Reason::UnreadableRange);
        assert!(!short.truncated);
        assert_eq!(short.truncated_raw(), "garbage");
    }

    // ---- repair ------------------------------------------------------

    /// The repair edits ONE parameter. Rebuilding the URL from the
    /// decoded memos dropped the others' raw text, so a link that was
    /// wrong twice lost its range to a click on "Drop filters" and ran
    /// the default window with nothing said about it.
    #[test]
    fn a_repair_replaces_only_the_parameter_it_names() {
        let params = [("q", "service=nginx"), ("f", "v1.!"), ("r", "garbage")];
        assert_eq!(
            repair_url(params, Param::Filters),
            "/search?q=service%3Dnginx&r=garbage"
        );
        assert_eq!(
            repair_url(params, Param::Range),
            "/search?q=service%3Dnginx&f=v1.!"
        );
        // And the second click, on the URL the first one produced.
        assert_eq!(
            repair_url([("q", "service=nginx"), ("r", "garbage")], Param::Range),
            "/search?q=service%3Dnginx"
        );
    }

    #[test]
    fn a_page_repair_writes_page_zero_where_the_page_was() {
        assert_eq!(
            repair_url(
                [
                    ("q", "service=nginx"),
                    ("page", "85899346"),
                    ("mode", "live")
                ],
                Param::Page
            ),
            "/search?q=service%3Dnginx&page=0&mode=live"
        );
        // Arrival order, not a canonical order: everything but the
        // repaired parameter is carried through as it stood.
        assert_eq!(
            repair_url([("page", "85899346"), ("q", "x")], Param::Page),
            "/search?page=0&q=x"
        );
    }

    /// A carried value must mean the same thing on the way back in.
    #[test]
    fn a_carried_value_survives_the_browser_unchanged() {
        // `+` is a space once the browser decodes, so it is encoded…
        assert_eq!(
            repair_url(
                [("f", "v1.!"), ("r", "2026-01-01T00:00:00+02:00..now")],
                Param::Filters
            ),
            "/search?r=2026-01-01T00:00:00%2B02:00..now"
        );
        // …while the readable range spelling keeps its colons, so a
        // repair elsewhere does not rewrite it.
        assert_eq!(
            repair_url(
                [("f", "v1.!"), ("r", "2026-01-01T00:00:00Z..now")],
                Param::Filters
            ),
            "/search?r=2026-01-01T00:00:00Z..now"
        );
        // An `&` or a `#` in a value cannot become URL structure.
        assert_eq!(
            repair_url([("f", "v1.!"), ("r", "a&b#c")], Param::Filters),
            "/search?r=a%26b%23c"
        );
        // The query text is encoded exactly as the producer writes it.
        assert_eq!(
            repair_url([("q", RESERVED_SET), ("f", "v1.!")], Param::Filters),
            format!("/search?q={RESERVED_SET_ENCODED}")
        );
    }

    /// A repaired URL is one the reader reads back without a banner.
    #[test]
    fn a_repaired_url_no_longer_names_the_parameter_it_repaired() {
        let repaired = repair_url([("q", "service=nginx"), ("r", "garbage")], Param::Range);
        assert_eq!(repaired, "/search?q=service%3Dnginx");
        assert!(!repaired.contains("r="));

        let repaired = repair_url([("q", "x"), ("page", "85899346")], Param::Page);
        assert_eq!(parse_page("0"), Verdict::Valid(0));
        assert!(repaired.ends_with("page=0"));
    }
}
