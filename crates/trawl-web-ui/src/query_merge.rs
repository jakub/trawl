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

/// Heuristic: does the search stage already carry a DSL time clause?
/// Word-boundary check to avoid matching `loglast=` or similar. Case-insensitive.
///
/// A backticked `` `last` `` is a field, not the grammar keyword, so the
/// walk skips quoted spans entirely — otherwise the UI would read the
/// field as an existing time clause and silently drop the range.
fn search_has_time_clause(search: &str) -> bool {
    let lower = search.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    scan_outside_quotes(&lower, |i, _| {
        ["last=", "earliest=", "latest="]
            .iter()
            .any(|needle| bytes[i..].starts_with(needle.as_bytes()))
            && (i == 0 || !is_ident_byte(bytes[i - 1]))
    })
    .is_some()
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
}
