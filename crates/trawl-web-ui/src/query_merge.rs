// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-Rust merge logic for `(base_q, filters, range)` → effective DSL.
//!
//! Kept ungated (no leptos / js-sys / wasm) so it compiles on native and
//! its tests run under plain `cargo test` / `cargo nextest`. The wasm-only
//! `state::query` module re-exports the types from here and layers the
//! URL encoding + signal plumbing on top.
//!
//! On native, only the tests consume these items — `#[allow(dead_code)]`
//! at module scope silences the bin-crate dead-code warning. Matches
//! `facets.rs` / `histogram.rs` / `offset.rs` pattern.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::fmt::Write;

use trawl_core::parser::scan::{ends_inside_comment, scan_outside_quotes};
use trawl_core::parser::suggest::quote_dsl_field;

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

/// Range window driven by the date-range popover. Merged into the wire
/// query at request time unless the user's base query already carries a
/// `last=`, `earliest=`, or `latest=` clause (user intent wins).
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

/// Merge `(base_q, filters, range)` into the DSL string that actually runs.
///
/// Rules:
/// - empty `base_q` → empty result (filters/range no-op without a query to
///   attach to; avoids fabricating a wildcard `*` that could flood the
///   server).
/// - filter clauses and the range clause prepend to the first search stage.
/// - if the base query already carries a time clause, the range's `last=` is
///   suppressed (user intent wins). Absolute ranges always inject
///   `_time>="..." _time<="..."` regardless.
/// - pipeline-only base (`| stats ...`) gets a synthetic `*` search stage
///   preceding the filter + range clauses.
#[must_use]
pub fn effective_query(base_q: &str, filters: &[Filter], range: &RangeSpec) -> String {
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

    let range_clause = match range {
        RangeSpec::Quick(q) if !has_time => Some(format!("last={q}")),
        RangeSpec::Absolute { from, to } => Some(format_absolute_range(from, to)),
        RangeSpec::Quick(_) => None, // suppressed by an existing time clause
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

/// The time restriction the effective query actually runs under.
///
/// The picker's trigger keeps saying what the URL carries (`r=15m`) even
/// when the DSL runs two hours; this is the truth the histogram caption
/// states (ADR-0027, amended 2026-09-12). It describes what
/// [`effective_query`] merged — it never changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveWindow {
    /// The user's own clauses won; the quick range was suppressed.
    Dsl(Vec<TimeClause>),
    /// The picker's quick range ran, with no DSL clause beside it.
    Quick(String),
    /// An absolute range always applies, AND-ed with any DSL clauses the
    /// search stage carries.
    Absolute {
        from: String,
        to: String,
        dsl: Vec<TimeClause>,
    },
}

/// Every DSL time clause in a search stage, in written order.
///
/// Word-boundary check to avoid matching `loglast=` or similar.
/// Case-insensitive. A backticked `` `last` `` is a field, not the
/// grammar keyword, and a `last=` inside a quoted value or a comment is
/// prose, so the walk skips those spans entirely — otherwise the UI
/// would read them as an existing time clause and silently drop the
/// range. The closure never claims a hit, so one walk collects them all.
#[must_use]
pub fn find_time_clauses(search: &str) -> Vec<TimeClause> {
    const KEYWORDS: [&str; 3] = ["last", "earliest", "latest"];
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

/// Which time restriction `(base_q, range)` actually runs under.
///
/// Mirrors [`effective_query`]'s merge exactly: DSL clauses suppress a
/// quick range, and an absolute range always applies, carrying any DSL
/// clauses beside it because both restrictions are AND-ed in the search
/// stage.
#[must_use]
pub fn effective_window(base_q: &str, range: &RangeSpec) -> EffectiveWindow {
    let (search_raw, _) = split_search_stage(base_q.trim());
    let dsl = find_time_clauses(search_raw.trim());
    match range {
        RangeSpec::Absolute { from, to } => EffectiveWindow::Absolute {
            from: from.clone(),
            to: to.clone(),
            dsl,
        },
        RangeSpec::Quick(q) if dsl.is_empty() => EffectiveWindow::Quick((*q).to_string()),
        RangeSpec::Quick(_) => EffectiveWindow::Dsl(dsl),
    }
}

/// The window as one readable phrase: `last 24h`, `earliest 2026-01-01
/// and latest 2026-01-02`, or `<from> to <to>` with ` and <clauses>`
/// appended when the DSL restricts it further.
#[must_use]
pub fn window_caption(window: &EffectiveWindow) -> String {
    match window {
        EffectiveWindow::Dsl(clauses) => clause_phrase(clauses),
        EffectiveWindow::Quick(q) => format!("last {q}"),
        EffectiveWindow::Absolute { from, to, dsl } => {
            let mut out = format!("{from} to {to}");
            if !dsl.is_empty() {
                out.push_str(" and ");
                out.push_str(&clause_phrase(dsl));
            }
            out
        }
    }
}

fn clause_phrase(clauses: &[TimeClause]) -> String {
    clauses
        .iter()
        .map(|c| format!("{} {}", c.keyword, c.value))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Does the search stage already carry a DSL time clause?
fn search_has_time_clause(search: &str) -> bool {
    !find_time_clauses(search).is_empty()
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

    #[test]
    fn existing_last_case_insensitive() {
        let q = effective_query("LAST=2h _severity=error", &[], &quick("15m"));
        assert_eq!(q, "LAST=2h _severity=error");
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

    fn clause(keyword: &'static str, value: &str) -> TimeClause {
        TimeClause {
            keyword,
            value: value.to_owned(),
        }
    }

    #[test]
    fn a_dsl_last_clause_wins_over_the_quick_range() {
        let w = effective_window("service=x last=2h", &quick("15m"));
        assert_eq!(w, EffectiveWindow::Dsl(vec![clause("last", "2h")]));
        assert_eq!(window_caption(&w), "last 2h");
    }

    #[test]
    fn earliest_and_latest_are_both_read_in_written_order() {
        let w = effective_window(
            r#"earliest="2026-01-01T00:00:00Z" service=web latest="2026-01-02T00:00:00Z""#,
            &quick("15m"),
        );
        assert_eq!(
            w,
            EffectiveWindow::Dsl(vec![
                clause("earliest", "2026-01-01T00:00:00Z"),
                clause("latest", "2026-01-02T00:00:00Z"),
            ])
        );
        assert_eq!(
            window_caption(&w),
            "earliest 2026-01-01T00:00:00Z and latest 2026-01-02T00:00:00Z"
        );
    }

    #[test]
    fn a_quoted_or_backticked_lookalike_falls_through_to_the_range() {
        for base in [
            "`last=2h`=x",
            r#"message="last=2h""#,
            "loglast=2h",
            "service=web # last=2h",
        ] {
            let w = effective_window(base, &quick("15m"));
            assert_eq!(w, EffectiveWindow::Quick("15m".into()), "{base}");
            assert_eq!(window_caption(&w), "last 15m", "{base}");
        }
    }

    #[test]
    fn an_absolute_range_names_itself_and_any_dsl_clause_beside_it() {
        let range = RangeSpec::Absolute {
            from: "2026-09-05T06:00:00Z".into(),
            to: "now".into(),
        };
        let w = effective_window("service=x", &range);
        assert_eq!(
            w,
            EffectiveWindow::Absolute {
                from: "2026-09-05T06:00:00Z".into(),
                to: "now".into(),
                dsl: vec![],
            }
        );
        assert_eq!(window_caption(&w), "2026-09-05T06:00:00Z to now");

        let both = effective_window("service=x last=2h", &range);
        assert_eq!(
            window_caption(&both),
            "2026-09-05T06:00:00Z to now and last 2h"
        );
    }

    #[test]
    fn a_time_clause_in_the_pipeline_is_not_the_search_stage_window() {
        // `split_search_stage` bounds the read the same way the merge does.
        let w = effective_window("service=x | stats count() by last=2h", &quick("1h"));
        assert_eq!(w, EffectiveWindow::Quick("1h".into()));
    }

    /// The caption describes what ran. For every base query where
    /// `effective_query` suppresses the quick range, `effective_window`
    /// must say `Dsl` — and where it injects one, `Quick`.
    #[test]
    fn the_window_agrees_with_the_merge_on_every_suppression() {
        for base in [
            "service=x last=2h",
            "LAST=2h service=x",
            r#"earliest="2026-01-01T00:00:00Z" service=web"#,
            r#"latest="2026-01-02T00:00:00Z" service=web"#,
            "url=https://a/b last=1h",
            "message=\"x\" last=1h",
            "service=x",
            "`last`=5",
            "loglast=2h",
            "service=web # latest=x",
            r#"message="earliest=x""#,
            "| stats count() by host",
        ] {
            let range = quick("15m");
            let merged = effective_query(base, &[], &range);
            let suppressed = !merged.contains("last=15m");
            let window = effective_window(base, &range);
            assert_eq!(
                suppressed,
                matches!(window, EffectiveWindow::Dsl(_)),
                "{base}: merged={merged}, window={window:?}"
            );
        }
        // An empty base runs nothing at all rather than suppressing
        // anything, so it is outside the table: there is no page to
        // caption, and the window still reads as the picker's range.
        assert_eq!(effective_query("", &[], &quick("15m")), "");
        assert_eq!(
            effective_window("", &quick("15m")),
            EffectiveWindow::Quick("15m".into())
        );
    }

    #[test]
    fn a_quoted_clause_value_is_unescaped_once() {
        let clauses = find_time_clauses(r#"earliest="2026-01-01T00:00:00Z\" x" service=web"#);
        assert_eq!(clauses.len(), 1);
        assert_eq!(clauses[0].value, r#"2026-01-01T00:00:00Z" x"#);
    }
}
