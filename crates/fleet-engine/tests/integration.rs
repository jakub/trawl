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

// -- Phase 7: Extended DSL stages --

#[test]
fn top_by_service() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("* | top 3 service", &glob).unwrap();
    // nginx=6, sshd=4, systemd=2 (kernel excluded by limit)
    assert_eq!(result.row_count(), 3);
    assert_eq!(result.columns[0].name, "service");
    assert_eq!(result.columns[1].name, "count");
    // first row = highest count = nginx
    assert_eq!(result.rows[0][0], Value::String("nginx".to_string()));
}

#[test]
fn rare_by_service() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("* | rare 2 service", &glob).unwrap();
    // kernel=1, systemd=2 (ascending order)
    assert_eq!(result.row_count(), 2);
    assert_eq!(result.columns[0].name, "service");
    // first row = lowest count = kernel
    assert_eq!(result.rows[0][0], Value::String("kernel".to_string()));
}

#[test]
fn drop_columns() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("* | drop message, src_ip", &glob)
        .unwrap();
    assert_eq!(result.row_count(), 13);
    let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(!col_names.contains(&"message"));
    assert!(!col_names.contains(&"src_ip"));
}

#[test]
fn let_computed_column() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("service:nginx | let duration_ms = duration * 1000", &glob)
        .unwrap();
    assert_eq!(result.row_count(), 6);
    let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(col_names.contains(&"duration_ms"));
}

#[test]
fn extract_ip_from_message() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max(
            r#"service:sshd | extract "from (?P<extracted_ip>[0-9]+[.][0-9]+[.][0-9]+[.][0-9]+)" from message"#,
            &glob,
        )
        .unwrap();
    assert_eq!(result.row_count(), 4);
    let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(col_names.contains(&"extracted_ip"));
}

#[test]
fn dedup_single_field() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("* | dedup host", &glob).unwrap();
    // 4 unique hosts: web01, web02, bastion, db01
    assert_eq!(result.row_count(), 4);
    // _rn helper column should be excluded from output
    let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(!col_names.contains(&"_rn"));
}

#[test]
fn dedup_multiple_fields() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("* | dedup host, service", &glob)
        .unwrap();
    // 6 unique combos: web01+nginx, web01+systemd, web02+nginx,
    // bastion+sshd, db01+systemd, db01+kernel
    assert_eq!(result.row_count(), 6);
}

#[test]
fn timechart_minute_buckets() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("* | timechart span=1m count()", &glob)
        .unwrap();
    // 3 minute buckets: 10:00 (6 nginx), 10:01 (4 sshd), 10:02 (3 system)
    assert_eq!(result.row_count(), 3);
    assert_eq!(result.columns[0].name, "_time");
    assert_eq!(result.columns[1].name, "count");
}

#[test]
fn pivot_on_service() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("* | pivot count() on service", &glob)
        .unwrap();
    // no GROUP BY → single aggregated row with dynamic columns per service
    assert_eq!(result.row_count(), 1);
    assert!(result.columns.len() >= 4); // at least nginx, sshd, systemd, kernel
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

// -- error path tests --------------------------------------------------------

#[test]
fn invalid_dsl_returns_parse_error() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("| | | broken {{{", &glob);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), EngineError::Parse(_)));
}

#[test]
fn missing_source_returns_error() {
    let exec = Executor::new().expect("executor should initialize");
    let result = exec.run_query_max("*", "/nonexistent/path/**/*.parquet");
    // DuckDB returns an IO error for missing files — should propagate.
    assert!(result.is_err());
}

// -- schema introspection tests ----------------------------------------------

#[test]
fn describe_schema_returns_columns() {
    let (exec, glob) = setup();
    let schema = exec.describe_schema(&glob).unwrap();

    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"timestamp"), "missing timestamp column");
    assert!(names.contains(&"host"), "missing host column");
    assert!(names.contains(&"service"), "missing service column");
    assert!(names.contains(&"level"), "missing level column");
    assert!(names.contains(&"message"), "missing message column");
    assert!(schema.file_count > 0, "should find fixture files");
}

#[test]
fn describe_schema_missing_source() {
    let exec = Executor::new().expect("executor should initialize");
    let result = exec.describe_schema("/nonexistent/path/**/*.parquet");
    assert!(result.is_err());
}
