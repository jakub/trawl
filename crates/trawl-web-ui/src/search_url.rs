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
//!   input to the SPA. Every bound fails closed on both sides: the
//!   reader refuses the whole link rather than answering from the part
//!   that fit ([`read_search`]), and the producer asks the reader before
//!   it navigates ([`admit_search`], [`admit_filters`]).

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

/// Largest raw query string read at all, checked before the string is
/// split into anything.
///
/// The address bar is attacker-controlled and every other cap in this
/// module applies to ONE parameter's value, which is a cap on nothing
/// while the number of parameters is unbounded: `/search?` followed by
/// `a&` a million times is 2 MB of input that used to be split, decoded
/// and collected into a million owned pairs before a single cap was
/// consulted. 32 KiB is roughly forty times the longest link this app
/// writes (a full `f` payload plus a range plus a long query), and past
/// it the whole link is one verdict rather than five.
pub const MAX_SEARCH_BYTES: usize = 32 * 1024;

/// Largest number of NON-EMPTY `&`-separated pairs a search string
/// within [`MAX_SEARCH_BYTES`] may carry. One more and the whole link is
/// refused, unread.
///
/// This app writes at most five, and the five it reads are the only ones
/// it looks for, so 64 is room for a hand-edited link to carry unknown
/// parameters ahead of the known ones and still be read as written.
///
/// Past the cap the link fails CLOSED, because the alternative failed
/// open: the cap used to stop the scan and everything after it read as
/// absent, so `?q=service%3Dnginx` followed by 64 `&` and `f=v1.!` ran
/// the query with no filters and no banner. The empty pairs filled the
/// budget and the unreadable `f` was never looked at. An empty pair is
/// not a parameter, so it costs nothing and counts for nothing; a link
/// with a stray `&&` is an ordinary link.
pub const MAX_SEARCH_PAIRS: usize = 64;

/// The parameters the search page reads. Every other pair in a query
/// string is skipped without decoding its value or allocating a copy of
/// it, which is what makes a link full of junk cost a scan instead of a
/// heap of owned strings.
pub const KNOWN_KEYS: [&str; 5] = ["q", "page", "mode", "f", "r"];

/// Longest a RAW name may be and still spell one of [`KNOWN_KEYS`]:
/// `page`, the longest, is 12 bytes with every character percent-encoded
/// (`%70%61%67%65`). A longer name cannot decode to a key we read, so it
/// is skipped without being decoded.
const MAX_KEY_RAW_BYTES: usize = 12;

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
/// the browser KEEPS in `location.search` once it has stored the link.
/// That is the string the bounds are measured against, so it is the one
/// pinned here — `encodeURIComponent` differs from it in exactly one
/// place, the apostrophe (see [`percent_encode`]).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub const RESERVED_SET_ENCODED: &str =
    "%20%23%26%2F%3A%25%27!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80";

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
    /// Not a parameter at all: the whole query string, refused by length
    /// before anything inside it was read.
    Link,
}

impl Param {
    /// The parameter as the address bar spells it.
    #[must_use]
    /// The parameter as the address bar spells it. [`Param::Link`] names
    /// no single parameter, so it has no key: its repair leaves the
    /// query string behind entirely instead of editing one pair out of
    /// it (see [`repair_url`]).
    pub const fn key(self) -> &'static str {
        match self {
            Self::Filters => "f",
            Self::Range => "r",
            Self::Page => "page",
            Self::Link => "",
        }
    }

    /// The parameter as the banner names it to a reader.
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Filters => "filters",
            Self::Range => "time range",
            Self::Page => "page",
            Self::Link => "link",
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
    /// The whole query string is over [`MAX_SEARCH_BYTES`], so no
    /// parameter inside it was read.
    LinkTooLong,
    /// The query string carries more than [`MAX_SEARCH_PAIRS`] non-empty
    /// pairs. The scan stops there, so what is past it cannot be read at
    /// all and the link is refused rather than answered from the half
    /// that fit.
    TooManyParameters,
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
    ///
    /// The whole-link verdict introduces nothing: its "value" is tens of
    /// kilobytes of address bar, and none of it is retained, so the
    /// sentence ends itself. Which sentence depends on the reason,
    /// because a link of sixty-six parameters can be three hundred bytes
    /// long and telling its reader it is too long would be a lie.
    #[must_use]
    pub fn message(&self) -> String {
        match self.param {
            Param::Link => match self.reason {
                Reason::TooManyParameters => {
                    "This link has too many parameters to read.".to_owned()
                }
                _ => "This link is too long to read.".to_owned(),
            },
            Param::Filters | Param::Range | Param::Page => {
                format!("This link's {} could not be read:", self.param.noun())
            }
        }
    }
}

/// One repair's button text.
///
/// A free function rather than a method on [`Malformed`] or [`Param`]:
/// the label belongs to the repair that is actually OFFERED, which
/// [`plan_repair`] decides, and a verdict-shaped `m.repair_label()` was
/// how the banner came to promise a button that refused itself.
#[must_use]
pub const fn repair_label(param: Param) -> &'static str {
    match param {
        Param::Filters => "Drop filters",
        Param::Range => "Use last 15 minutes",
        Param::Page => "Go to page 1",
        Param::Link => "Start over",
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

/// Percent-encode one URL query-parameter value into the string the
/// browser will KEEP: `A-Za-z0-9-_.!~*()` pass through, every other byte
/// of the UTF-8 encoding becomes `%XX` with uppercase hex.
///
/// That set is `encodeURIComponent`'s minus the apostrophe, and the
/// apostrophe is the whole reason this is not simply a mirror of that
/// function. `encodeURIComponent("'")` is `'`, but `'` is in the URL
/// standard's special-query percent-encode set, so the moment the
/// browser stores the link it serializes it back as `%27`: one typed
/// character, three bytes in `location.search`. Encoding it here is what
/// makes the producer's string byte-identical to the reader's — writing
/// it literally made [`admit_search`] measure a third of the truth, and
/// 10 900 apostrophes passed the door and came back as the "too long"
/// banner over an editor the page had just cleared.
///
/// Hand-rolled rather than `js_sys`, so `build_search_url` is a pure
/// function the host can test; the browser spec submits [`RESERVED_SET`]
/// and compares `location.search` against [`RESERVED_SET_ENCODED`] to
/// prove the two agree, apostrophe included.
#[must_use]
pub fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'(' | b')')
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

/// What one raw query string said: the parameters this app reads, or a
/// refusal of the whole link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchRead {
    /// Refused whole: over [`MAX_SEARCH_BYTES`], or carrying more than
    /// [`MAX_SEARCH_PAIRS`] parameters. Nothing inside was parsed, so
    /// the link is one verdict rather than a banner per parameter.
    Refused(Malformed),
    /// The known parameters, first occurrence of each, in arrival order.
    Params(Vec<(String, String)>),
}

impl SearchRead {
    /// The pairs, or none at all when the link was refused whole.
    #[must_use]
    pub fn params(&self) -> &[(String, String)] {
        match self {
            Self::Params(params) => params,
            Self::Refused(_) => &[],
        }
    }

    /// The whole-link failure, when there is one.
    #[must_use]
    pub const fn malformed(&self) -> Option<&Malformed> {
        match self {
            Self::Refused(m) => Some(m),
            Self::Params(_) => None,
        }
    }
}

/// Read a raw query string, BOUNDED before it is looked at.
///
/// The length check is the first thing this function does, ahead of any
/// split, decode or allocation: the address bar is attacker-controlled
/// input, and a cap that applies per parameter caps nothing while the
/// number of parameters does not. `/search?` plus `a&` a million times
/// used to become a million owned `(String, String)` pairs; now it is
/// one [`Reason::LinkTooLong`] verdict that retains none of the text.
///
/// Both bounds refuse the WHOLE link, and that is the point: a bound
/// that stops reading and answers from what it managed to read hands the
/// unread half whatever default the app has, which is how a filter cap
/// turned into a wider query than the link described.
///
/// This is the only door the app reads a search string through.
#[must_use]
pub fn read_search(raw_search: &str) -> SearchRead {
    // No prefix retained by either arm: the banner's whole sentence is
    // about the link, and echoing 120 characters of it says nothing a
    // reader can act on.
    let refused = |reason| SearchRead::Refused(Malformed::new(Param::Link, "", reason));
    if raw_search.len() > MAX_SEARCH_BYTES {
        return refused(Reason::LinkTooLong);
    }
    match query_params(raw_search) {
        Ok(params) => SearchRead::Params(params),
        Err(reason) => refused(reason),
    }
}

/// One raw name as the key it spells, or `None` for a parameter this app
/// does not read.
///
/// The raw comparison comes first and is the whole of the common case:
/// a name that is already one of [`KNOWN_KEYS`] costs a few byte
/// comparisons. Only a short name carrying an escape is decoded, because
/// `%71=x` IS `q=x` to the browser and reading it any other way would
/// make the app disagree with the address bar it came from.
fn known_key(raw_name: &str) -> Option<&'static str> {
    if let Some(key) = KNOWN_KEYS.iter().find(|key| **key == raw_name) {
        return Some(key);
    }
    if raw_name.len() > MAX_KEY_RAW_BYTES
        || !raw_name.bytes().any(|byte| byte == b'%' || byte == b'+')
    {
        return None;
    }
    let decoded = form_decode(raw_name);
    KNOWN_KEYS.iter().copied().find(|key| *key == decoded)
}

/// Read a bounded query string into the parameters this app knows,
/// decoding each value EXACTLY ONCE.
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
/// Bounded so an unbounded address bar cannot become unbounded work or
/// unbounded state, in two different ways:
///
/// - A pair whose name is not one of [`KNOWN_KEYS`] is skipped without
///   its value being decoded or copied. A repair therefore rebuilds the
///   link out of the five parameters this app reads, and an unknown one
///   the reader never looked at does not survive the click.
/// - At most [`MAX_SEARCH_PAIRS`] non-empty pairs are examined, and a
///   link with more is refused whole rather than answered from the ones
///   that fit. Empty pairs (`&&`) are not parameters: they are skipped
///   before the count, so they cannot push a real parameter past the
///   cap.
///
/// The FIRST occurrence of each key wins, which is what
/// `URLSearchParams.get` answers, so a duplicate cannot come back later
/// as its own replacement.
///
/// # Errors
/// [`Reason::TooManyParameters`] when the string carries more non-empty
/// pairs than the cap admits, so the ones past it were never examined.
fn query_params(raw_search: &str) -> Result<Vec<(String, String)>, Reason> {
    let mut params: Vec<(String, String)> = Vec::new();
    let mut examined: usize = 0;
    for pair in raw_search
        .strip_prefix('?')
        .unwrap_or(raw_search)
        .split('&')
    {
        if pair.is_empty() {
            continue;
        }
        examined += 1;
        if examined > MAX_SEARCH_PAIRS {
            return Err(Reason::TooManyParameters);
        }
        let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(key) = known_key(raw_name) else {
            continue;
        };
        if params.iter().any(|(name, _)| name == key) {
            continue;
        }
        params.push((key.to_owned(), form_decode(raw_value)));
    }
    Ok(params)
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
/// the garbage.) [`query_params`] applies the same rule as it reads, so
/// what reaches here holds one pair per key at most.
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

/// Whether a link this app is about to navigate to is one its own reader
/// will read back. The whole-link half of the producer's door, beside
/// [`admit_filters`]' per-parameter half.
///
/// Asked by the two navigators in `state::query`, so every producer on
/// the search page (submit, range, live, chips, facets, pagination,
/// repair, history rerun, saved-query open, row actions) is covered by
/// one check. A refusal is a toast and a navigation that does not
/// happen: the address bar and the editor buffer stay as they were.
/// Without it, a 33 KiB query built a URL [`read_search`] refuses whole,
/// the editor sync effect then cleared the user's text, and the only
/// control left was "Start over".
///
/// The rules are not restated here. The URL is handed to the reader
/// itself, so the two answer the same at every boundary byte by
/// construction rather than by a pair of constants that have to agree.
///
/// # Errors
/// The bound the link busts: [`Reason::LinkTooLong`], or
/// [`Reason::TooManyParameters`], which this app cannot produce, since
/// it writes five parameters, and which is checked anyway because a
/// producer that trusts its own arithmetic is how a reader and a writer
/// drift apart.
pub fn admit_search(url_or_search: &str) -> Result<(), Reason> {
    match read_search(search_of(url_or_search)) {
        SearchRead::Params(_) => Ok(()),
        SearchRead::Refused(m) => Err(m.reason),
    }
}

/// The query string of a URL this crate built, or the argument itself
/// when it is already one.
///
/// `build_search_url` always writes the `?`, and a path without one
/// carries no state to measure.
fn search_of(url_or_search: &str) -> &str {
    match url_or_search.split_once('?') {
        Some((_, search)) => search,
        None if url_or_search.starts_with('/') => "",
        None => url_or_search,
    }
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
    if which == Param::Link {
        // A link refused as a whole names no parameter to edit out of
        // it, and nothing inside it was read: the repair is the search
        // page with no query string at all.
        return String::from("/search");
    }
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

/// The repair the banner offers: where its button goes, and which repair
/// it turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    /// The link the button navigates to. Always one [`admit_search`]
    /// accepts.
    pub href: String,
    /// Which repair this is. Usually the parameter the banner named;
    /// [`Param::Link`] when the named repair degraded to starting over,
    /// which is also what says the editor buffer goes with it.
    pub param: Param,
}

impl Repair {
    /// The button's text.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        repair_label(self.param)
    }
}

/// Decide which repair a banner can actually offer, and where it goes.
///
/// [`repair_url`] carries every other parameter through and re-encodes
/// it on the way, so a link that is inside the length bound as it stands
/// can be rewritten into one past it: 10 918 spaces in `q` arrive as
/// bare `+` (one byte each) and go back out as `%20` (three), and the
/// range repair the banner named builds a 32 KiB+ link. Asking
/// [`admit_search`] here rather than at the click is what stops the
/// banner offering a button that refuses itself, leaving a reader with a
/// dead control and every other control disabled around it.
///
/// The fallback is the whole-link repair: `/search`, no query string,
/// which is admitted by construction and is exactly what a reader stuck
/// at an unrepairable link wants.
#[must_use]
pub fn plan_repair<'a>(
    params: impl IntoIterator<Item = (&'a str, &'a str)>,
    which: Param,
) -> Repair {
    let href = repair_url(params, which);
    if which != Param::Link && admit_search(&href).is_err() {
        return Repair {
            href: repair_url(std::iter::empty(), Param::Link),
            param: Param::Link,
        };
    }
    Repair { href, param: which }
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

/// What a refusal says to the person who clicked. One sentence, naming
/// the limit rather than the internal rule.
///
/// Two families reach here: a filter the link cannot carry, and a link
/// the reader would refuse whole. They read differently because they are
/// different acts. One click added a chip, the other tried to open a
/// search.
#[must_use]
pub const fn refusal_copy(reason: Reason) -> &'static str {
    match reason {
        Reason::LinkTooLong | Reason::TooManyParameters => "Can't open this search: link too long",
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

    /// The pairs a readable search string carries.
    ///
    /// Shadows the real `query_params` for the tables below, which are
    /// about what a link MEANS and hand it only links the reader
    /// accepts. The refusals are their own tests, on `read_search`,
    /// which is the door the app actually uses.
    fn query_params(raw: &str) -> Vec<(String, String)> {
        super::query_params(raw).expect("a link the reader accepts")
    }

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
    /// table below is checked against the rule rather than the code. The
    /// apostrophe is NOT in it: the browser re-encodes that one on the
    /// way into `location.search`, so a link that spelled it literally
    /// would be measured shorter than the link the browser keeps.
    fn unreserved(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'(' | b')')
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
        // The literal is the browser's own storage form, so it carries
        // the apostrophe encoded. `search-url.spec.ts` asserts
        // `location.search` against this same string; the one place
        // `encodeURIComponent` disagrees is right here.
        assert!(RESERVED_SET.contains('\''));
        assert!(!RESERVED_SET_ENCODED.contains('\''));
        assert_eq!(percent_encode("'"), "%27");
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
        assert_eq!(query_params("q"), vec![("q".into(), String::new())]);
        // …and only the FIRST `=` splits, so an `=` inside a DSL value
        // stays in the value.
        assert_eq!(
            query_params("q=service=nginx"),
            vec![("q".into(), "service=nginx".into())]
        );
    }

    /// Only the five parameters this app reads are kept, and a name is
    /// matched the way the browser would match it — `%71` IS `q`.
    #[test]
    fn only_the_known_parameters_are_read() {
        assert_eq!(
            query_params("utm_source=chat&q=x&ref=%2Fa&page=2&nope"),
            vec![("q".into(), "x".into()), ("page".into(), "2".into())]
        );
        assert_eq!(query_params("%71=x"), vec![("q".into(), "x".into())]);
        assert_eq!(query_params("%70age=2"), vec![("page".into(), "2".into())]);
        // Not a key we read, however it is spelled.
        for raw in ["live", "a%20b=1", "c%2Bd=2", "qq=x", "%71%71=x"] {
            assert_eq!(query_params(raw), Vec::new(), "{raw}");
        }
        // A name long enough to be junk is never decoded to find that
        // out: nothing that decodes to a 1-4 byte key can be over 12
        // bytes raw.
        assert_eq!(query_params("%71%20%20%20%20=x"), Vec::new());
    }

    /// The pair cap is a bound on work, and reaching it is a verdict
    /// about the whole link rather than a licence to answer from the
    /// pairs that fit.
    #[test]
    fn a_link_past_the_pair_cap_is_refused_whole() {
        // Distinct names, so nothing here is a duplicate being dropped.
        let junk = |n: usize| {
            (0..n).fold(String::new(), |mut acc, i| {
                let _ = write!(acc, "x{i}=1&");
                acc
            })
        };

        // The last pair the cap admits is read as written.
        let inside = format!("{}q=late", junk(MAX_SEARCH_PAIRS - 1));
        let read = read_search(&inside);
        assert_eq!(read.malformed(), None);
        assert_eq!(first_value(read.params(), "q"), Some("late"));

        // One more non-empty pair and the link is refused: `q` sits
        // past the scan, and reading the link as if it carried no `q`
        // would run the whole corpus over the default window.
        let past = format!("{}q=late", junk(MAX_SEARCH_PAIRS));
        let read = read_search(&past);
        let m = read.malformed().expect("65 pairs plus q is refused");
        assert_eq!(m.param, Param::Link);
        assert_eq!(m.reason, Reason::TooManyParameters);
        assert!(read.params().is_empty());
        assert_eq!(m.message(), "This link has too many parameters to read.");
        assert_eq!(repair_label(m.param), "Start over");
        assert_eq!(m.truncated_raw(), "");

        // 60 unknown pairs is comfortably inside it, `q` and all.
        let sixty = format!("{}q=late", junk(60));
        assert_eq!(first_value(read_search(&sixty).params(), "q"), Some("late"));

        // Junk alone, over the cap, is refused on its own: the rule is
        // the count, not whether a key we read sits past it.
        assert_eq!(
            read_search(&junk(MAX_SEARCH_PAIRS + 1))
                .malformed()
                .map(|m| m.reason),
            Some(Reason::TooManyParameters)
        );
    }

    /// The bypass the pair cap used to hand out: 64 EMPTY pairs spent
    /// the whole budget, so the `f` behind them was never examined and
    /// the link ran with no filters and no banner, the one outcome
    /// ADR-0027 exists to prevent. An empty pair is not a parameter.
    #[test]
    fn empty_pairs_neither_bypass_the_cap_nor_count_against_it() {
        let bypass = format!("q=service%3Dnginx{}f=v1.!", "&".repeat(MAX_SEARCH_PAIRS));
        let read = read_search(&bypass);
        assert_eq!(read.malformed(), None);
        // The `f` is read, and it is the one the banner names.
        assert_eq!(first_value(read.params(), "f"), Some("v1.!"));
        assert_eq!(
            decode_filters(first_value(read.params(), "f").unwrap())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UndecodableFilters)
        );

        // A handful of stray empties in an ordinary link changes
        // nothing about it.
        let stray = format!("q=x&&&&&f={}", encode_filters(&[inc("host", "web-01")]));
        let read = read_search(&stray);
        assert_eq!(read.malformed(), None);
        assert_eq!(first_value(read.params(), "q"), Some("x"));
        assert_eq!(
            decode_filters(first_value(read.params(), "f").unwrap()),
            Verdict::Valid(vec![inc("host", "web-01")])
        );

        // Even a million of them: they are skipped before the count, so
        // only the byte bound can refuse a link made of them.
        let empties = "&".repeat(MAX_SEARCH_BYTES - 4);
        assert_eq!(read_search(&format!("q=x{empties}")).malformed(), None);
    }

    /// Length first, split second. A million pairs is 2 MB of address
    /// bar, and the old reader turned it into a million owned pairs
    /// before any cap applied.
    #[test]
    fn a_search_string_over_the_bound_is_one_verdict_and_nothing_else() {
        let million = format!("?{}", "a&".repeat(1_000_000));
        let read = read_search(&million);
        let m = read.malformed().expect("a 2 MB link is malformed");
        assert_eq!(m.param, Param::Link);
        assert_eq!(m.reason, Reason::LinkTooLong);
        // Nothing was parsed and nothing was kept: no pairs, and not one
        // character of the raw text retained for the banner.
        assert!(read.params().is_empty());
        assert_eq!(m.truncated_raw(), "");
        assert_eq!(m.message(), "This link is too long to read.");
        assert_eq!(repair_label(m.param), "Start over");
        // The repair is the page with no query string, not an edit of a
        // link the reader never read.
        assert_eq!(
            repair_url(
                read.params().iter().map(|(n, v)| (n.as_str(), v.as_str())),
                Param::Link
            ),
            "/search"
        );
    }

    /// The producer asks the reader, so the app cannot navigate to a
    /// link its own banner would refuse. Before this door existed, a
    /// long query submitted from the editor navigated, hit the banner,
    /// and the editor sync effect wiped the text that caused it.
    #[test]
    fn the_producer_refuses_a_link_the_reader_would_refuse() {
        // 32 760 `a`s: inside no single parameter's cap, past the
        // link's once `q=` and `&page=0` are around it.
        let long = build_search_url(
            &"a".repeat(32_760),
            0,
            Mode::Snapshot,
            &[],
            &RangeSpec::default(),
        );
        assert_eq!(admit_search(&long), Err(Reason::LinkTooLong));

        // 30 KiB is a link this app is content to write.
        let ok = build_search_url(
            &"a".repeat(30 * 1024),
            0,
            Mode::Snapshot,
            &[],
            &RangeSpec::default(),
        );
        assert_eq!(admit_search(&ok), Ok(()));

        // Percent encoding is what decides it, not the typed length: 11
        // 000 spaces are 33 000 bytes of `%20` in the address bar.
        let spaced = build_search_url(
            &" ".repeat(11_000),
            0,
            Mode::Snapshot,
            &[],
            &RangeSpec::default(),
        );
        assert_eq!(admit_search(&spaced), Err(Reason::LinkTooLong));

        // A search string handed over without its path, as
        // `use_location().search` spells it, reads the same.
        assert_eq!(admit_search("q=x&page=0"), Ok(()));
        assert_eq!(admit_search("/search"), Ok(()));
    }

    /// The two doors agree at the boundary BYTE, which is the only way
    /// to be sure the producer's refusal and the reader's refusal are
    /// the same rule: both are asked about one URL, one byte on each
    /// side of the cap.
    #[test]
    fn the_producer_and_the_reader_share_one_boundary() {
        // `q=` + the text + `&page=0` is exactly MAX_SEARCH_BYTES.
        let at_cap = build_search_url(
            &"a".repeat(MAX_SEARCH_BYTES - "q=".len() - "&page=0".len()),
            0,
            Mode::Snapshot,
            &[],
            &RangeSpec::default(),
        );
        let search = at_cap.split_once('?').unwrap().1;
        assert_eq!(search.len(), MAX_SEARCH_BYTES);
        assert_eq!(admit_search(&at_cap), Ok(()));
        assert_eq!(read_search(search).malformed(), None);

        let over_cap = build_search_url(
            &"a".repeat(MAX_SEARCH_BYTES - "q=".len() - "&page=0".len() + 1),
            0,
            Mode::Snapshot,
            &[],
            &RangeSpec::default(),
        );
        let search = over_cap.split_once('?').unwrap().1;
        assert_eq!(search.len(), MAX_SEARCH_BYTES + 1);
        assert_eq!(admit_search(&over_cap), Err(Reason::LinkTooLong));
        assert_eq!(
            read_search(search).malformed().map(|m| m.reason),
            Some(Reason::LinkTooLong)
        );
    }

    /// The same boundary, walked with the one character the browser
    /// spells differently from `encodeURIComponent`.
    ///
    /// A DSL string literal is where apostrophes come in bulk, and
    /// `message="'''…"` is what the reviewer submitted. While the
    /// encoder wrote `'` literally, admission counted one byte per
    /// apostrophe and the address bar stored three: 10 900 of them
    /// passed the producer's door, the browser handed back a 32 KiB+
    /// query string, and the reader answered with the "too long" banner
    /// over an editor the sync effect had already emptied. Both doors
    /// now measure the same bytes.
    #[test]
    fn the_boundary_holds_for_the_character_the_browser_re_encodes() {
        // `message="` + `a` + N apostrophes + `"`. The wrapper is 16
        // bytes encoded (`message` verbatim, `%3D`, two `%22`), each
        // apostrophe is 3, the `a` is 1, and `q=` + `&page=0` add 9 —
        // so N = 10 914 puts the query string exactly on the cap.
        let at_cap = build_search_url(
            &format!("message=\"a{}\"", "'".repeat(10_914)),
            0,
            Mode::Snapshot,
            &[],
            &RangeSpec::default(),
        );
        let search = at_cap.split_once('?').unwrap().1;
        assert_eq!(search.len(), MAX_SEARCH_BYTES);
        // What admission measured is what the browser keeps: no
        // apostrophe survives into the link.
        assert!(!search.contains('\''));
        assert_eq!(admit_search(&at_cap), Ok(()));
        assert_eq!(read_search(search).malformed(), None);
        assert_eq!(
            first_value(read_search(search).params(), "q").map(str::len),
            Some("message=\"a\"".len() + 10_914)
        );

        let over_cap = build_search_url(
            &format!("message=\"aa{}\"", "'".repeat(10_914)),
            0,
            Mode::Snapshot,
            &[],
            &RangeSpec::default(),
        );
        let search = over_cap.split_once('?').unwrap().1;
        assert_eq!(search.len(), MAX_SEARCH_BYTES + 1);
        assert_eq!(admit_search(&over_cap), Err(Reason::LinkTooLong));
        assert_eq!(
            read_search(search).malformed().map(|m| m.reason),
            Some(Reason::LinkTooLong)
        );
    }

    #[test]
    fn the_search_length_bound_is_inclusive() {
        let at_cap = format!("q={}", "a".repeat(MAX_SEARCH_BYTES - 2));
        assert_eq!(at_cap.len(), MAX_SEARCH_BYTES);
        let read = read_search(&at_cap);
        assert_eq!(read.malformed(), None);
        assert_eq!(
            first_value(read.params(), "q").map(str::len),
            Some(MAX_SEARCH_BYTES - 2)
        );

        let over_cap = format!("{at_cap}a");
        assert_eq!(over_cap.len(), MAX_SEARCH_BYTES + 1);
        assert_eq!(
            read_search(&over_cap).malformed().map(|m| m.reason),
            Some(Reason::LinkTooLong)
        );
        // A single oversized `q` is the same verdict: the bound is on
        // the whole string, so there is no parameter to name.
        let long_q = format!("q={}", "a".repeat(33 * 1024));
        assert_eq!(
            read_search(&long_q).malformed().map(|m| m.param),
            Some(Param::Link)
        );
    }

    /// Duplicates: every pair is kept in arrival order, and the FIRST is
    /// the one a lookup answers with, as `URLSearchParams.get` does.
    #[test]
    fn a_duplicated_key_reads_as_its_first_value() {
        // The duplicate is dropped as the string is read, so the rule is
        // one place rather than one per consumer.
        let params = query_params("r=1h&q=a&r=garbage&q=b");
        assert_eq!(
            params,
            vec![("r".into(), "1h".into()), ("q".into(), "a".into())]
        );
        assert_eq!(first_value(&params, "r"), Some("1h"));
        assert_eq!(first_value(&params, "q"), Some("a"));
        assert_eq!(first_value(&params, "page"), None);
        // A repair therefore cannot bring the second `r` back as the
        // first one's replacement: it was never carried.
        assert_eq!(
            repair_url(
                params.iter().map(|(n, v)| (n.as_str(), v.as_str())),
                Param::Range
            ),
            "/search?q=a"
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
        // The link family: a search that cannot be written as a URL the
        // reader reads back. Different act, different sentence.
        assert_eq!(
            refusal_copy(Reason::LinkTooLong),
            "Can't open this search: link too long"
        );
        assert_eq!(
            refusal_copy(Reason::TooManyParameters),
            "Can't open this search: link too long"
        );
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
        assert_eq!(repair_label(m.param), "Drop filters");
        assert_eq!(m.truncated_raw().chars().count(), RAW_DISPLAY_CHARS + 1);
        assert!(m.truncated_raw().ends_with('\u{2026}'));

        let short = Malformed::new(Param::Range, "garbage", Reason::UnreadableRange);
        assert_eq!(short.message(), "This link's time range could not be read:");
        assert_eq!(repair_label(short.param), "Use last 15 minutes");
        assert_eq!(short.truncated_raw(), "garbage");

        // Cut on a char boundary, never inside a multibyte character.
        let wide = Malformed::new(Param::Page, &"日".repeat(200), Reason::PageOffsetOverflow);
        assert_eq!(wide.message(), "This link's page could not be read:");
        assert_eq!(repair_label(wide.param), "Go to page 1");
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

    /// A repair whose own link busts the bound degrades to starting
    /// over, decided here rather than at a click that would fail.
    ///
    /// The reviewer's link: `q=service%3Dnginx` followed by 10 918 bare
    /// `+`, which the browser hands over as spaces, and `r=garbage`. The
    /// raw link is 11 KB and reads fine; the range repair re-encodes
    /// those spaces as `%20` and lands past 32 KiB, so "Use last 15
    /// minutes" used to be a button that did nothing while every other
    /// control on the page was disabled.
    #[test]
    fn a_repair_that_cannot_be_admitted_degrades_to_starting_over() {
        let long_q = format!("service=nginx{}", " ".repeat(10_918));
        let params = [("q", long_q.as_str()), ("r", "garbage")];
        assert_eq!(
            admit_search(&repair_url(params, Param::Range)),
            Err(Reason::LinkTooLong)
        );

        let repair = plan_repair(params, Param::Range);
        assert_eq!(repair.param, Param::Link);
        assert_eq!(repair.href, "/search");
        assert_eq!(repair.label(), "Start over");
        assert_eq!(admit_search(&repair.href), Ok(()));

        // One space fewer and the named repair fits, so it stands: the
        // fallback is the exception, not the rule.
        let fits_q = format!("service=nginx{}", " ".repeat(10_917));
        let repair = plan_repair([("q", fits_q.as_str()), ("r", "garbage")], Param::Range);
        assert_eq!(repair.param, Param::Range);
        assert_eq!(repair.label(), "Use last 15 minutes");
        assert_eq!(admit_search(&repair.href), Ok(()));
        assert!(!repair.href.contains("r="));

        // An ordinary broken link is untouched by any of this.
        let repair = plan_repair([("q", "service=nginx"), ("f", "v1.!")], Param::Filters);
        assert_eq!(repair.param, Param::Filters);
        assert_eq!(repair.href, "/search?q=service%3Dnginx");

        // And the whole-link repair is its own answer, not a fallback
        // onto itself.
        let repair = plan_repair([("q", long_q.as_str())], Param::Link);
        assert_eq!(repair.param, Param::Link);
        assert_eq!(repair.href, "/search");
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
