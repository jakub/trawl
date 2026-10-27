// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-Rust merge logic for `(base_q, filters[, range])` → the DSL that
//! runs. Two faces over one merge: [`effective_query`] folds the range in
//! for a snapshot, [`live_query`] leaves it out for a live stream
//! (ADR-0027 as amended 2026-09-21). [`capture_parts`] reads the same state
//! as a capture request (ADR-0057): the range-free text and a structured
//! window, and [`pin_range`] makes a moving range absolute first.
//!
//! Kept ungated (no leptos / js-sys / wasm) so it compiles on native and
//! its tests run under plain `cargo test` / `cargo nextest`. The wasm-only
//! `state::query` module in `trawl-web-ui` re-exports the types from here
//! and layers the URL encoding + signal plumbing on top.

use std::fmt::Write;

use chrono::{DateTime, Datelike, SecondsFormat, SubsecRound, TimeDelta, Utc};

use trawl_core::parser::scan::{ends_inside_comment, scan_outside_quotes};
use trawl_core::parser::suggest::quote_dsl_field;

use crate::result_path::hull;

/// Include/exclude operator for a facet-driven filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Include,
    Exclude,
}

impl FilterOp {
    #[must_use]
    pub const fn prefix(self) -> char {
        match self {
            Self::Include => '+',
            Self::Exclude => '-',
        }
    }
}

/// One include/exclude clause bound to a field value. Multiple filters
/// combine with implicit AND in the search stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub field: String,
    pub value: String,
    pub op: FilterOp,
}

/// Range window driven by the date-range popover. Merged into a SNAPSHOT
/// wire query at request time unless the user's base query already carries
/// a `last=`, `earliest=`, or `latest=` clause (user intent wins). A live
/// stream merges no range at all (ADR-0027 as amended 2026-09-21), so it
/// never reaches [`live_query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeSpec {
    /// Relative window: `5m|15m|1h|4h|24h|7d`. Emitted as `last=<label>`.
    Quick(&'static str),
    /// Explicit from/to window. Values are RFC3339-ish; `to` accepts the
    /// literal `now` (emitted as the literal `now` — `DuckDB` parses it).
    Absolute { from: String, to: String },
}

impl Default for RangeSpec {
    fn default() -> Self {
        Self::Quick("15m")
    }
}

/// Quick-range presets offered by the date-range popover.
pub const QUICK_RANGES: &[&str] = &["5m", "15m", "1h", "4h", "24h", "7d"];

/// Merge `(base_q, filters, range)` into the DSL a SNAPSHOT runs.
///
/// Rules:
/// - empty `base_q` → empty result (filters/range no-op without a query to
///   attach to; avoids fabricating a wildcard `*` that could flood the
///   server).
/// - filter clauses and the range clause prepend to the first search stage.
///   A search stage of several OR groups is parenthesised first, so they
///   narrow every group (`host="a" (service=web OR service=db)`); any
///   other search stage is kept as written.
/// - if the base query already carries a time clause, the range's `last=` is
///   suppressed (user intent wins). Absolute ranges always inject
///   `_time>="..." _time<="..."` regardless.
/// - pipeline-only base (`| stats ...`) gets a synthetic `*` search stage
///   preceding the filter + range clauses.
#[must_use]
pub fn effective_query(base_q: &str, filters: &[Filter], range: &RangeSpec) -> String {
    merge(base_q, filters, Some(range))
}

/// Merge `(base_q, filters)` into the DSL a LIVE stream runs: the query
/// text and the filter clauses, and no range clause at all (ADR-0027 as
/// amended 2026-09-21). A `last=`/`earliest=`/`latest=` the reader typed
/// into `base_q` is part of the query and is carried through untouched.
#[must_use]
pub fn live_query(base_q: &str, filters: &[Filter]) -> String {
    merge(base_q, filters, None)
}

/// The one merge both faces run. Everything but the range clause is
/// shared: an absent `range` is the only difference between the DSL a
/// live stream runs and the DSL a snapshot runs.
fn merge(base_q: &str, filters: &[Filter], range: Option<&RangeSpec>) -> String {
    let trimmed = base_q.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let (search_raw, tail) = split_search_stage(trimmed);
    let search = search_raw.trim();

    let needs_star = search.is_empty() || search.starts_with('|');
    let has_time = search_has_time_clause(search);

    let mut prefix = String::new();
    for f in filters {
        let Some(clause) = format_filter(f) else {
            continue;
        };
        if !prefix.is_empty() {
            prefix.push(' ');
        }
        prefix.push_str(&clause);
    }

    // A filter, and an absolute range's `_time` filters, join the OR group
    // they are written in. A `last=` is lifted out of every group by the
    // parser and bounds the whole query wherever it is written.
    let mut binds_to_a_group = !prefix.is_empty();
    let range_clause = match range {
        Some(RangeSpec::Quick(q)) if !has_time => Some(format!("last={q}")),
        Some(RangeSpec::Absolute { from, to }) => {
            let clause = format_absolute_range(from, to);
            binds_to_a_group |= !clause.is_empty();
            Some(clause)
        }
        // suppressed by an existing time clause, or a live stream that
        // carries no range in the first place
        Some(RangeSpec::Quick(_)) | None => None,
    };
    if let Some(clause) = range_clause {
        if !prefix.is_empty() {
            prefix.push(' ');
        }
        prefix.push_str(&clause);
    }

    let new_search = if needs_star {
        if prefix.is_empty() {
            "*".to_string()
        } else {
            prefix
        }
    } else if prefix.is_empty() {
        search.to_string()
    } else if binds_to_a_group && splits_into_or_groups(&prefix, search) {
        format!("{prefix} {}", parenthesize(search))
    } else {
        format!("{prefix} {search}")
    };

    match tail {
        // A search half that ends inside an open comment cannot be joined
        // on one line: the `|` and everything after it would be comment
        // text, and the result still parses, as a query with no pipeline.
        // A newline closes the comment and nothing else moves.
        Some(t) if ends_inside_comment(&new_search) => {
            format!("{new_search}\n| {}", t.trim_start_matches('|').trim())
        }
        Some(t) => format!("{new_search} | {}", t.trim_start_matches('|').trim()),
        None => new_search,
    }
}

/// Would `prefix` written before `search` join only the first of several
/// OR groups? The grammar answers, so an `OR` inside a quoted value, a
/// backticked name, a regex, a comment or parentheses is not one, and a
/// stray leading `OR` that would split the prefix off is. Text the grammar
/// refuses runs nowhere, and is merged as it is.
fn splits_into_or_groups(prefix: &str, search: &str) -> bool {
    trawl_core::parser::parse(&format!("{prefix} {search}"))
        .is_ok_and(|query| query.search.groups.len() > 1)
}

/// `search` as one parenthesised group (`( … OR … )`), so a token written
/// before it narrows every group. The closing parenthesis must not change
/// the last token: a comment would swallow it, so a newline ends the
/// comment first, and a regex must be followed by whitespace, so a space
/// follows a closing `/`.
fn parenthesize(search: &str) -> String {
    let close = if ends_inside_comment(search) {
        "\n)"
    } else if search.ends_with('/') {
        " )"
    } else {
        ")"
    };
    format!("({search}{close}")
}

/// Split on the first top-level `|` (skipping slash-delimited regex
/// literals, quoted strings and backtick-quoted names). Returns
/// `(search_stage, optional_tail)`.
fn split_search_stage(input: &str) -> (&str, Option<&str>) {
    match scan_outside_quotes(input, |_, b| b == b'|') {
        Some(i) => (&input[..i], Some(&input[i + 1..])),
        None => (input, None),
    }
}

/// One time clause the user wrote in the search stage. `keyword` is the
/// bare grammar word (`last`, `earliest`, `latest`); the scanner matches
/// it followed by `=`. `value` is what the user typed after the `=`,
/// unquoted if it was a DSL string literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeClause {
    pub keyword: &'static str,
    pub value: String,
}

/// Every DSL time clause a word scan finds in a search stage, in written
/// order.
///
/// A scan, not the grammar: it reads `message=last=1h` as a clause and
/// `LAST=2h` as one too, where the grammar reads field filters. Whether a
/// text bounds itself is the grammar's answer wherever the text parses;
/// this scan answers it only for text the grammar refuses.
///
/// Word-boundary check to avoid matching `loglast=` or similar.
/// Case-insensitive. A backticked `` `last` `` is a field, not the
/// grammar keyword, and a `last=` inside a quoted value or a comment is
/// prose, so the walk skips those spans entirely — otherwise the UI
/// would read them as an existing time clause and silently drop the
/// range. The closure never claims a hit, so one walk collects them all.
#[must_use]
pub fn find_time_clauses(search: &str) -> Vec<TimeClause> {
    // The grammar's own closed set, so a fourth keyword reaches this
    // scanner the day the parser learns it (trawl-core pins the set
    // against `TimeClause` in `ast.rs`).
    const KEYWORDS: &[&str] = trawl_core::parser::suggest::GRAMMAR_KEYWORDS;
    let lower = search.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found: Vec<(usize, &'static str)> = Vec::new();
    scan_outside_quotes(&lower, |i, _| {
        if i > 0 && is_ident_byte(bytes[i - 1]) {
            return false;
        }
        for kw in KEYWORDS {
            if bytes[i..].starts_with(kw.as_bytes()) && bytes.get(i + kw.len()) == Some(&b'=') {
                found.push((i + kw.len() + 1, kw));
                break;
            }
        }
        false
    });
    // `to_ascii_lowercase` is byte-for-byte, so the offsets index the
    // original: the value keeps the case the user typed.
    found
        .into_iter()
        .map(|(at, keyword)| TimeClause {
            keyword,
            value: read_clause_value(search, at),
        })
        .collect()
}

/// The value token after a `<keyword>=`, with a DSL string literal
/// unquoted and unescaped. An unterminated literal reads to the end.
fn read_clause_value(search: &str, start: usize) -> String {
    let bytes = search.as_bytes();
    if start >= bytes.len() {
        return String::new();
    }
    if bytes[start] == b'"' {
        let mut out = String::new();
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if i + 1 < bytes.len() => {
                    // The next byte is taken verbatim; a continuation
                    // byte can only follow a multi-byte lead, never a
                    // backslash, so this stays on a char boundary.
                    let next = i + 1;
                    let end = search[next..]
                        .char_indices()
                        .nth(1)
                        .map_or(search.len(), |(o, _)| next + o);
                    out.push_str(&search[next..end]);
                    i = end;
                }
                b'"' => break,
                _ => {
                    let end = search[i..]
                        .char_indices()
                        .nth(1)
                        .map_or(search.len(), |(o, _)| i + o);
                    out.push_str(&search[i..end]);
                    i = end;
                }
            }
        }
        return out;
    }
    let end = bytes[start..]
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'|')
        .map_or(bytes.len(), |p| start + p);
    search[start..end].to_string()
}

/// Does the search stage already carry a DSL time clause?
///
/// The grammar answers when it reads the stage ([`SearchStage::time_clause`](trawl_core::ast::SearchStage::time_clause)):
/// a keyword inside a field's value (`url=https://a/last=1h`,
/// `message=last=1h`) is part of the value, which a word scan cannot
/// tell. Text the grammar refuses runs nowhere, so for it the scan's
/// answer stands, and the server's parse error is what the page shows.
fn search_has_time_clause(search: &str) -> bool {
    match trawl_core::parser::parse(search) {
        Ok(query) => query.search.time_clause().is_some(),
        Err(_) => !find_time_clauses(search).is_empty(),
    }
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
}

fn format_filter(f: &Filter) -> Option<String> {
    let op = match f.op {
        FilterOp::Include => "=",
        FilterOp::Exclude => "!=",
    };
    // Quote the value so spaces / special chars don't break the DSL, and
    // render the field through the one DSL name renderer — a name the
    // bare production can't spell (`x-forwarded-for`) or a keyword
    // (`last`) needs backticks or the clause is not a filter at all
    // (ADR-0013 ruling 7).
    let quoted = dsl_string_literal(&f.value);
    let field = quote_dsl_field(&f.field)?;
    Some(format!("{field}{op}{quoted}"))
}

/// One arbitrary string as a DSL double-quoted literal: the ONE escaper
/// for every value this crate interpolates into a query. A filter value
/// is a catalog value and a range bound is a URL parameter — both are
/// client text, and a `"` in either would otherwise close the literal
/// early and the rest would parse as grammar.
fn dsl_string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn format_absolute_range(from: &str, to: &str) -> String {
    // _time>="<from>" _time<="<to>" — DSL parses these as implicit-AND
    // comparison clauses in the search stage. The canonical column by
    // name: the DSL has zero aliases (ADR-0013 §6), so `@timestamp`
    // would name an ordinary sender field most events never carry.
    let mut out = String::new();
    if !from.is_empty() {
        write!(&mut out, "_time>={}", dsl_string_literal(from)).ok();
    }
    if !to.is_empty() && to != "now" {
        if !out.is_empty() {
            out.push(' ');
        }
        write!(&mut out, "_time<={}", dsl_string_literal(to)).ok();
    }
    out
}

/// A structured capture window: the half-open `[earliest, latest)` ADR-0057
/// takes beside the query text, both RFC 3339 instants in UTC with
/// microsecond precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureWindow {
    pub earliest: String,
    pub latest: String,
}

/// What a capture request carries for one Search URL state: the text that
/// runs, and the window when the text does not bound itself in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureParts {
    pub query: String,
    /// `None` when the text is closed by its own absolute `earliest=` and
    /// `latest=`: a request carries a window or those clauses, never both
    /// (ADR-0057).
    pub window: Option<CaptureWindow>,
}

/// Why a Search URL state is not a capture request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureRefusal {
    /// There is no query to run.
    EmptyQuery,
    /// The range moves: a quick range, or an absolute range with an open
    /// end. A capture is immutable, so the page pins the range to the
    /// instant it ran (see [`pin_range`]) before it asks.
    Moving,
    /// A bound is not an RFC 3339 instant, the range has no start, or its
    /// start is after its end.
    UnreadableRange,
    /// The window's exclusive end is past the last instant the wire form
    /// spells (year 9999).
    RangeOverflow,
    /// The text's own time clause moves: a `last=`, or an `earliest=` or
    /// `latest=` without the other end or with an end that is not an
    /// absolute instant. The range control cannot pin a clause in the
    /// text, so the person removes it and picks the range there.
    TextMoves,
    /// The capture would cover more than the server's `[capture]
    /// max_span_secs`, which it advertises as `bounds.max_span_secs` and
    /// refuses past. The interval is never narrowed to fit.
    SpanTooLong { max_span_secs: u64 },
}

/// Read a Search URL state (`base_q`, `filters`, `range`) as the parts of a
/// capture request.
///
/// The capture's window is half-open, and the range the merger writes is
/// inclusive (`_time>=from`, `_time<=to`), so an absolute `[from, to]`
/// becomes `[from, to + 1 µs)`: an event exactly at `to` is inside. The
/// window is not folded into the text, so time is applied once.
///
/// The text's own time clauses are read by the grammar. Closed by an
/// absolute `earliest=` and `latest=`, the text bounds the capture, no
/// window is sent, and the text is [`effective_query`]'s: the range is
/// then merged as the snapshot merged it (a quick range yields to the
/// clause; an absolute range stays as `_time` filters beside it). Any
/// other clause moves with the clock the capture runs on, and pinning the
/// range cannot fix it, so the request is refused.
///
/// # Errors
/// [`CaptureRefusal`]: the query is empty, the text's own clause moves,
/// the range moves, the range cannot be read, the window's end does not
/// fit, or the capture would cover more than `max_span_secs`, the limit
/// the server advertises (`None` when it is not known).
pub fn capture_parts(
    base_q: &str,
    filters: &[Filter],
    range: &RangeSpec,
    max_span_secs: Option<u64>,
) -> Result<CaptureParts, CaptureRefusal> {
    let trimmed = base_q.trim();
    if trimmed.is_empty() {
        return Err(CaptureRefusal::EmptyQuery);
    }
    match text_time(&live_query(base_q, filters)) {
        TextTime::Free => {}
        TextTime::Closed { earliest, latest } => {
            within_span(earliest, latest, max_span_secs)?;
            return Ok(CaptureParts {
                query: effective_query(base_q, filters, range),
                window: None,
            });
        }
        TextTime::Moves => return Err(CaptureRefusal::TextMoves),
    }
    let RangeSpec::Absolute { from, to } = range else {
        return Err(CaptureRefusal::Moving);
    };
    if to.is_empty() || to == "now" {
        return Err(CaptureRefusal::Moving);
    }
    let from = parse_instant(from).ok_or(CaptureRefusal::UnreadableRange)?;
    let to = parse_instant(to).ok_or(CaptureRefusal::UnreadableRange)?;
    if from > to {
        return Err(CaptureRefusal::UnreadableRange);
    }
    let latest = to
        .checked_add_signed(TimeDelta::microseconds(1))
        .filter(|latest| latest.year() <= MAX_WIRE_YEAR)
        .ok_or(CaptureRefusal::RangeOverflow)?;
    within_span(from, latest, max_span_secs)?;
    Ok(CaptureParts {
        query: live_query(base_q, filters),
        window: Some(CaptureWindow {
            earliest: from.to_rfc3339_opts(SecondsFormat::Micros, true),
            latest: latest.to_rfc3339_opts(SecondsFormat::Micros, true),
        }),
    })
}

/// What a capture text's own time clauses do, as the grammar reads them.
enum TextTime {
    /// No time clause: the window bounds the capture.
    Free,
    /// An `earliest=` and a `latest=` that both name an absolute instant,
    /// and no `last=`: the text is fixed, over `[earliest, latest)`.
    Closed {
        earliest: DateTime<Utc>,
        latest: DateTime<Utc>,
    },
    /// Any other clause: it ends when the capture runs.
    Moves,
}

/// Read `text`'s time clauses. A text the grammar refuses is read as
/// carrying none: the server refuses it with its parse error, which is
/// the answer the page shows.
fn text_time(text: &str) -> TextTime {
    let Ok(query) = trawl_core::parser::parse(text) else {
        return TextTime::Free;
    };
    let search = &query.search;
    if search.time_clause().is_none() {
        return TextTime::Free;
    }
    // Read as the server reads a clause's literal (`results/bounds.rs`
    // `clause_instant`): a `DuckDB` timestamp, its offset dropped.
    let absolute = |bound: Option<&trawl_core::ast::Spanned<String>>| match bound
        .map(|bound| trawl_core::compare::literal_timestamp(&bound.node))
    {
        Some(Some(trawl_core::compare::Instant::At(at))) => Some(at.and_utc()),
        _ => None,
    };
    match (
        &search.time_filter,
        absolute(search.earliest.as_ref()),
        absolute(search.latest.as_ref()),
    ) {
        (None, Some(earliest), Some(latest)) => TextTime::Closed { earliest, latest },
        _ => TextTime::Moves,
    }
}

/// Refuse a capture over `[start, end)` that covers more than
/// `max_span_secs`, as the server measures it: both instants at the
/// microsecond it stores. An unknown limit refuses nothing; the server
/// still decides.
fn within_span(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    max_span_secs: Option<u64>,
) -> Result<(), CaptureRefusal> {
    let Some(max_span_secs) = max_span_secs else {
        return Ok(());
    };
    // A limit no `TimeDelta` holds is longer than any span.
    let Some(limit) = i64::try_from(max_span_secs)
        .ok()
        .and_then(TimeDelta::try_seconds)
    else {
        return Ok(());
    };
    if end
        .trunc_subsecs(6)
        .signed_duration_since(start.trunc_subsecs(6))
        > limit
    {
        Err(CaptureRefusal::SpanTooLong { max_span_secs })
    } else {
        Ok(())
    }
}

/// Why a range cannot be pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinRefusal {
    /// The anchor is not an RFC 3339 instant.
    UnreadableAnchor,
    /// The quick label is not `<n>m`, `<n>h` or `<n>d`, a bound is not an
    /// instant, the range has no start, or its start is after the anchor.
    UnreadableRange,
}

/// The absolute range a moving `range` was, at `anchor`: the instant the
/// search ran, which a response reports as `execution.started_at`.
///
/// A quick range of duration `d` becomes `[anchor − d, anchor]`, and an
/// absolute range with an open end keeps its start and ends at `anchor`.
/// An absolute range with both ends is not moving and comes back as it is.
/// The search URL spells instants in whole seconds, so the pinned bounds
/// are the whole-second hull of the exact ones: the start floored and the
/// end ceiled, never narrower than what ran.
///
/// # Errors
/// [`PinRefusal`] when the anchor or the range cannot be read.
pub fn pin_range(range: &RangeSpec, anchor: &str) -> Result<RangeSpec, PinRefusal> {
    let anchor_at = parse_instant(anchor).ok_or(PinRefusal::UnreadableAnchor)?;
    let from_at = match range {
        RangeSpec::Absolute { to, .. } if !to.is_empty() && to != "now" => {
            return Ok(range.clone());
        }
        RangeSpec::Absolute { from, .. } => parse_instant(from),
        RangeSpec::Quick(label) => {
            quick_duration(label).and_then(|duration| anchor_at.checked_sub_signed(duration))
        }
    }
    .ok_or(PinRefusal::UnreadableRange)?;
    if from_at > anchor_at {
        return Err(PinRefusal::UnreadableRange);
    }
    let (from, to) = hull(
        &from_at.to_rfc3339_opts(SecondsFormat::Micros, true),
        &anchor_at.to_rfc3339_opts(SecondsFormat::Micros, true),
    )
    .ok_or(PinRefusal::UnreadableRange)?;
    Ok(RangeSpec::Absolute { from, to })
}

/// The last year the wire form spells with four digits.
const MAX_WIRE_YEAR: i32 = 9999;

fn parse_instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|instant| instant.with_timezone(&Utc))
}

/// The span a quick label names: `<n>m`, `<n>h` or `<n>d`.
fn quick_duration(label: &str) -> Option<TimeDelta> {
    let unit = label.chars().last()?;
    let count: i64 = label[..label.len() - unit.len_utf8()].parse().ok()?;
    match unit {
        'm' => TimeDelta::try_minutes(count),
        'h' => TimeDelta::try_hours(count),
        'd' => TimeDelta::try_days(count),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(q: &'static str) -> RangeSpec {
        RangeSpec::Quick(q)
    }

    fn inc(field: &str, value: &str) -> Filter {
        Filter {
            field: field.into(),
            value: value.into(),
            op: FilterOp::Include,
        }
    }

    fn exc(field: &str, value: &str) -> Filter {
        Filter {
            field: field.into(),
            value: value.into(),
            op: FilterOp::Exclude,
        }
    }

    #[test]
    fn empty_base_yields_empty() {
        assert_eq!(
            effective_query("", &[inc("host", "web-01")], &quick("15m")),
            ""
        );
        assert_eq!(effective_query("   ", &[], &quick("1h")), "");
    }

    #[test]
    fn base_only_passes_through() {
        assert_eq!(
            effective_query("_severity=error", &[], &RangeSpec::default()),
            "last=15m _severity=error"
        );
    }

    #[test]
    fn filters_and_range_prepend_to_search_stage() {
        let q = effective_query(
            "_severity=error | stats count() by host",
            &[inc("host", "web-01"), exc("source", "auth.log")],
            &quick("1h"),
        );
        assert_eq!(
            q,
            "host=\"web-01\" source!=\"auth.log\" last=1h _severity=error | stats count() by host"
        );
    }

    #[test]
    fn unrepresentable_filter_names_contribute_no_query_logic() {
        let q = effective_query(
            "_severity=error",
            &[inc("a\u{202e}b", "x"), inc("", "y"), inc("request id", "7")],
            &quick("1h"),
        );
        assert_eq!(q, "`request id`=\"7\" last=1h _severity=error");
        assert!(!q.contains('\u{202e}'));
    }

    #[test]
    fn existing_last_suppresses_range() {
        let q = effective_query("last=2h _severity=error", &[], &quick("15m"));
        assert_eq!(q, "last=2h _severity=error");
    }

    #[test]
    fn absolute_dsl_bounds_suppress_quick_range() {
        for base in [
            r#"earliest="2026-01-01T00:00:00Z" service=web"#,
            r#"latest="2026-01-02T00:00:00Z" service=web"#,
            r#"service=web earliest="2026-01-01T00:00:00Z" latest="2026-01-02T00:00:00Z" | stats count()"#,
        ] {
            let merged = effective_query(base, &[], &quick("15m"));
            assert_eq!(merged, base);
            let ast = trawl_core::parser::parse(&merged).expect("merged query parses");
            trawl_core::emitter::emit(
                &ast,
                "experiment.parquet",
                trawl_core::context::EvalContext::capture(),
            )
            .expect("time bounds remain valid");
        }
    }

    #[test]
    fn absolute_time_lookalikes_do_not_suppress_quick_range() {
        for base in [
            "`earliest`=x",
            "loglatest=x",
            r#"message="earliest=x""#,
            "service=web # latest=x",
        ] {
            assert_eq!(
                effective_query(base, &[], &quick("15m")),
                format!("last=15m {base}")
            );
        }
    }

    /// The grammar's keywords are lower case: `LAST=2h` is a filter on a
    /// field called `LAST`, so it bounds nothing and the range is merged.
    #[test]
    fn an_upper_case_keyword_is_a_field_filter() {
        let q = effective_query("LAST=2h _severity=error", &[], &quick("15m"));
        assert_eq!(q, "last=15m LAST=2h _severity=error");
    }

    #[test]
    fn lookalike_token_does_not_suppress_range() {
        let q = effective_query("loglast=2h", &[], &quick("15m"));
        assert_eq!(q, "last=15m loglast=2h");
    }

    #[test]
    fn pipeline_only_base_gets_synthetic_search_stage() {
        let q = effective_query(
            "| stats count() by host",
            &[inc("host", "web-01")],
            &quick("15m"),
        );
        assert_eq!(q, "host=\"web-01\" last=15m | stats count() by host");
    }

    #[test]
    fn absolute_range_emits_canonical_time_clauses() {
        let q = effective_query(
            "*",
            &[],
            &RangeSpec::Absolute {
                from: "2026-04-18T00:00:00Z".into(),
                to: "2026-04-18T23:59:59Z".into(),
            },
        );
        assert_eq!(
            q,
            "_time>=\"2026-04-18T00:00:00Z\" _time<=\"2026-04-18T23:59:59Z\" *"
        );
    }

    #[test]
    fn absolute_range_to_now_elides_upper_bound() {
        let q = effective_query(
            "*",
            &[],
            &RangeSpec::Absolute {
                from: "2026-04-18T00:00:00Z".into(),
                to: "now".into(),
            },
        );
        assert_eq!(q, "_time>=\"2026-04-18T00:00:00Z\" *");
    }

    #[test]
    fn absolute_range_overrides_existing_last_clause() {
        let q = effective_query(
            "last=1h",
            &[],
            &RangeSpec::Absolute {
                from: "2026-04-18T00:00:00Z".into(),
                to: "now".into(),
            },
        );
        assert_eq!(q, "_time>=\"2026-04-18T00:00:00Z\" last=1h");
    }

    /// A range bound is URL text, so it reaches the DSL through the same
    /// escaper a filter value does. A bound carrying `"` and `\\` must
    /// still parse as one literal rather than closing the `_time` clause
    /// and letting the rest read as grammar.
    #[test]
    fn absolute_bounds_pass_through_the_dsl_escaper() {
        let hostile = r#"2026-01-01T00:00:00Z" or host="evil\"#;
        let q = effective_query(
            "*",
            &[],
            &RangeSpec::Absolute {
                from: hostile.into(),
                to: "now".into(),
            },
        );
        assert_eq!(q, r#"_time>="2026-01-01T00:00:00Z\" or host=\"evil\\" *"#);
        let ast = trawl_core::parser::parse(&q).expect("the escaped bound parses");
        // Exactly one field filter, on `_time`, carrying the bound back
        // verbatim — no second clause was smuggled in.
        let filters: Vec<&trawl_core::ast::FieldFilter> = ast
            .search
            .all_tokens()
            .filter_map(|t| match &t.node {
                trawl_core::ast::SearchToken::FieldFilter(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(filters.len(), 1, "{filters:?}");
        assert_eq!(filters[0].field, "_time");
        assert_eq!(filters[0].op, trawl_core::ast::FilterOp::Gte);
        assert_eq!(
            filters[0].value,
            trawl_core::ast::FilterValue::Literal(hostile.to_string())
        );
    }

    #[test]
    fn value_with_quote_is_escaped() {
        let q = effective_query("*", &[inc("msg", "he said \"hi\"")], &quick("15m"));
        assert_eq!(q, "msg=\"he said \\\"hi\\\"\" last=15m *");
    }

    #[test]
    fn split_respects_quoted_pipes() {
        let (search, tail) = split_search_stage("message=\"a|b\" | stats count()");
        assert_eq!(search, "message=\"a|b\" ");
        assert_eq!(tail, Some(" stats count()"));
    }

    #[test]
    fn split_respects_regex_pipes() {
        let (search, tail) = split_search_stage("msg=/foo|bar/ | head 10");
        assert_eq!(search, "msg=/foo|bar/ ");
        assert_eq!(tail, Some(" head 10"));
    }

    #[test]
    fn filter_field_is_rendered_through_the_dsl_renderer() {
        let q = effective_query("*", &[inc("x-forwarded-for", "10.0.0.1")], &quick("15m"));
        assert_eq!(q, "`x-forwarded-for`=\"10.0.0.1\" last=15m *");
        let q = effective_query("*", &[exc("last", "5m")], &quick("15m"));
        assert_eq!(q, "`last`!=\"5m\" last=15m *");
    }

    #[test]
    fn split_respects_backticked_pipes() {
        let (search, tail) = split_search_stage("`a|b`=x | stats count()");
        assert_eq!(search, "`a|b`=x ");
        assert_eq!(tail, Some(" stats count()"));
    }

    #[test]
    fn backticked_last_field_does_not_suppress_range() {
        let q = effective_query("`last`=5", &[], &quick("15m"));
        assert_eq!(q, "last=15m `last`=5");
    }

    /// A `//` in a value is ordinary data (ADR-0014 ruling 3), and the
    /// shared scan primitive must not read the slashes as a regex open —
    /// that would hide the user's own `last=` clause behind a phantom
    /// span and silently override their time bound.
    #[test]
    fn slashes_in_a_value_do_not_hide_the_last_clause() {
        for base in [
            "url=https://a/b last=1h",
            "path=/api//v1 last=1h",
            "url=//cdn.example.com/x last=1h",
        ] {
            // the user's own `last=` wins: the quick range is suppressed
            assert_eq!(effective_query(base, &[], &quick("15m")), base, "{base}");
        }
        // …and an absolute range still rewrites, with the search stage
        // located correctly around the slashes.
        let q = effective_query(
            "url=https://a/b | stats count()",
            &[],
            &RangeSpec::Absolute {
                from: "2026-01-01T00:00:00Z".into(),
                to: "now".into(),
            },
        );
        assert!(q.starts_with("_time>="), "{q}");
        assert!(q.ends_with("url=https://a/b | stats count()"), "{q}");
    }

    /// A comment in the base query is prose: its words are not read as
    /// grammar, and the range clause still lands in the search stage.
    #[test]
    fn a_comment_is_not_read_as_grammar() {
        assert_eq!(
            effective_query("service=x # last=1h", &[], &quick("15m")),
            "last=15m service=x # last=1h"
        );
    }

    /// A search half that ends inside an open comment gets its pipeline
    /// back on a new line. Joined on one line the `|` and every stage
    /// after it would be comment text, and the result would still parse,
    /// as a query with no pipeline at all.
    #[test]
    fn a_pipeline_is_not_swallowed_by_an_open_comment() {
        // the split trims the newline that closed the comment, so the
        // rejoin has to put one back
        assert_eq!(
            effective_query("service=x # note\n| stats count()", &[], &quick("15m")),
            "last=15m service=x # note\n| stats count()"
        );
        // the newline is spent only where it is needed
        assert_eq!(
            effective_query("service=x | stats count()", &[], &quick("15m")),
            "last=15m service=x | stats count()"
        );
        // …and a comment closed by its own newline needs none either
        assert_eq!(
            effective_query(
                "service=x # note\nhost=y | stats count()",
                &[],
                &quick("15m")
            ),
            "last=15m service=x # note\nhost=y | stats count()"
        );
    }

    /// The scan and the grammar answer the same question: a `\` outside a
    /// quoted span does not escape, and a comment opens only after ASCII
    /// whitespace. Disagreeing either hides the base query's own `last=`
    /// and injects a second time bound, or reads a phantom one and drops
    /// the popover's range.
    #[test]
    fn the_scan_agrees_with_the_grammar_on_escapes_and_whitespace() {
        // `last=1h` lives inside the quoted phrase — not a time clause,
        // so the popover's own `last=15m` is injected
        assert_eq!(
            effective_query(r#"foo\" last=1h""#, &[], &quick("15m")),
            r#"last=15m foo\" last=1h""#
        );
        // a no-break space does not open a comment — the token charsets
        // end on ASCII whitespace, so the grammar refuses this `#` rather
        // than reading prose after it, and the `last=1h` is a real clause
        assert_eq!(
            effective_query("message=\"x\"\u{a0}# last=1h", &[], &quick("15m")),
            "message=\"x\"\u{a0}# last=1h"
        );
        // …while a real one still suppresses the injection
        assert_eq!(
            effective_query("message=\"x\" last=1h", &[], &quick("15m")),
            "message=\"x\" last=1h"
        );
    }

    #[test]
    fn split_without_pipe() {
        let (search, tail) = split_search_stage("_severity=error");
        assert_eq!(search, "_severity=error");
        assert_eq!(tail, None);
    }

    #[test]
    fn default_range_is_15m() {
        assert_eq!(RangeSpec::default(), RangeSpec::Quick("15m"));
    }

    #[test]
    fn filter_op_prefix_is_stable() {
        assert_eq!(FilterOp::Include.prefix(), '+');
        assert_eq!(FilterOp::Exclude.prefix(), '-');
    }

    #[test]
    fn quick_ranges_contains_default() {
        assert!(QUICK_RANGES.contains(&"15m"));
    }

    #[test]
    fn time_clauses_keep_written_order_and_values() {
        let clauses = find_time_clauses(
            r#"earliest="2026-01-01T00:00:00Z" service=web latest="2026-01-02T00:00:00Z""#,
        );
        assert_eq!(
            clauses,
            vec![
                TimeClause {
                    keyword: "earliest",
                    value: "2026-01-01T00:00:00Z".into()
                },
                TimeClause {
                    keyword: "latest",
                    value: "2026-01-02T00:00:00Z".into()
                },
            ]
        );
    }

    #[test]
    fn only_search_stage_time_clauses_suppress_the_quick_range() {
        for (base, suppressed) in [
            ("service=x last=2h", true),
            ("LAST=2h service=x", false),
            (r#"earliest="2026-01-01T00:00:00Z" service=web"#, true),
            (r#"latest="2026-01-02T00:00:00Z" service=web"#, true),
            ("url=https://a/b last=1h", true),
            ("message=\"x\" last=1h", true),
            ("service=x", false),
            ("`last`=5", false),
            ("`last=2h`=x", false),
            ("loglast=2h", false),
            ("service=web # latest=x", false),
            (r#"message="earliest=x""#, false),
            ("| stats count() by host", false),
            ("service=x | stats count() by last=2h", false),
        ] {
            let merged = effective_query(base, &[], &quick("15m"));
            assert_eq!(!merged.contains("last=15m"), suppressed, "{base}: {merged}");
        }
        assert_eq!(effective_query("", &[], &quick("15m")), "");
    }

    #[test]
    fn a_quoted_clause_value_is_unescaped_once() {
        let clauses = find_time_clauses(r#"earliest="2026-01-01T00:00:00Z\" x" service=web"#);
        assert_eq!(clauses.len(), 1);
        assert_eq!(clauses[0].value, r#"2026-01-01T00:00:00Z" x"#);
    }

    /// A live stream carries the query text and nothing the popover
    /// picked: the stream lane evaluates a `last=` per event against the
    /// server clock, so a folded range drops every event older than the
    /// window while the page still says Live.
    #[test]
    fn live_query_merges_no_range() {
        assert_eq!(live_query("_severity=error", &[]), "_severity=error");
        assert_eq!(
            effective_query("_severity=error", &[], &quick("15m")),
            "last=15m _severity=error",
        );
    }

    #[test]
    fn live_query_folds_filters_in_written_order() {
        assert_eq!(
            live_query(
                "_severity=error",
                &[inc("host", "web-01"), exc("source", "auth.log")],
            ),
            "host=\"web-01\" source!=\"auth.log\" _severity=error",
        );
        // …through the same escaper and the same DSL name renderer
        assert_eq!(
            live_query("*", &[inc("x-forwarded-for", "10.0.0.1")]),
            "`x-forwarded-for`=\"10.0.0.1\" *",
        );
        assert_eq!(
            live_query("*", &[inc("msg", "he said \"hi\"")]),
            "msg=\"he said \\\"hi\\\"\" *",
        );
    }

    /// A time clause the reader typed is part of the query, not the
    /// range, so it survives into the stream untouched. The ADR names
    /// all three keywords.
    #[test]
    fn a_typed_time_clause_survives_into_the_live_query() {
        for base in [
            "last=1h service=web",
            r#"earliest="2026-01-01T00:00:00Z" service=web"#,
            r#"latest="2026-01-02T00:00:00Z" service=web"#,
        ] {
            assert_eq!(live_query(base, &[]), base, "{base}");
        }
    }

    /// The stream-start effect refuses to open on an empty query, and
    /// reads this function's answer to decide.
    #[test]
    fn live_query_on_an_empty_base_is_empty() {
        assert_eq!(live_query("", &[inc("host", "web-01")]), "");
        assert_eq!(live_query("   ", &[]), "");
    }

    #[test]
    fn live_query_gives_a_pipeline_only_base_a_search_stage() {
        assert_eq!(
            live_query("| stats count() by host", &[]),
            "* | stats count() by host",
        );
        assert_eq!(
            live_query("| stats count() by host", &[inc("host", "web-01")]),
            "host=\"web-01\" | stats count() by host",
        );
    }

    fn abs(from: &str, to: &str) -> RangeSpec {
        RangeSpec::Absolute {
            from: from.into(),
            to: to.into(),
        }
    }

    fn window(parts: &CaptureParts) -> (&str, &str) {
        let w = parts.window.as_ref().expect("a window");
        (&w.earliest, &w.latest)
    }

    #[test]
    fn an_absolute_range_is_a_half_open_window_one_microsecond_past_to() {
        let parts = capture_parts(
            "_severity=error | stats count() by host",
            &[inc("host", "web-01")],
            &abs("2026-04-18T00:00:00Z", "2026-04-18T23:59:59Z"),
            None,
        )
        .expect("a capture request");
        assert_eq!(
            parts.query,
            "host=\"web-01\" _severity=error | stats count() by host"
        );
        assert_eq!(
            window(&parts),
            ("2026-04-18T00:00:00.000000Z", "2026-04-18T23:59:59.000001Z")
        );
    }

    /// The merger writes `_time<=to`, so an event exactly at `to` is in the
    /// snapshot; the window's exclusive end sits one microsecond past it.
    #[test]
    fn an_event_exactly_at_to_is_inside_the_window() {
        let parts = capture_parts(
            "*",
            &[],
            &abs("2026-04-18T00:00:00Z", "2026-04-18T00:00:00Z"),
            None,
        )
        .unwrap();
        let (earliest, latest) = window(&parts);
        let event = "2026-04-18T00:00:00.000000Z";
        assert!(earliest <= event && event < latest, "{earliest} {latest}");
        let after = "2026-04-18T00:00:00.000001Z";
        assert!(after >= latest);
    }

    #[test]
    fn offsets_are_applied_and_bounds_are_spelled_in_utc() {
        let parts = capture_parts(
            "*",
            &[],
            &abs("2026-04-18T02:00:00+02:00", "2026-04-18T03:30:00.5+02:00"),
            None,
        )
        .unwrap();
        assert_eq!(
            window(&parts),
            ("2026-04-18T00:00:00.000000Z", "2026-04-18T01:30:00.500001Z")
        );
    }

    #[test]
    fn the_text_is_range_free_and_keeps_filters_with_special_characters() {
        let parts = capture_parts(
            "service=web",
            &[
                inc("x-forwarded-for", "10.0.0.1"),
                exc("msg", "he said \"hi\" \\ | not a pipe"),
                inc("last", "5m"),
            ],
            &abs("2026-04-18T00:00:00Z", "2026-04-18T01:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(
            parts.query,
            "`x-forwarded-for`=\"10.0.0.1\" msg!=\"he said \\\"hi\\\" \\\\ | not a pipe\" \
             `last`=\"5m\" service=web"
        );
        assert_eq!(
            parts.query,
            live_query(
                "service=web",
                &[
                    inc("x-forwarded-for", "10.0.0.1"),
                    exc("msg", "he said \"hi\" \\ | not a pipe"),
                    inc("last", "5m"),
                ]
            )
        );
        // A filter on a field named like a time keyword is a filter, not
        // a time clause: the window is still sent.
        assert!(parts.window.is_some());
    }

    #[test]
    fn every_quick_range_moves() {
        for label in QUICK_RANGES {
            assert_eq!(
                capture_parts("*", &[], &quick(label), None),
                Err(CaptureRefusal::Moving),
                "{label}"
            );
        }
        assert_eq!(
            capture_parts("*", &[], &RangeSpec::default(), None),
            Err(CaptureRefusal::Moving)
        );
    }

    #[test]
    fn an_absolute_range_with_an_open_end_moves() {
        for to in ["now", ""] {
            assert_eq!(
                capture_parts("*", &[], &abs("2026-04-18T00:00:00Z", to), None),
                Err(CaptureRefusal::Moving),
                "to={to:?}"
            );
        }
    }

    const CLOSED: &str =
        r#"earliest="2026-01-01T00:00:00Z" latest="2026-01-02T00:00:00Z" service=web"#;

    /// A text closed by two absolute bounds is fixed: it bounds the
    /// capture itself, whatever the range is, and no window is sent.
    #[test]
    fn a_closed_absolute_text_clause_bounds_the_capture_and_no_window_is_sent() {
        for base in [
            CLOSED,
            r#"service=web latest="2026-01-02T00:00:00Z" earliest="2026-01-01T00:00:00Z" | stats count()"#,
            r#"service=web earliest="2026-01-01 00:00:00" latest="2026-01-02" OR host=a"#,
        ] {
            let parts = capture_parts(base, &[], &quick("15m"), None).expect(base);
            assert_eq!(parts.window, None, "{base}");
            assert_eq!(parts.query, base);
            let parts =
                capture_parts(base, &[], &abs("2026-04-18T00:00:00Z", "now"), None).expect(base);
            assert_eq!(parts.window, None, "{base}");
        }
    }

    /// A text clause that moves (`last=`, or a bound with no other end)
    /// cannot be pinned by the range control, so the request is refused
    /// whatever the range is: the person removes it and picks the range.
    #[test]
    fn a_moving_or_open_text_clause_is_refused() {
        for base in [
            "last=2h service=web",
            "service=web OR last=1h",
            "NOT last=1h service=web",
            r#"earliest="2026-01-01T00:00:00Z" service=web"#,
            r#"latest="2026-01-02T00:00:00Z" service=web"#,
            r#"service=web earliest="2026-01-01T00:00:00Z" | stats count()"#,
            r#"earliest="-1h" latest="2026-01-02T00:00:00Z" service=web"#,
            r#"earliest="2026-01-01T00:00:00Z" latest="now" service=web"#,
            r#"earliest="2026-01-01T00:00:00Z" latest="infinity" service=web"#,
        ] {
            for range in [
                quick("15m"),
                abs("2026-04-18T00:00:00Z", "now"),
                abs("2026-04-18T00:00:00Z", "2026-04-18T01:00:00Z"),
            ] {
                assert_eq!(
                    capture_parts(base, &[], &range, None),
                    Err(CaptureRefusal::TextMoves),
                    "{base} {range:?}"
                );
            }
        }
    }

    /// An absolute range beside a closed text clause is merged as the
    /// snapshot merged it, as `_time` filters, so the capture reads what
    /// the page showed; it is never also sent as a window.
    #[test]
    fn an_absolute_range_beside_a_text_clause_stays_in_the_text() {
        let parts = capture_parts(
            CLOSED,
            &[],
            &abs("2026-04-18T00:00:00Z", "2026-04-18T01:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(parts.window, None);
        assert_eq!(
            parts.query,
            format!("_time>=\"2026-04-18T00:00:00Z\" _time<=\"2026-04-18T01:00:00Z\" {CLOSED}")
        );
    }

    #[test]
    fn time_lookalikes_in_the_text_do_not_suppress_the_window() {
        for base in [
            "`earliest`=x",
            "loglatest=x",
            r#"message="earliest=x""#,
            "service=web # latest=x",
            "service=web | where last=3",
        ] {
            let parts = capture_parts(
                base,
                &[],
                &abs("2026-04-18T00:00:00Z", "2026-04-18T01:00:00Z"),
                None,
            )
            .expect(base);
            assert!(parts.window.is_some(), "{base}");
            assert_eq!(parts.query, base);
        }
    }

    /// A keyword inside a field's value is part of the value: the grammar
    /// reads no time clause there, so the text does not bound itself and
    /// the window is sent. Read as a clause, the request would carry no
    /// window and the server would apply its default trailing one.
    #[test]
    fn a_keyword_inside_a_value_is_not_a_time_clause() {
        for base in ["url=https://a/last=1h", "message=last=1h", "x=earliest=y"] {
            let parts = capture_parts(
                base,
                &[],
                &abs("2026-09-15T12:00:00Z", "2026-09-15T12:30:00Z"),
                None,
            )
            .expect(base);
            assert_eq!(
                parts.window,
                Some(CaptureWindow {
                    earliest: "2026-09-15T12:00:00.000000Z".into(),
                    latest: "2026-09-15T12:30:00.000001Z".into(),
                }),
                "{base}"
            );
            assert_eq!(parts.query, base);
            // The snapshot merge asks the same question.
            assert_eq!(
                effective_query(base, &[], &quick("15m")),
                format!("last=15m {base}")
            );
        }
    }

    /// The property the server enforces: a request never carries a window
    /// and a time clause in the text. Checked over a grid of texts and
    /// ranges, with the text parsed by the grammar itself.
    #[test]
    fn a_capture_never_applies_time_twice() {
        let texts = [
            "service=web",
            "last=1h service=web",
            r#"earliest="2026-01-01T00:00:00Z" service=web"#,
            CLOSED,
            "| stats count() by host",
            "service=web | stats count() by host",
        ];
        let ranges = [
            quick("15m"),
            abs("2026-04-18T00:00:00Z", "2026-04-18T01:00:00Z"),
            abs("2026-04-18T00:00:00Z", "now"),
        ];
        for text in texts {
            for range in &ranges {
                let Ok(parts) = capture_parts(text, &[inc("host", "a")], range, None) else {
                    continue;
                };
                let ast = trawl_core::parser::parse(&parts.query).expect("the text parses");
                let has_clause = ast.search.time_filter.is_some()
                    || ast.search.earliest.is_some()
                    || ast.search.latest.is_some();
                if parts.window.is_some() {
                    assert!(!has_clause, "{text} {range:?}: {}", parts.query);
                    assert!(!parts.query.contains("_time"), "{}", parts.query);
                }
            }
        }
    }

    #[test]
    fn an_empty_query_is_refused_before_the_range_is_read() {
        let range = abs("2026-04-18T00:00:00Z", "2026-04-18T01:00:00Z");
        assert_eq!(
            capture_parts("", &[inc("host", "a")], &range, None),
            Err(CaptureRefusal::EmptyQuery)
        );
        assert_eq!(
            capture_parts("  \n ", &[], &quick("15m"), None),
            Err(CaptureRefusal::EmptyQuery)
        );
    }

    #[test]
    fn unreadable_ranges_are_refused() {
        for (from, to) in [
            ("yesterday", "2026-04-18T01:00:00Z"),
            ("2026-04-18T00:00:00Z", "tomorrow"),
            ("", "2026-04-18T01:00:00Z"),
            ("2026-04-18T02:00:00Z", "2026-04-18T01:00:00Z"),
        ] {
            assert_eq!(
                capture_parts("*", &[], &abs(from, to), None),
                Err(CaptureRefusal::UnreadableRange),
                "{from:?}..{to:?}"
            );
        }
    }

    #[test]
    fn a_window_end_past_the_wire_form_is_an_overflow_not_a_panic() {
        assert_eq!(
            capture_parts(
                "*",
                &[],
                &abs("9999-12-31T00:00:00Z", "9999-12-31T23:59:59.999999Z"),
                None
            ),
            Err(CaptureRefusal::RangeOverflow)
        );
        let parts = capture_parts(
            "*",
            &[],
            &abs("9999-12-31T00:00:00Z", "9999-12-31T23:59:59.999998Z"),
            None,
        )
        .unwrap();
        assert_eq!(window(&parts).1, "9999-12-31T23:59:59.999999Z");
    }

    const WEEK: Option<u64> = Some(604_800);

    /// The 7-day preset, pinned, covers seven days and one microsecond
    /// once the inclusive end becomes the exclusive one, and the server
    /// refuses a span past its limit. The interval is refused, never
    /// narrowed to fit.
    #[test]
    fn a_window_past_the_capture_span_limit_is_refused() {
        let refused = Err(CaptureRefusal::SpanTooLong {
            max_span_secs: 604_800,
        });
        for anchor in ["2026-10-26T12:00:00Z", "2026-10-26T12:00:00.500000Z"] {
            let pinned = pin_range(&quick("7d"), anchor).unwrap();
            assert_eq!(capture_parts("*", &[], &pinned, WEEK), refused, "{anchor}");
            // An unknown limit refuses nothing: the server decides.
            assert!(capture_parts("*", &[], &pinned, None).is_ok());
        }
        // A fixed range is held to the same limit.
        assert_eq!(
            capture_parts(
                "*",
                &[],
                &abs("2026-10-19T12:00:00Z", "2026-10-26T12:00:00Z"),
                WEEK
            ),
            refused
        );
        // Exactly the limit fits: the window ends one microsecond past `to`.
        let parts = capture_parts(
            "*",
            &[],
            &abs("2026-10-19T12:00:00Z", "2026-10-26T11:59:59.999999Z"),
            WEEK,
        )
        .unwrap();
        assert_eq!(
            window(&parts),
            ("2026-10-19T12:00:00.000000Z", "2026-10-26T12:00:00.000000Z")
        );
    }

    /// A text closed by its own bounds is held to the limit too: the
    /// capture covers `[earliest, latest)`.
    #[test]
    fn a_closed_text_clause_past_the_capture_span_limit_is_refused() {
        let at_limit =
            r#"earliest="2026-10-19T12:00:00Z" latest="2026-10-26T12:00:00Z" service=web"#;
        assert!(capture_parts(at_limit, &[], &quick("15m"), WEEK).is_ok());
        let over =
            r#"earliest="2026-10-19T12:00:00Z" latest="2026-10-26T12:00:00.000001Z" service=web"#;
        assert_eq!(
            capture_parts(over, &[], &quick("15m"), WEEK),
            Err(CaptureRefusal::SpanTooLong {
                max_span_secs: 604_800
            })
        );
    }

    #[test]
    fn every_quick_label_pins_to_anchor_minus_its_duration() {
        let anchor = "2026-04-18T12:00:00Z";
        for (label, from) in [
            ("5m", "2026-04-18T11:55:00Z"),
            ("15m", "2026-04-18T11:45:00Z"),
            ("1h", "2026-04-18T11:00:00Z"),
            ("4h", "2026-04-18T08:00:00Z"),
            ("24h", "2026-04-17T12:00:00Z"),
            ("7d", "2026-04-11T12:00:00Z"),
        ] {
            assert_eq!(
                pin_range(&quick(label), anchor),
                Ok(abs(from, anchor)),
                "{label}"
            );
        }
    }

    #[test]
    fn a_pinned_range_is_the_whole_second_hull_of_the_exact_one() {
        assert_eq!(
            pin_range(&quick("15m"), "2026-04-18T12:00:00.750000Z"),
            Ok(abs("2026-04-18T11:45:00Z", "2026-04-18T12:00:01Z"))
        );
        assert_eq!(
            pin_range(&quick("1h"), "2026-04-18T14:00:00.250000+02:00"),
            Ok(abs("2026-04-18T11:00:00Z", "2026-04-18T12:00:01Z"))
        );
    }

    #[test]
    fn an_open_end_pins_to_the_anchor_and_a_closed_range_is_left_alone() {
        let anchor = "2026-04-18T12:00:00Z";
        assert_eq!(
            pin_range(&abs("2026-04-18T08:00:00Z", "now"), anchor),
            Ok(abs("2026-04-18T08:00:00Z", anchor))
        );
        assert_eq!(
            pin_range(&abs("2026-04-18T08:00:00Z", ""), anchor),
            Ok(abs("2026-04-18T08:00:00Z", anchor))
        );
        let closed = abs("2026-04-18T08:00:00Z", "2026-04-18T09:00:00Z");
        assert_eq!(pin_range(&closed, anchor), Ok(closed.clone()));
        // Pinning twice changes nothing.
        let pinned = pin_range(&quick("15m"), anchor).unwrap();
        assert_eq!(pin_range(&pinned, "2030-01-01T00:00:00Z"), Ok(pinned));
    }

    #[test]
    fn a_range_that_cannot_be_pinned_is_refused() {
        let anchor = "2026-04-18T12:00:00Z";
        assert_eq!(
            pin_range(&quick("15m"), "not a time"),
            Err(PinRefusal::UnreadableAnchor)
        );
        assert_eq!(
            pin_range(&quick("fortnight"), anchor),
            Err(PinRefusal::UnreadableRange)
        );
        assert_eq!(
            pin_range(&quick("9223372036854775807d"), anchor),
            Err(PinRefusal::UnreadableRange)
        );
        assert_eq!(
            pin_range(&abs("", "now"), anchor),
            Err(PinRefusal::UnreadableRange)
        );
        assert_eq!(
            pin_range(&abs("2026-04-19T00:00:00Z", "now"), anchor),
            Err(PinRefusal::UnreadableRange)
        );
    }

    const OR_BASE: &str = "service=web OR service=db";
    const F: &str = "2026-09-15T12:00:00Z";
    const T: &str = "2026-09-15T12:30:00Z";

    /// The search stage's groups as the grammar reads `text`: a merged
    /// filter binds to the whole search only when there is one group.
    fn top_level_groups(text: &str) -> usize {
        trawl_core::parser::parse(text)
            .unwrap_or_else(|e| panic!("{text:?} parses: {e:?}"))
            .search
            .groups
            .len()
    }

    /// `a OR b` is two groups, and a token written before it joins the
    /// first one only. A chip's filter narrows the whole search, so an OR
    /// search goes into parentheses before the filter is put in front.
    #[test]
    fn a_chip_filter_narrows_every_or_group() {
        let merged = live_query(OR_BASE, &[inc("host", "a")]);
        assert_eq!(merged, "host=\"a\" (service=web OR service=db)");
        assert_eq!(top_level_groups(&merged), 1, "{merged}");
    }

    #[test]
    fn an_absolute_range_bounds_every_or_group() {
        let merged = effective_query(OR_BASE, &[], &abs(F, T));
        assert_eq!(
            merged,
            format!("_time>=\"{F}\" _time<=\"{T}\" (service=web OR service=db)")
        );
        assert_eq!(top_level_groups(&merged), 1, "{merged}");
    }

    #[test]
    fn chips_and_a_range_bound_every_or_group_and_the_pipeline_follows() {
        let merged = effective_query(
            &format!("{OR_BASE} | stats count() by service"),
            &[inc("host", "a"), exc("source", "auth.log")],
            &abs(F, T),
        );
        assert_eq!(
            merged,
            format!(
                "host=\"a\" source!=\"auth.log\" _time>=\"{F}\" _time<=\"{T}\" \
                 (service=web OR service=db) | stats count() by service"
            )
        );
        assert_eq!(top_level_groups(&merged), 1, "{merged}");
    }

    /// The capture's range-free text is grouped the same way, so the
    /// window and the chips apply to every group, as in Search.
    #[test]
    fn the_capture_text_groups_an_or_search_as_search_does() {
        let parts = capture_parts(OR_BASE, &[inc("host", "a")], &abs(F, T), None).unwrap();
        assert_eq!(parts.query, "host=\"a\" (service=web OR service=db)");
        assert_eq!(parts.query, live_query(OR_BASE, &[inc("host", "a")]));
        assert!(parts.window.is_some());
    }

    /// Parentheses already in the text are kept inside the new group; a
    /// search that is one group already, parenthesised or not, is left as
    /// it was.
    #[test]
    fn written_parentheses_are_kept_and_one_group_is_not_wrapped() {
        assert_eq!(
            live_query("(a OR b) c OR d", &[inc("host", "x")]),
            "host=\"x\" ((a OR b) c OR d)"
        );
        assert_eq!(
            live_query("(a OR b) c", &[inc("host", "x")]),
            "host=\"x\" (a OR b) c"
        );
        assert_eq!(
            live_query("NOT (a OR b)", &[inc("host", "x")]),
            "host=\"x\" NOT (a OR b)"
        );
    }

    #[test]
    fn a_not_beside_an_or_is_grouped_with_it() {
        let merged = live_query("NOT service=web OR service=db", &[inc("host", "a")]);
        assert_eq!(merged, "host=\"a\" (NOT service=web OR service=db)");
        assert_eq!(top_level_groups(&merged), 1, "{merged}");
    }

    /// The grammar decides what is an OR: inside a quoted value, a
    /// backticked name, a regex or a comment it is data, and a lower-case
    /// `or` is the keyword.
    #[test]
    fn only_the_grammars_or_groups_the_search() {
        for base in [
            r#"msg="a OR b""#,
            "`OR`=x",
            "msg=/a OR b/",
            "service=web # a OR b",
            "service=web OR",
            "error ORACLE",
        ] {
            assert_eq!(
                live_query(base, &[inc("host", "a")]),
                format!("host=\"a\" {base}"),
                "{base}"
            );
        }
        assert_eq!(
            live_query("service=web or service=db", &[inc("host", "a")]),
            "host=\"a\" (service=web or service=db)"
        );
        // A stray leading OR still splits a prefixed filter off.
        assert_eq!(
            live_query("OR service=web", &[inc("host", "a")]),
            "host=\"a\" (OR service=web)"
        );
    }

    /// Without a top-level OR the merged text is the prefix, a space and
    /// the search, byte for byte.
    #[test]
    fn a_search_without_or_merges_byte_for_byte_as_before() {
        for base in [
            "service=web",
            "_severity=error | stats count() by host",
            r#"msg="a OR b" host=x"#,
            "(a OR b)",
            "service=x # note\n| stats count()",
            "url=https://a/b",
        ] {
            let (search, tail) = split_search_stage(base);
            let search = search.trim();
            let mut expected = format!("host=\"a\" _time>=\"{F}\" _time<=\"{T}\" {search}");
            if let Some(tail) = tail {
                let sep = if ends_inside_comment(&expected) {
                    "\n"
                } else {
                    " "
                };
                write!(expected, "{sep}| {}", tail.trim_start_matches('|').trim()).ok();
            }
            assert_eq!(
                effective_query(base, &[inc("host", "a")], &abs(F, T)),
                expected,
                "{base}"
            );
        }
    }

    /// A time clause applies to the whole query wherever it is written,
    /// so a quick range is not a reason to group, and a clause the person
    /// wrote inside an OR is still read as theirs.
    #[test]
    fn time_clauses_read_the_same_once_grouped() {
        // the quick range alone binds to no group: nothing is wrapped
        assert_eq!(
            effective_query(OR_BASE, &[], &quick("15m")),
            "last=15m service=web OR service=db"
        );
        // with a chip it is grouped, and `last=` is still the range
        let merged = effective_query(OR_BASE, &[inc("host", "a")], &quick("15m"));
        assert_eq!(merged, "host=\"a\" last=15m (service=web OR service=db)");
        let ast = trawl_core::parser::parse(&merged).unwrap();
        assert!(ast.search.time_filter.is_some());
        assert_eq!(ast.search.groups.len(), 1);
        // an own clause under OR still suppresses the quick range and is
        // still hoisted, and a capture still refuses it; the group it
        // leaves empty is dropped, so one group remains and none is added
        let base = "service=web OR last=1h";
        let merged = effective_query(base, &[inc("host", "a")], &quick("15m"));
        assert_eq!(merged, "host=\"a\" service=web OR last=1h");
        let ast = trawl_core::parser::parse(&merged).unwrap();
        assert!(ast.search.time_filter.is_some());
        assert_eq!(ast.search.groups.len(), 1);
        assert_eq!(
            capture_parts(base, &[inc("host", "a")], &abs(F, T), None),
            Err(CaptureRefusal::TextMoves)
        );
        // a closed text clause under OR still closes the capture
        let closed = r#"service=web earliest="2026-01-01T00:00:00Z" latest="2026-01-02T00:00:00Z" OR host=b"#;
        let parts = capture_parts(closed, &[inc("host", "a")], &abs(F, T), None).unwrap();
        assert_eq!(parts.window, None);
        assert_eq!(
            parts.query,
            format!("host=\"a\" _time>=\"{F}\" _time<=\"{T}\" ({closed})")
        );
    }

    /// The closing parenthesis must not change the last token: a regex
    /// needs whitespace after it, and a comment would swallow it.
    #[test]
    fn the_closing_parenthesis_keeps_the_last_token_whole() {
        let merged = live_query("service=web OR msg=/x+/", &[inc("host", "a")]);
        assert_eq!(merged, "host=\"a\" (service=web OR msg=/x+/ )");
        let ast = trawl_core::parser::parse(&merged).unwrap();
        let [group] = ast.search.groups.as_slice() else {
            panic!("one group: {merged}");
        };
        let trawl_core::ast::SearchToken::Group(inner) = &group[1].node else {
            panic!("the search is a parenthesised group: {merged}");
        };
        let trawl_core::ast::SearchToken::FieldFilter(filter) = &inner[1][0].node else {
            panic!("the second group is the msg filter: {merged}");
        };
        assert_eq!(filter.op, trawl_core::ast::FilterOp::Regex, "{merged}");

        let merged = live_query(
            "service=web OR service=db # note\n| stats count()",
            &[inc("host", "a")],
        );
        assert_eq!(
            merged,
            "host=\"a\" (service=web OR service=db # note\n) | stats count()"
        );
        let ast = trawl_core::parser::parse(&merged).unwrap();
        assert_eq!(ast.search.groups.len(), 1);
        assert_eq!(ast.pipeline.len(), 1, "{merged}");
    }

    /// The pinned range goes through the URL (`encode_range` then
    /// `decode_range`), and the capture it yields holds every event the
    /// moving search saw: those in `[anchor − d, anchor)`.
    #[test]
    fn a_pinned_range_survives_the_url_and_covers_what_ran() {
        use crate::search_url::{Verdict, decode_range, encode_range};
        let anchor = "2026-04-18T12:00:00.500000Z";
        let pinned = pin_range(&quick("15m"), anchor).unwrap();
        let Verdict::Valid(read) = decode_range(&encode_range(&pinned)) else {
            panic!("the pinned range did not read back");
        };
        assert_eq!(read, pinned);
        let parts = capture_parts("*", &[], &read, None).unwrap();
        let (earliest, latest) = window(&parts);
        for event in ["2026-04-18T11:45:00.500000Z", "2026-04-18T12:00:00.499999Z"] {
            assert!(earliest <= event && event < latest, "{event}");
        }
    }
}
