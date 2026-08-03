// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end integration tests: DSL → parse → emit → `DuckDB` → results.
//!
//! These tests validate the full pipeline against parquet fixture files
//! generated at test time. They catch bugs that unit tests miss, like
//! valid SQL that `DuckDB` actually rejects.

mod common;

use trawl_core::schema::FieldTypes;
use trawl_engine::error::EngineError;
use trawl_engine::executor::Executor;
use trawl_engine::value::{QueryResult, Value};

/// Test helper — runs query with no row limit.
trait RunQueryUnlimited {
    fn run_query_max(&self, dsl: &str, source: &str) -> Result<QueryResult, EngineError>;
}

impl RunQueryUnlimited for Executor {
    fn run_query_max(&self, dsl: &str, source: &str) -> Result<QueryResult, EngineError> {
        self.run_query(dsl, source, usize::MAX, 0)
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
    let result = exec.run_query_max("service=nginx", &glob).unwrap();
    assert_eq!(result.row_count(), 6);
}

#[test]
fn field_filter_level_error() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("level=error", &glob).unwrap();
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
    let result = exec.run_query_max("-error service=nginx", &glob).unwrap();
    // nginx rows whose message does NOT contain "error"
    // excludes the 500 row ("internal server error")
    assert!(result.row_count() < 6);
    assert!(result.row_count() > 0);
}

#[test]
fn status_comparison_gte() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("status>=400", &glob).unwrap();
    // 404, 500, 502
    assert_eq!(result.row_count(), 3);
}

#[test]
fn in_list_filter() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("status=200,301", &glob).unwrap();
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
fn stats_group_by_repairs_separates_clean_from_repaired() {
    // ADR-0009: `_repairs` is NULL on clean events and carries the repair
    // codes on repaired ones, so it must behave as an ordinary groupable
    // dimension — one group per distinct code plus a NULL group for the
    // untouched majority. The fixture seeds exactly one repaired row
    // (`host.from_peer`) among 13.
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("* | stats count() by _repairs", &glob)
        .unwrap();

    assert_eq!(result.columns[0].name, "_repairs");
    let groups: Vec<(&Value, &Value)> = result.rows.iter().map(|r| (&r[0], &r[1])).collect();
    assert_eq!(
        groups.len(),
        2,
        "expected a clean group and one repaired group, got {groups:?}"
    );
    assert!(
        groups.contains(&(&Value::Null, &Value::Integer(12))),
        "the 12 clean events must aggregate under a NULL `_repairs`: {groups:?}"
    );
    assert!(
        groups.contains(&(
            &Value::String("host.from_peer".to_string()),
            &Value::Integer(1)
        )),
        "the repaired event must aggregate under its repair code: {groups:?}"
    );
}

#[test]
fn stats_with_where_cte() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max(
            "service=nginx | stats count() by host | where count > 2",
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
            "service=nginx | stats count() by host | sort -count | limit 1",
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
        .run_query_max("service=nginx | table host, status, uri", &glob)
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
            "service=nginx | stats count(), avg(duration) by host | sort -count | limit 5",
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
    let result = exec.run_query_max("service=nonexistent", &glob).unwrap();
    assert_eq!(result.row_count(), 0);
    // columns should still be present from the parquet schema
    assert!(!result.columns.is_empty());
}

#[test]
fn time_filter_large_window() {
    let (exec, glob) = setup();
    // fixtures are from 2024-01-15 — use a massive window to include them
    let result = exec.run_query_max("last=99999d", &glob).unwrap();
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
        .run_query_max("service=nginx | let duration_ms = duration * 1000", &glob)
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
            r#"service=sshd | extract "from (?P<extracted_ip>[0-9]+[.][0-9]+[.][0-9]+[.][0-9]+)" from message"#,
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
fn timechart_bucket_values_group_by_full_expression() {
    // The bucket is aliased AS "_time" — the same name as the source column
    // it is derived from. Grouping must reference the full time_bucket
    // expression, not the ambiguous alias, or DuckDB groups by the raw
    // source column and the aggregation is silently wrong (one output row
    // per input row). Pin actual bucket values and counts, not SQL text.
    let (exec, glob) = setup();
    let result = exec
        .run_query_max("* | timechart span=1m count()", &glob)
        .unwrap();
    assert_eq!(result.row_count(), 3, "three distinct minute buckets");
    let buckets: Vec<(String, i64)> = result
        .rows
        .iter()
        .map(|r| {
            let Value::String(t) = &r[0] else {
                panic!("bucket must format as a timestamp string, got {:?}", r[0])
            };
            let Value::Integer(c) = r[1] else {
                panic!("count must be an integer, got {:?}", r[1])
            };
            (t.clone(), c)
        })
        .collect();
    assert_eq!(
        buckets,
        vec![
            ("2024-01-15 10:00:00".to_string(), 6),
            ("2024-01-15 10:01:00".to_string(), 4),
            ("2024-01-15 10:02:00".to_string(), 3),
        ],
        "rows must aggregate into whole-minute buckets"
    );
}

#[test]
fn level_gte_warn_returns_only_warn_and_above() {
    // `level>=warn` compiles to `severity >= 13`; only WARN-and-above rows
    // (severity 13 and 17 in the fixture) come back.
    let (exec, glob) = setup();
    let result = exec.run_query_max("level>=warn", &glob).unwrap();
    assert_eq!(result.row_count(), 7, "4 warn + 3 error rows");
    let sev_idx = result
        .columns
        .iter()
        .position(|c| c.name == "severity")
        .expect("severity column present");
    for row in &result.rows {
        let Value::Integer(sev) = row[sev_idx] else {
            panic!("severity must be an integer, got {:?}", row[sev_idx])
        };
        assert!(sev >= 13, "row below the WARN band leaked through: {sev}");
    }
}

#[test]
fn level_eq_error_matches_the_error_band() {
    // `level=error` compiles to `severity BETWEEN 17 AND 20`.
    let (exec, glob) = setup();
    let result = exec.run_query_max("level=error", &glob).unwrap();
    assert_eq!(result.row_count(), 3, "exactly the three ERROR-band rows");
}

/// Grouping or projecting on `level` must fail loudly through the whole
/// executor, not just inside the emitter.
///
/// `level` is consumed at ingest, so these used to reach `DuckDB` as a
/// missing column: the hot/cold ladder classified that binder error as
/// benign, fell back to hot-only, and the re-emit's second binder error
/// became an empty result — a pre-cutover saved query returned zero rows
/// and a 200 instead of saying its column was gone.
#[test]
fn level_outside_a_comparison_errors_instead_of_returning_no_rows() {
    let (exec, glob) = setup();
    for dsl in [
        "* | stats count() by level",
        "* | table level",
        "* | sort level",
    ] {
        let err = exec
            .run_query_max(dsl, &glob)
            .expect_err("a `level` column reference must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("filter-only alias"),
            "{dsl} must explain the severity alias, got: {msg}"
        );
    }
}

#[test]
fn bare_word_matches_content_only_in_raw() {
    // "gateway-detail" appears only in one row's `_raw`, never in `message`
    // — bare-word search must cover both columns.
    let (exec, glob) = setup();
    let result = exec.run_query_max("gateway-detail", &glob).unwrap();
    assert_eq!(result.row_count(), 1, "the _raw-only term must match");

    // And a message-only term still matches (that row's _raw is "raw4").
    let result = exec.run_query_max("redirect", &glob).unwrap();
    assert_eq!(result.row_count(), 1, "message-only terms keep matching");
}

/// Write a parquet file shaped like the ingest canonicalizer's output: no
/// collector sent a pre-parse line, so `_raw` is the server's JSON
/// serialization of the event as it arrived.
fn canonicalized_parquet(dir: &std::path::Path) -> String {
    let path = dir.join("canonical.parquet");
    let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES
             (TIMESTAMP '2024-01-15 10:00:00', 'nginx', 'started',
              '{{\"service\":\"nginx\",\"debug_mode\":false,\"message\":\"started\"}}'),
             (TIMESTAMP '2024-01-15 10:00:01', 'sshd', 'accepted key',
              '{{\"service\":\"sshd\",\"message\":\"accepted key\"}}')
         ) t(_time, service, message, _raw)) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .expect("canonicalized fixture should be written");
    path.display().to_string()
}

/// Bare-word search over a server-filled `_raw` is whole-event search
/// (ADR-0009): the term reaches another field's *value* and a field *name*,
/// and negation excludes on exactly the same basis. Pinned because it is a
/// decision the DSL reference documents, not an accident of the fill.
#[test]
fn bare_word_search_reaches_the_whole_event_through_raw() {
    let dir = tempfile::tempdir().unwrap();
    let source = canonicalized_parquet(dir.path());
    let exec = Executor::new().expect("executor should initialize");

    // Another field's value: "nginx" is in `service`, never in `message`.
    let by_value = exec.run_query_max("nginx", &source).expect("field value");
    assert_eq!(
        by_value.row_count(),
        1,
        "a bare term must find the event through another field's value"
    );

    // A field name: "debug" exists only as the key `debug_mode`.
    let by_name = exec.run_query_max("debug", &source).expect("field name");
    assert_eq!(
        by_name.row_count(),
        1,
        "serialized field names are part of the searched text"
    );

    // Negation is the exact mirror — the same row drops out.
    let negated = exec.run_query_max("-debug", &source).expect("negated");
    assert_eq!(
        negated.row_count(),
        1,
        "negation mirrors the positive match"
    );
    let svc = negated
        .columns
        .iter()
        .position(|c| c.name == "service")
        .expect("service column");
    assert_eq!(
        negated.rows[0][svc],
        Value::String("sshd".into()),
        "the row whose serialization contains 'debug' is the one excluded"
    );

    // A field filter never consults `_raw` — that is the narrow form.
    let confined = exec
        .run_query_max("message=/debug/", &source)
        .expect("field filter");
    assert_eq!(
        confined.row_count(),
        0,
        "message=/debug/ must not see the field name in _raw"
    );
}

#[test]
fn well_known_fields_match_core_schema() {
    // trawl-api duplicates the envelope ordering constants because it does
    // not depend on trawl-core; this pins the two in sync.
    assert_eq!(
        trawl_api::value::WELL_KNOWN_LOG_FIELDS,
        trawl_core::schema::LEADING_LOG_FIELDS,
        "WELL_KNOWN_LOG_FIELDS must mirror trawl_core::schema::LEADING_LOG_FIELDS"
    );
    assert_eq!(
        trawl_api::value::TRAILING_LOG_FIELDS,
        trawl_core::schema::TRAILING_LOG_FIELDS,
        "TRAILING_LOG_FIELDS must mirror trawl_core::schema::TRAILING_LOG_FIELDS"
    );
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
    let result = exec.run_query_max("service=nginx", &glob).unwrap();
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

// -- sources outside the ADR-0009 envelope -----------------------------------

/// Write a parquet file with a `message` but no `_raw` column — user-owned
/// data read in embedded mode (`trawl query --data ...`), or any corpus trawl
/// did not write itself.
fn foreign_parquet(dir: &std::path::Path) -> String {
    let path = dir.join("foreign.parquet");
    let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES
             (TIMESTAMP '2024-01-15 10:00:00', 'nginx', 'boom error'),
             (TIMESTAMP '2024-01-15 10:00:01', 'nginx', 'all quiet')
         ) t(_time, service, message)) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .expect("foreign fixture should be written");
    path.display().to_string()
}

/// Bare-word and quoted-phrase search degrade to `message` alone rather than
/// failing the whole query when the source has no `_raw` column.
#[test]
fn text_search_without_a_raw_column_searches_message() {
    let dir = tempfile::tempdir().unwrap();
    let source = foreign_parquet(dir.path());
    let exec = Executor::new().expect("executor should initialize");

    let bare = exec.run_query_max("boom", &source).expect("bare word");
    assert_eq!(bare.row_count(), 1, "bare word must match on message");

    let quoted = exec
        .run_query_max(r#""boom error""#, &source)
        .expect("quoted phrase");
    assert_eq!(quoted.row_count(), 1, "quoted phrase must match on message");

    let negated = exec.run_query_max("-boom", &source).expect("negated");
    assert_eq!(
        negated.row_count(),
        1,
        "negation must not veto on a missing _raw"
    );
    assert!(
        !negated.columns.iter().any(|c| c.name == "_raw"),
        "the fallback must not invent a _raw column: {:?}",
        negated.columns
    );
}

/// The fallback is evidence-based: it rescues a query whose only unbindable
/// column was `_raw`. A genuinely unknown field still errors.
#[test]
fn text_search_without_a_raw_column_keeps_unknown_field_errors() {
    let dir = tempfile::tempdir().unwrap();
    let source = foreign_parquet(dir.path());
    let exec = Executor::new().expect("executor should initialize");

    let result = exec.run_query_max("boom nosuchfield=1", &source);
    assert!(
        matches!(result, Err(EngineError::Emit(_))),
        "unknown field must still error: {result:?}"
    );
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
fn missing_source_returns_empty_result() {
    let exec = Executor::new().expect("executor should initialize");
    let result = exec
        .run_query_max("*", "/nonexistent/path/**/*.parquet")
        .expect("no-files-found should return empty result, not error");
    // A glob matching zero files is semantically "no data", not an error.
    assert!(result.is_empty());
}

// -- schema introspection tests ----------------------------------------------

#[test]
fn describe_schema_returns_columns() {
    let (exec, glob) = setup();
    let schema = exec.describe_schema(&glob).unwrap();

    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"_time"), "missing _time column");
    assert!(names.contains(&"_ingested"), "missing _ingested column");
    assert!(names.contains(&"_raw"), "missing _raw column");
    assert!(names.contains(&"env"), "missing env column");
    assert!(names.contains(&"host"), "missing host column");
    assert!(names.contains(&"service"), "missing service column");
    assert!(names.contains(&"severity"), "missing severity column");
    assert!(names.contains(&"message"), "missing message column");
    assert!(schema.file_count > 0, "should find fixture files");
}

#[test]
fn describe_schema_missing_source() {
    let exec = Executor::new().expect("executor should initialize");
    let result = exec.describe_schema("/nonexistent/path/**/*.parquet");
    assert!(result.is_err());
}

// -- extract kv integration tests -------------------------------------------

/// Create a parquet fixture with kv-style messages and return (executor, glob).
///
/// Uses a separate directory so the kv fixture doesn't pollute the main
/// `fixtures/parquet/**/*.parquet` glob used by other tests.
fn setup_kv() -> (Executor, String) {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("kv");
    let path = dir.join("kv_logs.parquet");
    if !path.exists() {
        std::fs::create_dir_all(&dir).unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE kv_logs (_time TIMESTAMP, host VARCHAR, message VARCHAR);
             INSERT INTO kv_logs VALUES
             ('2024-01-15 10:00:00', 'web01', 'method=GET status=200 path=/api duration=0.045'),
             ('2024-01-15 10:00:01', 'web01', 'method=POST status=500 path=/api/create duration=1.234'),
             ('2024-01-15 10:00:02', 'web02', 'method=GET status=200 path=/health duration=0.002'),
             ('2024-01-15 10:00:03', 'web02', 'method=PUT status=404 path=/api/update duration=0.100'),
             ('2024-01-15 10:00:04', 'web01', 'method=GET status=301 path=/old duration=0.001');",
        )
        .unwrap();
        conn.execute_batch(&format!(
            "COPY kv_logs TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }
    let exec = Executor::new().expect("executor should initialize");
    (exec, format!("{}", path.display()))
}

#[test]
fn extract_kv_basic_pipeline() {
    let (exec, src) = setup_kv();
    let result = exec
        .run_query("* | extract kv | head 5", &src, 1000, 0)
        .unwrap();
    // Should have original columns + extracted kv columns.
    let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(col_names.contains(&"method"), "missing extracted 'method'");
    assert!(col_names.contains(&"status"), "missing extracted 'status'");
    assert!(col_names.contains(&"path"), "missing extracted 'path'");
    assert!(
        col_names.contains(&"duration"),
        "missing extracted 'duration'"
    );
    assert_eq!(result.row_count(), 5);
}

#[test]
fn extract_kv_with_where() {
    let (exec, src) = setup_kv();
    let result = exec
        .run_query("* | extract kv | where status >= 400", &src, 1000, 0)
        .unwrap();
    // status >= 400: 500 and 404 → 2 rows.
    assert_eq!(result.row_count(), 2);
}

#[test]
fn extract_kv_with_stats() {
    let (exec, src) = setup_kv();
    let result = exec
        .run_query(
            "* | extract kv | stats count() by method | sort -count",
            &src,
            1000,
            0,
        )
        .unwrap();
    // GET: 3, POST: 1, PUT: 1 → 3 groups.
    assert_eq!(result.row_count(), 3);
    let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(col_names.contains(&"method"));
    assert!(col_names.contains(&"count"));
}

#[test]
fn extract_kv_with_search_prefix() {
    let (exec, src) = setup_kv();
    let result = exec
        .run_query("host=web01 | extract kv | head 10", &src, 1000, 0)
        .unwrap();
    // web01 has 3 rows.
    assert_eq!(result.row_count(), 3);
    let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(col_names.contains(&"method"));
}

// -- parquet export tests ----------------------------------------------------

#[test]
fn export_parquet_writes_valid_file() {
    let (exec, glob) = setup();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_owned();
    // Remove the temp file so export_parquet creates it fresh.
    drop(tmp);

    exec.export_parquet("service=nginx | head 3", &glob, &path, 1000)
        .unwrap();

    // Verify the file exists and is re-readable via DuckDB.
    assert!(path.exists());
    let read_glob = format!("{}", path.display());
    let result = exec.run_query_max("*", &read_glob).unwrap();
    assert_eq!(result.row_count(), 3);

    std::fs::remove_file(&path).ok();
}

#[test]
fn export_parquet_with_stats_roundtrips() {
    let (exec, glob) = setup();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_owned();
    drop(tmp);

    exec.export_parquet(
        "* | stats count() by service | sort -count",
        &glob,
        &path,
        1000,
    )
    .unwrap();

    let read_glob = format!("{}", path.display());
    let result = exec.run_query_max("*", &read_glob).unwrap();
    // 13 rows across nginx/sshd/systemd/kernel = 4 services.
    assert_eq!(result.row_count(), 4);

    std::fs::remove_file(&path).ok();
}

#[test]
fn export_parquet_rejects_rust_stages() {
    let (exec, src) = setup_kv();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_owned();
    drop(tmp);

    let err = exec
        .export_parquet("* | extract kv | head 5", &src, &path, 1000)
        .unwrap_err();
    assert!(
        err.to_string().contains("post-processing"),
        "expected post-processing error, got: {err}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn hot_cold_type_conflict_keeps_both_rows() {
    // Cold parquet has `meta` as a STRUCT (object value); the hot snapshot
    // has it as a plain string (VARCHAR). The hot+cold UNION ALL BY NAME
    // raises a Conversion Error. The executor must detect the conflict,
    // coerce `meta` to VARCHAR on both sides, and keep BOTH the cold and hot
    // rows — not silently fall back to hot-only and drop the cold row.
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let cold = dir.path().join("cold.parquet");
    let hot = dir.path().join("hot.ndjson");

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, {{'a': 1}} AS meta) \
         TO '{}' (FORMAT PARQUET)",
        cold.display()
    ))
    .unwrap();

    std::fs::write(
        &hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"meta\":\"plain\"}\n",
    )
    .unwrap();

    let exec = Executor::new().expect("executor should initialize");
    let source = format!("{}/*.parquet", dir.path().display());
    let result = exec
        .run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .unwrap();

    assert_eq!(
        result.row_count(),
        2,
        "both cold (struct meta) and hot (string meta) rows must survive the coerced retry"
    );
}

#[test]
fn hot_cold_type_conflict_keeps_both_rows_for_a_pruned_list_source() {
    // Same STRUCT-vs-VARCHAR `meta` conflict as above, but behind the LIST
    // source shape the server emits for every time-filtered query, with one
    // element pointing at an hour dir holding no file (routine for a
    // sparse-traffic service). The first read of that list reports "no files"
    // — so the conflict cannot surface until the empty element is pruned, and
    // the pruned read must get the coerced retry too. Pre-fix it did not, and
    // the repairable conflict fell through the outcome policy as a hard error.
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    let hot = dir.path().join("hot.ndjson");

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, {{'a': 1}} AS meta) \
         TO '{}' (FORMAT PARQUET)",
        full.join("cold.parquet").display()
    ))
    .unwrap();

    std::fs::write(
        &hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"meta\":\"plain\"}\n",
    )
    .unwrap();

    let exec = Executor::new().expect("executor should initialize");
    let source = format!(
        "['{}/*.parquet', '{}/*.parquet']",
        full.display(),
        empty.display()
    );
    let result = exec
        .run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect("a repairable hot/cold conflict must not hard-error on a pruned list source");

    assert_eq!(
        result.row_count(),
        2,
        "both cold (struct meta) and hot (string meta) rows must survive the coerced retry"
    );
}

#[test]
fn export_parquet_hot_cold_type_conflict_keeps_both_rows() {
    // The export lane of `hot_cold_type_conflict_keeps_both_rows`: same
    // STRUCT-vs-VARCHAR `meta` conflict, exported instead of queried. The
    // conflict is repairable, so the coerced retry must keep BOTH rows.
    // Pre-fix only the query path retried, so the very same query succeeded as
    // CSV/JSON (which route through the query path) and 500'd as parquet.
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let cold = dir.path().join("cold.parquet");
    let hot = dir.path().join("hot.ndjson");
    let out = dir.path().join("export.parquet");

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, {{'a': 1}} AS meta) \
         TO '{}' (FORMAT PARQUET)",
        cold.display()
    ))
    .unwrap();

    std::fs::write(
        &hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"meta\":\"plain\"}\n",
    )
    .unwrap();

    let exec = Executor::new().expect("executor should initialize");
    let source = format!("{}/*.parquet", dir.path().display());
    exec.export_parquet_with_hot(
        "*",
        &source,
        hot.to_str().unwrap(),
        &FieldTypes::new(),
        &out,
        1000,
    )
    .expect("a repairable hot/cold conflict must not hard-error the parquet export");

    let rows: i64 = conn
        .query_row(
            &format!(
                "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                out.display()
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 2,
        "both cold (struct meta) and hot (string meta) rows must land in the exported parquet"
    );
}

#[test]
fn export_parquet_hot_cold_type_conflict_keeps_both_rows_for_a_pruned_list_source() {
    // The export lane of the pruned-list conflict: the first read of the list
    // reports "no files" because one element points at an hour dir holding no
    // file, so the conflict cannot surface until the empty element is pruned —
    // and the pruned export must get the coerced retry too.
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    let hot = dir.path().join("hot.ndjson");
    let out = dir.path().join("export.parquet");

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, {{'a': 1}} AS meta) \
         TO '{}' (FORMAT PARQUET)",
        full.join("cold.parquet").display()
    ))
    .unwrap();

    std::fs::write(
        &hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"meta\":\"plain\"}\n",
    )
    .unwrap();

    let exec = Executor::new().expect("executor should initialize");
    let source = format!(
        "['{}/*.parquet', '{}/*.parquet']",
        full.display(),
        empty.display()
    );
    exec.export_parquet_with_hot(
        "*",
        &source,
        hot.to_str().unwrap(),
        &FieldTypes::new(),
        &out,
        1000,
    )
    .expect("a repairable conflict behind a pruned list source must not fail the export");

    let rows: i64 = conn
        .query_row(
            &format!(
                "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                out.display()
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 2,
        "both cold (struct meta) and hot (string meta) rows must land in the exported parquet"
    );
}

#[test]
fn hot_cold_malformed_timestamp_keeps_cold_data() {
    // A malformed timestamp in the hot buffer must not throw the hot+cold
    // union (ADR-0008: the partition key is never hard-CAST). Pre-fix this
    // raised a Conversion Error that was misread as a schema conflict and
    // silently degraded the query to hot-only — dropping the entire parquet
    // history. With TRY_CAST on the union's hot side, both rows survive.
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let cold = dir.path().join("cold.parquet");
    let hot = dir.path().join("hot.ndjson");

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, 'cold row' AS message) \
         TO '{}' (FORMAT PARQUET)",
        cold.display()
    ))
    .unwrap();

    std::fs::write(
        &hot,
        "{\"_time\":\"not-a-date\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hot row\"}\n",
    )
    .unwrap();

    let exec = Executor::new().expect("executor should initialize");
    let source = format!("{}/*.parquet", dir.path().display());
    let result = exec
        .run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect("hot+cold query must not error on a malformed hot timestamp");

    assert_eq!(
        result.row_count(),
        2,
        "the cold parquet row must survive alongside the malformed-timestamp hot row"
    );
}

#[test]
fn hot_sparse_repair_column_survives_inside_the_sample_window() {
    // `_repairs` is by construction sparse — it appears only on repaired
    // events (ADR-0009). DuckDB's JSON auto-detection samples a
    // bounded prefix by default (~20480 rows) and then errors on any later
    // record carrying a key outside the inferred schema, so the hot-buffer
    // snapshot writer hoists one event per novel key to the front of the
    // file (`HotBuffer::build_snapshot`) rather than making these readers
    // pay whole-file detection on every query. This asserts the reader half
    // of that contract: a snapshot far larger than the sample window keeps
    // the sparse column as long as it appears in the prefix. Checked with
    // and without cold parquet present: those are two different reader call
    // sites (hot+cold union vs the hot-only reader used before any file has
    // been compacted).
    use duckdb::Connection;
    use std::fmt::Write as _;

    for with_cold in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let hot = dir.path().join("hot.ndjson");

        let mut lines = String::from(
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\
             \"message\":\"repaired\",\"_repairs\":\"time.from_ingest\"}\n",
        );
        for i in 0..30_000 {
            writeln!(
                lines,
                "{{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"m{i}\"}}"
            )
            .unwrap();
        }
        std::fs::write(&hot, lines).unwrap();

        if with_cold {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                              'svc' AS service, 'cold row' AS message) \
                 TO '{}' (FORMAT PARQUET)",
                dir.path().join("cold.parquet").display()
            ))
            .unwrap();
        }

        let exec = Executor::new().expect("executor should initialize");
        let source = format!("{}/*.parquet", dir.path().display());
        let result = exec
            .run_query_with_hot(
                "*",
                &source,
                hot.to_str().unwrap(),
                &FieldTypes::new(),
                usize::MAX,
                0,
            )
            .expect("hot query must succeed");

        let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(
            col_names.contains(&"_repairs"),
            "the sparse repair column must survive a hot snapshot larger than \
             the JSON sample window (with_cold={with_cold}); got columns {col_names:?}"
        );
        // Row counts pin that this rode the real union (with cold present)
        // rather than a silent hot-only fallback.
        let expected = 30_001 + usize::from(with_cold);
        assert_eq!(
            result.row_count(),
            expected,
            "every hot row (and the cold row when present) must survive \
             (with_cold={with_cold})"
        );
    }
}
