//! SQL emitter for `DuckDB`.
//!
//! Walks the AST and produces parameterized `DuckDB` SQL.
//! Uses CTEs to handle multi-stage pipeline queries.

mod expr;
mod fields;
mod functions;
mod pipeline;
mod search;
mod state;
mod validate;

use crate::ast::Query;
use state::EmitterState;

pub use state::validate_source_path;
pub use validate::validate_pipeline;

use std::fmt;

/// The result of emitting SQL from a parsed query.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedQuery {
    /// The parameterized SQL string (placeholders are `?`).
    pub sql: String,
    /// Ordered parameter values corresponding to each `?` placeholder.
    pub params: Vec<SqlValue>,
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
    UnknownFunction { name: String },
    InvalidAggregation { message: String },
    UnsupportedOperation { message: String },
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFunction { name } => write!(f, "unknown function: {name}"),
            Self::InvalidAggregation { message } => write!(f, "invalid aggregation: {message}"),
            Self::UnsupportedOperation { message } => {
                write!(f, "unsupported operation: {message}")
            }
        }
    }
}

impl std::error::Error for EmitError {}

/// Emit parameterized `DuckDB` SQL from a parsed query.
///
/// `source` is the parquet glob path, e.g. `"/data/**/*.parquet"`.
pub fn emit(query: &Query, source: &str) -> Result<EmittedQuery, EmitError> {
    let mut state = EmitterState::new(source)?;

    // pre-validate pipeline stages before mutating emission state
    validate::validate_pipeline(&query.pipeline)?;

    // translate search stage into WHERE clauses
    search::emit_search(&query.search, &mut state);

    // walk pipe stages — validate that pivot is terminal if present
    let stage_count = query.pipeline.len();
    for (i, stage) in query.pipeline.iter().enumerate() {
        if matches!(stage.node, crate::ast::PipeStage::Pivot(_)) && i != stage_count - 1 {
            return Err(EmitError::UnsupportedOperation {
                message: "pivot must be the last stage in the pipeline".to_string(),
            });
        }
        pipeline::process_stage(&stage.node, &mut state)?;
    }

    // finalize into SQL
    let sql = state.finalize();
    let params = state.into_params();

    Ok(EmittedQuery { sql, params })
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
        out
    }

    // -----------------------------------------------------------------------
    // search only
    // -----------------------------------------------------------------------

    #[test]
    fn search_single_field_filter() {
        assert_snapshot!(emit_dsl("service:nginx"));
    }

    #[test]
    fn search_multiple_filters() {
        assert_snapshot!(emit_dsl("service:nginx level:error"));
    }

    #[test]
    fn search_comparison_gt() {
        assert_snapshot!(emit_dsl("status:>400"));
    }

    #[test]
    fn search_comparison_gte() {
        assert_snapshot!(emit_dsl("status:>=400"));
    }

    #[test]
    fn search_comparison_lt() {
        assert_snapshot!(emit_dsl("status:<300"));
    }

    #[test]
    fn search_comparison_ne() {
        assert_snapshot!(emit_dsl("status:!=200"));
    }

    #[test]
    fn search_in_list() {
        assert_snapshot!(emit_dsl("status:200,301,404"));
    }

    #[test]
    fn search_glob() {
        assert_snapshot!(emit_dsl("path:glob:/api/*"));
    }

    #[test]
    fn search_regex() {
        assert_snapshot!(emit_dsl(r"host:/web-\d+/"));
    }

    #[test]
    fn search_time_filter() {
        assert_snapshot!(emit_dsl("last:2h"));
    }

    #[test]
    fn search_time_filter_days() {
        assert_snapshot!(emit_dsl("last:7d"));
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
            r#"service:nginx level:error last:2h "connection refused" -debug"#
        ));
    }

    #[test]
    fn search_or_two_services() {
        assert_snapshot!(emit_dsl("service:kernel OR service:fleetd"));
    }

    #[test]
    fn search_or_with_stats() {
        assert_snapshot!(emit_dsl("service:kernel OR service:fleetd | stats count()"));
    }

    #[test]
    fn search_or_multi_token_groups() {
        assert_snapshot!(emit_dsl(
            "service:nginx level:error OR service:postgres level:warn"
        ));
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
            "service:nginx | stats count() by host | where count > 10"
        ));
    }

    #[test]
    fn multi_stats_then_sort_then_limit() {
        assert_snapshot!(emit_dsl(
            "service:nginx | stats count() by host | sort -count | limit 10"
        ));
    }

    #[test]
    fn multi_full_pipeline() {
        assert_snapshot!(emit_dsl(
            "service:nginx last:2h | stats count() by host | where count > 10 | sort -count | limit 5"
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
            "service:nginx | stats avg(duration) by status | table status, avg_duration"
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
        assert_snapshot!(emit_dsl("status:200"));
    }

    #[test]
    fn field_filter_with_float_coercion() {
        assert_snapshot!(emit_dsl("score:3.14"));
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
        let result = emit(&query, "~/.fleet/data/*.parquet");
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
        assert_snapshot!(emit_dsl("service:nginx | top 10 uri"));
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
            r#"service:nginx | extract "(?P<code>[0-9]{3})" from message | where code == "500""#
        ));
    }

    #[test]
    fn error_extract_no_named_groups() {
        assert_snapshot!(emit_dsl_err(r#"* | extract "([0-9]+)" from message"#));
    }

    #[test]
    fn error_extract_kv_not_implemented() {
        assert_snapshot!(emit_dsl_err("* | extract kv"));
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
        assert_snapshot!(emit_dsl("service:nginx | dedup host"));
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
        assert_snapshot!(emit_dsl("last:1h | timechart count()"));
    }

    #[test]
    fn pipe_timechart_auto_bucket_7d() {
        assert_snapshot!(emit_dsl("last:7d | timechart count()"));
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
            "service:nginx | pivot avg(duration) on status by host"
        ));
    }

    #[test]
    fn error_pivot_not_terminal() {
        assert_snapshot!(emit_dsl_err(
            "* | pivot count() on status | where count > 5"
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
