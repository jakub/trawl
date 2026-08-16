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
/// `last=` clause (user intent wins).
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

impl RangeSpec {
    /// Compact label for the status bar / date-range pill.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Quick(q) => (*q).to_string(),
            Self::Absolute { from, to } => format!("{from} → {to}"),
        }
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
/// - if the base query already carries `last=X`, the range's `last=` is
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
    let has_last = search_has_last_clause(search);

    let mut prefix = String::new();
    for f in filters {
        let clause = format_filter(f);
        if !prefix.is_empty() {
            prefix.push(' ');
        }
        prefix.push_str(&clause);
    }

    let range_clause = match range {
        RangeSpec::Quick(q) if !has_last => Some(format!("last={q}")),
        RangeSpec::Absolute { from, to } => Some(format_absolute_range(from, to)),
        RangeSpec::Quick(_) => None, // suppressed by existing `last=`
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
        Some(t) => format!("{new_search} | {}", t.trim_start_matches('|').trim()),
        None => new_search,
    }
}

/// Split on the first top-level `|` (skipping slashes-delimited regex
/// literals and quoted strings). Returns `(search_stage, optional_tail)`.
fn split_search_stage(input: &str) -> (&str, Option<&str>) {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_double_quote = false;
    let mut in_single_quote = false;
    let mut in_regex = false;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'"' if !in_single_quote && !in_regex => {
                in_double_quote = !in_double_quote;
                i += 1;
            }
            b'\'' if !in_double_quote && !in_regex => {
                in_single_quote = !in_single_quote;
                i += 1;
            }
            b'/' if !in_double_quote && !in_single_quote => {
                in_regex = !in_regex;
                i += 1;
            }
            b'|' if !in_double_quote && !in_single_quote && !in_regex => {
                return (&input[..i], Some(&input[i + 1..]));
            }
            _ => i += 1,
        }
    }
    (input, None)
}

/// Heuristic: does the search stage already carry a `last=<units>` clause?
/// Word-boundary check to avoid matching `loglast=` or similar. Case-insensitive.
fn search_has_last_clause(search: &str) -> bool {
    let lower = search.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let needle = b"last=";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if bytes[i..i + needle.len()] == *needle {
            let prev_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            if prev_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
}

fn format_filter(f: &Filter) -> String {
    let op = match f.op {
        FilterOp::Include => "=",
        FilterOp::Exclude => "!=",
    };
    // Quote the value so spaces / special chars don't break the DSL.
    let quoted = format!("\"{}\"", f.value.replace('\\', "\\\\").replace('"', "\\\""));
    // The FIELD is quoted by its own rule (`quote_dsl_name`) — a facet
    // name carrying a space is a valid column and must produce a valid
    // clause. A name the grammar cannot express falls back to its raw
    // spelling: the clause is then no worse than it was, and the caller
    // sees a parse error rather than a silently different query.
    let field = trawl_core::parser::quote_dsl_name(&f.field).unwrap_or_else(|| f.field.clone());
    format!("{field}{op}{quoted}")
}

fn format_absolute_range(from: &str, to: &str) -> String {
    // _time>="<from>" _time<="<to>" — DSL parses these as implicit-AND
    // comparison clauses in the search stage. The canonical column by
    // name: the DSL has zero aliases since ADR-0013 §6, so `@timestamp`
    // would name an ordinary sender field most events never carry.
    let mut out = String::new();
    if !from.is_empty() {
        write!(&mut out, "_time>=\"{from}\"").ok();
    }
    if !to.is_empty() && to != "now" {
        if !out.is_empty() {
            out.push(' ');
        }
        write!(&mut out, "_time<=\"{to}\"").ok();
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

    /// A facet name carrying a space is a valid column, so the clause it
    /// synthesises has to be valid DSL — through `trawl_core`'s one
    /// renderer, never a hand-rolled wrap here.
    #[test]
    fn a_facet_field_that_needs_quoting_gets_it() {
        assert_eq!(format_filter(&inc("request id", "7")), "`request id`=\"7\"");
        assert_eq!(format_filter(&inc("host", "web-01")), "host=\"web-01\"");
        // …and the rendered clause parses
        assert!(trawl_core::parser::parse(&format_filter(&exc("request id", "7"))).is_ok());
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
    fn existing_last_suppresses_range() {
        let q = effective_query("last=2h _severity=error", &[], &quick("15m"));
        assert_eq!(q, "last=2h _severity=error");
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
    fn range_label_renders_sensibly() {
        assert_eq!(RangeSpec::Quick("1h").label(), "1h");
        assert_eq!(
            RangeSpec::Absolute {
                from: "a".into(),
                to: "b".into()
            }
            .label(),
            "a → b"
        );
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
