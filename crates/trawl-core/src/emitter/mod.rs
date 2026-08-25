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
pub use functions::{DATE_PART_UNITS, DATE_UNITS};
pub(crate) use functions::{
    format_literal_position, unit_literal_positions, validate_format_literal,
    validate_function_arity, validate_unit_literal,
};
pub use functions::{function_result_pin, is_aggregate_function};
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
    /// The instant this statement's `now()` reads (ADR-0017 §3).
    ///
    /// Captured ONCE per logical query by the caller and stamped here, so
    /// every reader of this emission shares one clock: the SQL prefix
    /// binds it as a TIMESTAMP parameter, and the `rust_stages` tail
    /// behind `extract kv` evaluates under the SAME anchor
    /// ([`crate::context::EvalContext`]). A re-emission of the same
    /// logical query — the executor's hot-only fallback — INHERITS this
    /// value rather than sampling a second one.
    pub anchor: crate::context::EvalContext,
}

/// A parameter value for a SQL query placeholder.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// A naive UTC instant bound as `DuckDB` TIMESTAMP — the statement's
    /// `now()` anchor (ADR-0017 §3). Zoneless because `DuckDB`'s
    /// TIMESTAMP is, and every conforming connection runs under
    /// `TimeZone='UTC'` ([`crate::conform::SESSION_TIME_ZONE_SQL`]).
    Timestamp(chrono::NaiveDateTime),
}

/// The TIMESTAMP literal text a bound [`SqlValue::Timestamp`] denotes —
/// the ONE rendering, shared by [`fmt::Display`] and the PIVOT lane's
/// parameter inlining, so the inlined form and the bound form can never
/// name different instants.
///
/// Typed (`TIMESTAMP '…'`) so the literal has a type wherever it lands,
/// and FIXED six-digit microseconds so no value renders with a precision
/// `DuckDB`'s domain does not hold. Trailing zeros are kept: `DuckDB`
/// parses `…:00.000000` and `…:00` to the same instant, and a fixed width
/// is one rule instead of two.
///
/// The leading `+` chrono puts on a year outside `0..=9999` is STRIPPED:
/// `DuckDB`'s timestamp parser accepts a leading `-` but not a leading
/// `+`, so `+10000-01-01` is a conversion error where the SAME instant
/// bound as a parameter is fine — the two renderings of one anchor
/// disagreeing at the edge of the domain. A production clock never gets
/// there, but [`crate::context::EvalContext::at`] is public. Probed in
/// `trawl-engine/tests/duckdb_probe.rs`.
fn timestamp_literal(at: chrono::NaiveDateTime) -> String {
    let rendered = at.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
    let unsigned = rendered.strip_prefix('+').unwrap_or(&rendered);
    format!("TIMESTAMP '{unsigned}'")
}

impl fmt::Display for SqlValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "'{s}'"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Timestamp(at) => write!(f, "{}", timestamp_literal(*at)),
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
    /// A literal the ADR-0011 pin rule table refuses under the field's
    /// catalog pin. Reads as an unsupported operation, and carries the
    /// typed cause so a caller can classify the refusal without reading
    /// its prose (see the [`From`] impl below).
    Comparison(crate::compare::CompareError),
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
            Self::Comparison(err) => write!(f, "unsupported operation: {err}"),
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
///
/// The SENTENCE is unchanged: [`EmitError::Comparison`] renders through
/// the same `unsupported operation: {…}` arm the stringified form used to
/// take, so no user-facing text, snapshot or wire message moves. What
/// changed is that the cause survives as a TYPE instead of as prose. The
/// pin-aware fuzz target (issue #114) has to decide whether an emitter
/// refusal is a legitimate outcome for the pin map it invented — an
/// `_severity` pin plus a literal naming no ladder point is expected, a
/// panic never is — and that verdict must not be a substring test against
/// an error message anyone is free to reword.
impl From<crate::compare::CompareError> for EmitError {
    fn from(err: crate::compare::CompareError) -> Self {
        Self::Comparison(err)
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
///
/// `anchor` is the statement's `now()` instant (ADR-0017 §3): capture it
/// ONCE per logical query, at the head of the operation, and pass the
/// same value to every emission that operation performs.
pub fn emit(
    query: &Query,
    source: &str,
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || EmitterState::new(source, anchor))
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
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        Ok(EmitterState::new(source, anchor)?.with_compare_pins(pins))
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
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        Ok(
            EmitterState::with_hot_source(source, hot_source, hot_pins, anchor)?
                .with_compare_pins(pins),
        )
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
/// The `anchor` is the ORIGINAL emission's
/// ([`EmittedQuery::anchor`]), never a fresh capture: this lane re-emits
/// one logical query against a narrower source, so re-sampling the clock
/// here would make the fallback answer a `now()` comparison differently
/// from the union attempt it replaces.
pub fn emit_hot_only(
    query: &Query,
    hot_source: &str,
    hot_pins: &crate::schema::FieldTypes,
    pins: &crate::schema::FieldTypes,
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_with_raw_fallback(query, || {
        Ok(
            EmitterState::with_hot_only_source(hot_source, hot_pins, anchor)?
                .with_compare_pins(pins),
        )
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

        // A pending pivot is flushed to a CTE before ANY following
        // stage, so that stage reads the pivot's dynamic output columns.
        //
        // Including another PIVOT. The exception that used to sit here
        // (`&& !matches!(stage.node, PipeStage::Pivot(_))`) dates from
        // the commit that lifted the pivot-must-be-terminal restriction
        // and predates any pivot-of-pivot case: `process_pivot` opens
        // with an ORDINARY `flush_to_cte`, whose `build_select` does not
        // render `self.pivot`, and then OVERWRITES the pending spec —
        // so skipping the flush silently dropped the first pivot and ran
        // the second over pre-pivot input. A pivot that is TERMINAL is
        // still never flushed here (no stage follows it); `finalize`
        // renders it.
        if state.has_pivot() {
            state.flush_pivot_to_cte()?;
        }
        pipeline::process_stage(&stage.node, &mut state)?;
        // AFTER the stage: its own expressions resolve against the
        // incoming schema; the next stage sees this one's output scope.
        state.advance_pin_scope(&stage.node);
    }

    let needs_column_reorder = state.needs_column_reorder();
    let referenced_raw = state.bound_raw_column();
    let anchor = state.anchor();
    let sql = state.finalize()?;
    let params = state.into_params();

    Ok((
        EmittedQuery {
            sql,
            params,
            rust_stages,
            rust_stage_pins,
            needs_column_reorder,
            raw_free_sql: None,
            anchor,
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

    /// The instant emitter tests emit under.
    ///
    /// FIXED, not captured: `now()` binds the anchor as a parameter
    /// (ADR-0017 §3), so a snapshot of a query carrying one would
    /// otherwise change on every run.
    fn anchor() -> crate::context::EvalContext {
        crate::context::EvalContext::at(
            chrono::DateTime::parse_from_rfc3339("2026-02-03T04:05:06.789012Z")
                .expect("a valid RFC 3339 instant")
                .into(),
        )
    }

    /// Parse a DSL string and emit SQL; format both for snapshot comparison.
    fn emit_dsl(input: &str) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        let result = emit(&query, SRC, anchor()).expect("emit should succeed");
        format_result(&result)
    }

    /// Parse and emit, expecting an `EmitError`; return its Display string.
    fn emit_dsl_err(input: &str) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        let err = emit(&query, SRC, anchor()).expect_err("emit should fail");
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
    fn search_negated_list() {
        assert_snapshot!(emit_dsl("status!=200,301,404"));
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
            emit(&query, SRC, anchor()).unwrap_or_else(|e| panic!("{dsl} must emit, got {e}"));
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
        for dsl in [
            "boom",
            r#""boom error""#,
            "-boom service=nginx",
            "NOT boom",
            r#"NOT "boom error""#,
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
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
            let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
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
    // backtick-quoted names (ADR-0013 ruling 7)
    // -----------------------------------------------------------------------

    /// A backticked name reaches the SQL as the verbatim identifier it
    /// spells — the quotes are lexing, and `quote_field` is still the
    /// only thing between the name and the query.
    #[test]
    fn backticked_table_fields_quote_verbatim() {
        assert_snapshot!(emit_dsl("* | table `request id`, `http-status`"));
    }

    #[test]
    fn backticked_group_key_quotes_verbatim() {
        assert_snapshot!(emit_dsl("* | stats count() by `where`"));
    }

    /// `last=` is still the time filter; the backticked spelling is the
    /// field, and both survive in one query.
    #[test]
    fn backticked_keyword_field_filter_beside_the_time_filter() {
        assert_snapshot!(emit_dsl("`last`=5 last=2h"));
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
        let err = emit(&query, "/data/foo';DROP TABLE x;--/*.parquet", anchor())
            .expect_err("should reject single quote in source path");
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn rejects_source_with_semicolon() {
        let query = parser::parse("*").unwrap();
        let err = emit(&query, "/data/foo;bar.parquet", anchor())
            .expect_err("should reject semicolon in source path");
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn accepts_valid_glob_source() {
        let query = parser::parse("*").unwrap();
        let result = emit(&query, "/data/**/*.parquet", anchor());
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_tilde_source() {
        let query = parser::parse("*").unwrap();
        let result = emit(&query, "~/.trawl/data/*.parquet", anchor());
        assert!(result.is_err());
    }

    #[test]
    fn rejects_dotdot_source() {
        let query = parser::parse("*").unwrap();
        let result = emit(&query, "/data/../etc/passwd/*.parquet", anchor());
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

    /// The overwriting wildcard folds ASCII and nothing else, exactly as
    /// `DuckDB` binds identifiers: a backtickable non-ASCII target must
    /// not exclude a differently-cased non-ASCII column the query never
    /// named (`lower()` on both sides used to delete it silently).
    #[test]
    fn let_wildcard_folds_ascii_only() {
        let sql = emit_dsl("* | let `Ü` = 1, `HOST` = 2");
        assert!(sql.contains("NOT IN ('Ü', 'host')"), "{sql}");
        assert!(!sql.contains("'ü'"), "{sql}");
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

    /// `pivot` inlines every `?` because `DuckDB` cannot bind a PIVOT —
    /// and a backticked name may now contain a `?` of its own (ADR-0013
    /// ruling 7). Only a placeholder OUTSIDE a quoted region may be
    /// spliced: splicing into the identifier or the COLUMNS lambda string
    /// would corrupt the name AND shift every later binding, spilling the
    /// user's value into the statement with only `'` escaped.
    #[test]
    fn pivot_inlining_never_splices_into_a_quoted_region() {
        let query = parser::parse(r#"* | let `a?b` = "v" | pivot count() on status"#)
            .expect("parse should succeed");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(
            emitted.sql.contains(r"NOT IN ('a?b')"),
            "lambda name corrupted: {}",
            emitted.sql
        );
        assert!(
            emitted.sql.contains(r#"('v') AS "a?b""#),
            "value must land at the placeholder, name intact: {}",
            emitted.sql
        );
        assert!(emitted.params.is_empty(), "pivot inlines every param");

        let query = parser::parse("`a?b`=v | pivot count() on status").expect("parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(
            emitted.sql.contains(r#""a?b" = 'v'"#),
            "filter corrupted: {}",
            emitted.sql
        );

        // Not a backtick story: a `?` glob in the source path sits in a
        // string literal too.
        let query = parser::parse("service=nginx | pivot count() on status").expect("parse");
        let globbed =
            emit(&query, "/data/2026-01-0?/*.parquet", anchor()).expect("emit should succeed");
        assert!(
            globbed.sql.contains("'/data/2026-01-0?/*.parquet'"),
            "source glob corrupted: {}",
            globbed.sql
        );
        assert!(
            globbed.sql.contains("= 'nginx'"),
            "filter value must still inline: {}",
            globbed.sql
        );
    }

    /// A multiline filter remains a bound value through ordinary CTE
    /// finalization. This pins the parameter bytes before the PIVOT lane has
    /// to inline the same value.
    #[test]
    fn cte_finalization_preserves_a_multiline_parameter() {
        let query = parser::parse("message=\"a\nb\" | stats count()")
            .expect("multiline string should parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert_eq!(emitted.params, [SqlValue::String("a\nb".to_owned())]);
        assert!(
            emitted.sql.contains(r#""message" = ?"#),
            "filter placeholder missing: {}",
            emitted.sql
        );
    }

    /// Once a stage follows PIVOT, the inlined PIVOT is finalized as a CTE.
    /// Indentation belongs to SQL lines, never to bytes inside the literal.
    #[test]
    fn pivot_cte_finalization_preserves_a_multiline_literal() {
        let query = parser::parse("message=\"a\nb\" | pivot count() on status | sort `200`")
            .expect("multiline string should parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(emitted.params.is_empty(), "PIVOT inlines every parameter");
        assert!(
            emitted.sql.contains("\"message\" = 'a\nb'"),
            "literal bytes changed during finalization: {}",
            emitted.sql
        );
        assert!(
            !emitted.sql.contains("\"message\" = 'a\n  b'"),
            "CTE indentation leaked into the literal: {}",
            emitted.sql
        );
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
            anchor(),
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
            anchor(),
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
            anchor(),
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
            anchor(),
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
            anchor(),
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
            anchor(),
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
        let result =
            emit_with_pins(&query, SRC, &pins(entries), anchor()).expect("emit should succeed");
        format_result(&result)
    }

    /// Parse and emit pin-aware SQL, expecting an `EmitError`.
    fn emit_dsl_err_with_pins(
        input: &str,
        entries: &[(&str, crate::schema::CanonicalType)],
    ) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        emit_with_pins(&query, SRC, &pins(entries), anchor())
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

    /// A whole severity list is ONE membership test over the ladder
    /// points its bands cover — the subject written once, however many
    /// bands the list names (issue #82).
    #[test]
    fn pinned_severity_in_list_binds_the_subject_once() {
        assert_snapshot!(emit_dsl_with_pins("_severity=warn,error", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_negated_list_composes_scalar_widening() {
        assert_snapshot!(emit_dsl_with_pins("_severity!=warn,error", &SEVERITY_PIN));
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

    #[test]
    fn pinned_varchar_negated_list_composes_scalar_equalities() {
        assert_snapshot!(emit_dsl_with_pins(
            "status!=200,301",
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
            let emitted = emit_with_pins(&query, SRC, &ft, anchor()).expect("emit should succeed");
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
            anchor(),
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

        let quoted_sql =
            emit_with_pins(&quoted, SRC, &pins(entries), anchor()).expect("pinned emit");
        let bare_sql = emit_with_pins(&bare, SRC, &pins(entries), anchor()).expect("pinned emit");
        let unpinned = emit(&quoted, SRC, anchor()).expect("unpinned emit");

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
        let pinned = emit_with_pins(&query, SRC, &pins(&[("status", CT::Varchar)]), anchor())
            .expect("pinned emit");
        let unpinned = emit(&query, SRC, anchor()).expect("unpinned emit");
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
                anchor(),
            )
            .expect("pinned emit");
            let unpinned = emit(&query, SRC, anchor()).expect("unpinned emit");
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
            let pinned = emit_with_pins(&query, SRC, &pins(&[("dur", CT::BigInt)]), anchor())
                .expect("pinned emit");
            let unpinned = emit(&query, SRC, anchor()).expect("unpinned emit");
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
        let emitted = emit_with_pins(&query, SRC, &ft, anchor()).unwrap();
        assert_eq!(emitted.rust_stages.len(), 2);
        assert_eq!(
            emitted.rust_stage_pins.pin_for("st"),
            Some(CT::Varchar),
            "the boundary scope carries the renamed pin"
        );
        assert_eq!(emitted.rust_stage_pins.pin_for("status"), None);

        // The pin-blind door stamps an empty scope.
        let blind = emit(&query, SRC, anchor()).unwrap();
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
                    let unpinned = emit(&query, SRC, anchor()).expect("unpinned emit");
                    let entries: Vec<(&str, CT)> = pin.map(|t| ("f", t)).into_iter().collect();
                    let pinned = emit_with_pins(&query, SRC, &pins(&entries), anchor())
                        .expect("pinned emit");

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
                    let unpinned = emit(&query, SRC, anchor()).expect("unpinned emit");
                    let entries: Vec<(&str, CT)> = pin.map(|t| ("f", t)).into_iter().collect();
                    let pinned = emit_with_pins(&query, SRC, &pins(&entries), anchor())
                        .expect("pinned emit");

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
            anchor(),
        )
        .unwrap();
        assert_snapshot!(format_result(&result));
    }

    #[test]
    fn list_source_rejects_invalid_path() {
        let query = parser::parse("*").unwrap();
        let err = emit(
            &query,
            "['/data/ok/*.parquet', '/data/bad;drop/*.parquet']",
            anchor(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn list_source_rejects_malformed_list() {
        let query = parser::parse("*").unwrap();
        let err = emit(&query, "[not-quoted]", anchor()).unwrap_err();
        assert!(err.to_string().contains("invalid source list element"));
    }

    // -----------------------------------------------------------------------
    // sev() — the ladder function (ADR-0013 slice 2, ruling 9)
    // -----------------------------------------------------------------------

    /// `sev(x)` emits the reading kernel over the argument's TEXT form,
    /// bound ONCE — the subject may carry parameters, and the emitter
    /// pushes one value per call, not per occurrence.
    #[test]
    fn sev_emits_the_reading_kernel_over_the_arguments_text() {
        let sql = emit_dsl("* | let s = sev(level)");
        assert!(
            sql.contains(&crate::conform::severity_reading_sql_bind_once(
                &crate::conform::untyped_text("\"level\""),
                crate::severity::Dialect::Otel
            )),
            "{sql}"
        );
    }

    /// The dialect argument SELECTS the expression and is never bound as a
    /// parameter — the emitted SQL differs, and no `?` is spent on it.
    #[test]
    fn sev_dialect_argument_selects_the_expression_and_binds_nothing() {
        let otel = emit_dsl("* | let s = sev(level)");
        let syslog = emit_dsl(r#"* | let s = sev(level, "syslog")"#);
        assert_ne!(otel, syslog);
        assert!(
            !syslog.contains("params:"),
            "the dialect bound a param: {syslog}"
        );
        assert!(
            syslog.contains(&crate::conform::severity_reading_sql_bind_once(
                &crate::conform::untyped_text("\"level\""),
                crate::severity::Dialect::Syslog
            )),
            "{syslog}"
        );
        // Case-insensitive, like every other token in the vocabulary.
        assert_eq!(emit_dsl(r#"* | let s = sev(level, "SYSLOG")"#), syslog);
        assert_eq!(emit_dsl(r#"* | let s = sev(level, "OTel")"#), otel);
    }

    /// Issue #82: a severity predicate names its subject once PER
    /// CONTIGUOUS RANGE — which is once outright for every natural query,
    /// because a band, a run of bands and a band-plus-adjacent-point all
    /// merge into ONE range.
    ///
    /// The counting rule is the whole point of the merge: the pre-#82
    /// rendering wrote the subject once per BAND (six copies for the six
    /// base bands), and the subject is over a kilobyte of `sev()` SQL.
    ///
    /// That `sev()` substring is derived from the ONE builder that emits it
    /// (`conform::severity_reading_sql_bind_once` over
    /// `conform::untyped_text`), never hand-typed: a substring that drifts
    /// from the emitter would pass this test while asserting nothing.
    #[test]
    fn severity_predicates_render_the_subject_once_per_range() {
        let column = r#""_severity""#;
        let sev_subject = crate::conform::severity_reading_sql_bind_once(
            &crate::conform::untyped_text(r#""level""#),
            crate::severity::Dialect::Otel,
        );
        // The bare-column cases, all contiguous: ERROR alone, WARN+ERROR
        // (13-20), and WARN plus the adjacent point 17 (13-17).
        for dsl in [
            "_severity=error",
            "_severity=warn,error",
            "_severity=warn,17",
            r#"* | where _severity != "error""#,
        ] {
            let sql = emit_dsl_with_pins(dsl, &SEVERITY_PIN);
            assert_eq!(sql.matches(column).count(), 1, "{dsl}: {sql}");
        }
        // A genuinely DISJOINT selection is the documented exception: WARN
        // (13-16) and FATAL (21-24) share no boundary, so there are two
        // ranges and therefore two subjects. Nothing can merge them
        // without changing what matches.
        let disjoint = emit_dsl_with_pins("_severity=warn,fatal", &SEVERITY_PIN);
        assert_eq!(disjoint.matches(column).count(), 2, "{disjoint}");
        assert!(
            disjoint.contains(&format!(
                "({column} BETWEEN 13 AND 16 OR {column} BETWEEN 21 AND 24)"
            )),
            "{disjoint}"
        );
        // The one documented second occurrence for a CONTIGUOUS shape: the
        // search stage's `!=` widening is `(pred OR "_severity" IS NULL)`,
        // so the column appears twice — once as the range subject, once
        // inside the widening the pre-existing NULL policy owns.
        let widened = emit_dsl_with_pins("_severity!=error", &SEVERITY_PIN);
        assert_eq!(widened.matches(column).count(), 2, "{widened}");
        assert_eq!(
            widened.matches(&format!("{column} IS NULL")).count(),
            1,
            "{widened}"
        );
        assert_eq!(
            widened
                .matches(&format!("NOT ({column} BETWEEN 17 AND 20)"))
                .count(),
            1,
            "{widened}"
        );
        // A singleton run renders as an equality, not a degenerate range.
        let singleton = emit_dsl_with_pins("_severity=error2,error2", &SEVERITY_PIN);
        assert!(singleton.contains(&format!("{column} = 18")), "{singleton}");
        // The expensive subject: a `sev()` call is over a kilobyte of SQL,
        // and a contiguous comparison writes it once however many bands it
        // names — `error`+`fatal` is 17-24, one range.
        assert!(
            sev_subject.len() > 500,
            "the subject is the cost: {sev_subject}"
        );
        for dsl in [
            r#"* | where sev(level) in ("error", "fatal")"#,
            r#"* | where sev(level) in ("trace", "debug", "info", "warn", "error", "fatal")"#,
            r#"* | where sev(level) == "error""#,
            r#"* | where sev(level) != "error""#,
        ] {
            let sql = emit_dsl(dsl);
            assert_eq!(sql.matches(sev_subject.as_str()).count(), 1, "{dsl}: {sql}");
        }
        // …and twice for the disjoint one, for the same structural reason.
        let sql = emit_dsl(r#"* | where sev(level) in ("warn", "fatal")"#);
        assert_eq!(sql.matches(sev_subject.as_str()).count(), 2, "{sql}");
    }

    /// Review finding A: out-of-ladder points cannot amplify the render.
    ///
    /// A `SEVERITY` subject evaluates to 1-24 or NULL, so every point
    /// outside that range is unmatchable and they are all interchangeable
    /// — the renderer keeps exactly ONE. Without the collapse each
    /// non-adjacent literal would be its own run carrying its own ~1.2 KB
    /// copy of a `sev()` subject, which the 64 KB query-text cap does not
    /// bound.
    #[test]
    fn out_of_ladder_severity_points_render_one_representative() {
        let column = r#""_severity""#;
        // Entirely out of ladder: ONE comparison, the smallest point.
        let all_out = emit_dsl_with_pins("_severity=99,101,250", &SEVERITY_PIN);
        assert_eq!(all_out.matches(column).count(), 1, "{all_out}");
        assert!(all_out.contains(&format!("{column} = 99")), "{all_out}");
        // Mixed: the in-ladder band stands, plus the one representative.
        let mixed = emit_dsl_with_pins("_severity=error,99,101,250", &SEVERITY_PIN);
        assert_eq!(mixed.matches(column).count(), 2, "{mixed}");
        assert!(
            mixed.contains(&format!("({column} BETWEEN 17 AND 20 OR {column} = 99)")),
            "{mixed}"
        );
        // A representative ADJACENT to an in-ladder run extends it rather
        // than emitting separately: 0 never matches a stored severity, so
        // `BETWEEN 0 AND 4` accepts exactly the TRACE band. The negation
        // of this shape is the one that would expose an unsound widening,
        // and it is exhaustively parity-tested in `filter_parity.rs`.
        let adjacent = emit_dsl_with_pins("_severity=0,trace", &SEVERITY_PIN);
        assert!(
            adjacent.contains(&format!("{column} BETWEEN 0 AND 4")),
            "{adjacent}"
        );
        assert_eq!(adjacent.matches(column).count(), 1, "{adjacent}");
        // THE AMPLIFICATION GUARD: a long alternating in/out list renders
        // at most 13 subjects — 12 possible in-ladder runs plus the one
        // representative — however many literals it names.
        let mut list: Vec<String> = Vec::new();
        for n in (1..=23).step_by(2) {
            list.push(n.to_string());
        }
        for n in (100..400).step_by(2) {
            list.push(n.to_string());
        }
        let long = format!("_severity={}", list.join(","));
        let sql = emit_dsl_with_pins(&long, &SEVERITY_PIN);
        assert!(list.len() > 150, "the input must actually be long");
        assert!(
            sql.matches(column).count() <= 13,
            "{} subjects for {} literals: {sql}",
            sql.matches(column).count(),
            list.len()
        );
        // The same bound over the EXPENSIVE subject, which is the case the
        // finding is about.
        let sev_subject = crate::conform::severity_reading_sql_bind_once(
            &crate::conform::untyped_text(r#""level""#),
            crate::severity::Dialect::Otel,
        );
        let sql = emit_dsl(&format!("* | where sev(level) in ({})", list.join(", ")));
        assert!(
            sql.matches(sev_subject.as_str()).count() <= 13,
            "{} subjects for {} literals",
            sql.matches(sev_subject.as_str()).count(),
            list.len()
        );
    }

    /// Issue #82, AC3: the severity set is INLINED, so it binds nothing
    /// between a subject's parameters and whatever follows.
    ///
    /// The set is a SINGLE run here on purpose (review finding B). A
    /// multi-run set repeats the subject, so a subject carrying `?`
    /// placeholders would emit more placeholders than parameters were
    /// pushed — that shape is out of contract, documented at
    /// `severity_ranges_sql`, and structurally unreachable (see
    /// [`severity_subjects_never_push_parameters`]). Asserting positions
    /// over a repeated subject would document a contract the renderer
    /// does not hold.
    #[test]
    fn a_severity_set_pushes_no_parameters_and_preserves_positions() {
        let mut state = EmitterState::new("/data/**/*.parquet", anchor()).expect("source");
        let before = state.push_param(SqlValue::String("before".to_owned()));
        let subject = format!("upper({before})");
        let clause = compare::in_list_sql(
            &subject,
            vec![
                crate::compare::CompareForm::SeverityBand { lo: 17, hi: 20 },
                crate::compare::CompareForm::SeverityExact(18),
            ],
            &mut state,
        );
        let after = state.push_param(SqlValue::String("after".to_owned()));
        // 18 sits inside the ERROR band, so this is ONE run and the
        // subject is written exactly once.
        assert_eq!(clause, format!("{subject} BETWEEN 17 AND 20"));
        assert_eq!(clause.matches(&subject).count(), 1, "{clause}");
        // Placeholders are positional `?`, so the ORDER of the collected
        // params is the whole assertion: the set bound nothing between
        // them, and `after` is still the second value.
        assert_eq!(before, "?");
        assert_eq!(after, "?");
        assert_eq!(
            state.into_params(),
            vec![
                SqlValue::String("before".to_owned()),
                SqlValue::String("after".to_owned())
            ]
        );
    }

    /// The real regression guard behind AC3 (review finding B): NO subject
    /// the pin scope admits can push a bound parameter, so the multi-run
    /// repetition can never desynchronize placeholders from parameters.
    ///
    /// `PinScope::subject_pin` admits exactly two shapes — a bare pinned
    /// field, and a pin-declaring call over a bare field whose remaining
    /// arguments are string literals — and `sev()`'s dialect is inlined as
    /// TEXT (`functions::literal_text_positions`) rather than bound. This
    /// asserts that end to end: a severity predicate emits no parameters
    /// at all, in the search stage and in both operand orders of the
    /// pipeline, for single-run AND multi-run sets.
    ///
    /// The last block ties the guard to the TABLE rather than to the one
    /// name `sev`: every `KNOWN_FUNCTIONS` entry that declares a SEVERITY
    /// result is admitted by `subject_pin`, so a future Severity-returning
    /// function joins this test automatically — and if its argument shape
    /// makes the probe DSL unemittable, the panic names the obligation
    /// instead of silently covering nothing.
    #[test]
    fn severity_subjects_never_push_parameters() {
        for dsl in [
            "_severity=error",
            "_severity=warn,fatal",
            "_severity!=error",
            "_severity=99,101",
        ] {
            let sql = emit_dsl_with_pins(dsl, &SEVERITY_PIN);
            assert!(!sql.contains("\n  0: "), "{dsl} bound a parameter: {sql}");
        }
        for dsl in [
            r#"* | where sev(level) == "error""#,
            r#"* | where "error" == sev(level)"#,
            r#"* | where sev(level) in ("warn", "fatal")"#,
            r#"* | where sev(level, "syslog") in ("warn", "fatal")"#,
            r#"* | where sev(level) != "error""#,
        ] {
            let sql = emit_dsl(dsl);
            assert!(!sql.contains("\n  0: "), "{dsl} bound a parameter: {sql}");
        }

        // Structural: derive the covered functions from the same table the
        // emitter validates against, not from a hand-written list.
        let severity_fns: Vec<&str> = crate::parser::suggest::KNOWN_FUNCTIONS
            .iter()
            .copied()
            .filter(|f| {
                functions::function_result_pin(f) == Some(crate::schema::CanonicalType::Severity)
            })
            .collect();
        assert!(
            severity_fns.contains(&"sev"),
            "the SEVERITY-declaring set must not be empty, or this guard covers nothing"
        );
        for func in severity_fns {
            for dsl in [
                format!(r#"* | where {func}(level) == "error""#),
                format!(r#"* | where {func}(level) in ("warn", "fatal")"#),
            ] {
                let query = parser::parse(&dsl).unwrap_or_else(|e| {
                    panic!(
                        "{func} declares a SEVERITY result, so it is a comparison SUBJECT and \
                         must be covered by this guard — but the probe DSL {dsl:?} does not \
                         parse ({e:?}). Give the guard a shape that fits {func}'s arguments."
                    )
                });
                let emitted = emit(&query, SRC, anchor()).unwrap_or_else(|e| {
                    panic!(
                        "{func} declares a SEVERITY result but the probe DSL {dsl:?} does not \
                         emit ({e}). Give the guard a shape that fits {func}'s arguments."
                    )
                });
                assert!(
                    emitted.params.is_empty(),
                    "{dsl} bound {} parameter(s): a subject that pushes parameters cannot be \
                     repeated per range — see severity_ranges_sql's contract",
                    emitted.params.len()
                );
            }
        }
    }

    /// A literal subject carries its own type: `to_json(?)` gives `DuckDB`
    /// nothing to infer a parameter's type from.
    #[test]
    fn sev_over_a_literal_types_its_parameter() {
        let sql = emit_dsl(r#"* | let s = sev("error")"#);
        assert_eq!(
            sql.matches("CAST(? AS VARCHAR)").count(),
            1,
            "one occurrence, one param: {sql}"
        );
        assert!(sql.contains("\n  0: "), "one bound param: {sql}");
        assert!(!sql.contains("\n  1: "), "only one bound param: {sql}");
        let sql = emit_dsl("* | let s = sev(7)");
        assert_eq!(sql.matches("CAST(? AS BIGINT)").count(), 1, "{sql}");
        assert!(sql.contains("\n  0: "), "one bound param: {sql}");
        assert!(!sql.contains("\n  1: "), "only one bound param: {sql}");
    }

    /// The dialect vocabulary is closed, and the refusal NAMES it.
    #[test]
    fn sev_refuses_an_unknown_dialect() {
        let err = emit_dsl_err(r#"* | let s = sev(level, "rfc5424")"#);
        assert!(err.contains("otel, syslog"), "{err}");
        assert!(err.contains("dialect"), "{err}");
    }

    /// A computed dialect can never be honoured — the stream lane resolves
    /// no expressions at compile time — so it is refused in both.
    #[test]
    fn sev_refuses_a_non_literal_dialect() {
        let err = emit_dsl_err("* | let s = sev(level, other)");
        assert!(
            err.contains("must be a string literal dialect name"),
            "{err}"
        );
    }

    /// Arity BEFORE vocabulary: a third argument is an arity error, not a
    /// complaint about the second.
    #[test]
    fn sev_reports_arity_before_vocabulary() {
        let err = emit_dsl_err(r#"* | let s = sev(level, "otel", 1)"#);
        assert!(err.contains("sev() requires 1 to 2 arguments"), "{err}");
        let err = emit_dsl_err("* | let s = sev()");
        assert!(err.contains("sev() requires 1 to 2 arguments"), "{err}");
    }

    /// `sev` is a SCALAR: it gains no aggregate status, so `stats sev(x)`
    /// is projected exactly as any other scalar in that position is.
    #[test]
    fn sev_is_not_an_aggregate() {
        assert!(!is_aggregate_function("sev"));
        let sql = emit_dsl("* | stats sev(level)");
        assert!(sql.contains("AS \"sev_level\""), "{sql}");
        assert!(!sql.contains("GROUP BY"), "{sql}");
    }

    // ── the now() anchor (ADR-0017 §3) ──────────────────────────────────

    /// One instant per STATEMENT: `now()` in two positions binds the same
    /// value twice, so a `let` and a `where` in one query cannot see
    /// different clocks.
    #[test]
    fn two_now_calls_bind_one_instant() {
        let query = parser::parse(
            "* | let age = date_diff(\"second\", _time, now()) | where _time < now()",
        )
        .expect("parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        let bound: Vec<&SqlValue> = emitted
            .params
            .iter()
            .filter(|p| matches!(p, SqlValue::Timestamp(_)))
            .collect();
        assert_eq!(
            bound.len(),
            2,
            "one parameter per call site: {:?}",
            emitted.params
        );
        assert_eq!(bound[0], bound[1], "both name the statement's anchor");
        assert_eq!(
            *bound[0],
            SqlValue::Timestamp(anchor().now_timestamp()),
            "and the anchor is the caller's, not a fresh clock read"
        );
        assert_eq!(emitted.anchor, anchor(), "stamped onto the emission");
        assert_eq!(
            emitted.sql.matches("CAST(? AS TIMESTAMP)").count(),
            2,
            "{}",
            emitted.sql
        );
    }

    /// PIVOT cannot take parameters, so its lane INLINES them — the
    /// anchor lands as the typed literal that denotes the very instant
    /// the bound form does.
    #[test]
    fn a_pivot_inlines_the_anchor_as_a_typed_literal() {
        let query = parser::parse("* | pivot max(now()) on status").expect("parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(
            emitted
                .sql
                .contains("TIMESTAMP '2026-02-03 04:05:06.789012'"),
            "{}",
            emitted.sql
        );
        assert!(
            !emitted.sql.contains('?'),
            "a pivot leaves no placeholder behind: {}",
            emitted.sql
        );
    }

    /// The `Display` rendering and the PIVOT inlining are the SAME text —
    /// they have to be, or the two lanes would name different instants.
    #[test]
    fn the_inlined_and_displayed_anchor_agree() {
        let value = SqlValue::Timestamp(anchor().now_timestamp());
        assert_eq!(value.to_string(), "TIMESTAMP '2026-02-03 04:05:06.789012'");
    }

    /// A year outside `0..=9999` renders UNSIGNED (#106, review F5).
    ///
    /// chrono writes `+10000-01-01`, which `DuckDB`'s parser refuses
    /// while accepting the same year unsigned and a negative year signed
    /// — so the sign would make the inlined PIVOT literal fail for an
    /// instant the bound parameter handles. Executed against the engine
    /// in `trawl-engine/tests/duckdb_probe.rs`.
    #[test]
    fn a_year_outside_four_digits_renders_without_a_plus() {
        let at = |year: i32| {
            chrono::NaiveDate::from_ymd_opt(year, 1, 1)
                .expect("a valid date")
                .and_hms_micro_opt(0, 0, 0, 0)
                .expect("a valid time")
        };
        assert_eq!(
            SqlValue::Timestamp(at(10_000)).to_string(),
            "TIMESTAMP '10000-01-01 00:00:00.000000'"
        );
        assert_eq!(
            SqlValue::Timestamp(at(9_999)).to_string(),
            "TIMESTAMP '9999-01-01 00:00:00.000000'"
        );
        // A negative year KEEPS its sign — that spelling `DuckDB` reads.
        assert_eq!(
            SqlValue::Timestamp(at(-1)).to_string(),
            "TIMESTAMP '-0001-01-01 00:00:00.000000'"
        );
    }

    /// A whole-second anchor still renders six fractional digits — one
    /// width, so no value can render at a precision `DuckDB` does not
    /// hold.
    #[test]
    fn a_whole_second_anchor_renders_six_digits() {
        let at = crate::context::EvalContext::at(
            chrono::DateTime::parse_from_rfc3339("2026-02-03T04:05:06Z")
                .expect("a valid RFC 3339 instant")
                .into(),
        );
        assert_eq!(
            SqlValue::Timestamp(at.now_timestamp()).to_string(),
            "TIMESTAMP '2026-02-03 04:05:06.000000'"
        );
    }

    /// The ordering guard is STRUCTURAL: an aggregate that pushed a
    /// parameter gets the pending predicate flushed into a CTE, so the
    /// CTE's placeholder renders before the SELECT list's and the
    /// positional binding matches the parameter list.
    #[test]
    fn an_aggregate_that_pushes_a_parameter_flushes_the_predicate_first() {
        let query = parser::parse("service=nginx | stats max(now()) as n").expect("parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(emitted.sql.starts_with("WITH _s0 AS ("), "{}", emitted.sql);
        let predicate = emitted
            .sql
            .find("WHERE \"service\" = ?")
            .expect("the predicate keeps its placeholder");
        let aggregate = emitted
            .sql
            .find("MAX(CAST(? AS TIMESTAMP))")
            .expect("the aggregate keeps its placeholder");
        assert!(
            predicate < aggregate,
            "the predicate's placeholder must render FIRST: {}",
            emitted.sql
        );
        assert_eq!(
            emitted.params,
            vec![
                SqlValue::String("nginx".to_owned()),
                SqlValue::Timestamp(anchor().now_timestamp()),
            ],
            "and the parameter list must run in that same order"
        );
    }

    /// …and only then. An aggregate that pushes nothing keeps the plain
    /// single-SELECT shape — the commonest query in the language must
    /// not grow a pointless CTE.
    #[test]
    fn an_aggregate_that_pushes_nothing_is_left_unnested() {
        let query = parser::parse("service=nginx | stats count() by host").expect("parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(
            !emitted.sql.contains("WITH "),
            "no parameter was pushed into the SELECT list: {}",
            emitted.sql
        );
        assert_eq!(emitted.params, vec![SqlValue::String("nginx".to_owned())]);
    }

    /// The same guard on `timechart`, whose aggregate SELECT list is
    /// built the same way.
    #[test]
    fn a_timechart_aggregate_parameter_flushes_the_predicate_first() {
        let query =
            parser::parse("service=nginx | timechart span=1m max(now()) as n").expect("parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(emitted.sql.starts_with("WITH _s0 AS ("), "{}", emitted.sql);
        assert_eq!(
            emitted.params,
            vec![
                SqlValue::String("nginx".to_owned()),
                SqlValue::Timestamp(anchor().now_timestamp()),
            ]
        );
    }

    /// The bug the guard fixes is not the anchor's: an ordinary literal
    /// inside an aggregate argument has always pushed a parameter into
    /// the SELECT list, and mis-bound the same way.
    #[test]
    fn an_aggregate_literal_argument_takes_the_same_guard() {
        let query =
            parser::parse("service=nginx | stats max(substr(message, 1, 3)) as m").expect("parse");
        let emitted = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(emitted.sql.starts_with("WITH _s0 AS ("), "{}", emitted.sql);
        assert_eq!(
            emitted.params,
            vec![
                SqlValue::String("nginx".to_owned()),
                SqlValue::Int(1),
                SqlValue::Int(3),
            ]
        );
    }

    /// A refused pinned comparison keeps its cause as a TYPE and renders
    /// the sentence it always rendered.
    ///
    /// Both halves matter and they pull in opposite directions. The type
    /// is what the pin-aware fuzz target classifies on (issue #114): a
    /// `SEVERITY` pin plus a literal naming no ladder point is a
    /// legitimate outcome, so the target must recognize it without
    /// matching prose. The TEXT is what users, snapshots and the wire
    /// already see, so the `unsupported operation: ` prefix stays exactly
    /// where the stringifying `From` impl used to put it.
    #[test]
    fn a_refused_pinned_comparison_keeps_its_type_and_its_sentence() {
        let query = parser::parse("_severity=nosuchlevel").expect("parse should succeed");
        let mut pins = crate::schema::FieldTypes::new();
        pins.insert("_severity", crate::schema::CanonicalType::Severity);
        let err = emit_with_pins(&query, SRC, &pins, anchor()).expect_err("the token is unknown");

        let EmitError::Comparison(
            cause @ crate::compare::CompareError::UnknownSeverityToken { token },
        ) = &err
        else {
            panic!("expected a typed comparison refusal, got {err:?}");
        };
        assert_eq!(token, "nosuchlevel");

        // The WHOLE rendering, composed from the typed cause. This was a
        // `starts_with` against a prefix, which is no guard at all for a
        // test whose only job is that the sentence has not moved: appending
        // ` [comparison refusal]` to the `Display` impl would have sailed
        // straight through it.
        assert_eq!(err.to_string(), format!("unsupported operation: {cause}"));
    }
}
