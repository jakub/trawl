//! End-to-end integration tests: DSL → parse → emit → `DuckDB` → results.
//!
//! These tests validate the full pipeline against parquet fixture files
//! generated at test time. They catch bugs that unit tests miss, like
//! valid SQL that `DuckDB` actually rejects.

mod common;

use fleet_engine::error::EngineError;
use fleet_engine::executor::Executor;
use fleet_engine::value::{QueryResult, Value};

/// Test helper — runs query with no row limit.
trait RunQueryUnlimited {
    fn run_query_max(&self, dsl: &str, source: &str) -> Result<QueryResult, EngineError>;
}

impl RunQueryUnlimited for Executor {
    fn run_query_max(&self, dsl: &str, source: &str) -> Result<QueryResult, EngineError> {
        self.run_query(dsl, source, usize::MAX)
    }
}

fn setup() -> (Executor, String) {
    let glob = common::fixture_glob();
    let exec = Executor::new().expect("executor should initialize");
    (exec, glob)
}

#[test]
fn wildcard_returns_all_rows() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("*", &glob).unwrap();
    assert_eq!(result.row_count(), 13);
}

#[test]
fn field_filter_service() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("service:nginx", &glob).unwrap();
    assert_eq!(result.row_count(), 6);
}

#[test]
fn field_filter_level_error() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("level:error", &glob).unwrap();
    // nginx 500, nginx 502, sshd "Connection refused"
    assert_eq!(result.row_count(), 3);
}

#[test]
fn text_search_ilike() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("error", &glob).unwrap();
    // matches messages containing "error": nginx 500 ("internal server error"),
    // nginx 502 ("bad gateway" — no "error"), sshd ("Connection refused" — no "error")
    // only the 500 message contains the word "error"
    assert!(result.row_count() >= 1);
}

#[test]
fn quoted_search() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max(r#""connection refused""#, &glob)
        .unwrap();
    // case-insensitive: matches "Connection refused from 10.0.0.99"
    assert_eq!(result.row_count(), 1);
}

#[test]
fn negated_text_search() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("-error service:nginx", &glob).unwrap();
    // nginx rows whose message does NOT contain "error"
    // excludes the 500 row ("internal server error")
    assert!(result.row_count() < 6);
    assert!(result.row_count() > 0);
}

#[test]
fn status_comparison_gte() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("status:>=400", &glob).unwrap();
    // 404, 500, 502
    assert_eq!(result.row_count(), 3);
}

#[test]
fn in_list_filter() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("status:200,301", &glob).unwrap();
    // 2x 200, 1x 301
    assert_eq!(result.row_count(), 3);
}

#[test]
fn stats_count_by_service() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("* | stats count() by service", &glob)
        .unwrap();
    // nginx, sshd, systemd, kernel
    assert_eq!(result.row_count(), 4);
    assert_eq!(result.columns[0].name, "service");
    assert_eq!(result.columns[1].name, "count");
}

#[test]
fn stats_with_where_cte() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max(
            "service:nginx | stats count() by host | where count > 2",
            &glob,
        )
        .unwrap();
    // web01 has 4 nginx rows, web02 has 2 — only web01 passes
    assert_eq!(result.row_count(), 1);
    assert_eq!(result.rows[0][0], Value::String("web01".to_string()));
}

#[test]
fn sort_and_limit() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max(
            "service:nginx | stats count() by host | sort -count | limit 1",
            &glob,
        )
        .unwrap();
    assert_eq!(result.row_count(), 1);
    // web01 has 4 nginx rows (most), should be first when sorted desc
    assert_eq!(result.rows[0][0], Value::String("web01".to_string()));
}

#[test]
fn table_projection() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("service:nginx | table host, status, uri", &glob)
        .unwrap();
    assert_eq!(result.columns.len(), 3);
    assert_eq!(result.columns[0].name, "host");
    assert_eq!(result.columns[1].name, "status");
    assert_eq!(result.columns[2].name, "uri");
    assert_eq!(result.row_count(), 6);
}

#[test]
fn full_pipeline() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max(
            "service:nginx | stats count(), avg(duration) by host | sort -count | limit 5",
            &glob,
        )
        .unwrap();
    assert!(result.row_count() > 0);
    // should have: host, count, avg_duration
    assert!(result.columns.len() >= 3);
}

#[test]
fn empty_result() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("service:nonexistent", &glob).unwrap();
    assert_eq!(result.row_count(), 0);
    // columns should still be present from the parquet schema
    assert!(!result.columns.is_empty());
}

#[test]
fn time_filter_large_window() {
    let (exec, glob) = setup();
    // fixtures are from 2024-01-15 — use a massive window to include them
    let result = exec.run_query_max("last:99999d", &glob).unwrap();
    assert_eq!(result.row_count(), 13);
}

// -- JSON source tests (validates read_json_auto pipeline) --

fn setup_json() -> (Executor, String) {
    let glob = common::fixture_glob_json();
    let exec = Executor::new().expect("executor should initialize");
    (exec, glob)
}

#[test]
fn json_wildcard_returns_all_rows() {
    let (exec, glob) = setup_json();
    let result = exec.run_query_max("*", &glob).unwrap();
    assert_eq!(result.row_count(), 13);
}

#[test]
fn json_field_filter() {
    let (exec, glob) = setup_json();
    let result = exec.run_query_max("service:nginx", &glob).unwrap();
    assert_eq!(result.row_count(), 6);
}

#[test]
fn json_stats_pipeline() {
    let (exec, glob) = setup_json();
    let result = exec
        .run_query_max("* | stats count() by service | sort -count", &glob)
        .unwrap();
    assert!(result.row_count() > 0);
    assert_eq!(result.columns[0].name, "service");
}
