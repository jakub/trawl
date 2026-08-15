// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SQL emitter for `DuckDB`.
//!
//! Walks the AST and produces parameterized `DuckDB` SQL.
//! Uses CTEs to handle multi-stage pipeline queries.

mod compare;
mod expr;
mod fields;
mod functions;
mod pipeline;
mod search;
pub(crate) mod state;
mod validate;

use crate::ast::{PipeStage, Query, Spanned};
use state::EmitterState;

pub(crate) use fields::coerce_filter_value;
pub use functions::is_aggregate_function;
pub use functions::{DATE_PART_UNITS, DATE_UNITS};
pub(crate) use functions::{
    format_literal_position, unit_literal_positions, validate_format_literal, validate_unit_literal,
};
pub use state::{hot_source_reader, source_reader, validate_source_path};
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
    /// The pin scope in force at the kv split point (ADR-0011 slice A′):
    /// the interpretation [`rust_stages`](Self::rust_stages) must be
    /// evaluated under, carried so the batch tail's `where`/`let` use the
    /// SAME pins the SQL prefix used — including every rename/let/stats
    /// scope change before the split. Empty for pin-blind emission.
    pub rust_stage_pins: crate::pin_scope::PinScope,
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

/// A literal the pin rule table refuses (`crate::compare::CompareError`)
/// is an unsupported operation to every emitter caller — one conversion,
/// so the search stage, the pipeline emitter, the live filter and the
/// stream compiler all report the same sentence.
impl From<crate::compare::CompareError> for EmitError {
    fn from(err: crate::compare::CompareError) -> Self {
        Self::UnsupportedOperation {
            message: err.to_string(),
        }
    }
}

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
///
/// Deliberately PIN-BLIND: comparisons stay literal-driven (ADR-0011 slice
/// A's documented embedded-mode behavior). This is the door for embedded
/// `--data` queries, the fuzz target and the snapshot tests; every
/// catalog-backed caller goes through [`emit_with_pins`] or
/// [`emit_with_hot_source`].
pub fn emit(query: &Query, source: &str) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || EmitterState::new(source))
}

/// Emit SQL with the field catalog's pins typing the search-stage
/// comparisons (ADR-0011 slice A).
///
/// `pins` is the FULL catalog snapshot (not intersected with any hot key
/// set): a VARCHAR-pinned field compares as text under `=`/`!=`/IN,
/// numerically in [`crate::conform::DECIMAL_COMPARISON_SPACE`] for
/// ordered numeric literals (both sides cast, so the literal never
/// round-trips through `f64`), and typed pins glob/regex through
/// `CAST(col AS VARCHAR)`. A typed pin's COMPARISONS emit exactly what the
/// unpinned path emits — the column on disk already is the pinned type —
/// and travel to the live matcher, which has to conform the wire value
/// before it can answer the same question ([`crate::filter`]). See
/// [`crate::compare`] for the rule table. Empty `pins` emits exactly what
/// [`emit`] emits.
pub fn emit_with_pins(
    query: &Query,
    source: &str,
    pins: &crate::schema::FieldTypes,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        Ok(EmitterState::new(source)?.with_compare_pins(pins))
    })
}

/// Emit SQL that unions the primary parquet source with a hot buffer ndjson file.
///
/// Produces a `UNION ALL BY NAME` composite source so that fresh events
/// in the hot buffer are visible alongside compacted parquet data.
///
/// Two pin sets, two roles, never conflated (ADR-0011 slice A):
///
/// - `hot_pins` — the catalog's pins intersected with the snapshot's
///   observed keys: each pinned field is conformed on the HOT branch only,
///   through the same text-first guarded cast compaction writes with
///   ([`crate::conform`]), so a hot value disagreeing with the write-time
///   pin degrades to NULL instead of throwing the union — and one that
///   agrees reads exactly as it will once compacted. An intersected set,
///   because the `REPLACE` list must never name a column absent from the
///   snapshot.
/// - `pins` — the FULL catalog snapshot typing the search-stage
///   comparisons (see [`emit_with_pins`]). Full, because a cold-only
///   field's comparison semantics must not depend on ingest timing.
///
/// Empty sets (embedded mode, catalog-less buffer) leave the union plain
/// apart from the unconditional envelope timestamp `TRY_CAST`s (ADR-0008)
/// and the comparisons literal-driven.
pub fn emit_with_hot_source(
    query: &Query,
    source: &str,
    hot_source: &str,
    hot_pins: &crate::schema::FieldTypes,
    pins: &crate::schema::FieldTypes,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        Ok(EmitterState::with_hot_source(source, hot_source, hot_pins)?.with_compare_pins(pins))
    })
}

/// Emit SQL reading ONLY the hot-buffer ndjson, conformed exactly as the
/// union's hot branch is (ADR-0011 slice A).
///
/// Same two pin sets, same two roles as [`emit_with_hot_source`]:
/// `hot_pins` (intersected with the snapshot's keys) conforms the hot
/// columns, `pins` (the full catalog snapshot) types the comparisons. This
/// is the executor's cold-start lane — reading the raw ndjson through
/// [`emit_with_pins`] instead would let `read_json`'s inference, not the
/// catalog, decide a hot column's type, so a query's answer would change
/// the moment the first parquet file landed.
pub fn emit_hot_only(
    query: &Query,
    hot_source: &str,
    hot_pins: &crate::schema::FieldTypes,
    pins: &crate::schema::FieldTypes,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        Ok(EmitterState::with_hot_only_source(hot_source, hot_pins)?.with_compare_pins(pins))
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
    let mut rust_stage_pins = crate::pin_scope::PinScope::unpinned();

    for (i, stage) in query.pipeline.iter().enumerate() {
        // Check if this stage is a kv extraction — can't be expressed as SQL.
        // Collect it and all remaining stages into rust_stages, stamping
        // the pin scope in force at the split so the batch tail evaluates
        // under the same interpretation the SQL prefix used (slice A′).
        if matches!(
            stage.node,
            PipeStage::Extract(crate::ast::ExtractStage {
                mode: crate::ast::ExtractMode::KeyValue { .. },
                ..
            })
        ) {
            rust_stages = query.pipeline[i..].to_vec();
            rust_stage_pins = state.pin_scope().clone();
            break;
        }

        // If a pivot is pending and the next stage isn't another pivot,
        // flush the pivot to a CTE so downstream stages can reference
        // the pivot-generated columns.
        if state.has_pivot() && !matches!(stage.node, PipeStage::Pivot(_)) {
            state.flush_pivot_to_cte();
        }
        pipeline::process_stage(&stage.node, &mut state)?;
        // AFTER the stage: its own expressions resolve against the
        // incoming schema; the next stage sees this one's output scope.
        state.advance_pin_scope(&stage.node);
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
            rust_stage_pins,
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
        assert_snapshot!(emit_dsl("service=nginx _severity=error"));
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
            r#"service=nginx _severity=error last=2h "connection refused" -debug"#
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
            "service=nginx _severity=error OR service=postgres _severity=warn"
        ));
    }

    #[test]
    fn search_or_with_time_filter() {
        // Time filter should be emitted as top-level WHERE, outside OR parens.
        assert_snapshot!(emit_dsl("service=nginx last=2h OR service=postgres"));
    }

    // -----------------------------------------------------------------------
    // zero DSL aliases (ADR-0013 §6)
    // -----------------------------------------------------------------------

    /// `level` is ordinary sender vocabulary now — one name, one column,
    /// in every position that used to reject it. The severity band
    /// vocabulary lives on `_severity`, which nothing can shadow.
    #[test]
    fn level_is_an_ordinary_field_in_every_position() {
        for dsl in [
            "level=error",
            "level=gold",
            "level=error,fatal",
            "level>=warn",
            "level=err*",
            "* | stats count() by level",
            "* | table host, level",
            "* | sort -level",
            r#"* | where lower(level) == "error""#,
            r#"* | let level = "error""#,
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
            let query = parser::parse(dsl).expect("parse should succeed");
            emit(&query, SRC).unwrap_or_else(|e| panic!("{dsl} must emit, got {e}"));
        }
    }

    /// `level=gold` — the #60 canonical example — filters the sender's
    /// own column, verbatim.
    #[test]
    fn search_level_is_a_plain_field_filter() {
        assert_snapshot!(emit_dsl("level=gold"));
    }

    #[test]
    fn where_level_is_a_plain_comparison() {
        assert_snapshot!(emit_dsl(r#"* | where level == "error""#));
    }

    /// The pipeline may not MINT a reserved name (ADR-0013 §5): the same
    /// predicate ingest strips by.
    #[test]
    fn reserved_names_cannot_be_minted_by_the_pipeline() {
        for dsl in [
            "* | let _foo = 1",
            "* | eval _severity = 17",
            "* | rename service as _svc",
            "* | stats count() as _severity",
            "* | eventstats count() as _time",
            "* | timechart span=5m count() as _raw",
            "* | pivot count() as _repairs on service",
        ] {
            assert!(parser::parse(dsl).is_err(), "{dsl} must be a parse error");
        }
        // The capture-group door is the one place the regex is already
        // compiled, reached by both lanes through `validate_pipeline`.
        let err = emit_dsl_err(r#"* | extract "(?P<_foo>.)" from message"#);
        assert!(err.contains("reserved namespace"), "{err}");
    }

    // -----------------------------------------------------------------------
    // the physical `_time` column (ADR-0013: no aliases resolve onto it)
    // -----------------------------------------------------------------------

    /// `timestamp` and `@timestamp` are ORDINARY sender field names —
    /// each names the column it spells, and only `_time` is `_time`.
    #[test]
    fn time_alias_spellings_name_their_own_columns() {
        assert!(emit_dsl("* | sort _time").contains("\"_time\""));
        assert!(emit_dsl("* | sort timestamp").contains("\"timestamp\""));
        assert!(emit_dsl("* | sort @timestamp").contains("\"@timestamp\""));
        assert!(!emit_dsl("* | sort timestamp").contains("\"_time\""));
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
            r#"earliest="2026-03-14T03:00:00Z" latest="2026-03-14T03:15:00Z" _severity=error"#
        ));
    }

    #[test]
    fn search_earliest_only() {
        assert_snapshot!(emit_dsl(
            r#"earliest="2026-03-14T00:00:00Z" _severity=error"#
        ));
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
        assert_snapshot!(emit_dsl(
            "* | eventstats avg(duration) as avg_duration by service"
        ));
    }

    #[test]
    fn pipe_eventstats_no_by() {
        assert_snapshot!(emit_dsl("* | eventstats count() as total"));
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
            "NOT (service=nginx OR service=apache) _severity=error"
        ));
    }

    #[test]
    fn search_paren_group() {
        assert_snapshot!(emit_dsl("(service=nginx OR service=apache) last=1h"));
    }

    #[test]
    fn error_eventstats_dc() {
        assert_snapshot!(emit_dsl_err("* | eventstats dc(host) as hosts by service"));
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
        let sql = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &pins,
            &crate::schema::FieldTypes::new(),
        )
        .unwrap()
        .sql;
        // Typed pin: the guarded cast over the column's TEXT form, exactly
        // what compaction writes (NULL on mismatch, never a union throw —
        // ADR-0008). The cast never touches the column itself, so the
        // domain cannot vary with read_json's inference.
        let duration_text = r#"json_extract_string(to_json("duration"), '$')"#;
        assert_eq!(
            sql.matches(&format!(
                "{} AS \"duration\"",
                crate::conform::guarded_cast(duration_text, crate::schema::CanonicalType::BigInt)
            ))
            .count(),
            1,
            "hot branch must conform the BIGINT pin through the shared guard: {sql}"
        );
        assert!(
            !sql.contains(r#"TRY_CAST("duration""#),
            "no cast may bind the raw hot column: {sql}"
        );
        // VARCHAR pin: the untyped json path, so strings land unquoted
        // whatever the snapshot column inferred as.
        assert!(
            sql.contains(r#"json_extract_string(to_json("note"), '$') AS "note""#),
            "hot branch must conform the VARCHAR pin untyped: {sql}"
        );
        // The conform appears on the hot branch only. The cold branch is
        // plain: parquet is write-time conformant, and a defensive cold
        // cast would mask a real invariant breach.
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
        let sql = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &pins,
            &crate::schema::FieldTypes::new(),
        )
        .unwrap()
        .sql;
        assert_eq!(sql.matches(r#"AS "_time""#).count(), 1, "{sql}");
        assert_eq!(sql.matches(r#"AS "_ingested""#).count(), 1, "{sql}");
    }

    #[test]
    fn hot_source_non_ascii_distinct_pins_each_get_an_entry() {
        // DuckDB folds identifiers over ASCII only, so `café` and `cafÉ`
        // (the ingest-folded form of `CAFÉ`) are two distinct columns and
        // two distinct pins — each must keep its own REPLACE entry.
        let query = parser::parse("*").unwrap();
        let mut pins = crate::schema::FieldTypes::new();
        pins.insert("café", crate::schema::CanonicalType::Varchar);
        pins.insert("cafÉ", crate::schema::CanonicalType::BigInt);
        let sql = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &pins,
            &crate::schema::FieldTypes::new(),
        )
        .unwrap()
        .sql;
        assert!(
            sql.contains(r#"json_extract_string(to_json("café"), '$') AS "café""#),
            "{sql}"
        );
        assert!(
            sql.contains(&format!(
                "{} AS \"cafÉ\"",
                crate::conform::guarded_cast(
                    r#"json_extract_string(to_json("cafÉ"), '$')"#,
                    crate::schema::CanonicalType::BigInt
                )
            )),
            "{sql}"
        );
    }

    #[test]
    fn hot_source_empty_pins_is_plain_union_with_timestamp_casts() {
        // Empty pins (embedded mode, catalog-less buffer) must emit exactly
        // the shape the coerced path emits for zero coercions: plain cold
        // select, hot side with only the two timestamp TRY_CASTs.
        let query = parser::parse("service=nginx").unwrap();
        let sql = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &crate::schema::FieldTypes::new(),
            &crate::schema::FieldTypes::new(),
        )
        .unwrap()
        .sql;
        assert!(
            sql.contains("(SELECT * FROM read_parquet("),
            "cold branch must be plain: {sql}"
        );
        assert!(
            sql.contains(
                r#"REPLACE (TRY_CAST("_time" AS TIMESTAMP) AS "_time", TRY_CAST("_ingested" AS TIMESTAMP) AS "_ingested") FROM read_json("#
            ),
            "hot branch must carry exactly the two timestamp TRY_CASTs \
             (TRY_CAST — the partition key is never hard-CAST, ADR-0008): {sql}"
        );
        assert_eq!(
            sql.matches("REPLACE").count(),
            1,
            "empty pins must add no further REPLACE entries: {sql}"
        );
    }

    // -----------------------------------------------------------------------
    // pin-aware comparisons (ADR-0011 slice A)
    // -----------------------------------------------------------------------

    fn pins(entries: &[(&str, crate::schema::CanonicalType)]) -> crate::schema::FieldTypes {
        let mut ft = crate::schema::FieldTypes::new();
        for (field, ty) in entries {
            ft.insert(field, *ty);
        }
        ft
    }

    /// Parse a DSL string and emit pin-aware SQL; format for snapshots.
    fn emit_dsl_with_pins(input: &str, entries: &[(&str, crate::schema::CanonicalType)]) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        let result = emit_with_pins(&query, SRC, &pins(entries)).expect("emit should succeed");
        format_result(&result)
    }

    /// Parse and emit pin-aware SQL, expecting an `EmitError`.
    fn emit_dsl_err_with_pins(
        input: &str,
        entries: &[(&str, crate::schema::CanonicalType)],
    ) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        emit_with_pins(&query, SRC, &pins(entries))
            .expect_err("emit should fail")
            .to_string()
    }

    /// The declared `_severity` pin, as the catalog seed installs it.
    const SEVERITY_PIN: [(&str, crate::schema::CanonicalType); 1] = [("_severity", CT::Severity)];

    use crate::schema::CanonicalType as CT;

    /// A numeric literal binds BOTH the text and its numeric reading: the
    /// stored text of a number is `read_json`'s inference rendered
    /// (`"200.0"`), so exact text alone would be a batch miss where the
    /// live matcher — which only sees the wire `200` — hits.
    // --- the SEVERITY pin (ADR-0013): tokens ride the rule table ---

    #[test]
    fn pinned_severity_eq_band_token() {
        assert_snapshot!(emit_dsl_with_pins("_severity=error", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_eq_exact_otel_name() {
        assert_snapshot!(emit_dsl_with_pins("_severity=error2", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_gte_token_is_the_exact_number() {
        assert_snapshot!(emit_dsl_with_pins("_severity>=warn", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_ne_band_widens_with_null_in_the_search_stage() {
        assert_snapshot!(emit_dsl_with_pins("_severity!=info", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_in_list_expands_to_or_of_bands() {
        assert_snapshot!(emit_dsl_with_pins("_severity=warn,error", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_glob_matches_the_canonical_token_text() {
        assert_snapshot!(emit_dsl_with_pins("_severity=warn*", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_regex_matches_the_canonical_token_text() {
        assert_snapshot!(emit_dsl_with_pins("_severity=/^err/", &SEVERITY_PIN));
    }

    /// The pipeline lane binds the same rule with the STRICT null policy:
    /// `!=` keeps plain SQL null propagation (ADR-0011 slice A′).
    #[test]
    fn pinned_severity_where_eq_band() {
        assert_snapshot!(emit_dsl_with_pins(
            r#"* | where _severity == "error""#,
            &SEVERITY_PIN
        ));
    }

    #[test]
    fn pinned_severity_where_ne_band_stays_strict() {
        assert_snapshot!(emit_dsl_with_pins(
            r#"* | where _severity != "error""#,
            &SEVERITY_PIN
        ));
    }

    #[test]
    fn pinned_severity_where_gte_token() {
        assert_snapshot!(emit_dsl_with_pins(
            r#"* | where _severity >= "warn""#,
            &SEVERITY_PIN
        ));
    }

    #[test]
    fn error_severity_unknown_token() {
        assert_snapshot!(emit_dsl_err_with_pins("_severity=spicy", &SEVERITY_PIN));
    }

    #[test]
    fn error_severity_unknown_token_in_where() {
        assert_snapshot!(emit_dsl_err_with_pins(
            r#"* | where _severity == "spicy""#,
            &SEVERITY_PIN
        ));
    }

    #[test]
    fn pinned_varchar_eq_numeric_binds_text_and_reading() {
        assert_snapshot!(emit_dsl_with_pins("status=200", &[("status", CT::Varchar)]));
    }

    #[test]
    fn pinned_varchar_ne_numeric_binds_text_and_reading_keeping_null_policy() {
        assert_snapshot!(emit_dsl_with_pins(
            "status!=200",
            &[("status", CT::Varchar)]
        ));
    }

    /// A list with a numeric element expands to the OR of its per-element
    /// equalities — an element with two arms has no single bound value.
    #[test]
    fn pinned_varchar_in_list_expands_to_or_of_equalities() {
        assert_snapshot!(emit_dsl_with_pins(
            "status=200,301,404",
            &[("status", CT::Varchar)]
        ));
    }

    /// A list with no numeric element keeps the plain `IN (…)` shape.
    #[test]
    fn pinned_varchar_in_list_without_numbers_keeps_in_shape() {
        assert_snapshot!(emit_dsl_with_pins(
            "status=accepted,pending",
            &[("status", CT::Varchar)]
        ));
    }

    /// Mixed lists mix the shapes, element by element.
    #[test]
    fn pinned_varchar_in_list_mixes_text_and_numeric_elements() {
        assert_snapshot!(emit_dsl_with_pins(
            "status=200,accepted",
            &[("status", CT::Varchar)]
        ));
    }

    /// The ordered rung casts BOTH sides into the one comparison space:
    /// binding the literal as a number would put it back on the `f64`
    /// path that made every id above 2^53 equal to its neighbours
    /// (ADR-0011 ruling #6).
    #[test]
    fn pinned_varchar_ordered_numeric_compares_in_decimal_space() {
        assert_snapshot!(emit_dsl_with_pins(
            "status>=400",
            &[("status", CT::Varchar)]
        ));
    }

    #[test]
    fn pinned_varchar_ordered_lexical_unchanged() {
        assert_snapshot!(emit_dsl_with_pins("host>alpha", &[("host", CT::Varchar)]));
    }

    #[test]
    fn pinned_bigint_glob_casts_to_varchar() {
        assert_snapshot!(emit_dsl_with_pins("status=4*", &[("status", CT::BigInt)]));
    }

    /// A TIMESTAMP pin renders the canonical RFC 3339 pattern text, not
    /// `DuckDB`'s space-separated CAST rendering — the live matcher builds
    /// the same string from the wire value (`compare::
    /// canonical_timestamp_text`), so an anchored pattern means one thing.
    #[test]
    fn pinned_timestamp_regex_renders_rfc3339_text() {
        assert_snapshot!(emit_dsl_with_pins(
            r"_time=/2026-01-.*/",
            &[("_time", CT::Timestamp)]
        ));
    }

    /// Pin lookup goes through `catalog_key`: a mixed-case reference names
    /// the same (ingest-folded, DuckDB-case-insensitive) column and must
    /// find the same pin — never silently fall back to unpinned. The
    /// identifier keeps the user's spelling (`DuckDB` folds it), so the
    /// evidence is the bound parameters: under the VARCHAR pin the same
    /// literal text binds twice — once for the text arm, once as the
    /// numeric arm's cast input — where an unpinned emission binds the
    /// coerced number alone.
    #[test]
    fn pinned_lookup_is_case_folded() {
        let ft = pins(&[("status", CT::Varchar)]);
        for dsl in ["status=200", "Status=200"] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let emitted = emit_with_pins(&query, SRC, &ft).expect("emit should succeed");
            assert_eq!(
                emitted.params,
                vec![
                    SqlValue::String("200".into()),
                    SqlValue::String("200".into())
                ],
                "{dsl} must bind the pinned two-armed equality"
            );
        }
    }

    /// Composite hot source with BOTH pin sets: `hot_pins` conforms the
    /// hot branch (REPLACE), `pins` types the comparison — two distinct
    /// roles, never conflated.
    #[test]
    fn hot_source_with_comparison_pins() {
        let query = parser::parse("status=200 duration>5").unwrap();
        let mut hot_pins = crate::schema::FieldTypes::new();
        hot_pins.insert("duration", CT::BigInt);
        let comparison = pins(&[("status", CT::Varchar), ("duration", CT::BigInt)]);
        let result = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &hot_pins,
            &comparison,
        )
        .unwrap();
        assert_snapshot!(format_result(&result));
    }

    // -----------------------------------------------------------------------
    // pin-aware pipeline comparisons (ADR-0011 slice A′)
    // -----------------------------------------------------------------------

    /// `| where` over a VARCHAR pin compares ordered-numeric in the one
    /// DECIMAL space — the Conversion error the pin-blind emission raised
    /// becomes an answer.
    #[test]
    fn pinned_where_varchar_ordered_numeric_compares_in_decimal_space() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | where status > 400",
            &[("status", CT::Varchar)]
        ));
    }

    /// `| where` equality against a VARCHAR pin binds text + reading,
    /// exactly like the search stage's `status=200`.
    #[test]
    fn pinned_where_varchar_eq_numeric_binds_text_and_reading() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | where status == 200",
            &[("status", CT::Varchar)]
        ));
    }

    /// The pipeline `!=` keeps plain SQL null propagation: NO
    /// `OR field IS NULL` widening — a repin must not change
    /// missing-field semantics (`NullPolicy::Strict`).
    #[test]
    fn pinned_where_varchar_ne_keeps_strict_null_policy() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | where status != 200",
            &[("status", CT::Varchar)]
        ));
    }

    /// `where f in (…)` over a VARCHAR pin expands numeric elements to
    /// the two-armed equalities, like the search stage's IN list.
    #[test]
    fn pinned_where_in_list_expands_numeric_elements() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | where status in (200, \"accepted\")",
            &[("status", CT::Varchar)]
        ));
    }

    /// Either operand order: `400 < status` is `status > 400`.
    #[test]
    fn pinned_where_reversed_operands_bind_the_field_rule() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | where 400 < status",
            &[("status", CT::Varchar)]
        ));
    }

    /// Quote provenance is discarded: `where status > "400"` binds like
    /// `status > 400` (content decides, per the rule table).
    #[test]
    fn pinned_where_quoted_numeric_literal_binds_content() {
        let a = emit_dsl_with_pins("* | where status > \"400\"", &[("status", CT::Varchar)]);
        let b = emit_dsl_with_pins("* | where status > 400", &[("status", CT::Varchar)]);
        assert_eq!(a, b);
    }

    /// Pattern ops against a typed pin target the canonical text form —
    /// `matches` over a BIGINT column casts to VARCHAR.
    #[test]
    fn pinned_where_matches_typed_pin_targets_canonical_text() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | where dur matches \"^4\"",
            &[("dur", CT::BigInt)]
        ));
    }

    /// LIKE over a TIMESTAMP pin matches the RFC 3339 strftime text.
    #[test]
    fn pinned_where_like_timestamp_pin_renders_rfc3339_text() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | where ts like \"2026-01-%\"",
            &[("ts", CT::Timestamp)]
        ));
    }

    /// The same rules apply inside `| let` — the comparison is a
    /// SELECT-list value there (TRUE/FALSE/NULL).
    #[test]
    fn pinned_let_comparison_adopts_the_rule_table() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | let is_err = status >= 400",
            &[("status", CT::Varchar)]
        ));
    }

    /// The scope walk: `rename status as st` carries the pin to `st`.
    #[test]
    fn pinned_where_after_rename_is_pin_aware_under_new_name() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | rename status as st | where st > 400",
            &[("status", CT::Varchar)]
        ));
    }

    /// Backticks are lexing, not policy: the ASCII fold still applies, so
    /// `` `Status` `` IS `status` and binds the same catalog pin
    /// (ADR-0013 ruling 7). The SQL identifier stays the verbatim text —
    /// `DuckDB` identifiers are case-insensitive, so it reads the same
    /// column.
    #[test]
    fn backticked_name_folds_to_the_same_pin() {
        let entries = &[("status", CT::Varchar)];
        let quoted = parser::parse("`Status`=200").expect("parse should succeed");
        let bare = parser::parse("status=200").expect("parse should succeed");

        let quoted_sql = emit_with_pins(&quoted, SRC, &pins(entries)).expect("pinned emit");
        let bare_sql = emit_with_pins(&bare, SRC, &pins(entries)).expect("pinned emit");
        let unpinned = emit(&quoted, SRC).expect("unpinned emit");

        assert!(
            quoted_sql.sql.contains(r#""Status""#),
            "the identifier is verbatim: {}",
            quoted_sql.sql
        );
        assert_eq!(
            quoted_sql.sql.replace(r#""Status""#, r#""status""#),
            bare_sql.sql,
            "the folded name binds the VARCHAR pin's rule, spelling aside"
        );
        assert_ne!(
            quoted_sql.sql, unpinned.sql,
            "a pin-blind emission is a different rule — the pin really bound"
        );
    }

    /// The scope walk: a computed `let` kills the pin, so the following
    /// `where` is literal-driven (byte-identical to unpinned emission).
    #[test]
    fn pinned_where_after_computed_let_is_literal_driven() {
        let query = parser::parse("* | let status = length(status) | where status > 400")
            .expect("parse should succeed");
        let pinned =
            emit_with_pins(&query, SRC, &pins(&[("status", CT::Varchar)])).expect("pinned emit");
        let unpinned = emit(&query, SRC).expect("unpinned emit");
        assert_eq!(pinned.sql, unpinned.sql);
        assert_eq!(pinned.params, unpinned.params);
    }

    /// The scope walk: a stats group-by key keeps its pin.
    #[test]
    fn pinned_where_after_stats_group_key_stays_pin_aware() {
        assert_snapshot!(emit_dsl_with_pins(
            "* | stats count() by status | where status == 200",
            &[("status", CT::Varchar)]
        ));
    }

    /// Excluded shapes stay literal-driven, structurally: field-vs-field,
    /// function-wrapped fields, arithmetic on the field, `== null`, and a
    /// pattern with the field as the RIGHT operand.
    #[test]
    fn pinned_where_excluded_shapes_emit_byte_identical_sql() {
        for dsl in [
            "* | where status == other",
            "* | where lower(status) == \"a\"",
            "* | where status * 2 > 400",
            "* | where status == null",
            "* | where \"x\" matches status",
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let pinned = emit_with_pins(
                &query,
                SRC,
                &pins(&[("status", CT::Varchar), ("other", CT::Varchar)]),
            )
            .expect("pinned emit");
            let unpinned = emit(&query, SRC).expect("unpinned emit");
            assert_eq!(pinned.sql, unpinned.sql, "{dsl}");
            assert_eq!(pinned.params, unpinned.params, "{dsl}");
        }
    }

    /// A typed pin's plain comparisons emit byte-identical SQL to the
    /// unpinned emission — the column on disk already IS the type; the
    /// pin travels for the live mirror's sake.
    #[test]
    fn pinned_where_typed_pin_comparison_is_byte_identical() {
        for dsl in ["* | where dur > 400", "* | where dur == 400"] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let pinned =
                emit_with_pins(&query, SRC, &pins(&[("dur", CT::BigInt)])).expect("pinned emit");
            let unpinned = emit(&query, SRC).expect("unpinned emit");
            assert_eq!(pinned.sql, unpinned.sql, "{dsl}");
            assert_eq!(pinned.params, unpinned.params, "{dsl}");
        }
    }

    /// The kv split stamps the scope in force at the boundary, so the
    /// `rust_stages` tail evaluates with the same pins the SQL prefix used
    /// — including a rename's remap before the split.
    #[test]
    fn kv_split_stamps_rust_stage_pins_at_the_boundary() {
        let query = parser::parse("* | rename status as st | extract kv | where st > 400").unwrap();
        let ft = pins(&[("status", CT::Varchar)]);
        let emitted = emit_with_pins(&query, SRC, &ft).unwrap();
        assert_eq!(emitted.rust_stages.len(), 2);
        assert_eq!(
            emitted.rust_stage_pins.pin_for("st"),
            Some(CT::Varchar),
            "the boundary scope carries the renamed pin"
        );
        assert_eq!(emitted.rust_stage_pins.pin_for("status"), None);

        // The pin-blind door stamps an empty scope.
        let blind = emit(&query, SRC).unwrap();
        assert!(blind.rust_stage_pins.is_empty());
    }

    /// Minimality is a tested invariant: outside the changed cells of the
    /// ADR-0011 slice A table, pinned emission is byte-identical to
    /// unpinned emission — a repin changes no other query's meaning.
    #[test]
    fn pinned_emission_is_byte_identical_outside_the_changed_cells() {
        use crate::ast::FilterOp;

        let pin_states: [Option<CT>; 6] = [
            None,
            Some(CT::Varchar),
            Some(CT::BigInt),
            Some(CT::Double),
            Some(CT::Timestamp),
            Some(CT::Boolean),
        ];
        // (dsl template, op class) — `{}` is replaced by the literal.
        let cases: [(&str, FilterOp); 9] = [
            ("f={}", FilterOp::Eq),
            ("f!={}", FilterOp::Ne),
            ("f>{}", FilterOp::Gt),
            ("f>={}", FilterOp::Gte),
            ("f<{}", FilterOp::Lt),
            ("f<={}", FilterOp::Lte),
            ("f={},{}", FilterOp::Eq),     // IN list
            ("f={}*", FilterOp::Glob),     // glob (auto-detected)
            ("f=/{}.*/", FilterOp::Regex), // regex
        ];
        let literals: [(&str, bool); 4] = [
            ("200", true),
            ("1.5", true),
            ("accepted", false),
            ("9999999999999999999", true),
        ];

        for pin in pin_states {
            for (template, op) in cases {
                for (lit, numeric) in literals {
                    let dsl = template.replace("{}", lit);
                    let Ok(query) = parser::parse(&dsl) else {
                        continue;
                    };
                    let unpinned = emit(&query, SRC).expect("unpinned emit");
                    let entries: Vec<(&str, CT)> = pin.map(|t| ("f", t)).into_iter().collect();
                    let pinned = emit_with_pins(&query, SRC, &pins(&entries)).expect("pinned emit");

                    let is_pattern = matches!(op, FilterOp::Glob | FilterOp::Regex);
                    let changed = match pin {
                        Some(CT::Varchar) => {
                            // eq/ne/IN rebind numeric literals as text;
                            // ordered numeric literals move to TRY_CAST.
                            !is_pattern && numeric
                        }
                        Some(_) => is_pattern,
                        None => false,
                    };
                    let identical = pinned.sql == unpinned.sql && pinned.params == unpinned.params;
                    assert_eq!(
                        identical, !changed,
                        "{dsl:?} under pin {pin:?}: expected changed={changed}\n\
                         unpinned sql: {}\nparams: {:?}\n\
                         pinned sql: {}\nparams: {:?}",
                        unpinned.sql, unpinned.params, pinned.sql, pinned.params
                    );
                }
            }
        }
    }

    /// The same minimality invariant over the PIPELINE lane (slice A′):
    /// outside the changed cells — VARCHAR pin × numeric literal for
    /// comparisons, typed pins for patterns — a `| where` emits
    /// byte-identical SQL under pins.
    #[test]
    fn pinned_pipeline_emission_is_byte_identical_outside_the_changed_cells() {
        let pin_states: [Option<CT>; 6] = [
            None,
            Some(CT::Varchar),
            Some(CT::BigInt),
            Some(CT::Double),
            Some(CT::Timestamp),
            Some(CT::Boolean),
        ];
        // (template, is_pattern) — `{}` replaced by the literal, quoted
        // for non-numeric values (a bare word is a field ref in `where`).
        let cases: [(&str, bool); 8] = [
            ("* | where f == {}", false),
            ("* | where f != {}", false),
            ("* | where f > {}", false),
            ("* | where f <= {}", false),
            ("* | where f in ({}, {})", false),
            ("* | let x = f >= {}", false),
            ("* | where f matches \"^a\"", true),
            ("* | where f like \"a%\"", true),
        ];
        let literals: [(&str, bool); 4] = [
            ("200", true),
            ("1.5", true),
            ("\"accepted\"", false),
            ("9999999999999999999", true),
        ];

        for pin in pin_states {
            for (template, is_pattern) in cases {
                for (lit, numeric) in literals {
                    let dsl = template.replace("{}", lit);
                    let Ok(query) = parser::parse(&dsl) else {
                        continue;
                    };
                    let unpinned = emit(&query, SRC).expect("unpinned emit");
                    let entries: Vec<(&str, CT)> = pin.map(|t| ("f", t)).into_iter().collect();
                    let pinned = emit_with_pins(&query, SRC, &pins(&entries)).expect("pinned emit");

                    let changed = match pin {
                        Some(CT::Varchar) => !is_pattern && numeric,
                        Some(_) => is_pattern,
                        None => false,
                    };
                    let identical = pinned.sql == unpinned.sql && pinned.params == unpinned.params;
                    assert_eq!(
                        identical, !changed,
                        "{dsl:?} under pin {pin:?}: expected changed={changed}\n\
                         unpinned sql: {}\nparams: {:?}\n\
                         pinned sql: {}\nparams: {:?}",
                        unpinned.sql, unpinned.params, pinned.sql, pinned.params
                    );
                }
            }
        }
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
