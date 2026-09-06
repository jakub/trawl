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
//!   codec touches `r`: the browser decodes a query value exactly once
//!   before the app sees it, and the old `abs:<from>:<to>` form was
//!   split apart at the hour colon after that decode. `f` stays opaque
//!   and versioned because catalog names carry `,`, `=` and `&`.
//! - Decoding answers a [`Verdict`], not a default. A link whose
//!   structured state cannot be read is shown with a banner and does not
//!   run, so a bad `f` can no longer widen a query silently. Every
//!   decode is bounded before it allocates — a URL is attacker-controlled
//!   input to the SPA.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
// The verdict's display half (the banner's sentence, its repair label,
// the truncated raw value) has no browser consumer until the wiring
// checkpoint mounts the notice component over it.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};

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

/// Largest raw `f` value read at all, checked before base64 or JSON
/// allocation.
pub const MAX_FILTER_PAYLOAD_BYTES: usize = 4096;
/// Largest number of filters one link may carry.
pub const MAX_FILTERS: usize = 32;
/// Largest field name, in bytes, inside a decoded filter.
pub const MAX_FILTER_FIELD_BYTES: usize = 255;
/// Largest value, in bytes, inside a decoded filter.
pub const MAX_FILTER_VALUE_BYTES: usize = 1024;

/// Characters a percent-encoded reserved set exercises, shared by the
/// native table test and the browser spec so both pin one literal.
pub const RESERVED_SET: &str = " #&/:%'!~*()日本語😀";
/// [`RESERVED_SET`] as `percent_encode` renders it, which is also what
/// the browser's own `encodeURIComponent` renders.
pub const RESERVED_SET_ENCODED: &str =
    "%20%23%26%2F%3A%25'!~*()%E6%97%A5%E6%9C%AC%E8%AA%9E%F0%9F%98%80";

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
    /// `r` is neither a quick label nor a readable `<from>..<to>` pair.
    UnreadableRange,
    /// `r`'s bounds are readable but `from` is after `to`.
    RangeReversed,
    /// `page * PAGE_SIZE` does not fit — the link claims a page that
    /// cannot be asked for.
    PageOffsetOverflow,
}

/// One parameter that could not be read, with the raw value verbatim so
/// the banner can show the reader what their link actually says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    pub param: Param,
    pub raw: String,
    pub reason: Reason,
}

/// How much of the raw value the banner shows.
const RAW_DISPLAY_CHARS: usize = 120;

impl Malformed {
    fn new(param: Param, raw: &str, reason: Reason) -> Self {
        Self {
            param,
            raw: raw.to_string(),
            reason,
        }
    }

    /// The raw value cut to [`RAW_DISPLAY_CHARS`] characters on a char
    /// boundary, with an ellipsis when anything was cut. Characters, not
    /// bytes: the cut is a display bound, and a byte cut could split a
    /// multibyte name in half.
    #[must_use]
    pub fn truncated_raw(&self) -> String {
        let mut out: String = self.raw.chars().take(RAW_DISPLAY_CHARS).collect();
        if self.raw.chars().nth(RAW_DISPLAY_CHARS).is_some() {
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
        let _ = write!(url, "&f={}", encode_filters(filters));
    }
    if *range != RangeSpec::default() {
        let _ = write!(url, "&r={}", encode_range(range));
    }
    url
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
    };
    if parts.len() > MAX_FILTERS {
        return bad(Reason::TooManyFilters);
    }
    let mut filters = Vec::with_capacity(parts.len());
    for (op, field, value) in parts {
        if field.len() > MAX_FILTER_FIELD_BYTES {
            return bad(Reason::FilterFieldTooLong);
        }
        if value.len() > MAX_FILTER_VALUE_BYTES {
            return bad(Reason::FilterValueTooLong);
        }
        // `decode_payload` yields only `+` and `-`.
        let op = if op == '+' {
            FilterOp::Include
        } else {
            FilterOp::Exclude
        };
        filters.push(Filter { field, value, op });
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

/// Read the `r` parameter. Empty is missing; a known quick label is
/// itself; `<from>..<to>` needs canonical UTC bounds (`now` allowed on
/// the right only) in order. Everything else — including the old `abs:`
/// form — is malformed, with no special copy for it: it is one more
/// range the app cannot read.
#[must_use]
pub fn decode_range(raw: &str) -> Verdict<RangeSpec> {
    if raw.is_empty() {
        return Verdict::Absent;
    }
    if let Some(q) = QUICK_RANGES.iter().find(|q| **q == raw) {
        return Verdict::Valid(RangeSpec::Quick(q));
    }
    let bad = |reason| Verdict::Malformed(Malformed::new(Param::Range, raw, reason));
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
    let Ok(page) = raw.parse::<u64>() else {
        return Verdict::Valid(0);
    };
    match page
        .checked_mul(PAGE_SIZE_U64)
        .and_then(|offset| usize::try_from(offset).ok())
    {
        // The offset fits, so the page number does too.
        Some(_) => Verdict::Valid(usize::try_from(page).unwrap_or(0)),
        None => Verdict::Malformed(Malformed::new(Param::Page, raw, Reason::PageOffsetOverflow)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inc(field: &str, value: &str) -> Filter {
        Filter {
            field: field.into(),
            value: value.into(),
            op: FilterOp::Include,
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
        // The largest page whose offset still fits is fine.
        let last = u64::MAX / PAGE_SIZE_U64;
        assert!(matches!(parse_page(&last.to_string()), Verdict::Valid(_)));
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
}
