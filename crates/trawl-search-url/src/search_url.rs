// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL contract (ADR-0027): one producer, one reader, both pure.

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};

use trawl_core::parser::suggest::quote_dsl_field;

use crate::filter_codec::{self, PayloadError};
use crate::query_merge::{Filter, FilterOp, QUICK_RANGES, RangeSpec, effective_query, live_query};

pub const PAGE_SIZE: usize = 50;

/// Widened once, so the offset check answers the same on a 64-bit host
/// test and a 32-bit wasm browser. A `usize` parse would make
/// `page=18446744073709551615` a parse failure in the browser and an
/// arithmetic overflow on the test host — two different contracts.
const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;

pub const MAX_OFFSET: u64 = u32::MAX as u64;

pub const MAX_SEARCH_BYTES: usize = 32 * 1024;

pub const MAX_SEARCH_PAIRS: usize = 64;

pub const KNOWN_KEYS: [&str; 5] = ["q", "page", "mode", "f", "r"];

pub const MAX_FILTER_PAYLOAD_BYTES: usize = 4096;
pub const MAX_FILTERS: usize = 32;
pub const MAX_FILTER_FIELD_BYTES: usize = 255;
pub const MAX_FILTER_VALUE_BYTES: usize = 1024;

pub const MAX_RANGE_BYTES: usize = 64;

pub const RESERVED_SET: &str = " #&/:%'!~*()日本語😀";
pub const RESERVED_SET_ENCODED: &str =
    "%20%23%26%2F%3A%25%27!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80";

#[must_use]
pub fn is_executable(query: &str) -> bool {
    !query.trim().is_empty()
}

pub const EMPTY_QUERY_REFUSAL: &str = "Nothing to export: the query is empty.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Snapshot,
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

/// `range` is taken in both arms on purpose: the caller hands over the
/// URL's range and this function decides, so no caller can decide it
/// differently.
#[must_use]
pub fn mode_query(mode: Mode, base_q: &str, filters: &[Filter], range: &RangeSpec) -> String {
    match mode {
        Mode::Snapshot => effective_query(base_q, filters, range),
        Mode::Live => live_query(base_q, filters),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    Filters,
    Range,
    Page,
    Link,
}

impl Param {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Filters => "f",
            Self::Range => "r",
            Self::Page => "page",
            Self::Link => "",
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    UnversionedFilters,
    UndecodableFilters,
    FilterPayloadTooLarge,
    TooManyFilters,
    FilterFieldTooLong,
    FilterValueTooLong,
    InvalidFilterRecord,
    UnrenderableFilterField,
    UnreadableRange,
    RangeTooLong,
    RangeReversed,
    PageOffsetOverflow,
    LinkTooLong,
    TooManyParameters,
}

/// The value is attacker-controlled and unbounded, and the verdict is
/// cloned into every memo that reads it, so keeping the whole string
/// would hand the address bar a megabyte of resident state per read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    pub param: Param,
    raw_prefix: String,
    truncated: bool,
    pub reason: Reason,
}

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

    #[must_use]
    pub fn truncated_raw(&self) -> String {
        let mut out = self.raw_prefix.clone();
        if self.truncated {
            out.push('\u{2026}');
        }
        out
    }

    /// A link of sixty-six parameters can be three hundred bytes long, and
    /// telling its reader it is too long would be a lie.
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

/// A free function rather than a method on [`Malformed`] or [`Param`]:
/// the label belongs to the repair that is actually OFFERED, which
/// [`plan_repair`] decides.
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
    #[must_use]
    pub const fn malformed(&self) -> Option<&Malformed> {
        match self {
            Self::Malformed(m) => Some(m),
            Self::Absent | Self::Valid(_) => None,
        }
    }

    #[must_use]
    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Valid(v) => Some(v),
            Self::Absent | Self::Malformed(_) => None,
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchRead {
    Refused(Malformed),
    Params(Vec<(String, String)>),
}

impl SearchRead {
    #[must_use]
    pub fn params(&self) -> &[(String, String)] {
        match self {
            Self::Params(params) => params,
            Self::Refused(_) => &[],
        }
    }

    #[must_use]
    pub const fn malformed(&self) -> Option<&Malformed> {
        match self {
            Self::Refused(m) => Some(m),
            Self::Params(_) => None,
        }
    }
}

#[must_use]
pub fn read_search(raw_search: &str) -> SearchRead {
    let refused = |reason| SearchRead::Refused(Malformed::new(Param::Link, "", reason));
    if raw_search.len() > MAX_SEARCH_BYTES {
        return refused(Reason::LinkTooLong);
    }
    match query_params(raw_search, &KNOWN_KEYS) {
        Ok(params) => SearchRead::Params(params),
        Err(reason) => refused(reason),
    }
}

/// Only a short name carrying an escape is decoded, because `%71=x` IS
/// `q=x` to the browser and reading it any other way would make the app
/// disagree with the address bar it came from.
fn known_key(raw_name: &str, keys: &[&'static str]) -> Option<&'static str> {
    if let Some(key) = keys.iter().find(|key| **key == raw_name) {
        return Some(key);
    }
    // Each byte of a known key can occupy at most one percent escape.
    let max_raw_bytes = keys.iter().map(|key| key.len()).max().unwrap_or(0) * 3;
    if raw_name.len() > max_raw_bytes || !raw_name.bytes().any(|byte| byte == b'%' || byte == b'+')
    {
        return None;
    }
    let decoded = form_decode(raw_name);
    keys.iter().copied().find(|key| *key == decoded)
}

pub fn query_params(
    raw_search: &str,
    keys: &[&'static str],
) -> Result<Vec<(String, String)>, Reason> {
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
        let Some(key) = known_key(raw_name, keys) else {
            continue;
        };
        if params.iter().any(|(name, _)| name == key) {
            continue;
        }
        params.push((key.to_owned(), form_decode(raw_value)));
    }
    Ok(params)
}

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

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[must_use]
pub fn first_value<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

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
        // before it navigates. No assertion here — a debug build that
        // panics on a URL is worse than the banner.
        let _ = write!(url, "&f={}", encode_filters(filters));
    }
    if *range != RangeSpec::default() {
        let _ = write!(url, "&r={}", encode_range(range));
    }
    url
}

pub fn admit_search(url_or_search: &str) -> Result<(), Reason> {
    match read_search(search_of(url_or_search)) {
        SearchRead::Params(_) => Ok(()),
        SearchRead::Refused(m) => Err(m.reason),
    }
}

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
/// and carrying every other parameter through, because a link can be wrong
/// in more than one place at once.
///
/// `params` is the query as the router hands it over: each value
/// decoded exactly once, in the order the URL carries them.
#[must_use]
pub fn repair_url<'a>(
    params: impl IntoIterator<Item = (&'a str, &'a str)>,
    which: Param,
) -> String {
    if which == Param::Link {
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
    #[must_use]
    pub const fn label(&self) -> &'static str {
        repair_label(self.param)
    }
}

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

#[must_use]
pub fn encode_filters(filters: &[Filter]) -> String {
    filter_codec::encode_payload(
        filters
            .iter()
            .map(|f| (f.op.prefix(), f.field.as_str(), f.value.as_str())),
    )
}

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

pub fn admit_filters(filters: &[Filter]) -> Result<(), Reason> {
    admit_records(filters)?;
    if encode_filters(filters).len() > MAX_FILTER_PAYLOAD_BYTES {
        return Err(Reason::FilterPayloadTooLarge);
    }
    Ok(())
}

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

fn canonical_instant(raw: &str) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw).ok()?;
    let utc = parsed.with_timezone(&Utc);
    (utc.to_rfc3339_opts(SecondsFormat::Secs, true) == raw).then_some(utc)
}

#[must_use]
pub fn normalize_instant(raw: &str) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(raw.trim()).ok()?;
    Some(
        parsed
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

/// The applied History view. Draft input never changes this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryView {
    pub filter: String,
    pub page: usize,
}

const HISTORY_ENCODING_ERROR: &str = "This history link has invalid parameter encoding.";
const HISTORY_FILTER_TOO_LONG: &str = "This history filter is too long to keep in a pageable link.";

/// Read the browser's raw query once. History's names and filter are strict;
/// Search's deliberately forgiving decoder and History's page grammar remain.
pub fn read_history_view(raw_search: &str) -> Result<HistoryView, &'static str> {
    let raw_search = raw_search.strip_prefix('?').unwrap_or(raw_search);
    if raw_search.len() > MAX_SEARCH_BYTES {
        return Err("This history link is too long to read.");
    }
    if raw_search
        .split('&')
        .filter(|pair| !pair.is_empty())
        .count()
        > MAX_SEARCH_PAIRS
    {
        return Err("This history link has too many parameters to read.");
    }
    let mut filter = None;
    let mut page = None;
    for pair in raw_search.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match history_form_decode(name)?.as_str() {
            "hq" => {
                if filter.is_some() {
                    return Err("This history link repeats its filter.");
                }
                filter = Some(history_form_decode(value)?);
            }
            "hpage" if page.is_none() => page = Some(form_decode(value)),
            _ => {} // Unknown values are ignored without decoding.
        }
    }
    let raw = page.as_deref().unwrap_or_default();
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) && raw.parse::<u64>().is_err() {
        return Err("This history page is too large to request.");
    }
    let Verdict::Valid(page) = parse_page(raw) else {
        return Err("This history page is too large to request.");
    };
    admit_history_view(filter.as_deref().unwrap_or_default(), page)
}

fn history_form_decode(raw: &str) -> Result<String, &'static str> {
    let mut output = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        let byte = match byte {
            b'+' => b' ',
            b'%' => {
                let hi = bytes
                    .next()
                    .and_then(hex_digit)
                    .ok_or(HISTORY_ENCODING_ERROR)?;
                let lo = bytes
                    .next()
                    .and_then(hex_digit)
                    .ok_or(HISTORY_ENCODING_ERROR)?;
                (hi << 4) | lo
            }
            byte => byte,
        };
        if byte == 0 {
            return Err(HISTORY_ENCODING_ERROR);
        }
        output.push(byte);
    }
    String::from_utf8(output).map_err(|_| HISTORY_ENCODING_ERROR)
}

/// Admit both submissions and pasted filters with room for the largest page.
pub fn admit_history_view(filter: &str, page: usize) -> Result<HistoryView, &'static str> {
    if filter.contains('\0') {
        return Err(HISTORY_ENCODING_ERROR);
    }
    if (page as u64) > MAX_OFFSET / PAGE_SIZE_U64 {
        return Err("This history page is too large to request.");
    }
    if !filter.is_empty() {
        // Bound before percent encoding can allocate up to three times input.
        if filter.len() > MAX_SEARCH_BYTES {
            return Err(HISTORY_FILTER_TOO_LONG);
        }
        let reservation = format!(
            "hq={}&hpage={}",
            percent_encode(filter),
            MAX_OFFSET / PAGE_SIZE_U64
        );
        if reservation.len() > MAX_SEARCH_BYTES {
            return Err(HISTORY_FILTER_TOO_LONG);
        }
    }
    Ok(HistoryView {
        filter: filter.to_owned(),
        page,
    })
}

/// Canonical History URL. Call admission before navigating to this URL.
pub fn build_history_url(view: &HistoryView) -> String {
    let mut params = Vec::new();
    if !view.filter.is_empty() {
        params.push(format!("hq={}", percent_encode(&view.filter)));
    }
    if view.page != 0 {
        params.push(format!("hpage={}", view.page));
    }
    if params.is_empty() {
        "/search/history".to_owned()
    } else {
        format!("/search/history?{}", params.join("&"))
    }
}

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

    #[test]
    fn mode_query_selects_the_dsl_each_mode_runs() {
        let q = "service=nginx";
        assert_eq!(
            mode_query(Mode::Live, q, &[], &RangeSpec::default()),
            "service=nginx"
        );
        assert_eq!(
            mode_query(Mode::Snapshot, q, &[], &RangeSpec::default()),
            "last=15m service=nginx"
        );
        let abs = RangeSpec::Absolute {
            from: "2026-01-01T00:00:00Z".into(),
            to: "now".into(),
        };
        assert_eq!(mode_query(Mode::Live, q, &[], &abs), "service=nginx");
        let f = [Filter {
            field: "host".into(),
            value: "web-01".into(),
            op: FilterOp::Include,
        }];
        assert_eq!(
            mode_query(Mode::Live, q, &f, &RangeSpec::default()),
            "host=\"web-01\" service=nginx"
        );
        assert_eq!(
            mode_query(
                Mode::Live,
                "last=1h service=nginx",
                &[],
                &RangeSpec::default()
            ),
            "last=1h service=nginx"
        );
    }

    #[test]
    fn history_page_uses_single_decode_and_checked_offset() {
        let read_history_page = |raw: &str| read_history_view(raw).map(|view| view.page);
        assert_eq!(read_history_page("?hpage=%31"), Ok(1));
        assert_eq!(read_history_page("?%68%70%61%67%65=1"), Ok(1));
        assert_eq!(read_history_page("?hpage=%2531"), Ok(0));
        assert_eq!(read_history_page("?%68page=2&hpage=3"), Ok(2));
        assert_eq!(read_history_page("?hpage=85899345"), Ok(85_899_345));
        assert!(read_history_page("?hpage=85899346").is_err());
        assert!(read_history_page("?hpage=184467440737095516160").is_err());
        assert!(read_history_page(&format!("?{}", "a&".repeat(MAX_SEARCH_PAIRS + 1))).is_err());
    }

    #[test]
    fn history_filter_codec_is_strict_and_single_decode() {
        for (raw, expected) in [
            ("", ""),
            ("?hq=", ""),
            ("?hq=a++b", "a  b"),
            ("?%68q=%2B%25%27%5C", "+%'\\"),
            ("?hq=%2531", "%31"),
            ("?hq=%E6%97%A5%E6%9C%AC%E8%AA%9E", "日本語"),
            ("?junk=%FF%GG%00&hq=ok", "ok"),
        ] {
            let view = read_history_view(raw).unwrap();
            assert_eq!(view.filter, expected);
            let url = build_history_url(&view);
            assert_eq!(
                read_history_view(url.split_once('?').map_or("", |(_, query)| query)).unwrap(),
                view
            );
        }
        for raw in [
            "?hq=a&%68q=b",
            "?hq=&hq=",
            "?hq=%",
            "?hq=%GG",
            "?hq=%FF",
            "?hq=%E6%97",
            "?hq=%00",
            "?%FF=x",
            "?%=x",
            "?%00=x",
        ] {
            assert!(read_history_view(raw).is_err(), "{raw}");
        }
        for raw in ["?hpage=%GG", "?hpage=%FF", "?hpage=%00", "?hpage=1&hpage=2"] {
            assert!(
                read_history_view(raw).is_ok(),
                "legacy hpage semantics: {raw}"
            );
        }
        assert_eq!(
            build_history_url(&admit_history_view("", 0).unwrap()),
            "/search/history"
        );
        assert_eq!(
            build_history_url(&admit_history_view("+%'", 0).unwrap()),
            "/search/history?hq=%2B%25%27"
        );
    }

    #[test]
    fn history_filter_admission_reserves_every_page_with_exact_byte_bounds() {
        let max_page = usize::try_from(MAX_OFFSET / PAGE_SIZE_U64).unwrap();
        assert_eq!(max_page, 85_899_345);
        let overhead = "hq=&hpage=85899345".len();
        let text = "x".repeat(MAX_SEARCH_BYTES - overhead);
        let view = admit_history_view(&text, max_page).unwrap();
        let url = build_history_url(&view);
        let raw = url.split_once('?').unwrap().1;
        assert_eq!(raw.len(), MAX_SEARCH_BYTES);
        assert_eq!(read_history_view(raw).unwrap(), view);
        assert_eq!(read_history_view(&format!("?{raw}")).unwrap(), view);
        assert!(admit_history_view(&format!("{text}x"), 0).is_err());
        assert!(read_history_view(&format!("hq={text}x")).is_err());
        let text = format!("{}日本語+%'", "x".repeat(MAX_SEARCH_BYTES - overhead - 36));
        assert!(admit_history_view(&text, max_page).is_ok());
        assert!(admit_history_view(&format!("{text}x"), 0).is_err());
        let unknown = format!("ignored={}", "x".repeat(MAX_SEARCH_BYTES - 8));
        assert!(read_history_view(&format!("?{unknown}")).is_ok());
        assert!(read_history_view(&format!("?{unknown}x")).is_err());
        assert!(read_history_view(&vec!["ignored=%FF"; 64].join("&")).is_ok());
        assert!(read_history_view(&vec!["ignored=%FF"; 65].join("&")).is_err());
        assert!(admit_history_view("\0", 0).is_err());
    }

    /// Shadows the real `query_params` for the tables below, which are
    /// about what a link MEANS and hand it only links the reader
    /// accepts. The refusals are their own tests, on `read_search`,
    /// which is the door the app actually uses.
    fn query_params(raw: &str) -> Vec<(String, String)> {
        super::query_params(raw, &KNOWN_KEYS).expect("a link the reader accepts")
    }

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

    #[test]
    fn an_empty_or_blank_query_never_executes() {
        for raw in ["", " ", "\t", "\n", "  \t\n "] {
            assert!(!is_executable(raw), "{raw:?}");
        }
        for raw in ["*", "service=nginx", " service=nginx "] {
            assert!(is_executable(raw), "{raw:?}");
        }
    }

    /// Whether a byte passes through unencoded, spelled out here so the
    /// table below is checked against the rule rather than the code.
    fn unreserved(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'(' | b')')
    }

    #[test]
    fn every_byte_a_string_can_carry_encodes_as_the_browser_would() {
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
        assert_eq!(percent_encode("%41"), "%2541");
        assert_eq!(percent_encode("%zz"), "%25zz");
    }

    #[test]
    fn a_percent_in_the_query_text_survives_exactly_one_decode() {
        let params = query_params("q=message%3D%2F100%2541%2F&page=0");
        assert_eq!(first_value(&params, "q"), Some("message=/100%41/"));
        assert_ne!(first_value(&params, "q"), Some("message=/100A/"));
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
            ("q=%zz", "%zz"),
            ("q=%", "%"),
            ("q=%4", "%4"),
            ("q=50%%2041", "50% 41"),
            ("q=%E6%97%A5", "\u{65e5}"),
            ("q=%e6%97%a5", "\u{65e5}"),
            ("q=%F0%9F%98%80", "\u{1f600}"),
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
        assert_eq!(query_params(""), Vec::new());
        assert_eq!(query_params("?"), Vec::new());
        assert_eq!(
            query_params("?&q=x&&page=0&"),
            vec![("q".into(), "x".into()), ("page".into(), "0".into())]
        );
        assert_eq!(query_params("q"), vec![("q".into(), String::new())]);
        assert_eq!(
            query_params("q=service=nginx"),
            vec![("q".into(), "service=nginx".into())]
        );
    }

    #[test]
    fn only_the_known_parameters_are_read() {
        assert_eq!(
            query_params("utm_source=chat&q=x&ref=%2Fa&page=2&nope"),
            vec![("q".into(), "x".into()), ("page".into(), "2".into())]
        );
        assert_eq!(query_params("%71=x"), vec![("q".into(), "x".into())]);
        assert_eq!(query_params("%70age=2"), vec![("page".into(), "2".into())]);
        for raw in ["live", "a%20b=1", "c%2Bd=2", "qq=x", "%71%71=x"] {
            assert_eq!(query_params(raw), Vec::new(), "{raw}");
        }
        // A name long enough to be junk is never decoded to find that
        // out: nothing that decodes to a 1-4 byte key can be over 12
        // bytes raw.
        assert_eq!(query_params("%71%20%20%20%20=x"), Vec::new());
    }

    #[test]
    fn a_link_past_the_pair_cap_is_refused_whole() {
        // Distinct names, so nothing here is a duplicate being dropped.
        let junk = |n: usize| {
            (0..n).fold(String::new(), |mut acc, i| {
                let _ = write!(acc, "x{i}=1&");
                acc
            })
        };

        let inside = format!("{}q=late", junk(MAX_SEARCH_PAIRS - 1));
        let read = read_search(&inside);
        assert_eq!(read.malformed(), None);
        assert_eq!(first_value(read.params(), "q"), Some("late"));

        let past = format!("{}q=late", junk(MAX_SEARCH_PAIRS));
        let read = read_search(&past);
        let m = read.malformed().expect("65 pairs plus q is refused");
        assert_eq!(m.param, Param::Link);
        assert_eq!(m.reason, Reason::TooManyParameters);
        assert!(read.params().is_empty());
        assert_eq!(m.message(), "This link has too many parameters to read.");
        assert_eq!(repair_label(m.param), "Start over");
        assert_eq!(m.truncated_raw(), "");

        let sixty = format!("{}q=late", junk(60));
        assert_eq!(first_value(read_search(&sixty).params(), "q"), Some("late"));

        assert_eq!(
            read_search(&junk(MAX_SEARCH_PAIRS + 1))
                .malformed()
                .map(|m| m.reason),
            Some(Reason::TooManyParameters)
        );
    }

    #[test]
    fn empty_pairs_neither_bypass_the_cap_nor_count_against_it() {
        let bypass = format!("q=service%3Dnginx{}f=v1.!", "&".repeat(MAX_SEARCH_PAIRS));
        let read = read_search(&bypass);
        assert_eq!(read.malformed(), None);
        assert_eq!(first_value(read.params(), "f"), Some("v1.!"));
        assert_eq!(
            decode_filters(first_value(read.params(), "f").unwrap())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UndecodableFilters)
        );

        let stray = format!("q=x&&&&&f={}", encode_filters(&[inc("host", "web-01")]));
        let read = read_search(&stray);
        assert_eq!(read.malformed(), None);
        assert_eq!(first_value(read.params(), "q"), Some("x"));
        assert_eq!(
            decode_filters(first_value(read.params(), "f").unwrap()),
            Verdict::Valid(vec![inc("host", "web-01")])
        );

        let empties = "&".repeat(MAX_SEARCH_BYTES - 4);
        assert_eq!(read_search(&format!("q=x{empties}")).malformed(), None);
    }

    #[test]
    fn a_search_string_over_the_bound_is_one_verdict_and_nothing_else() {
        let million = format!("?{}", "a&".repeat(1_000_000));
        let read = read_search(&million);
        let m = read.malformed().expect("a 2 MB link is malformed");
        assert_eq!(m.param, Param::Link);
        assert_eq!(m.reason, Reason::LinkTooLong);
        assert!(read.params().is_empty());
        assert_eq!(m.truncated_raw(), "");
        assert_eq!(m.message(), "This link is too long to read.");
        assert_eq!(repair_label(m.param), "Start over");
        assert_eq!(
            repair_url(
                read.params().iter().map(|(n, v)| (n.as_str(), v.as_str())),
                Param::Link
            ),
            "/search"
        );
    }

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

    #[test]
    fn the_producer_and_the_reader_share_one_boundary() {
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
        let long_q = format!("q={}", "a".repeat(33 * 1024));
        assert_eq!(
            read_search(&long_q).malformed().map(|m| m.param),
            Some(Param::Link)
        );
    }

    #[test]
    fn a_duplicated_key_reads_as_its_first_value() {
        let params = query_params("r=1h&q=a&r=garbage&q=b");
        assert_eq!(
            params,
            vec![("r".into(), "1h".into()), ("q".into(), "a".into())]
        );
        assert_eq!(first_value(&params, "r"), Some("1h"));
        assert_eq!(first_value(&params, "q"), Some("a"));
        assert_eq!(first_value(&params, "page"), None);
        assert_eq!(
            repair_url(
                params.iter().map(|(n, v)| (n.as_str(), v.as_str())),
                Param::Range
            ),
            "/search?q=a"
        );
    }

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
        assert_eq!(
            repair_url(
                params.iter().map(|(n, v)| (n.as_str(), v.as_str())),
                Param::Filters
            ),
            "/search?q=service%3Dnginx&page=0&r=garbage&mode=live"
        );

        let legacy = query_params("q=service%3Dnginx&f=%2Bhost%3Dweb-01");
        assert_eq!(first_value(&legacy, "f"), Some("+host=web-01"));
        assert_eq!(
            decode_filters(first_value(&legacy, "f").unwrap())
                .malformed()
                .map(|m| m.reason),
            Some(Reason::UnversionedFilters)
        );

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
        assert_eq!(url.matches('%').count(), 1);
    }

    #[test]
    fn a_quick_range_that_is_not_the_default_is_written() {
        assert_eq!(
            build_search_url("*", 0, Mode::Snapshot, &[], &RangeSpec::Quick("1h")),
            "/search?q=*&page=0&r=1h"
        );
    }

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
        let at_cap = format!("v1.{}", "A".repeat(MAX_FILTER_PAYLOAD_BYTES - 3));
        assert_eq!(at_cap.len(), MAX_FILTER_PAYLOAD_BYTES);
        assert_eq!(
            decode_filters(&at_cap).malformed().map(|m| m.reason),
            Some(Reason::UndecodableFilters)
        );
        let over_cap = format!("{at_cap}A");
        assert_eq!(over_cap.len(), MAX_FILTER_PAYLOAD_BYTES + 1);
        assert_eq!(
            decode_filters(&over_cap).malformed().map(|m| m.reason),
            Some(Reason::FilterPayloadTooLarge)
        );
    }

    #[test]
    fn a_dense_array_shaped_payload_inside_the_raw_cap_is_malformed() {
        let records: Vec<String> = (0..MAX_FILTERS)
            .map(|i| format!("[\"+\",\"f{i}\",\"{}\"]", "v".repeat(75)))
            .collect();
        let json = format!("[{}]", records.join(","));
        let payload = format!("v1.{}", base64_url(json.as_bytes()));
        assert_eq!(payload.len(), 3831);
        assert!(payload.len() <= MAX_FILTER_PAYLOAD_BYTES);
        let equivalent: Vec<Filter> = (0..MAX_FILTERS)
            .map(|i| inc(&format!("f{i}"), &"v".repeat(75)))
            .collect();
        assert_eq!(encode_filters(&equivalent).len(), 4727);
        assert_eq!(
            admit_filters(&equivalent),
            Err(Reason::FilterPayloadTooLarge)
        );
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
        assert!(matches!(
            decode_filters(&encode_filters(&[inc("x-forwarded-for", "10.0.0.1")])),
            Verdict::Valid(_)
        ));
    }

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

    #[test]
    fn an_oversized_range_never_reaches_the_parser() {
        let huge = "9".repeat(10_000);
        assert_eq!(
            decode_range(&huge).malformed().map(|m| m.reason),
            Some(Reason::RangeTooLong)
        );

        let longest = encode_range(&RangeSpec::Absolute {
            from: "2026-01-01T00:00:00Z".into(),
            to: "2026-12-31T23:59:59Z".into(),
        });
        assert_eq!(longest.len(), 42);
        assert!(longest.len() <= MAX_RANGE_BYTES);
        assert!(matches!(decode_range(&longest), Verdict::Valid(_)));

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
        assert_eq!(parse_page("99999999999999999999999999"), Verdict::Valid(0));
        for raw in ["18446744073709551615", "368934881474191033"] {
            assert_eq!(
                parse_page(raw).malformed().map(|m| m.reason),
                Some(Reason::PageOffsetOverflow),
                "{raw}"
            );
        }
    }

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

        let wide = Malformed::new(Param::Page, &"日".repeat(200), Reason::PageOffsetOverflow);
        assert_eq!(wide.message(), "This link's page could not be read:");
        assert_eq!(repair_label(wide.param), "Go to page 1");
        assert!(wide.truncated_raw().starts_with('日'));
        assert_eq!(wide.truncated_raw().chars().count(), RAW_DISPLAY_CHARS + 1);
    }

    #[test]
    fn a_refused_value_is_not_retained_past_the_display_window() {
        let huge = format!("v1.{}", "A".repeat(1_000_000));
        let v = decode_filters(&huge);
        let m = v.malformed().expect("a 1 MB payload is malformed");
        assert_eq!(m.reason, Reason::FilterPayloadTooLarge);
        assert_eq!(m.raw_prefix.chars().count(), RAW_DISPLAY_CHARS);
        assert!(m.truncated);
        assert_eq!(m.truncated_raw().chars().count(), RAW_DISPLAY_CHARS + 1);

        let long_range = "9".repeat(10_000);
        let r = decode_range(&long_range);
        let m = r.malformed().expect("a 10 KB range is malformed");
        assert_eq!(m.raw_prefix.chars().count(), RAW_DISPLAY_CHARS);

        let short = Malformed::new(Param::Range, "garbage", Reason::UnreadableRange);
        assert!(!short.truncated);
        assert_eq!(short.truncated_raw(), "garbage");
    }

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
        assert_eq!(
            repair_url([("page", "85899346"), ("q", "x")], Param::Page),
            "/search?page=0&q=x"
        );
    }

    #[test]
    fn a_carried_value_survives_the_browser_unchanged() {
        assert_eq!(
            repair_url(
                [("f", "v1.!"), ("r", "2026-01-01T00:00:00+02:00..now")],
                Param::Filters
            ),
            "/search?r=2026-01-01T00:00:00%2B02:00..now"
        );
        assert_eq!(
            repair_url(
                [("f", "v1.!"), ("r", "2026-01-01T00:00:00Z..now")],
                Param::Filters
            ),
            "/search?r=2026-01-01T00:00:00Z..now"
        );
        assert_eq!(
            repair_url([("f", "v1.!"), ("r", "a&b#c")], Param::Filters),
            "/search?r=a%26b%23c"
        );
        assert_eq!(
            repair_url([("q", RESERVED_SET), ("f", "v1.!")], Param::Filters),
            format!("/search?q={RESERVED_SET_ENCODED}")
        );
    }

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

        let fits_q = format!("service=nginx{}", " ".repeat(10_917));
        let repair = plan_repair([("q", fits_q.as_str()), ("r", "garbage")], Param::Range);
        assert_eq!(repair.param, Param::Range);
        assert_eq!(repair.label(), "Use last 15 minutes");
        assert_eq!(admit_search(&repair.href), Ok(()));
        assert!(!repair.href.contains("r="));

        let repair = plan_repair([("q", "service=nginx"), ("f", "v1.!")], Param::Filters);
        assert_eq!(repair.param, Param::Filters);
        assert_eq!(repair.href, "/search?q=service%3Dnginx");

        let repair = plan_repair([("q", long_q.as_str())], Param::Link);
        assert_eq!(repair.param, Param::Link);
        assert_eq!(repair.href, "/search");
    }

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
