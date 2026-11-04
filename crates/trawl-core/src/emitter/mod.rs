// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SQL emitter for `DuckDB`.
//!
//! Walks the AST and produces parameterized `DuckDB` SQL.
//! Uses CTEs to handle multi-stage pipeline queries.

#[cfg(test)]
mod absent_tests;
mod columns;
mod compare;
mod expr;
mod fields;
mod functions;
mod pipeline;
mod relation;
mod search;
pub(crate) mod state;
mod validate;

use crate::ast::{PipeStage, Query, Spanned};
use state::EmitterState;

pub use columns::{ColdColumns, HotColumns, InputColumns};
pub(crate) use fields::coerce_filter_value;
pub use functions::{DATE_PART_UNITS, DATE_UNITS};
pub(crate) use functions::{
    format_literal_position, unit_literal_positions, validate_format_literal,
    validate_function_arity, validate_unit_literal,
};
pub use functions::{function_result_pin, is_aggregate_function};
pub use state::{
    MAX_REGEX_PARAM_BYTES, MAX_REGEX_PARAMS, hot_source_reader, source_reader, validate_source_path,
};
pub use validate::validate_pipeline;

use std::fmt;

/// Whether emitting `query` needs to know what its input carries
/// (ADR-0062): the executor's predicate for skipping the schema probe.
///
/// True when the query reads a field other than the envelope timestamps,
/// has a text search, or extracts from the default `message`. `message`
/// and `_raw` are never assumed present: a foreign source can lack them.
/// A query that is false here reads only `_time` and `_ingested`, which
/// are never resolved to NULL anyway, so its SQL is the same with or
/// without a description.
///
/// The hot snapshot can need a description whatever the query reads; ask
/// [`hot_needs_input_columns`] too.
#[must_use]
pub fn needs_input_columns(query: &Query) -> bool {
    crate::field_refs::referenced_fields(query)
        .iter()
        .any(|field| !crate::schema::TIMESTAMP_COLUMNS.contains(&field.as_str()))
        || crate::field_refs::reads_text_implicitly(query)
}

/// Whether a hot snapshot needs the input's description whatever the query
/// reads (ADR-0062): it holds a key that was only ever JSON `null` and that
/// no pin in `hot_pins` conforms. Only the cold description says whether
/// that key reads as a typed NULL or as absent (the hot projection in
/// [`state`]), and without it the key stays inferred JSON, which widens a
/// cold column of the same name in `*` output.
#[must_use]
pub fn hot_needs_input_columns(hot: &HotColumns, hot_pins: &crate::schema::FieldTypes) -> bool {
    hot.null_only().any(|key| {
        !crate::schema::TIMESTAMP_COLUMNS.contains(&key) && hot_pins.pin_for(key).is_none()
    })
}

/// The source a saved run that recorded no schema reads through: one dummy
/// column, no rows (`trawl_server::from_saved`). Parquet cannot hold a
/// zero-column result, so there are no names to read. The dummy column
/// is the reader's, never a field: the resolver hides it.
pub const ZERO_COLUMN_SOURCE: &str = "(SELECT 1 WHERE FALSE)";

/// A statement that answers "what type is this timechart's bucket
/// source, where that timechart reads it?".
///
/// `timechart on <column>` buckets the named column as stored, so a
/// column that is not a timestamp has to be refused. Neither of the two
/// obvious places can answer that. The binder only speaks when no
/// overload matches, and `DuckDB` resolves an untyped NULL to the `DATE`
/// overload, so silence is not proof. The finished statement's own
/// `_time` column is a different question: a later `stats` removes it, a
/// later plain `timechart` re-types it through its `TRY_CAST`, and a
/// `pivot` can manufacture a column of that name out of a data value.
///
/// So the question is asked where and when it is well posed: one probe
/// per timechart, built from the relation entering that stage, carrying
/// the parameters that relation's placeholders need
/// ([`crate::emitter::state::EmitterState::stage_input_probe`]).
#[derive(Debug, Clone, PartialEq)]
pub struct TimechartInputCheck {
    /// The column the stage buckets, as the reader spelled it.
    pub column: String,
    /// `SELECT "<column>" FROM (<the stage's input relation>) LIMIT 0`.
    pub sql: String,
    /// The values [`sql`](Self::sql)'s placeholders bind, in order.
    pub params: Vec<SqlValue>,
}

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
    /// The pin scope in force at the kv split point: the interpretation
    /// [`rust_stages`](Self::rust_stages) must be evaluated under, so the
    /// batch tail's `where`/`let` bind the same pins the SQL prefix did,
    /// including every rename/let/stats scope change before the split.
    /// Empty for pin-blind emission.
    pub rust_stage_pins: crate::pin_scope::PinScope,
    /// Whether the executor should reorder result columns to put well-known
    /// fields first. `true` when no stage defined a complete output column
    /// set (table/fields, stats, top, rare, timechart, pivot).
    pub needs_column_reorder: bool,
    /// One probe per explicit `timechart on <field>`, in pipeline order
    /// — empty when no stage named a bucket column, or named only
    /// `_time`.
    ///
    /// The executor runs these before [`sql`](Self::sql) and refuses the
    /// query when one of them reports a type no chart can bucket. They
    /// are not an input to the statement: the bucket expressions in
    /// `sql` already name their columns.
    pub timechart_input_checks: Vec<TimechartInputCheck>,
    /// The instant this statement's `now()` reads (ADR-0017 §3).
    ///
    /// The caller captures it once per logical query and stamps it here,
    /// so every reader of this emission shares one clock: the SQL prefix
    /// binds it as a TIMESTAMP parameter, and the `rust_stages` tail
    /// behind `extract kv` evaluates under the same anchor
    /// ([`crate::context::EvalContext`]). A re-emission of the same
    /// logical query, such as the executor's hot-only fallback, inherits
    /// this value rather than sampling a second one.
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

/// The TIMESTAMP literal text a bound [`SqlValue::Timestamp`] denotes.
///
/// The one rendering, shared by [`fmt::Display`] and the PIVOT lane's
/// parameter inlining, so the inlined form and the bound form can never
/// name different instants.
///
/// Typed (`TIMESTAMP '…'`) so the literal has a type wherever it lands,
/// and fixed six-digit microseconds so no value renders at a precision
/// `DuckDB`'s domain does not hold. Trailing zeros are kept: `DuckDB`
/// parses `…:00.000000` and `…:00` to the same instant, and a fixed width
/// is one rule instead of two.
///
/// The leading `+` chrono puts on a year outside `0..=9999` is stripped:
/// `DuckDB`'s timestamp parser accepts a leading `-` but not a leading
/// `+`, so `+10000-01-01` is a conversion error where the same instant
/// bound as a parameter is fine. A production clock never gets there, but
/// [`crate::context::EvalContext::at`] is public. Probed in
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
    /// The right-hand side of a `matches`, `like` or `ilike` is not
    /// literal text in the query ([`crate::pattern::literal_operand`],
    /// ADR-0063). The parser refuses such a query first; this refusal
    /// holds the rule for an AST that never went through the parser.
    ComputedPattern,
    /// The statement would bind more regex pattern copies than
    /// [`MAX_REGEX_PARAMS`], or more pattern bytes than
    /// [`MAX_REGEX_PARAM_BYTES`], counting every copy a same-stage alias, a
    /// repeating rendering or a pivot makes (ADR-0063). `count` and `bytes`
    /// are the totals with the refused pattern included.
    RegexParameters {
        count: u64,
        bytes: u64,
    },
}

/// The message, then the hint in parentheses when there is one. A caller
/// that carries the hint in its own field reads [`EmitError::message`]
/// instead, so the hint is not said twice.
impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())?;
        if let Some(hint) = self.hint() {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for EmitError {}

/// A literal the pin rule table refuses (`crate::compare::CompareError`)
/// is an unsupported operation to every emitter caller: one conversion, so
/// the search stage, the pipeline emitter, the live filter and the stream
/// compiler all report the same sentence.
///
/// The cause travels as a type rather than as prose, because the pin-aware
/// fuzz target has to decide whether an emitter refusal is a legitimate
/// outcome for the pin map it invented (a `SEVERITY` pin plus a literal
/// naming no ladder point is expected, a panic never is), and that verdict
/// must not be a substring test against an error message anyone is free to
/// reword.
impl From<crate::compare::CompareError> for EmitError {
    fn from(err: crate::compare::CompareError) -> Self {
        Self::Comparison(err)
    }
}

impl EmitError {
    /// The user-facing sentence without the hint: what `Display` prints
    /// before its parenthesized hint. The server's error envelope carries
    /// this as a detail's `message` beside the detail's own `hint`.
    pub fn message(&self) -> String {
        match self {
            Self::UnknownFunction { name, .. } => format!("unknown function: {name}"),
            Self::InvalidAggregation { message } => format!("invalid aggregation: {message}"),
            Self::UnsupportedOperation { message } => format!("unsupported operation: {message}"),
            Self::Comparison(err) => format!("unsupported operation: {err}"),
            Self::InvalidFormat { func_name, format } => {
                format!("{func_name}(): invalid format string {format:?}")
            }
            Self::ComputedPattern => format!(
                "unsupported operation: {}",
                crate::pattern::PatternError::Computed
            ),
            Self::RegexParameters { count, bytes } => format!(
                "unsupported operation: the query binds {count} regex patterns of {bytes} bytes \
                 in all, counting each copy a same-stage alias or a pivot makes, over the \
                 limit of {MAX_REGEX_PARAMS} patterns or {MAX_REGEX_PARAM_BYTES} bytes"
            ),
        }
    }

    /// Produce a user-facing hint string, if applicable.
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::UnknownFunction {
                suggestion: Some(s),
                ..
            } => Some(format!("did you mean '{s}'?")),
            Self::ComputedPattern => {
                Some("write the pattern itself in the query, as a quoted string".to_string())
            }
            Self::RegexParameters { .. } => Some(
                "use fewer regex patterns, compute a value several outputs read in a `| let` \
                 stage of its own, or compute the value a pivot aggregates in a `| let` stage \
                 before the pivot"
                    .to_string(),
            ),
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
/// Deliberately pin-blind: comparisons stay literal-driven, which is the
/// same answer a catalog-backed caller gets by passing an empty pin set,
/// as embedded `--data` does (ADR-0011). Unprobed too: every name binds
/// ([`InputColumns::unprobed`]). The fuzz target and the crate's own tests
/// enter here; the execution lanes go through [`emit_with_pins`],
/// [`emit_with_hot_source`] or [`emit_hot_only`].
///
/// `anchor` is the statement's `now()` instant (ADR-0017 §3): capture it
/// once per logical query, at the head of the operation, and pass the
/// same value to every emission that operation performs.
pub fn emit(
    query: &Query,
    source: &str,
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_from_state(
        query,
        EmitterState::new(source, &InputColumns::unprobed(), anchor)?,
    )
}

/// Emit SQL with the field catalog's pins typing the comparisons, in the
/// search stage and in the pipeline alike (ADR-0011).
///
/// `pins` is the full catalog snapshot, not intersected with any hot key
/// set: a VARCHAR-pinned field compares as text under `=`/`!=`/IN,
/// numerically in [`crate::conform::DECIMAL_COMPARISON_SPACE`] for
/// ordered numeric literals (both sides cast, so the literal never
/// round-trips through `f64`), and typed pins glob/regex against the
/// stored value's canonical text: `CAST(col AS VARCHAR)` for `BIGINT`,
/// `DOUBLE` and `BOOLEAN`, `strftime` for `TIMESTAMP`, the ladder's short
/// names for `SEVERITY`. A typed pin's comparisons emit exactly what the
/// unpinned path emits, the column on disk already being the pinned type;
/// the pin still travels to the live matcher, which has to conform the
/// wire value before it can answer the same question ([`crate::filter`]).
/// See [`crate::compare`] for the rule table. Empty `pins` with
/// [`InputColumns::unprobed`] emits exactly what [`emit`] emits.
///
/// `columns` describes what the selected sources carry (ADR-0062).
/// [`InputColumns::unprobed`] binds every name.
pub fn emit_with_pins(
    query: &Query,
    source: &str,
    pins: &crate::schema::FieldTypes,
    columns: &InputColumns,
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_from_state(
        query,
        EmitterState::new(source, columns, anchor)?.with_compare_pins(pins),
    )
}

/// Emit SQL that unions the primary parquet source with a hot buffer ndjson file.
///
/// Produces a `UNION ALL BY NAME` composite source so that fresh events
/// in the hot buffer are visible alongside compacted parquet data.
///
/// Two pin sets, two roles, never conflated (ADR-0011):
///
/// - `hot_pins`: the catalog's pins intersected with the snapshot's
///   observed keys. Each pinned field is conformed on the hot branch only,
///   through the same text-first guarded cast compaction writes with
///   ([`crate::conform`]), so a hot value disagreeing with the write-time
///   pin degrades to NULL instead of throwing the union, and one that
///   agrees reads exactly as it will once compacted. Intersected, because
///   the `REPLACE` list must never name a column absent from the snapshot.
/// - `pins`: the full catalog snapshot typing the comparisons (see
///   [`emit_with_pins`]). Full, because a cold-only field's comparison
///   semantics must not depend on ingest timing.
///
/// Empty sets (embedded mode, catalog-less buffer) leave the union plain
/// apart from the unconditional envelope timestamp `TRY_CAST`s (ADR-0008)
/// and the comparisons literal-driven.
///
/// `columns` describes what both sides of the union carry, as in
/// [`emit_with_pins`].
pub fn emit_with_hot_source(
    query: &Query,
    source: &str,
    hot_source: &str,
    hot_pins: &crate::schema::FieldTypes,
    pins: &crate::schema::FieldTypes,
    columns: &InputColumns,
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_from_state(
        query,
        EmitterState::with_hot_source(source, hot_source, hot_pins, columns, anchor)?
            .with_compare_pins(pins),
    )
}

/// Emit SQL reading only the hot-buffer ndjson, conformed exactly as the
/// union's hot branch is (ADR-0011).
///
/// Same two pin sets, same two roles as [`emit_with_hot_source`]:
/// `hot_pins` (intersected with the snapshot's keys) conforms the hot
/// columns, `pins` (the full catalog snapshot) types the comparisons. This
/// is the executor's cold-start lane; reading the raw ndjson through
/// [`emit_with_pins`] instead would let `read_json`'s inference, not the
/// catalog, decide a hot column's type, so a query's answer would change
/// the moment the first parquet file landed.
///
/// The `anchor` is the original emission's ([`EmittedQuery::anchor`]),
/// never a fresh capture: this lane re-emits one logical query against a
/// narrower source, so re-sampling the clock here would make the fallback
/// answer a `now()` comparison differently from the union attempt it
/// replaces.
///
/// `columns` describes the snapshot alone: the union's descriptor through
/// [`InputColumns::hot_only`].
pub fn emit_hot_only(
    query: &Query,
    hot_source: &str,
    hot_pins: &crate::schema::FieldTypes,
    pins: &crate::schema::FieldTypes,
    columns: &InputColumns,
    anchor: crate::context::EvalContext,
) -> Result<EmittedQuery, EmitError> {
    emit_from_state(
        query,
        EmitterState::with_hot_only_source(hot_source, hot_pins, columns, anchor)?
            .with_compare_pins(pins),
    )
}

/// Emit the query from a prepared state.
///
/// The state carries the input's columns (ADR-0062): every field read
/// resolves against the relation each stage reads, and a field that
/// relation lacks reads as NULL ([`EmitterState::read_field`]).
fn emit_from_state(query: &Query, mut state: EmitterState) -> Result<EmittedQuery, EmitError> {
    validate::validate_pipeline(&query.pipeline)?;

    if query.from_saved_stage().is_some() && !query.has_empty_search() {
        return Err(EmitError::UnsupportedOperation {
            message: "'from saved' cannot be combined with search filters".to_string(),
        });
    }

    search::emit_search(&query.search, &mut state)?;

    let mut rust_stages = Vec::new();
    let mut rust_stage_pins = crate::pin_scope::PinScope::unpinned();

    for (i, stage) in query.pipeline.iter().enumerate() {
        // A kv extraction mints dynamic columns, so it cannot be expressed
        // as SQL: it and every remaining stage become rust_stages, stamped
        // with the pin scope in force at the split so the batch tail
        // evaluates under the interpretation the SQL prefix used.
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

        // A pending pivot is flushed to a CTE before any following stage,
        // so that stage reads the pivot's dynamic output columns.
        //
        // That includes a second pivot: `process_pivot` opens with an
        // ordinary `flush_to_cte`, whose `build_select` does not render
        // `self.pivot`, and then overwrites the pending spec, so skipping
        // the flush would drop the first pivot and run the second over
        // pre-pivot input. A terminal pivot is never flushed here (no
        // stage follows it); `finalize` renders it.
        if state.has_pivot() {
            state.flush_pivot_to_cte()?;
        }
        // The regex copies `DuckDB` binds for this stage beyond the
        // patterns it writes, charged before the stage pushes any of its
        // own, so a refusal comes before its parameters exist.
        state.charge_regex_copies(crate::complexity::stage_regex_copies(&stage.node))?;
        pipeline::process_stage(&stage.node, &mut state)?;
        // After the stage: its own expressions resolve against the
        // incoming schema; the next stage sees this one's output scope
        // and columns.
        state.advance_scopes(&stage.node);
    }
    state.cast_surviving_nulls()?;

    let needs_column_reorder = state.needs_column_reorder();
    let timechart_input_checks = std::mem::take(&mut state.timechart_input_checks);
    let anchor = state.anchor();
    let sql = state.finalize()?;
    let params = state.into_params();

    Ok(EmittedQuery {
        sql,
        params,
        rust_stages,
        rust_stage_pins,
        needs_column_reorder,
        timechart_input_checks,
        anchor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use insta::assert_snapshot;

    const SRC: &str = "/data/**/*.parquet";

    /// The instant emitter tests emit under.
    ///
    /// Fixed rather than captured: `now()` binds the anchor as a parameter
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

    /// `!=` with a pattern is the complement of `=` under the search-stage
    /// `!=` rule (ADR-0060): a NULL target counts as no match, so the
    /// shape is `NOT COALESCE(…, FALSE)`.
    #[test]
    fn search_ne_glob() {
        assert_snapshot!(emit_dsl("host!=web*"));
    }

    #[test]
    fn search_ne_regex() {
        assert_snapshot!(emit_dsl(r"host!=/web-\d+/"));
    }

    /// A list honours each element's kind: the exact elements stay one
    /// membership test, and each pattern ORs its own predicate after it.
    #[test]
    fn search_in_list_with_patterns() {
        assert_snapshot!(emit_dsl("host=a,b*,/c/"));
    }

    #[test]
    fn search_in_list_of_patterns_only() {
        assert_snapshot!(emit_dsl("host=a*,b*"));
    }

    /// …and `!=` ANDs each element's own `!=`, in written order.
    #[test]
    fn search_negated_list_with_patterns() {
        assert_snapshot!(emit_dsl("host!=a,b*,/c/"));
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

    /// `_`, `%` and `\` in a bare term or quoted phrase bind as escaped
    /// literals under `ESCAPE '\'`, never as LIKE wildcards (ADR-0015).
    #[test]
    fn search_text_metacharacters_bind_literally() {
        let cases: &[(&str, &str)] = &[
            ("user_42", r"%user\_42%"),
            ("100%", r"%100\%%"),
            (r"a\b", r"%a\\b%"),
            (r"ab\", r"%ab\\%"),
            (r"a_b%c\d", r"%a\_b\%c\\d%"),
            ("-user_42", r"%user\_42%"),
            ("NOT 100%", r"%100\%%"),
            (r#""user_42 100% a\\b""#, r"%user\_42 100\% a\\b%"),
            (r#"NOT "a_b%c\\d""#, r"%a\_b\%c\\d%"),
        ];
        for &(dsl, pattern) in cases {
            let query = parser::parse(dsl).expect("parse should succeed");
            let result = emit(&query, SRC, anchor()).expect("emit should succeed");
            let expected = SqlValue::String(pattern.to_string());
            assert_eq!(result.params, [expected.clone(), expected], "{dsl:?}");
            assert_eq!(
                result.sql.matches(r"ILIKE ? ESCAPE '\'").count(),
                2,
                "{dsl:?}"
            );
        }
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

    /// `level` is ordinary sender vocabulary: one name, one column, in
    /// every field position. The severity band vocabulary lives on
    /// `_severity`, which nothing can shadow.
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

    /// `level=gold` filters the sender's own column, verbatim.
    #[test]
    fn search_level_is_a_plain_field_filter() {
        assert_snapshot!(emit_dsl("level=gold"));
    }

    #[test]
    fn where_level_is_a_plain_comparison() {
        assert_snapshot!(emit_dsl(r#"* | where level == "error""#));
    }

    /// The pipeline may not mint a reserved name (ADR-0013 §5): the same
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

    /// `timestamp` and `@timestamp` are ordinary sender field names: each
    /// names the column it spells, and only `_time` is `_time`.
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

    /// `drop` excludes through the folded `COLUMNS` lambda, as `let` and
    /// `rename` do, so a field no incoming column carries binds and drops
    /// nothing (ADR-0001: the live tail ignores it too). `* EXCLUDE`
    /// refuses a name the input lacks.
    #[test]
    fn pipe_drop_excludes_through_the_columns_lambda() {
        assert_eq!(
            emit_dsl("* | drop a, b"),
            "SELECT COLUMNS(c -> translate(c, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', \
             'abcdefghijklmnopqrstuvwxyz') NOT IN ('a', 'b'))\n\
             FROM read_parquet('/data/**/*.parquet', union_by_name=true)"
        );
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
    /// named (folding both sides with `lower()` deletes it silently).
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
    fn emitter_timechart_span_uses_the_resolver() {
        let sql = emit_dsl("service=x last=3h | timechart count()");
        assert!(sql.contains("5 minutes"), "{sql}");

        let sql = emit_dsl("last=3h | timechart span=2m count()");
        assert!(sql.contains("2 minutes"), "{sql}");
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

    /// `pivot` inlines every `?` because `DuckDB` cannot bind a PIVOT, and
    /// a backticked name may contain a `?` of its own (ADR-0013 ruling 7).
    /// Only a placeholder outside a quoted region may be spliced: splicing
    /// into the identifier or the COLUMNS lambda string would corrupt the
    /// name and shift every later binding, spilling the user's value into
    /// the statement with only `'` escaped.
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
            &InputColumns::unprobed(),
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
            &InputColumns::unprobed(),
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
            &InputColumns::unprobed(),
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
            &InputColumns::unprobed(),
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
            &InputColumns::unprobed(),
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
        // Empty pins (embedded mode, catalog-less buffer): plain cold
        // select, hot side with only the two timestamp TRY_CASTs.
        let query = parser::parse("service=nginx").unwrap();
        let sql = emit_with_hot_source(
            &query,
            SRC,
            "/tmp/hot_abc123.ndjson",
            &crate::schema::FieldTypes::new(),
            &crate::schema::FieldTypes::new(),
            &InputColumns::unprobed(),
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
    // pin-aware comparisons (ADR-0011)
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
        let result = emit_with_pins(
            &query,
            SRC,
            &pins(entries),
            &InputColumns::unprobed(),
            anchor(),
        )
        .expect("emit should succeed");
        format_result(&result)
    }

    /// Parse and emit pin-aware SQL, expecting an `EmitError`.
    fn emit_dsl_err_with_pins(
        input: &str,
        entries: &[(&str, crate::schema::CanonicalType)],
    ) -> String {
        let query = parser::parse(input).expect("parse should succeed");
        emit_with_pins(
            &query,
            SRC,
            &pins(entries),
            &InputColumns::unprobed(),
            anchor(),
        )
        .expect_err("emit should fail")
        .to_string()
    }

    /// The declared `_severity` pin, as the catalog seed installs it.
    const SEVERITY_PIN: [(&str, crate::schema::CanonicalType); 1] = [("_severity", CT::Severity)];

    use crate::schema::CanonicalType as CT;

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

    /// A whole severity list is one membership test over the ladder points
    /// its bands cover: the subject is written once, however many bands
    /// the list names.
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

    /// The ladder text's `CASE` has no `ELSE`, which is why a negated
    /// pattern COALESCEs rather than adding `OR field IS NULL`.
    #[test]
    fn pinned_severity_ne_glob_coalesces_the_ladder_text() {
        assert_snapshot!(emit_dsl_with_pins("_severity!=warn*", &SEVERITY_PIN));
    }

    #[test]
    fn pinned_severity_list_ors_a_glob_after_the_band_ranges() {
        assert_snapshot!(emit_dsl_with_pins("_severity=error,warn*", &SEVERITY_PIN));
    }

    /// The pipeline lane binds the same rule with the strict null policy:
    /// `!=` keeps plain SQL null propagation (ADR-0011).
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

    /// A numeric literal binds both the text and its numeric reading: the
    /// stored text of a number is `read_json`'s inference rendered
    /// (`"200.0"`), so exact text alone would be a batch miss where the
    /// live matcher, which only sees the wire `200`, hits.
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

    /// The ordered rung casts both sides into the one comparison space:
    /// binding the literal as a number would compare through `f64`, which
    /// makes every id above 2^53 equal to its neighbours (ADR-0011 ruling
    /// #6).
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

    #[test]
    fn pinned_bigint_ne_glob_casts_to_varchar() {
        assert_snapshot!(emit_dsl_with_pins("status!=4*", &[("status", CT::BigInt)]));
    }

    /// An exact element keeps its pinned `!=` shape (the VARCHAR two-armed
    /// complement), beside a pattern element's own complement.
    #[test]
    fn pinned_varchar_negated_list_with_a_glob() {
        assert_snapshot!(emit_dsl_with_pins(
            "status!=200,4*",
            &[("status", CT::Varchar)]
        ));
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
            let emitted = emit_with_pins(&query, SRC, &ft, &InputColumns::unprobed(), anchor())
                .expect("emit should succeed");
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
            &InputColumns::unprobed(),
            anchor(),
        )
        .unwrap();
        assert_snapshot!(format_result(&result));
    }

    // -----------------------------------------------------------------------
    // pin-aware pipeline comparisons (ADR-0011)
    // -----------------------------------------------------------------------

    /// `| where` over a VARCHAR pin compares ordered-numeric in the one
    /// DECIMAL space, where a pin-blind emission raises a Conversion
    /// error instead.
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

    /// The pipeline `!=` keeps plain SQL null propagation, with no
    /// `OR field IS NULL` widening: a repin must not change missing-field
    /// semantics (`NullPolicy::Strict`).
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

        let quoted_sql = emit_with_pins(
            &quoted,
            SRC,
            &pins(entries),
            &InputColumns::unprobed(),
            anchor(),
        )
        .expect("pinned emit");
        let bare_sql = emit_with_pins(
            &bare,
            SRC,
            &pins(entries),
            &InputColumns::unprobed(),
            anchor(),
        )
        .expect("pinned emit");
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
        let pinned = emit_with_pins(
            &query,
            SRC,
            &pins(&[("status", CT::Varchar)]),
            &InputColumns::unprobed(),
            anchor(),
        )
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
    /// pattern over a function-wrapped field. A field on the RIGHT of a
    /// pattern operator is no shape at all: the pattern must be literal
    /// (`the_emitter_refuses_a_computed_pattern`).
    #[test]
    fn pinned_where_excluded_shapes_emit_byte_identical_sql() {
        for dsl in [
            "* | where status == other",
            "* | where lower(status) == \"a\"",
            "* | where status * 2 > 400",
            "* | where status == null",
            "* | where lower(status) matches \"x\"",
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let pinned = emit_with_pins(
                &query,
                SRC,
                &pins(&[("status", CT::Varchar), ("other", CT::Varchar)]),
                &InputColumns::unprobed(),
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
            let pinned = emit_with_pins(
                &query,
                SRC,
                &pins(&[("dur", CT::BigInt)]),
                &InputColumns::unprobed(),
                anchor(),
            )
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
        let emitted =
            emit_with_pins(&query, SRC, &ft, &InputColumns::unprobed(), anchor()).unwrap();
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
    /// ADR-0011 rule table, pinned emission is byte-identical to unpinned
    /// emission, so a repin changes no other query's meaning.
    #[test]
    fn pinned_emission_is_byte_identical_outside_the_changed_cells() {
        let pin_states: [Option<CT>; 6] = [
            None,
            Some(CT::Varchar),
            Some(CT::BigInt),
            Some(CT::Double),
            Some(CT::Timestamp),
            Some(CT::Boolean),
        ];
        // (dsl template, is a pattern) — `{}` is replaced by the literal.
        let cases: [(&str, bool); 9] = [
            ("f={}", false),
            ("f!={}", false),
            ("f>{}", false),
            ("f>={}", false),
            ("f<{}", false),
            ("f<={}", false),
            ("f={},{}", false), // IN list
            ("f={}*", true),    // glob (auto-detected)
            ("f=/{}.*/", true), // regex
        ];
        let literals: [(&str, bool); 4] = [
            ("200", true),
            ("1.5", true),
            ("accepted", false),
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
                    let pinned = emit_with_pins(
                        &query,
                        SRC,
                        &pins(&entries),
                        &InputColumns::unprobed(),
                        anchor(),
                    )
                    .expect("pinned emit");

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

    /// The same minimality invariant over the pipeline lane: outside the
    /// changed cells (VARCHAR pin × numeric literal for comparisons,
    /// typed pins for patterns) a `| where` emits byte-identical SQL
    /// under pins.
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
                    let pinned = emit_with_pins(
                        &query,
                        SRC,
                        &pins(&entries),
                        &InputColumns::unprobed(),
                        anchor(),
                    )
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

    /// A severity predicate names its subject once per contiguous range,
    /// which is once outright for every natural query: a band, a run of
    /// bands and a band-plus-adjacent-point all merge into one range.
    ///
    /// Counting matters because the subject can be over a kilobyte of
    /// `sev()` SQL, so one copy per band would multiply the statement.
    ///
    /// That `sev()` substring is derived from the builder that emits it
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

    /// Out-of-ladder points cannot amplify the render.
    ///
    /// A `SEVERITY` subject evaluates to 1-24 or NULL, so every point
    /// outside that range is unmatchable and they are all interchangeable:
    /// the renderer keeps exactly one. Without the collapse each
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
        // The amplification guard: a long alternating in/out list renders
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
        // The same bound over the expensive subject, where amplification
        // would actually cost bytes: a `sev()` call, not a bare column.
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

    /// The severity set is inlined, so it binds nothing between a
    /// subject's parameters and whatever follows.
    ///
    /// The set is a single run here on purpose. A multi-run set repeats
    /// the subject, so a subject carrying `?` placeholders would emit more
    /// placeholders than parameters were pushed: that shape is out of
    /// contract, documented at `severity_ranges_sql`, and structurally
    /// unreachable (see [`severity_subjects_never_push_parameters`]).
    /// Asserting positions over a repeated subject would document a
    /// contract the renderer does not hold.
    #[test]
    fn a_severity_set_pushes_no_parameters_and_preserves_positions() {
        let mut state =
            EmitterState::new("/data/**/*.parquet", &InputColumns::unprobed(), anchor())
                .expect("source");
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

    /// No subject the pin scope admits can push a bound parameter, so the
    /// multi-run repetition can never desynchronize placeholders from
    /// parameters.
    ///
    /// `PinScope::subject_pin` admits exactly two shapes, a bare pinned
    /// field and a pin-declaring call over a bare field whose remaining
    /// arguments are string literals, and `sev()`'s dialect is inlined as
    /// text (`functions::literal_text_positions`) rather than bound. This
    /// asserts that end to end: a severity predicate emits no parameters
    /// at all, in the search stage and in both operand orders of the
    /// pipeline, for single-run and multi-run sets.
    ///
    /// The last block ties the guard to the function table rather than to
    /// the one name `sev`: every `KNOWN_FUNCTIONS` entry that declares a
    /// `SEVERITY` result is admitted by `subject_pin`, so a future
    /// severity-returning function joins this test automatically, and if
    /// its argument shape makes the probe DSL unemittable, the panic names
    /// the obligation instead of silently covering nothing.
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

    /// A year outside `0..=9999` renders unsigned.
    ///
    /// chrono writes `+10000-01-01`, which `DuckDB`'s parser refuses while
    /// accepting the same year unsigned and a negative year signed, so the
    /// sign would make the inlined PIVOT literal fail for an instant the
    /// bound parameter handles. Executed against the engine in
    /// `trawl-engine/tests/duckdb_probe.rs`.
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

    /// The guard is not about the anchor: any literal inside an aggregate
    /// argument pushes a parameter into the SELECT list, and mis-binds
    /// without the same flush.
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

    /// A refused pinned comparison keeps its cause as a type and renders
    /// the same sentence every other unsupported operation renders.
    ///
    /// Both halves matter and they pull in opposite directions. The type
    /// is what the pin-aware fuzz target classifies on: a `SEVERITY` pin
    /// plus a literal naming no ladder point is a legitimate outcome, so
    /// the target must recognize it without matching prose. The text is
    /// what users, snapshots and the wire see, so the
    /// `unsupported operation: ` prefix has to stay where it is.
    #[test]
    fn a_refused_pinned_comparison_keeps_its_type_and_its_sentence() {
        let query = parser::parse("_severity=nosuchlevel").expect("parse should succeed");
        let mut pins = crate::schema::FieldTypes::new();
        pins.insert("_severity", crate::schema::CanonicalType::Severity);
        let err = emit_with_pins(&query, SRC, &pins, &InputColumns::unprobed(), anchor())
            .expect_err("the token is unknown");

        let EmitError::Comparison(
            cause @ crate::compare::CompareError::UnknownSeverityToken { token },
        ) = &err
        else {
            panic!("expected a typed comparison refusal, got {err:?}");
        };
        assert_eq!(token, "nosuchlevel");

        // The whole rendering, composed from the typed cause. A
        // `starts_with` against the prefix would be no guard at all for a
        // test whose only job is that the sentence has not moved:
        // appending ` [comparison refusal]` to the `Display` impl would
        // sail straight through it.
        assert_eq!(err.to_string(), format!("unsupported operation: {cause}"));
    }

    /// `message()` is `Display` without the hint, and `Display` still
    /// prints exactly what it printed before `message()` existed: the CLI,
    /// the telemetry row and the envelope's summary `message` all read the
    /// `Display` text, so each variant's rendering is pinned byte for byte.
    #[test]
    fn message_is_display_without_the_hint_and_display_is_unchanged() {
        let cases = [
            (
                EmitError::UnknownFunction {
                    name: "countt".into(),
                    suggestion: Some("count".into()),
                },
                "unknown function: countt (did you mean 'count'?)",
                "unknown function: countt",
            ),
            (
                EmitError::UnknownFunction {
                    name: "nosuchfunc".into(),
                    suggestion: None,
                },
                "unknown function: nosuchfunc",
                "unknown function: nosuchfunc",
            ),
            (
                EmitError::InvalidAggregation {
                    message: "x".into(),
                },
                "invalid aggregation: x",
                "invalid aggregation: x",
            ),
            (
                EmitError::UnsupportedOperation {
                    message: "y".into(),
                },
                "unsupported operation: y",
                "unsupported operation: y",
            ),
            (
                EmitError::InvalidFormat {
                    func_name: "strftime".into(),
                    format: "%Q".into(),
                },
                "strftime(): invalid format string \"%Q\"",
                "strftime(): invalid format string \"%Q\"",
            ),
        ];
        for (err, display, message) in cases {
            assert_eq!(err.to_string(), display);
            assert_eq!(err.message(), message);
        }

        let cause = crate::compare::CompareError::UnknownSeverityToken {
            token: "nosuchlevel".into(),
        };
        let err = EmitError::Comparison(cause.clone());
        assert_eq!(err.to_string(), format!("unsupported operation: {cause}"));
        assert_eq!(err.message(), err.to_string());
    }

    /// The emitter's own did-you-mean reaches `Display` from a real query,
    /// so the pinned rendering above is the one a user sees.
    #[test]
    fn an_unknown_function_with_a_near_miss_displays_its_hint_once() {
        assert_eq!(
            emit_dsl_err("* | stats countt(x) by host"),
            "unknown function: countt (did you mean 'count'?)"
        );
    }

    // -----------------------------------------------------------------------
    // literal-only patterns (ADR-0063)
    // -----------------------------------------------------------------------

    /// A spanless expression node, for an AST built by hand.
    fn node(expr: crate::ast::Expr) -> crate::ast::Spanned<crate::ast::Expr> {
        crate::ast::Spanned::new(expr, 0..0)
    }

    /// `subject <op> pattern` as an expression, built by hand so the
    /// parser's own literal rule never sees it.
    fn pattern_test(
        subject: &str,
        op: crate::ast::BinaryOp,
        pattern: crate::ast::Expr,
    ) -> crate::ast::Spanned<crate::ast::Expr> {
        node(crate::ast::Expr::Binary {
            lhs: Box::new(node(crate::ast::Expr::FieldRef(subject.to_owned()))),
            op,
            rhs: Box::new(node(pattern)),
        })
    }

    /// `*` followed by the hand-built `stage`.
    fn hand_built(stage: PipeStage) -> Query {
        let mut query = parser::parse("*").expect("parse should succeed");
        query.pipeline.push(crate::ast::Spanned::new(stage, 0..0));
        query
    }

    /// The pattern operands the literal rule refuses: a function of a
    /// literal, a field, and a literal that is not text.
    fn computed_operands() -> Vec<crate::ast::Expr> {
        use crate::ast::{Expr, LiteralValue};
        let text = |s: &str| node(Expr::Literal(LiteralValue::String(s.to_owned())));
        vec![
            Expr::FunctionCall {
                name: "lower".to_owned(),
                args: vec![text("a")],
            },
            Expr::FunctionCall {
                name: "concat".to_owned(),
                args: vec![text("a"), text("%")],
            },
            Expr::FieldRef("host".to_owned()),
            Expr::Literal(LiteralValue::Int(5)),
            Expr::Literal(LiteralValue::Null),
        ]
    }

    /// A computed `matches`, `like` or `ilike` operand gets the typed
    /// refusal from the emitter itself, in every expression position and
    /// on the pinned path too, so the rule does not depend on `parse`
    /// having run.
    #[test]
    fn the_emitter_refuses_a_computed_pattern() {
        use crate::ast::{AggExpr, BinaryOp, LetStage, StatsStage, WhereStage};
        for op in [BinaryOp::Matches, BinaryOp::Like, BinaryOp::ILike] {
            for operand in computed_operands() {
                let condition = pattern_test("dur", op, operand.clone());
                let stages = [
                    PipeStage::Where(WhereStage {
                        condition: condition.clone(),
                    }),
                    PipeStage::Let(LetStage {
                        assignments: vec![("hit".to_owned(), condition.clone())],
                        keyword: "let",
                    }),
                    PipeStage::Stats(StatsStage {
                        aggregations: vec![AggExpr {
                            function: "count".to_owned(),
                            args: vec![node(crate::ast::Expr::FunctionCall {
                                name: "if".to_owned(),
                                args: vec![
                                    condition.clone(),
                                    node(crate::ast::Expr::Literal(crate::ast::LiteralValue::Int(
                                        1,
                                    ))),
                                    node(crate::ast::Expr::Literal(crate::ast::LiteralValue::Null)),
                                ],
                            })],
                            alias: Some("hits".to_owned()),
                        }],
                        group_by: vec![],
                    }),
                ];
                for stage in stages {
                    let query = hand_built(stage);
                    assert_eq!(
                        emit(&query, SRC, anchor()).map(|_| ()),
                        Err(EmitError::ComputedPattern),
                        "{op:?} {operand:?} unpinned"
                    );
                    // A BIGINT pin takes the pinned pattern path.
                    assert_eq!(
                        emit_with_pins(&query, SRC, &pins(&[("dur", CT::BigInt)]), anchor())
                            .map(|_| ()),
                        Err(EmitError::ComputedPattern),
                        "{op:?} {operand:?} pinned"
                    );
                }
            }
        }
        // The cause reads as the pattern owner says it, so every door
        // carries one sentence.
        assert_eq!(
            EmitError::ComputedPattern.to_string(),
            format!(
                "unsupported operation: {} (write the pattern itself in the query, as a \
                 quoted string)",
                crate::pattern::PatternError::Computed
            )
        );
    }

    // -----------------------------------------------------------------------
    // extract captures from the syntax tree (ADR-0063)
    // -----------------------------------------------------------------------

    /// `DuckDB`'s scalar `regexp_extract` reads groups 0 to 9 only, so the
    /// batch lane binds an `extract` body with its unnamed groups made
    /// non-capturing and reads each named group by its place among the
    /// named ones. `(a|b)(?P<x>b)-(?P<y>c)` binds as
    /// `(?:a|b)(?P<x>b)-(?P<y>c)`, where `x` is group 1 and `y` group 2,
    /// so `ab-c` binds `x=b` and `y=c`, which is what the live lane reads
    /// by name. Ten groups deep, `x` is still group 1.
    #[test]
    fn extract_reads_each_named_group_by_its_place_among_named_groups() {
        let query =
            parser::parse(r#"* | extract "(a|b)(?P<x>b)-(?P<y>c)""#).expect("parse should succeed");
        let result = emit(&query, SRC, anchor()).expect("emit should succeed");
        assert!(
            result
                .sql
                .contains(r#"nullif(regexp_extract("message", ?, 1), '') AS "x""#),
            "{}",
            result.sql
        );
        assert!(
            result
                .sql
                .contains(r#"nullif(regexp_extract("message", ?, 2), '') AS "y""#),
            "{}",
            result.sql
        );
        let bound = SqlValue::String("(?:a|b)(?P<x>b)-(?P<y>c)".to_owned());
        assert_eq!(result.params, vec![bound.clone(), bound]);

        let sql = emit_dsl(r#"* | extract "((((((((((?P<x>j))))))))))""#);
        assert!(
            sql.contains(r#"nullif(regexp_extract("message", ?, 1), '') AS "x""#),
            "{sql}"
        );
    }

    /// Validation, emission and pin-scope advancement read `extract`
    /// capture names and indices from the regex syntax tree, so none of
    /// them builds a regex.
    #[test]
    fn capture_metadata_builds_nothing() {
        let dsl = r#"* | extract "(a|b)(?P<x>b)-(?P<y>c)" | where x == "b""#;
        let query = parser::parse(dsl).expect("parse should succeed");
        let pins = pins(&[("x", CT::BigInt), ("y", CT::Varchar)]);

        let before = crate::pattern::thread_builds();
        validate::validate_pipeline(&query.pipeline).expect("the pipeline validates");
        emit(&query, SRC, anchor()).expect("emit should succeed");
        let emitted = emit_with_pins(&query, SRC, &pins, anchor()).expect("pinned emit");
        let mut scope = crate::pin_scope::PinScope::root(&pins);
        scope.advance(&query.pipeline[0].node);
        assert_eq!(crate::pattern::thread_builds(), before);

        // The scope walk still sees both captures: each one is a fresh
        // column, so neither keeps the pin its name had before.
        assert_eq!(scope.pin_for("x"), None);
        assert_eq!(scope.pin_for("y"), None);
        // ...and so the `where` after it compares `x` as written.
        assert!(emitted.sql.contains(r#"("x" = ?)"#), "{}", emitted.sql);
    }

    /// The refusals validation makes from the capture list keep their
    /// sentences: no named group, a reserved name, and two names that
    /// fold to one column.
    #[test]
    fn extract_capture_refusals_read_the_syntax_tree() {
        assert_eq!(
            emit_dsl_err(r#"* | extract "(a)(b)""#),
            "unsupported operation: extract regex must contain at least one named capture \
             group (?P<name>...)"
        );
        assert!(
            emit_dsl_err(r#"* | extract "(?P<_raw>a)""#).contains("reserved namespace"),
            "a capture may not mint a reserved name"
        );
        assert!(
            emit_dsl_err(r#"* | extract "(?P<Ip>a)(?P<ip>b)""#).contains("ip"),
            "two captures that fold to one name are refused"
        );
    }

    // -----------------------------------------------------------------------
    // the emit-time regex parameter counter (ADR-0063)
    // -----------------------------------------------------------------------

    use state::{MAX_REGEX_PARAM_BYTES, MAX_REGEX_PARAMS};

    /// A `let` of `n` assignments `hI = message matches "<body(I)>"`,
    /// built by hand so it can go past the parser's own pattern cap.
    fn regex_lets(n: u64, body: impl Fn(u64) -> String) -> PipeStage {
        use crate::ast::{BinaryOp, Expr, LetStage, LiteralValue};
        PipeStage::Let(LetStage {
            assignments: (0..n)
                .map(|i| {
                    let pattern = Expr::Literal(LiteralValue::String(body(i)));
                    (
                        format!("h{i}"),
                        pattern_test("message", BinaryOp::Matches, pattern),
                    )
                })
                .collect(),
            keyword: "let",
        })
    }

    /// A three-byte body per index, so a byte total is easy to state.
    fn short_body(i: u64) -> String {
        format!("{i:03}")
    }

    /// The counter's refusal at `count` patterns and `bytes` bytes.
    fn counter_refusal(count: u64, bytes: u64) -> Result<(), EmitError> {
        Err(EmitError::RegexParameters { count, bytes })
    }

    /// Every pattern that reaches RE2 is counted where it is written: a
    /// `matches` operand, a search-stage `/regex/`, and each named group
    /// of an `extract`, which binds the body once per group. The 257th is
    /// refused, and so is a byte total over 256 KiB.
    #[test]
    fn emit_time_pattern_counter_counts_every_written_pattern() {
        let at = hand_built(regex_lets(MAX_REGEX_PARAMS, short_body));
        emit(&at, SRC, anchor()).expect("the cap's worth of patterns is admitted");
        let over = hand_built(regex_lets(MAX_REGEX_PARAMS + 1, short_body));
        assert_eq!(
            emit(&over, SRC, anchor()).map(|_| ()),
            counter_refusal(MAX_REGEX_PARAMS + 1, 3 * (MAX_REGEX_PARAMS + 1))
        );

        // A search-stage regex and an extract's two groups take the last
        // three places.
        let mut query = parser::parse(r#"message=/s/ | extract "(?P<a>x)(?P<b>y)""#)
            .expect("parse should succeed");
        query.pipeline.insert(
            0,
            Spanned::new(regex_lets(MAX_REGEX_PARAMS - 3, short_body), 0..0),
        );
        emit(&query, SRC, anchor()).expect("256 patterns with the extract's groups");
        query
            .pipeline
            .insert(0, Spanned::new(regex_lets(1, short_body), 0..0));
        // 254 three-byte bodies, `s`, and the 16-byte extract body twice.
        let body_bytes = 3 * (MAX_REGEX_PARAMS - 2) + 1 + 2 * 16;
        assert_eq!(
            emit(&query, SRC, anchor()).map(|_| ()),
            counter_refusal(MAX_REGEX_PARAMS + 1, body_bytes)
        );

        // Bytes: a cap of bodies at the length limit is exactly the byte
        // limit, and one byte more is over it.
        let full = || "a".repeat(crate::pattern::MAX_REGEX_LEN);
        let at = hand_built(regex_lets(MAX_REGEX_PARAMS, |_| full()));
        emit(&at, SRC, anchor()).expect("256 KiB of patterns is admitted");
        let over = hand_built(regex_lets(MAX_REGEX_PARAMS, |i| {
            if i == 0 {
                format!("{}a", full())
            } else {
                full()
            }
        }));
        assert_eq!(
            emit(&over, SRC, anchor()).map(|_| ()),
            counter_refusal(MAX_REGEX_PARAMS, MAX_REGEX_PARAM_BYTES + 1)
        );
    }

    /// An `extract` is charged the body it binds, whose unnamed groups are
    /// non-capturing: `(a)(?P<x>b)` binds as the 13 bytes of
    /// `(?:a)(?P<x>b)`, not the 11 written.
    #[test]
    fn emit_time_pattern_counter_charges_the_bound_extract_body() {
        let mut query =
            parser::parse(r#"* | extract "(a)(?P<x>b)""#).expect("parse should succeed");
        query.pipeline.insert(
            0,
            Spanned::new(regex_lets(MAX_REGEX_PARAMS, short_body), 0..0),
        );
        assert_eq!(
            emit(&query, SRC, anchor()).map(|_| ()),
            counter_refusal(MAX_REGEX_PARAMS + 1, 3 * MAX_REGEX_PARAMS + 13)
        );
    }

    /// GLOB and LIKE parameters never reach RE2, so they are not counted:
    /// a full cap of regexes still takes any number of them.
    #[test]
    fn emit_time_pattern_counter_skips_glob_and_like() {
        let mut query = parser::parse(
            r#"host=web* service=ng?nx | where host like "w%" and service ilike "N%""#,
        )
        .expect("parse should succeed");
        query
            .pipeline
            .push(Spanned::new(regex_lets(MAX_REGEX_PARAMS, short_body), 0..0));
        emit(&query, SRC, anchor()).expect("globs and likes are free");
    }

    /// `DuckDB` binds a same-stage alias by copying the expression it names
    /// (ADR-0024), regex parameter and all, so one written pattern binds
    /// once per reference. The counter charges the copies before the
    /// stage's own parameters, from the stage's render profiles.
    ///
    /// The subject is a literal so that `a` weighs three nodes: 256
    /// references then stay inside the alias-expansion budget, and the
    /// regex counter is the limit they meet first.
    #[test]
    fn emit_time_pattern_counter_counts_lateral_copies() {
        let lets = |references: u64| {
            let names = (0..references)
                .map(|i| format!(", b{i} = a"))
                .collect::<Vec<_>>()
                .concat();
            parser::parse(&format!(r#"* | let a = "abc" matches "x"{names}"#))
                .expect("parse should succeed")
        };

        // One written pattern and 255 copies: the cap, admitted.
        let emitted = emit(&lets(MAX_REGEX_PARAMS - 1), SRC, anchor())
            .expect("256 bound copies are admitted");
        let written = emitted
            .params
            .iter()
            .filter(|p| **p == SqlValue::String("x".to_owned()))
            .count();
        assert_eq!(written, 1, "the statement writes the pattern once");

        assert_eq!(
            emit(&lets(MAX_REGEX_PARAMS), SRC, anchor()).map(|_| ()),
            counter_refusal(MAX_REGEX_PARAMS + 1, MAX_REGEX_PARAMS + 1)
        );
    }

    /// `emit_ordered_select` runs an aggregation's emission twice when the
    /// first attempt pushed a parameter behind one already pending, and
    /// discards the first. The discarded attempt's patterns are not
    /// counted: 254 search regexes, a `where` and a `stats` pattern are
    /// the cap, not one over it.
    #[test]
    fn emit_time_pattern_counter_rolls_back_a_discarded_attempt() {
        let searches = (0..MAX_REGEX_PARAMS - 2)
            .map(|i| format!("message=/r{i:03}/ "))
            .collect::<Vec<_>>()
            .concat();
        let query = parser::parse(&format!(
            r#"{searches}| where message matches "a" | stats count(if(message matches "b", 1, null)) as n"#
        ))
        .expect("parse should succeed");
        let emitted = emit(&query, SRC, anchor()).expect("each pattern counts once");
        assert!(
            emitted.sql.starts_with("WITH "),
            "the second attempt flushed to a CTE, so the first was discarded: {}",
            emitted.sql
        );
    }

    /// A pivot inlines every parameter into its SQL and clears the list,
    /// but the patterns still reach RE2, so the count runs on across it.
    #[test]
    fn emit_time_pattern_counter_survives_pivot_inlining() {
        let mut query =
            parser::parse("* | pivot count() on service by host").expect("parse should succeed");
        query
            .pipeline
            .insert(0, Spanned::new(regex_lets(200, short_body), 0..0));
        query.pipeline.push(Spanned::new(
            regex_lets(MAX_REGEX_PARAMS - 200, short_body),
            0..0,
        ));
        emit(&query, SRC, anchor()).expect("256 patterns across the pivot");
        query
            .pipeline
            .push(Spanned::new(regex_lets(1, short_body), 0..0));
        assert_eq!(
            emit(&query, SRC, anchor()).map(|_| ()),
            counter_refusal(MAX_REGEX_PARAMS + 1, 3 * (MAX_REGEX_PARAMS + 1))
        );
    }

    /// The raw-free pass and the hot-only fallback re-emit the query on a
    /// fresh state, so each statement counts its own patterns once.
    #[test]
    fn emit_time_pattern_counter_counts_each_pass_once() {
        let mut query = hand_built(regex_lets(MAX_REGEX_PARAMS, short_body));
        query.search = parser::parse("hello").expect("parse should succeed").search;
        let emitted = emit(&query, SRC, anchor()).expect("each pass counts 256");
        assert!(
            emitted.raw_free_sql.is_some(),
            "a bare word binds `_raw`, so the raw-free pass ran"
        );
        let none = crate::schema::FieldTypes::new();
        emit_hot_only(&query, "/data/hot.ndjson", &none, &none, anchor())
            .expect("the hot-only re-emission counts 256 too");
    }

    /// `pivot count(coalesce(if(host matches "rNN", 1, null), …)) on
    /// service by host`, with `patterns` three-byte patterns in its one
    /// aggregate.
    fn pivot_patterns(patterns: u64) -> String {
        let ifs = (0..patterns)
            .map(|i| format!(r#"if(host matches "{}", 1, null)"#, short_body(i)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("pivot count(coalesce({ifs})) on service by host")
    }

    /// A pivot's aggregate binds once per pivot value up to `DuckDB`'s
    /// filter threshold, and the number of values comes from the data, so
    /// every pattern in it is charged at that worst case. Twelve patterns
    /// are 240 copies and are admitted; thirteen are 260 and are refused.
    /// With the 240, sixteen search regexes reach the cap exactly.
    ///
    /// Each refusal reports the totals at the pattern that crossed the
    /// cap: the further copies are charged before the stage writes its
    /// own patterns, so the crossing is always at 257.
    #[test]
    fn emit_time_pattern_counter_charges_every_pivot_copy() {
        let worst = crate::complexity::PIVOT_FILTER_THRESHOLD;
        let fits = MAX_REGEX_PARAMS / worst;
        assert_eq!(fits, 12, "the boundary this test states");

        let pivot = |patterns: u64, searches: u64| {
            let search = if searches == 0 {
                "*".to_owned()
            } else {
                (0..searches)
                    .map(|i| format!("message=/s{i:02}/"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            parser::parse(&format!("{search} | {}", pivot_patterns(patterns)))
                .expect("parse should succeed")
        };
        // Every pattern here is three bytes.
        let over = counter_refusal(MAX_REGEX_PARAMS + 1, 3 * (MAX_REGEX_PARAMS + 1));

        emit(&pivot(fits, 0), SRC, anchor()).expect("240 pivot copies are admitted");
        assert_eq!(emit(&pivot(fits + 1, 0), SRC, anchor()).map(|_| ()), over);

        let room = MAX_REGEX_PARAMS - worst * fits;
        emit(&pivot(fits, room), SRC, anchor()).expect("the cap exactly");
        assert_eq!(
            emit(&pivot(fits, room + 1), SRC, anchor()).map(|_| ()),
            over,
            "one search regex past the cap"
        );
    }
}
