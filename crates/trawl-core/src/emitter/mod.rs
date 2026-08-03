// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SQL emitter for `DuckDB`.
//!
//! Walks the AST and produces parameterized `DuckDB` SQL.
//! Uses CTEs to handle multi-stage pipeline queries.

mod expr;
mod fields;
mod functions;
mod pipeline;
mod search;
pub(crate) mod severity;
mod state;
mod validate;

use crate::ast::{PipeStage, Query, Spanned};
use state::EmitterState;

pub use fields::map_field_name;
pub use functions::is_aggregate_function;
pub use functions::{DATE_PART_UNITS, DATE_UNITS};
pub(crate) use functions::{
    format_literal_position, unit_literal_positions, validate_format_literal, validate_unit_literal,
};
pub use state::{hot_source_reader, source_reader, validate_source_path};
pub(crate) use validate::validate_level_references;
pub use validate::validate_pipeline;

use std::fmt;

/// The result of emitting SQL from a parsed query.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedQuery {
    /// The parameterized SQL string (placeholders are `?`).
    pub sql: String,
    /// Ordered parameter values corresponding to each `?` placeholder.
    pub params: Vec<SqlValue>,
    /// Pipe stages the executor must apply in Rust after SQL execution.
    ///
    /// Non-empty when the pipeline contains operations that can't be
    /// expressed as SQL (e.g. kv extraction with dynamic columns).
    /// The executor runs the SQL prefix, then applies these stages
    /// to the result set using the streaming engine.
    pub rust_stages: Vec<Spanned<PipeStage>>,
    /// Whether the executor should reorder result columns to put well-known
    /// fields first. `true` when the pipeline has no explicit column selection
    /// or aggregation — i.e. the column set comes from `SELECT *`.
    pub needs_column_reorder: bool,
    /// The same query with text search's `_raw` side bound to a typed NULL
    /// instead of the column — `Some` only when a text search referenced
    /// `_raw`.
    ///
    /// `_raw` is a server guarantee, not a guarantee of every source a query
    /// can be pointed at: user-owned parquet read in embedded mode has no
    /// such column, and binding it there would fail the whole query instead
    /// of searching `message` alone (ADR-0009: bare search covers `_raw`
    /// *where present*). [`params`](Self::params) applies unchanged — the
    /// raw-free pass pushes the same parameters in the same order — so the
    /// executor can retry with this SQL against a source that has no `_raw`.
    pub raw_free_sql: Option<String>,
}

/// A parameter value for a SQL query placeholder.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl fmt::Display for SqlValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "'{s}'"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Bool(b) => write!(f, "{b}"),
        }
    }
}

/// Errors that can occur during SQL emission.
#[derive(Debug, Clone, PartialEq)]
pub enum EmitError {
    UnknownFunction {
        name: String,
        suggestion: Option<String>,
    },
    InvalidAggregation {
        message: String,
    },
    UnsupportedOperation {
        message: String,
    },
    /// A `strftime`/`strptime` format-string literal contains an invalid code.
    InvalidFormat {
        func_name: String,
        format: String,
    },
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFunction { name, .. } => {
                write!(f, "unknown function: {name}")?;
                if let Some(hint) = self.hint() {
                    write!(f, " ({hint})")?;
                }
                Ok(())
            }
            Self::InvalidAggregation { message } => write!(f, "invalid aggregation: {message}"),
            Self::UnsupportedOperation { message } => {
                write!(f, "unsupported operation: {message}")
            }
            Self::InvalidFormat { func_name, format } => {
                write!(f, "{func_name}(): invalid format string {format:?}")
            }
        }
    }
}

impl std::error::Error for EmitError {}

impl EmitError {
    /// Produce a user-facing hint string, if applicable.
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::UnknownFunction {
                suggestion: Some(s),
                ..
            } => Some(format!("did you mean '{s}'?")),
            _ => None,
        }
    }

    /// Convert this emitter error into parse errors with the given query span.
    ///
    /// Used by validation endpoints that need to return `Vec<ParseError>`
    /// from an `EmitError` (server validate handler, TUI real-time validation).
    pub fn to_parse_errors(&self, query_len: usize) -> Vec<crate::parser::ParseError> {
        vec![crate::parser::ParseError {
            message: self.to_string(),
            span: 0..query_len,
            label: None,
            hint: self.hint(),
        }]
    }
}

/// Emit parameterized `DuckDB` SQL from a parsed query.
///
/// `source` is the parquet glob path, e.g. `"/data/**/*.parquet"`.
pub fn emit(query: &Query, source: &str) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || EmitterState::new(source))
}

/// Emit SQL that unions the primary parquet source with a hot buffer ndjson file.
///
/// Produces a `UNION ALL BY NAME` composite source so that fresh events
/// in the hot buffer are visible alongside compacted parquet data. `pins`
/// is the field catalog's pinned types intersected with the snapshot's
/// observed keys: each pinned field is conformed on the HOT branch only
/// (`TRY_CAST` for typed pins, the untyped json path for VARCHAR), so a
/// hot value disagreeing with the write-time pin degrades to NULL instead
/// of throwing the union. Empty `pins` (embedded mode, catalog-less
/// buffer) leaves the union plain apart from the unconditional envelope
/// timestamp `TRY_CAST`s (ADR-0008).
pub fn emit_with_hot_source(
    query: &Query,
    source: &str,
    hot_source: &str,
    pins: &crate::schema::FieldTypes,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        EmitterState::with_hot_source(source, hot_source, pins)
    })
}

/// Emit a hot+cold union query with `varchar_cols` coerced to VARCHAR on
/// both sides.
///
/// The executor calls this to retry a query whose hot+cold union failed on
/// a column type conflict, coercing the conflicting columns so both the hot
/// and cold rows survive instead of dropping the cold side.
pub fn emit_with_hot_source_coerced(
    query: &Query,
    source: &str,
    hot_source: &str,
    varchar_cols: &[String],
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        EmitterState::with_hot_source_coerced(source, hot_source, varchar_cols)
    })
}

/// Emit the query, and — when text search bound `_raw` — a second time with
/// the column replaced by a typed NULL, stored as
/// [`EmittedQuery::raw_free_sql`] for the executor's fallback.
///
/// `make_state` is called once per pass so both start from an identical
/// state; the raw-free pass pushes the same parameters in the same order, so
/// the two SQL strings share one parameter list.
fn emit_with_raw_fallback(
    query: &Query,
    make_state: impl Fn() -> Result<EmitterState, EmitError>,
) -> Result<EmittedQuery, EmitError> {
    let (mut emitted, referenced_raw) = emit_from_state(query, make_state()?)?;
    if referenced_raw {
        let (raw_free, _) = emit_from_state(query, make_state()?.without_raw_column())?;
        debug_assert_eq!(
            raw_free.params, emitted.params,
            "raw-free pass must keep the parameter list identical"
        );
        emitted.raw_free_sql = Some(raw_free.sql);
    }
    Ok(emitted)
}

/// Emit one pass. Returns the query and whether text search referenced the
/// `_raw` column.
fn emit_from_state(
    query: &Query,
    mut state: EmitterState,
) -> Result<(EmittedQuery, bool), EmitError> {
    validate::validate_pipeline(&query.pipeline)?;

    // `from saved` cannot be combined with search-stage filters.
    if query.from_saved_stage().is_some() && !query.has_empty_search() {
        return Err(EmitError::UnsupportedOperation {
            message: "'from saved' cannot be combined with search filters".to_string(),
        });
    }

    search::emit_search(&query.search, &mut state)?;

    let mut rust_stages = Vec::new();

    for (i, stage) in query.pipeline.iter().enumerate() {
        // Check if this stage is a kv extraction — can't be expressed as SQL.
        // Collect it and all remaining stages into rust_stages.
        if matches!(
            stage.node,
            PipeStage::Extract(crate::ast::ExtractStage {
                mode: crate::ast::ExtractMode::KeyValue { .. },
                ..
            })
        ) {
            rust_stages = query.pipeline[i..].to_vec();
            break;
        }

        // If a pivot is pending and the next stage isn't another pivot,
        // flush the pivot to a CTE so downstream stages can reference
        // the pivot-generated columns.
        if state.has_pivot() && !matches!(stage.node, PipeStage::Pivot(_)) {
            state.flush_pivot_to_cte();
        }
        pipeline::process_stage(&stage.node, &mut state)?;
    }

    let needs_column_reorder = state.needs_column_reorder();
    let referenced_raw = state.bound_raw_column();
    let sql = state.finalize();
    let params = state.into_params();

    Ok((
        EmittedQuery {
            sql,
            params,
            rust_stages,
            needs_column_reorder,
            raw_free_sql: None,
        },
        referenced_raw,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use insta::assert_snapshot;

    const SRC: &str = "/data/**/*.parquet";

    /// Parse a DSL string and emit SQL; format both for snapshot comparison.
    fn emit_dsl(input: &str) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        let result = emit(&query, SRC).expect("emit should succeed");
        format_result(&result)
    }

    /// Parse and emit, expecting an `EmitError`; return its Display string.
    fn emit_dsl_err(input: &str) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        let err = emit(&query, SRC).expect_err("emit should fail");
        err.to_string()
    }

    fn format_result(result: &EmittedQuery) -> String {
        use std::fmt::Write as _;
        let mut out = result.sql.clone();
        if !result.params.is_empty() {
            out.push_str("\n---\nparams:");
            for (i, p) in result.params.iter().enumerate() {
                let _ = write!(out, "\n  {i}: {p}");
            }
        }
        if !result.rust_stages.is_empty() {
            let _ = write!(out, "\n---\nrust_stages: {}", result.rust_stages.len());
            for stage in &result.rust_stages {
                let _ = write!(out, "\n  {:?}", stage.node);
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // search only
    // -----------------------------------------------------------------------

    #[test]
    fn search_single_field_filter() {
        assert_snapshot!(emit_dsl("service=nginx"));
    }

    #[test]
    fn search_multiple_filters() {
        assert_snapshot!(emit_dsl("service=nginx level=error"));
    }

    #[test]
    fn search_comparison_gt() {
        assert_snapshot!(emit_dsl("status>400"));
    }

    #[test]
    fn search_comparison_gte() {
        assert_snapshot!(emit_dsl("status>=400"));
    }

    #[test]
    fn search_comparison_lt() {
        assert_snapshot!(emit_dsl("status<300"));
    }

    #[test]
    fn search_comparison_ne() {
        assert_snapshot!(emit_dsl("status!=200"));
    }

    #[test]
    fn search_in_list() {
        assert_snapshot!(emit_dsl("status=200,301,404"));
    }

    #[test]
    fn search_glob() {
        assert_snapshot!(emit_dsl("path=glob:/api/*"));
    }

    #[test]
    fn search_regex() {
        assert_snapshot!(emit_dsl(r"host=/web-\d+/"));
    }

    #[test]
    fn search_time_filter() {
        assert_snapshot!(emit_dsl("last=2h"));
    }

    #[test]
    fn search_time_filter_days() {
        assert_snapshot!(emit_dsl("last=7d"));
    }

    #[test]
    fn search_bare_text() {
        assert_snapshot!(emit_dsl("error"));
    }

    #[test]
    fn search_negated_text() {
        assert_snapshot!(emit_dsl("-debug"));
    }

    #[test]
    fn search_quoted_phrase() {
        assert_snapshot!(emit_dsl(r#""connection refused""#));
    }

    #[test]
    fn search_wildcard_no_filter() {
        assert_snapshot!(emit_dsl("*"));
    }

    #[test]
    fn search_kitchen_sink() {
        assert_snapshot!(emit_dsl(
            r#"service=nginx level=error last=2h "connection refused" -debug"#
        ));
    }

    #[test]
    fn search_or_two_services() {
        assert_snapshot!(emit_dsl("service=kernel OR service=trawld"));
    }

    #[test]
    fn search_or_with_stats() {
        assert_snapshot!(emit_dsl("service=kernel OR service=trawld | stats count()"));
    }

    #[test]
    fn search_or_multi_token_groups() {
        assert_snapshot!(emit_dsl(
            "service=nginx level=error OR service=postgres level=warn"
        ));
    }

    #[test]
    fn search_or_with_time_filter() {
        // Time filter should be emitted as top-level WHERE, outside OR parens.
        assert_snapshot!(emit_dsl("service=nginx last=2h OR service=postgres"));
    }

    // -----------------------------------------------------------------------
    // level → severity band alias (ADR-0009)
    // -----------------------------------------------------------------------

    #[test]
    fn search_level_eq_band() {
        assert_snapshot!(emit_dsl("level=error"));
    }

    #[test]
    fn search_level_gte_number() {
        assert_snapshot!(emit_dsl("level>=warn"));
    }

    #[test]
    fn search_level_ne_band() {
        assert_snapshot!(emit_dsl("level!=info"));
    }

    #[test]
    fn search_level_in_list() {
        assert_snapshot!(emit_dsl("level=error,fatal"));
    }

    #[test]
    fn search_level_case_insensitive_token() {
        assert_snapshot!(emit_dsl("level=WARN"));
    }

    #[test]
    fn error_level_unknown_token() {
        assert_snapshot!(emit_dsl_err("level=spicy"));
    }

    #[test]
    fn error_level_glob() {
        assert_snapshot!(emit_dsl_err("level=glob:err*"));
    }

    #[test]
    fn where_level_eq_band() {
        assert_snapshot!(emit_dsl(r#"* | where level == "error""#));
    }

    #[test]
    fn where_level_gte_number() {
        assert_snapshot!(emit_dsl(r#"* | where level >= "warn""#));
    }

    /// `level` is consumed at ingest, so a non-comparison use names a
    /// column that does not exist. Emitting it verbatim gets a binder
    /// error that the hot/cold ladder downgrades to an empty 200 — every
    /// pre-cutover saved query grouping on `level` would silently return
    /// nothing. Reject it instead, everywhere a field name can appear.
    #[test]
    fn error_level_in_stats_by() {
        assert_snapshot!(emit_dsl_err("* | stats count() by level"));
    }

    #[test]
    fn error_level_in_table() {
        assert_snapshot!(emit_dsl_err("* | table host, level"));
    }

    #[test]
    fn error_level_in_sort() {
        assert_snapshot!(emit_dsl_err("* | sort -level"));
    }

    #[test]
    fn error_level_in_expression() {
        assert_snapshot!(emit_dsl_err(r#"* | where lower(level) == "error""#));
    }

    /// A `level` column defined mid-pipeline would shadow the severity
    /// alias for every later stage, so the write side is rejected too.
    #[test]
    fn error_level_as_assignment_target() {
        assert_snapshot!(emit_dsl_err(r#"* | let level = "error""#));
    }

    /// Every other field-name position routes through the same check.
    #[test]
    fn error_level_in_remaining_positions() {
        for dsl in [
            "* | top 5 level",
            "* | rare 5 level",
            "* | dedup level",
            "* | drop level",
            "* | rename level as lvl",
            "* | rename service as level",
            "* | timechart span=5m count() by level",
            "* | pivot count() on level",
            "* | stats count() as level",
            "* | extract kv from level",
            r#"* | where level in ("error")"#,
        ] {
            let err = emit_dsl_err(dsl);
            assert!(
                err.contains("filter-only alias"),
                "{dsl} should be rejected, got: {err}"
            );
        }
    }

    /// The rejection must not swallow the legal comparison forms, in
    /// either stage.
    #[test]
    fn level_comparisons_still_emit() {
        for dsl in [
            "level=error",
            "level>=warn",
            r#"* | where level == "error""#,
            r#"* | where level != "info" and service == "nginx""#,
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            emit(&query, SRC).expect("level comparison should still emit");
        }
    }

    // -----------------------------------------------------------------------
    // _time alias inversion (ADR-0009)
    // -----------------------------------------------------------------------

    /// `timestamp`, `@timestamp` and `_time` all resolve to the physical
    /// `_time` column.
    #[test]
    fn time_aliases_resolve_identically() {
        let canonical = emit_dsl("* | sort _time");
        assert_eq!(emit_dsl("* | sort timestamp"), canonical);
        assert_eq!(emit_dsl("* | sort @timestamp"), canonical);
        assert!(canonical.contains("\"_time\""));
        assert!(!canonical.contains("\"timestamp\""));
    }

    #[test]
    fn time_filter_uses_time_column() {
        let sql = emit_dsl("last=1h");
        assert!(
            sql.contains(r#"TRY_CAST("_time" AS TIMESTAMP)"#),
            "time filter must target _time: {sql}"
        );
    }

    /// Bare search covers `message` OR `_raw`.
    #[test]
    fn bare_search_covers_raw() {
        let sql = emit_dsl("error");
        assert!(sql.contains(r#""message""#), "message side: {sql}");
        assert!(sql.contains(r#""_raw""#), "_raw side: {sql}");
    }

    /// Text search carries a raw-free variant for sources with no `_raw`
    /// column — same parameters, `_raw` bound to a typed NULL so the
    /// predicate degrades to `message` alone.
    #[test]
    fn text_search_carries_a_raw_free_variant() {
        for dsl in ["boom", r#""boom error""#, "-boom service=nginx"] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let emitted = emit(&query, SRC).expect("emit should succeed");
            let raw_free = emitted
                .raw_free_sql
                .as_ref()
                .unwrap_or_else(|| panic!("{dsl} must carry a raw-free variant"));

            assert!(
                !raw_free.contains(r#""_raw""#),
                "{dsl} raw-free variant still binds _raw: {raw_free}"
            );
            assert!(
                raw_free.contains("NULL::VARCHAR"),
                "{dsl} raw-free variant must bind a typed NULL: {raw_free}"
            );
            assert_eq!(
                raw_free.matches('?').count(),
                emitted.params.len(),
                "{dsl} raw-free variant must reuse the same parameter list"
            );
            assert_eq!(
                raw_free.replace("NULL::VARCHAR", r#""_raw""#),
                emitted.sql,
                "{dsl} raw-free variant must differ only in the _raw side"
            );
        }
    }

    /// A query that never binds `_raw` needs no fallback.
    #[test]
    fn queries_without_text_search_have_no_raw_free_variant() {
        for dsl in ["service=nginx", "* | stats count() by host", "last=1h"] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let emitted = emit(&query, SRC).expect("emit should succeed");
            assert!(
                emitted.raw_free_sql.is_none(),
                "{dsl} must not carry a raw-free variant"
            );
        }
    }

    // -----------------------------------------------------------------------
    // single pipe stages
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_stats_no_group() {
        assert_snapshot!(emit_dsl("* | stats count()"));
    }

    #[test]
    fn pipe_stats_with_group() {
        assert_snapshot!(emit_dsl("* | stats count() by host"));
    }

    #[test]
    fn pipe_stats_with_alias() {
        assert_snapshot!(emit_dsl("* | stats count() as total by host"));
    }

    #[test]
    fn pipe_stats_multiple_aggs() {
        assert_snapshot!(emit_dsl("* | stats count(), avg(duration) by host"));
    }

    #[test]
    fn pipe_stats_avg() {
        assert_snapshot!(emit_dsl("* | stats avg(duration)"));
    }

    #[test]
    fn pipe_where() {
        assert_snapshot!(emit_dsl("* | where status > 400"));
    }

    #[test]
    fn pipe_sort_asc() {
        assert_snapshot!(emit_dsl("* | sort host"));
    }

    #[test]
    fn pipe_sort_desc() {
        assert_snapshot!(emit_dsl("* | sort -count"));
    }

    #[test]
    fn pipe_sort_timestamp() {
        assert_snapshot!(emit_dsl("* | sort @timestamp"));
    }

    #[test]
    fn pipe_limit() {
        assert_snapshot!(emit_dsl("* | limit 20"));
    }

    #[test]
    fn pipe_table() {
        assert_snapshot!(emit_dsl("* | table host, service, message"));
    }

    // -----------------------------------------------------------------------
    // multi-stage pipelines (CTE flushing)
    // -----------------------------------------------------------------------

    #[test]
    fn multi_stats_then_where() {
        assert_snapshot!(emit_dsl(
            "service=nginx | stats count() by host | where count > 10"
        ));
    }

    #[test]
    fn multi_stats_then_sort_then_limit() {
        assert_snapshot!(emit_dsl(
            "service=nginx | stats count() by host | sort -count | limit 10"
        ));
    }

    #[test]
    fn multi_full_pipeline() {
        assert_snapshot!(emit_dsl(
            "service=nginx last=2h | stats count() by host | where count > 10 | sort -count | limit 5"
        ));
    }

    #[test]
    fn multi_double_stats() {
        assert_snapshot!(emit_dsl(
            "* | stats count() by host | stats count() as num_hosts"
        ));
    }

    #[test]
    fn multi_stats_then_table() {
        assert_snapshot!(emit_dsl(
            "service=nginx | stats avg(duration) by status | table status, avg_duration"
        ));
    }

    #[test]
    fn multi_table_then_stats() {
        assert_snapshot!(emit_dsl("* | table host, service | stats count() by host"));
    }

    // -----------------------------------------------------------------------
    // expressions in where
    // -----------------------------------------------------------------------

    #[test]
    fn expr_compound_and() {
        assert_snapshot!(emit_dsl("* | where status > 400 and status < 500"));
    }

    #[test]
    fn expr_compound_or() {
        assert_snapshot!(emit_dsl("* | where status == 404 or status == 500"));
    }

    #[test]
    fn expr_not_equal() {
        assert_snapshot!(emit_dsl("* | where count != 1"));
    }

    #[test]
    fn expr_in_list() {
        assert_snapshot!(emit_dsl("* | where status in (200, 301, 404)"));
    }

    #[test]
    fn expr_negation() {
        assert_snapshot!(emit_dsl("* | where not status > 400"));
    }

    #[test]
    fn expr_arithmetic() {
        assert_snapshot!(emit_dsl("* | where duration * 2 > 1000"));
    }

    // -----------------------------------------------------------------------
    // edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_query() {
        assert_snapshot!(emit_dsl(""));
    }

    #[test]
    fn timestamp_mapping() {
        assert_snapshot!(emit_dsl("* | sort @timestamp"));
    }

    #[test]
    fn field_filter_with_numeric_coercion() {
        assert_snapshot!(emit_dsl("status=200"));
    }

    #[test]
    fn field_filter_with_float_coercion() {
        assert_snapshot!(emit_dsl("score=3.14"));
    }

    // -----------------------------------------------------------------------
    // error cases
    // -----------------------------------------------------------------------

    #[test]
    fn error_unknown_function() {
        assert_snapshot!(emit_dsl_err("* | stats bogus()"));
    }

    // -----------------------------------------------------------------------
    // source path validation
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_source_with_single_quote() {
        let query = parser::parse("*").unwrap();
        let err = emit(&query, "/data/foo';DROP TABLE x;--/*.parquet")
            .expect_err("should reject single quote in source path");
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn rejects_source_with_semicolon() {
        let query = parser::parse("*").unwrap();
        let err = emit(&query, "/data/foo;bar.parquet")
            .expect_err("should reject semicolon in source path");
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn accepts_valid_glob_source() {
        let query = parser::parse("*").unwrap();
        let result = emit(&query, "/data/**/*.parquet");
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_tilde_source() {
        let query = parser::parse("*").unwrap();
        let result = emit(&query, "~/.trawl/data/*.parquet");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_dotdot_source() {
        let query = parser::parse("*").unwrap();
        let result = emit(&query, "/data/../etc/passwd/*.parquet");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("path traversal"));
    }

    // -----------------------------------------------------------------------
    // top / rare
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_top_basic() {
        assert_snapshot!(emit_dsl("* | top 10 host"));
    }

    #[test]
    fn pipe_top_with_by() {
        assert_snapshot!(emit_dsl("* | top 5 uri by host"));
    }

    #[test]
    fn pipe_top_with_search() {
        assert_snapshot!(emit_dsl("service=nginx | top 10 uri"));
    }

    #[test]
    fn pipe_rare_basic() {
        assert_snapshot!(emit_dsl("* | rare 3 service"));
    }

    #[test]
    fn pipe_rare_with_by() {
        assert_snapshot!(emit_dsl("* | rare 5 status by host"));
    }

    // -----------------------------------------------------------------------
    // drop
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_drop_single() {
        assert_snapshot!(emit_dsl("* | drop message"));
    }

    #[test]
    fn pipe_drop_multiple() {
        assert_snapshot!(emit_dsl("* | drop message, raw, src_ip"));
    }

    #[test]
    fn multi_stats_then_drop() {
        assert_snapshot!(emit_dsl(
            "* | stats count(), avg(duration) by host | drop avg_duration"
        ));
    }

    // -----------------------------------------------------------------------
    // let
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_let_arithmetic() {
        assert_snapshot!(emit_dsl("* | let duration_ms = duration * 1000"));
    }

    #[test]
    fn pipe_let_function_call() {
        assert_snapshot!(emit_dsl("* | let host_lower = lower(host)"));
    }

    #[test]
    fn multi_let_then_where() {
        assert_snapshot!(emit_dsl(
            "* | let duration_ms = duration * 1000 | where duration_ms > 500"
        ));
    }

    #[test]
    fn multi_let_chained() {
        assert_snapshot!(emit_dsl("* | let x = duration * 1000 | let y = x + 100"));
    }

    #[test]
    fn pipe_let_multi_assignment() {
        assert_snapshot!(emit_dsl("* | let a = lower(service), b = length(service)"));
    }

    #[test]
    fn pipe_let_override_existing() {
        assert_snapshot!(emit_dsl("* | let message = lower(message)"));
    }

    // -----------------------------------------------------------------------
    // extract
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_extract_named_group() {
        assert_snapshot!(emit_dsl(
            r#"* | extract "(?P<ip>[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+)" from message"#
        ));
    }

    #[test]
    fn pipe_extract_default_field() {
        assert_snapshot!(emit_dsl(
            r#"* | extract "(?P<method>[A-Z]+) (?P<path>/[^ ]+)""#
        ));
    }

    #[test]
    fn multi_extract_then_where() {
        assert_snapshot!(emit_dsl(
            r#"service=nginx | extract "(?P<code>[0-9]{3})" from message | where code == "500""#
        ));
    }

    #[test]
    fn error_extract_no_named_groups() {
        assert_snapshot!(emit_dsl_err(r#"* | extract "([0-9]+)" from message"#));
    }

    #[test]
    fn pipe_extract_kv_basic() {
        assert_snapshot!(emit_dsl("* | extract kv"));
    }

    #[test]
    fn pipe_extract_kv_with_downstream() {
        assert_snapshot!(emit_dsl(
            "* | extract kv | where status > 200 | stats count() by method"
        ));
    }

    #[test]
    fn pipe_extract_kv_with_search_prefix() {
        assert_snapshot!(emit_dsl("service=nginx | extract kv from message | head 5"));
    }

    #[test]
    fn pipe_extract_kv_with_sep() {
        assert_snapshot!(emit_dsl(r#"* | extract kv sep=":""#));
    }

    // -----------------------------------------------------------------------
    // dedup
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_dedup_bare() {
        assert_snapshot!(emit_dsl("* | dedup"));
    }

    #[test]
    fn pipe_dedup_single_field() {
        assert_snapshot!(emit_dsl("* | dedup host"));
    }

    #[test]
    fn pipe_dedup_multiple_fields() {
        assert_snapshot!(emit_dsl("* | dedup host, service"));
    }

    #[test]
    fn multi_search_then_dedup() {
        assert_snapshot!(emit_dsl("service=nginx | dedup host"));
    }

    // -----------------------------------------------------------------------
    // timechart
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_timechart_explicit_span() {
        assert_snapshot!(emit_dsl("* | timechart span=5m count()"));
    }

    #[test]
    fn pipe_timechart_with_by() {
        assert_snapshot!(emit_dsl("* | timechart span=1h count() by service"));
    }

    #[test]
    fn pipe_timechart_auto_bucket_1h() {
        assert_snapshot!(emit_dsl("last=1h | timechart count()"));
    }

    #[test]
    fn pipe_timechart_auto_bucket_7d() {
        assert_snapshot!(emit_dsl("last=7d | timechart count()"));
    }

    #[test]
    fn pipe_timechart_multiple_aggs() {
        assert_snapshot!(emit_dsl(
            "* | timechart span=5m count(), avg(duration) by service"
        ));
    }

    // -----------------------------------------------------------------------
    // pivot
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_pivot_basic() {
        assert_snapshot!(emit_dsl("* | pivot count() on status by host"));
    }

    #[test]
    fn pipe_pivot_no_by() {
        assert_snapshot!(emit_dsl("* | pivot count() on service"));
    }

    #[test]
    fn pipe_pivot_with_search() {
        assert_snapshot!(emit_dsl(
            "service=nginx | pivot avg(duration) on status by host"
        ));
    }

    #[test]
    fn pipe_pivot_then_where() {
        assert_snapshot!(emit_dsl(
            "* | pivot count() on status by host | where count > 5"
        ));
    }

    #[test]
    fn pipe_timechart_by_then_where() {
        assert_snapshot!(emit_dsl(
            "last=1h | timechart span=5m count() by severity | where count > 10"
        ));
    }

    // -----------------------------------------------------------------------
    // tail
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_tail() {
        assert_snapshot!(emit_dsl("* | tail 5"));
    }

    #[test]
    fn pipe_tail_after_sort() {
        assert_snapshot!(emit_dsl("* | sort service | tail 3"));
    }

    // -----------------------------------------------------------------------
    // rename
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_rename_single() {
        assert_snapshot!(emit_dsl("* | rename service as svc"));
    }

    #[test]
    fn pipe_rename_multiple() {
        assert_snapshot!(emit_dsl("* | rename service as svc, host as hostname"));
    }

    // -----------------------------------------------------------------------
    // where matches with regex literal
    // -----------------------------------------------------------------------

    #[test]
    fn expr_matches_regex_literal() {
        assert_snapshot!(emit_dsl(r"* | where host matches /prod-.*/"));
    }

    #[test]
    fn expr_matches_string_literal() {
        assert_snapshot!(emit_dsl(r#"* | where host matches "pattern""#));
    }

    // -----------------------------------------------------------------------
    // like / ilike operators
    // -----------------------------------------------------------------------

    #[test]
    fn expr_like() {
        assert_snapshot!(emit_dsl(r#"* | where host like "prod-%""#));
    }

    #[test]
    fn expr_ilike() {
        assert_snapshot!(emit_dsl(r#"* | where message ilike "%timeout%""#));
    }

    // -----------------------------------------------------------------------
    // round with precision
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_let_round_with_precision() {
        assert_snapshot!(emit_dsl("* | let pct = round(count * 100.0 / 1000, 2)"));
    }

    #[test]
    fn pipe_let_round_no_precision() {
        assert_snapshot!(emit_dsl("* | let r = round(duration)"));
    }

    #[test]
    fn error_round_non_int_precision() {
        assert_snapshot!(emit_dsl_err(r#"* | let r = round(duration, "two")"#));
    }

    // -----------------------------------------------------------------------
    // new string functions
    // -----------------------------------------------------------------------

    #[test]
    fn fn_contains() {
        assert_snapshot!(emit_dsl(r#"* | where contains(message, "error")"#));
    }

    #[test]
    fn fn_startswith() {
        assert_snapshot!(emit_dsl(r#"* | where startswith(path, "/api/")"#));
    }

    #[test]
    fn fn_endswith() {
        assert_snapshot!(emit_dsl(r#"* | where endswith(host, ".local")"#));
    }

    #[test]
    fn fn_split() {
        assert_snapshot!(emit_dsl(r#"* | let seg = split(path, "/", 2)"#));
    }

    #[test]
    fn fn_concat() {
        assert_snapshot!(emit_dsl(r#"* | let full = concat(host, ":", service)"#));
    }

    // -----------------------------------------------------------------------
    // date/time functions
    // -----------------------------------------------------------------------

    #[test]
    fn fn_date_part() {
        assert_snapshot!(emit_dsl(r#"* | let hour = date_part("hour", timestamp)"#));
    }

    #[test]
    fn fn_date_trunc() {
        assert_snapshot!(emit_dsl(r#"* | let day = date_trunc("day", timestamp)"#));
    }

    #[test]
    fn fn_date_diff() {
        assert_snapshot!(emit_dsl(
            r#"* | let age = date_diff("second", timestamp, now())"#
        ));
    }

    #[test]
    fn fn_strftime_dsl_order() {
        // strftime emits in DSL order (ts, fmt); DuckDB's STRFTIME is overloaded
        // so no arg swap is needed (see emitter::functions strftime arm).
        assert_snapshot!(emit_dsl(
            r#"* | let formatted = strftime(timestamp, "%Y-%m-%d")"#
        ));
    }

    // -----------------------------------------------------------------------
    // case() conditional
    // -----------------------------------------------------------------------

    #[test]
    fn fn_case_with_default() {
        assert_snapshot!(emit_dsl(
            r#"* | let sev = case(status >= 500, "server_error", status >= 400, "client_error", "ok")"#
        ));
    }

    #[test]
    fn fn_case_no_default() {
        assert_snapshot!(emit_dsl(
            r#"* | let sev = case(status >= 500, "5xx", status >= 400, "4xx")"#
        ));
    }

    // -----------------------------------------------------------------------
    // json functions
    // -----------------------------------------------------------------------

    #[test]
    fn fn_json_extract() {
        assert_snapshot!(emit_dsl(r#"* | where json(message, "$.user") == "alice""#));
    }

    #[test]
    fn fn_json_valid() {
        assert_snapshot!(emit_dsl("* | where json_valid(message)"));
    }

    // -----------------------------------------------------------------------
    // earliest / latest absolute time bounds
    // -----------------------------------------------------------------------

    #[test]
    fn search_earliest_latest() {
        assert_snapshot!(emit_dsl(
            r#"earliest="2026-03-14T03:00:00Z" latest="2026-03-14T03:15:00Z" level=error"#
        ));
    }

    #[test]
    fn search_earliest_only() {
        assert_snapshot!(emit_dsl(r#"earliest="2026-03-14T00:00:00Z" level=error"#));
    }

    // -----------------------------------------------------------------------
    // sample
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_sample_percent() {
        assert_snapshot!(emit_dsl("* | sample 10%"));
    }

    #[test]
    fn pipe_sample_count() {
        assert_snapshot!(emit_dsl("* | sample 1000"));
    }

    // -----------------------------------------------------------------------
    // eventstats
    // -----------------------------------------------------------------------

    #[test]
    fn pipe_eventstats_basic() {
        assert_snapshot!(emit_dsl("* | eventstats avg(duration) by service"));
    }

    #[test]
    fn pipe_eventstats_no_by() {
        assert_snapshot!(emit_dsl("* | eventstats count()"));
    }

    #[test]
    fn pipe_eventstats_multi_agg() {
        assert_snapshot!(emit_dsl(
            "* | eventstats avg(duration) as avg_dur, count() as total by service"
        ));
    }

    #[test]
    fn pipe_eventstats_then_where() {
        assert_snapshot!(emit_dsl(
            "* | eventstats avg(duration) as avg_dur by service | where duration > avg_dur"
        ));
    }

    // -----------------------------------------------------------------------
    // NOT and parentheses in search stage
    // -----------------------------------------------------------------------

    #[test]
    fn search_not_field() {
        assert_snapshot!(emit_dsl("NOT service=nginx"));
    }

    #[test]
    fn search_not_paren_group() {
        assert_snapshot!(emit_dsl(
            "NOT (service=nginx OR service=apache) level=error"
        ));
    }

    #[test]
    fn search_paren_group() {
        assert_snapshot!(emit_dsl("(service=nginx OR service=apache) last=1h"));
    }

    #[test]
    fn error_eventstats_dc() {
        assert_snapshot!(emit_dsl_err("* | eventstats dc(host) by service"));
    }

    #[test]
    fn error_last_with_earliest() {
        assert_snapshot!(emit_dsl_err(r#"last=1h earliest="2026-03-14T00:00:00Z""#));
    }

    // -----------------------------------------------------------------------
    // composite hot source
    // -----------------------------------------------------------------------

    #[test]
    fn hot_source_emits_union_all() {
        let query = parser::parse("service=nginx").unwrap();
        let result = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &crate::schema::FieldTypes::new(),
        )
        .unwrap();
        assert_snapshot!(format_result(&result));
    }

    #[test]
    fn hot_source_rejects_invalid_path() {
        let query = parser::parse("*").unwrap();
        let err = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/bad;path.ndjson",
            &crate::schema::FieldTypes::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn hot_source_pins_cast_hot_branch_only() {
        let query = parser::parse("*").unwrap();
        let mut pins = crate::schema::FieldTypes::new();
        pins.insert("duration", crate::schema::CanonicalType::BigInt);
        pins.insert("note", crate::schema::CanonicalType::Varchar);
        let sql = emit_with_hot_source(&query, SRC, "/tmp/hot_abc123.ndjson", &pins)
            .unwrap()
            .sql;
        // Typed pin: TRY_CAST on the hot branch (NULL on mismatch, never a
        // union throw — ADR-0008).
        assert!(
            sql.contains(r#"TRY_CAST("duration" AS BIGINT) AS "duration""#),
            "hot branch must conform the BIGINT pin: {sql}"
        );
        // VARCHAR pin: the untyped json path, so strings land unquoted
        // whatever the snapshot column inferred as.
        assert!(
            sql.contains(r#"json_extract_string(to_json("note"), '$') AS "note""#),
            "hot branch must conform the VARCHAR pin untyped: {sql}"
        );
        // The pin casts appear ONCE — on the hot branch only. The cold
        // branch is plain: parquet is write-time conformant, and a
        // defensive cold cast would mask a real invariant breach.
        assert_eq!(sql.matches(r#"TRY_CAST("duration""#).count(), 1);
        assert!(
            sql.contains("(SELECT * FROM read_parquet("),
            "cold branch must have no REPLACE: {sql}"
        );
    }

    #[test]
    fn hot_source_pinned_timestamp_columns_not_duplicated() {
        // The two envelope TIMESTAMP columns get their unconditional
        // TRY_CASTs; a pin on them must not add a second REPLACE entry.
        let query = parser::parse("*").unwrap();
        let mut pins = crate::schema::FieldTypes::new();
        pins.insert("_time", crate::schema::CanonicalType::Timestamp);
        pins.insert("_ingested", crate::schema::CanonicalType::Timestamp);
        let sql = emit_with_hot_source(&query, SRC, "/tmp/hot_abc123.ndjson", &pins)
            .unwrap()
            .sql;
        assert_eq!(sql.matches(r#"AS "_time""#).count(), 1, "{sql}");
        assert_eq!(sql.matches(r#"AS "_ingested""#).count(), 1, "{sql}");
    }

    #[test]
    fn hot_source_empty_pins_is_plain_union_with_timestamp_casts() {
        // Empty pins (embedded mode, catalog-less buffer) must emit exactly
        // the shape the coerced path emits for zero coercions: plain cold
        // select, hot side with only the two timestamp TRY_CASTs.
        let query = parser::parse("service=nginx").unwrap();
        let plain = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &crate::schema::FieldTypes::new(),
        )
        .unwrap();
        let coerced =
            emit_with_hot_source_coerced(&query, SRC, "/tmp/hot_abc123.ndjson", &[]).unwrap();
        assert_eq!(plain.sql, coerced.sql);
    }

    #[test]
    fn hot_source_coerced_empty_matches_plain() {
        // The empty-coercion path must be byte-identical to the plain hot
        // source so the common case is unchanged.
        let query = parser::parse("service=nginx").unwrap();
        let plain = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &crate::schema::FieldTypes::new(),
        )
        .unwrap();
        let coerced =
            emit_with_hot_source_coerced(&query, SRC, "/tmp/hot_abc123.ndjson", &[]).unwrap();
        assert_eq!(plain.sql, coerced.sql);
    }

    #[test]
    fn hot_source_coerced_casts_columns_both_sides() {
        let query = parser::parse("*").unwrap();
        let cols = vec!["status".to_string(), "containerID".to_string()];
        let sql = emit_with_hot_source_coerced(&query, SRC, "/tmp/hot_abc123.ndjson", &cols)
            .unwrap()
            .sql;
        // Cold (parquet) side casts the conflicting columns to VARCHAR.
        assert!(
            sql.contains(r#"REPLACE (CAST("status" AS VARCHAR) AS "status""#),
            "cold side should cast status: {sql}"
        );
        assert!(
            sql.contains(r#"CAST("containerID" AS VARCHAR) AS "containerID""#),
            "should cast containerID: {sql}"
        );
        // Hot side keeps the casts for BOTH envelope timestamp columns
        // (TRY_CAST — the partition key is never hard-CAST, ADR-0008)
        // and adds the VARCHAR casts.
        assert!(
            sql.contains(r#"TRY_CAST("_time" AS TIMESTAMP) AS "_time""#),
            "hot side keeps _time TRY_CAST: {sql}"
        );
        assert!(
            sql.contains(r#"TRY_CAST("_ingested" AS TIMESTAMP) AS "_ingested""#),
            "hot side keeps _ingested TRY_CAST: {sql}"
        );
    }

    // -----------------------------------------------------------------------
    // list-format source paths
    // -----------------------------------------------------------------------

    #[test]
    fn list_source_emits_read_parquet_list() {
        let query = parser::parse("*").unwrap();
        let result = emit(
            &query,
            "['/data/2026-02-12/14/*.parquet', '/data/2026-02-12/15/*.parquet']",
        )
        .unwrap();
        assert_snapshot!(format_result(&result));
    }

    #[test]
    fn list_source_rejects_invalid_path() {
        let query = parser::parse("*").unwrap();
        let err = emit(&query, "['/data/ok/*.parquet', '/data/bad;drop/*.parquet']").unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn list_source_rejects_malformed_list() {
        let query = parser::parse("*").unwrap();
        let err = emit(&query, "[not-quoted]").unwrap_err();
        assert!(err.to_string().contains("invalid source list element"));
    }
}
