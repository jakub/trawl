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
        self.run_query(dsl, source, &FieldTypes::new(), usize::MAX, 0)
    }
}

fn setup() -> (Executor, String) {
    let glob = common::fixture_glob();
    let exec = Executor::new().expect("executor should initialize");
    (exec, glob)
}

fn integer_column(result: &QueryResult, name: &str) -> Vec<i64> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("missing {name} column: {:?}", result.columns));
    let mut values = result
        .rows
        .iter()
        .map(|row| match row[index] {
            Value::Integer(value) => value,
            ref other => panic!("{name} must be an integer, got {other:?}"),
        })
        .collect::<Vec<_>>();
    values.sort_unstable();
    values
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
fn field_filter_severity_number() {
    let (exec, glob) = setup();
    // Embedded `--data` is pin-blind (ADR-0013 residual): the severity
    // TOKEN vocabulary needs the catalog's SEVERITY pin, so here the
    // ladder number is the query.
    let result = exec.run_query_max("severity=17", &glob).unwrap();
    // nginx 500, nginx 502, sshd "Connection refused"
    assert_eq!(result.row_count(), 3);
}

#[test]
fn text_search_ilike() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("error", &glob).unwrap();
    // only the nginx 500 message ("internal server error") carries the word
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

// -- extended DSL stages --

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
fn severity_gte_warn_returns_only_warn_and_above() {
    // Only WARN-and-above rows (severity 13 and 17 in the fixture).
    let (exec, glob) = setup();
    let result = exec.run_query_max("severity>=13", &glob).unwrap();
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
fn severity_error_band_matches_three_rows() {
    let (exec, glob) = setup();
    let result = exec.run_query_max("severity>=17", &glob).unwrap();
    assert_eq!(result.row_count(), 3, "exactly the three ERROR-band rows");
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

/// The PIVOT lane inlines every parameter (`DuckDB` cannot parameterize a
/// PIVOT), and `sev()` emits a `?` inside a string literal: its digits
/// guard, `'[+-]?[0-9]+'`. An inliner that scans for `?` without skipping
/// string literals splices the next user literal into the middle of that
/// regex and shifts every later parameter by one, so this runs the whole
/// shape end to end and the SQL has to actually parse and answer.
#[test]
fn pivot_over_sev_keeps_the_digits_guard_intact() {
    let (exec, glob) = setup();
    let result = exec
        .run_query_max(
            r#"service=nginx | let s = sev(severity_text)                | where message == "GET /missing 404 0.001s not found"                | pivot count() on s"#,
            &glob,
        )
        .expect("pivot over sev() must emit parseable SQL");
    assert_eq!(result.row_count(), 1);
    // Exactly one nginx row carries that message and it is a `warn`, so
    // the pivot has exactly one dynamic column: the ladder number, which
    // is what the column holds. A parameter spliced into the regex, or
    // shifted past it, would either fail to parse or let the other
    // severities through as extra columns.
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["13"], "unexpected pivot columns");
}

/// An aggregation position is still a call, so the per-position literal
/// rules apply there too: binding the dialect of
/// `stats sev(severity, "syslog")` as a parameter would hand
/// `translate_function` a literal `?` and error with
/// `dialect "?" is not in the allowed set`.
#[test]
fn sev_in_an_aggregation_position_keeps_its_dialect() {
    let (exec, glob) = setup();
    // The fixture's `severity` column carries OTel numbers; read as
    // syslog, 9 (info) is out of the 0-7 range and 17 has no reading
    // either, so a syslog reading of this corpus is all NULL while the
    // OTel one is the ladder itself. Both must emit.
    let syslog = exec
        .run_query_max(r#"* | stats max(sev(severity, "syslog")) as m"#, &glob)
        .expect(r#"stats sev(x, "syslog") must emit"#);
    assert_eq!(
        syslog.rows[0][0],
        Value::Null,
        "syslog: 9/13/17 read as nothing"
    );

    let otel = exec
        .run_query_max("* | stats max(sev(severity)) as m", &glob)
        .expect("stats sev(x) must emit");
    assert_eq!(otel.rows[0][0], Value::Integer(17));

    // The bare (non-aggregate) form in the same position still works,
    // and the inversion is real where the numeral IS syslog-shaped.
    let inverted = exec
        .run_query_max(r#"* | let s = 3 | stats max(sev(s, "syslog")) as m"#, &glob)
        .expect("a syslog numeral must invert");
    assert_eq!(inverted.rows[0][0], Value::Integer(17), "syslog 3 is err");

    // And the vocabulary is still closed in that position.
    let err = exec
        .run_query_max(r#"* | stats max(sev(severity, "rfc5424")) as m"#, &glob)
        .expect_err("an unknown dialect must be refused");
    assert!(
        err.to_string().contains("otel, syslog"),
        "must name the vocabulary: {err}"
    );
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
             (1, TIMESTAMP '2024-01-15 10:00:00', 'nginx', 'boom error'),
             (2, TIMESTAMP '2024-01-15 10:00:01', 'nginx', 'all quiet')
         ) t(case_id, _time, service, message)) TO '{}' (FORMAT PARQUET)",
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
    assert_eq!(
        integer_column(&bare, "case_id"),
        [1],
        "bare word must match on message"
    );

    let quoted = exec
        .run_query_max(r#""boom error""#, &source)
        .expect("quoted phrase");
    assert_eq!(
        integer_column(&quoted, "case_id"),
        [1],
        "quoted phrase must match on message"
    );

    for dsl in ["-boom", "NOT boom", r#"NOT "boom error""#] {
        let negated = exec.run_query_max(dsl, &source).expect("negated");
        assert_eq!(
            integer_column(&negated, "case_id"),
            [2],
            "{dsl} must be the complement on a missing _raw"
        );
        assert!(
            !negated.columns.iter().any(|c| c.name == "_raw"),
            "the fallback must not invent a _raw column: {:?}",
            negated.columns
        );
    }
}

/// Every per-row presence/NULL combination of `message` and `_raw` has a
/// two-valued containment answer (ADR-0015). A NULL cell is how a sparse
/// parquet row represents an event that did not carry that field.
#[test]
fn text_containment_is_total_for_sparse_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sparse-text.parquet");
    let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES
             (1, NULL, NULL),
             (2, NULL, 'clean'),
             (3, 'clean', NULL),
             (4, 'clean', 'also clean'),
             (5, 'multi word needle', NULL),
             (6, NULL, 'multi word needle'),
             (7, 'multi word needle', 'clean'),
             (8, 'clean', 'multi word needle'),
             (9, 'multi word needle', 'multi word needle')
         ) t(case_id, message, _raw)) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .expect("sparse text fixture should be written");

    let exec = Executor::new().expect("executor should initialize");
    let source = path.display().to_string();
    let positive = exec
        .run_query_max("needle", &source)
        .expect("positive containment");
    assert_eq!(integer_column(&positive, "case_id"), [5, 6, 7, 8, 9]);

    for dsl in ["-needle", "NOT needle", r#"NOT "multi word""#] {
        let negative = exec
            .run_query_max(dsl, &source)
            .expect("negated containment");
        assert_eq!(
            integer_column(&negative, "case_id"),
            [1, 2, 3, 4],
            "{dsl} must include exactly rows where no carried column contains the term"
        );
    }
}

#[test]
fn text_containment_treats_non_text_columns_as_non_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("non-text.parquet");
    let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES
             (1, 123, TRUE),
             (2, NULL, FALSE)
         ) t(case_id, message, _raw)) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .expect("non-text fixture should be written");

    let exec = Executor::new().expect("executor should initialize");
    let source = path.display().to_string();
    for dsl in ["123", "true"] {
        let positive = exec.run_query_max(dsl, &source).expect("positive search");
        assert!(
            integer_column(&positive, "case_id").is_empty(),
            "{dsl} must not stringify non-text columns"
        );

        let negative = exec
            .run_query_max(&format!("NOT {dsl}"), &source)
            .expect("negated search");
        assert_eq!(integer_column(&negative, "case_id"), [1, 2]);
    }
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
        .run_query("* | extract kv | head 5", &src, &FieldTypes::new(), 1000, 0)
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
        .run_query(
            "* | extract kv | where status >= 400",
            &src,
            &FieldTypes::new(),
            1000,
            0,
        )
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
            &FieldTypes::new(),
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
        .run_query(
            "host=web01 | extract kv | head 10",
            &src,
            &FieldTypes::new(),
            1000,
            0,
        )
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

    exec.export_parquet(
        "service=nginx | head 3",
        &glob,
        &FieldTypes::new(),
        &path,
        1000,
    )
    .unwrap();

    // Verify the file exists and is re-readable via DuckDB.
    assert!(path.exists());
    let read_glob = format!("{}", path.display());
    let result = exec.run_query_max("*", &read_glob).unwrap();
    assert_eq!(result.row_count(), 3);

    std::fs::remove_file(&path).ok();
}

/// A `max_rows` BELOW the query's own row count caps the written file.
/// Every other export test passes a limit larger than the fixture, so the
/// bounded arm could be deleted with the suite still green while
/// `[server] max_export_rows` silently stopped applying.
#[test]
fn export_parquet_row_limit_binds() {
    let (exec, glob) = setup();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_owned();
    drop(tmp);

    // The nginx fixture head is 3 rows; the cap is 2.
    exec.export_parquet(
        "service=nginx | head 3",
        &glob,
        &FieldTypes::new(),
        &path,
        2,
    )
    .unwrap();

    let read_glob = format!("{}", path.display());
    let result = exec.run_query_max("*", &read_glob).unwrap();
    assert_eq!(result.row_count(), 2, "export must honour max_rows");

    std::fs::remove_file(&path).ok();
}

/// A cap ABOVE the INT64 LIMIT domain takes the unbounded shape rather
/// than a `DuckDB` conversion error: `usize::MAX` is the sentinel, but
/// `[server] max_export_rows` is operator-set and every value past
/// `i64::MAX` is equally unnameable in a LIMIT.
#[test]
fn export_parquet_row_limit_above_i64_max_is_unbounded() {
    let (exec, glob) = setup();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_owned();
    drop(tmp);

    exec.export_parquet(
        "service=nginx | head 3",
        &glob,
        &FieldTypes::new(),
        &path,
        usize::MAX - 1,
    )
    .unwrap();

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
        &FieldTypes::new(),
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
        .export_parquet(
            "* | extract kv | head 5",
            &src,
            &FieldTypes::new(),
            &path,
            1000,
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("post-processing"),
        "expected post-processing error, got: {err}"
    );

    std::fs::remove_file(&path).ok();
}

/// Write a one-row cold parquet with a BIGINT `duration` next to a hot
/// ndjson whose `duration` is the string `"n/a"` — the hot-side conflict
/// shape the field catalog resolves via pins. Returns the hot path.
fn write_pin_conflict_corpus(cold_dir: &std::path::Path, hot: &std::path::Path) {
    use duckdb::Connection;
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, 410 AS duration) \
         TO '{}' (FORMAT PARQUET)",
        cold_dir.join("cold.parquet").display()
    ))
    .unwrap();
    std::fs::write(
        hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"duration\":\"n/a\"}\n",
    )
    .unwrap();
}

/// Write a one-row cold parquet whose `meta` is a STRUCT next to a hot
/// ndjson whose `meta` is a plain string — a FOREIGN nonconformant corpus:
/// server-written parquet can never hold a STRUCT (write-time conformance,
/// ADR-0009), so no catalog pin exists for it.
fn write_foreign_struct_corpus(cold_dir: &std::path::Path, hot: &std::path::Path) {
    use duckdb::Connection;
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, {{'a': 1}} AS meta) \
         TO '{}' (FORMAT PARQUET)",
        cold_dir.join("cold.parquet").display()
    ))
    .unwrap();
    std::fs::write(
        hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"meta\":\"plain\"}\n",
    )
    .unwrap();
}

/// The catalog pins for the pin-conflict corpus: `duration` is BIGINT.
fn duration_bigint_pins() -> FieldTypes {
    let mut pins = FieldTypes::new();
    pins.insert("duration", trawl_core::schema::CanonicalType::BigInt);
    pins
}

/// Assert the pin-conflict result: both rows present from one execution of
/// the pin-conformed union, the cold value still a BIGINT integer and the
/// nonconforming hot value degraded to NULL. A String in the duration
/// column would mean a second, stringifying execution ran.
fn assert_pin_conflict_rows(columns: &[trawl_engine::value::Column], rows: &[Vec<Value>]) {
    let dur = columns
        .iter()
        .position(|c| c.name == "duration")
        .expect("duration column must be present");
    assert_eq!(rows.len(), 2, "both cold and hot rows must survive");
    let mut values: Vec<&Value> = rows.iter().map(|r| &r[dur]).collect();
    values.sort_by_key(|v| matches!(v, Value::Null));
    assert_eq!(
        *values[0],
        Value::Integer(410),
        "the cold value must stay BIGINT — a String here means the deleted \
         coerced retry ran a second, stringifying execution"
    );
    assert_eq!(
        *values[1],
        Value::Null,
        "the nonconforming hot value must degrade to NULL, not a string"
    );
}

#[test]
fn hot_pin_conflict_nulls_hot_value_keeps_both_rows() {
    // Cold parquet has `duration` as BIGINT (write-time conformant); the hot
    // snapshot carries "n/a" for it. Under the BIGINT pin the emitter
    // TRY_CASTs the hot branch, so one execution returns both rows with the
    // hot value NULL: no second pass, no VARCHAR coercion, cold data intact.
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    write_pin_conflict_corpus(dir.path(), &hot);

    let exec = Executor::new().expect("executor should initialize");
    let source = format!("{}/*.parquet", dir.path().display());
    let result = exec
        .run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &duration_bigint_pins(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect("a pinned hot conflict must resolve in one execution");

    assert_pin_conflict_rows(&result.columns, &result.rows);
}

// NOTE: no case-variant pin test lives here. Field names are ASCII-folded
// at `envelope::canonicalize`, the one door every producer enters, and
// again at the catalog's entry points (boot seeding, compaction proposals),
// so a `FieldTypes` carrying two spellings of one DuckDB identifier cannot
// be produced by the wired system. The end-to-end proofs live in
// trawl-server's `case_variant_field_names_fold_to_one_column_across_services`
// and `syslog_mixed_case_sd_param_lands_folded_and_pins_folded`.

#[test]
fn hot_pin_conflict_nulls_hot_value_for_a_pruned_list_source() {
    // Same pinned conflict behind the list source shape the server emits,
    // with one element pointing at an hour dir holding no file. Resolution
    // narrows the list before the read, and the narrowed source must carry
    // the pins too.
    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    let hot = dir.path().join("hot.ndjson");
    write_pin_conflict_corpus(&full, &hot);

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
            &duration_bigint_pins(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect("a pinned hot conflict behind a pruned list source must resolve");

    assert_pin_conflict_rows(&result.columns, &result.rows);
}

#[test]
fn export_parquet_hot_pin_conflict_nulls_hot_value() {
    // The export lane of the pinned conflict: both rows land in the exported
    // parquet, with `duration` still BIGINT (a VARCHAR column would mean a
    // second, coercing execution ran).
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    let out = dir.path().join("export.parquet");
    write_pin_conflict_corpus(dir.path(), &hot);

    let exec = Executor::new().expect("executor should initialize");
    let source = format!("{}/*.parquet", dir.path().display());
    exec.export_parquet_with_hot(
        "*",
        &source,
        hot.to_str().unwrap(),
        &duration_bigint_pins(),
        &FieldTypes::new(),
        &out,
        1000,
    )
    .expect("a pinned hot conflict must not fail the parquet export");

    let conn = Connection::open_in_memory().unwrap();
    let (rows, dtype): (i64, String) = conn
        .query_row(
            &format!(
                "SELECT count(*)::BIGINT, \
                        (SELECT typeof(duration) FROM read_parquet('{p}') \
                          WHERE duration IS NOT NULL) \
                 FROM read_parquet('{p}')",
                p = out.display()
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(rows, 2, "both rows must land in the exported parquet");
    assert_eq!(
        dtype, "BIGINT",
        "the pinned column must stay BIGINT in the export"
    );
}

#[test]
fn export_parquet_hot_pin_conflict_nulls_hot_value_for_a_pruned_list_source() {
    // Export lane crossed with the list source shape whose second element
    // reaches no file.
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    let hot = dir.path().join("hot.ndjson");
    let out = dir.path().join("export.parquet");
    write_pin_conflict_corpus(&full, &hot);

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
        &duration_bigint_pins(),
        &FieldTypes::new(),
        &out,
        1000,
    )
    .expect("a pinned conflict behind a pruned list source must not fail the export");

    let conn = Connection::open_in_memory().unwrap();
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
    assert_eq!(rows, 2, "both rows must land in the exported parquet");
}

#[test]
fn foreign_nonconformant_corpus_errors_loudly() {
    // A STRUCT-typed parquet column can only come from foreign parquet
    // dropped into the data root (the boot pass conforms everything else),
    // so no pin exists and the union hard-errors. Degrading to a coerced or
    // hot-only result would hide the breach; the contract is a loud error
    // the operator can act on.
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    write_foreign_struct_corpus(dir.path(), &hot);

    let exec = Executor::new().expect("executor should initialize");
    let source = format!("{}/*.parquet", dir.path().display());
    let err = exec
        .run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect_err("a nonconformant corpus with cold files present must error, not 200");
    assert!(
        matches!(err, EngineError::Database(_)),
        "expected a loud database error, got: {err}"
    );
}

#[test]
fn foreign_nonconformant_corpus_errors_loudly_for_a_pruned_list_source() {
    // Same foreign corpus behind a list source one of whose elements
    // reaches no file: the conflict surfaces on the narrowed read, which
    // must also error loudly rather than fall back hot-only past existing
    // cold files.
    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    let hot = dir.path().join("hot.ndjson");
    write_foreign_struct_corpus(&full, &hot);

    let exec = Executor::new().expect("executor should initialize");
    let source = format!(
        "['{}/*.parquet', '{}/*.parquet']",
        full.display(),
        empty.display()
    );
    let err = exec
        .run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect_err("a nonconformant corpus behind a pruned list must error, not 200");
    assert!(
        matches!(err, EngineError::Database(_)),
        "expected a loud database error, got: {err}"
    );
}

#[test]
fn export_parquet_foreign_nonconformant_corpus_errors_loudly() {
    // Export lane of the foreign-corpus contract: error, not a hot-only file.
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    let out = dir.path().join("export.parquet");
    write_foreign_struct_corpus(dir.path(), &hot);

    let exec = Executor::new().expect("executor should initialize");
    let source = format!("{}/*.parquet", dir.path().display());
    let err = exec
        .export_parquet_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            1000,
        )
        .expect_err("a nonconformant corpus must fail the export loudly");
    assert!(
        matches!(err, EngineError::Database(_)),
        "expected a loud database error, got: {err}"
    );
    assert!(
        !out.exists(),
        "no partial hot-only export may be left behind"
    );
}

#[test]
fn export_parquet_foreign_nonconformant_corpus_errors_loudly_for_a_pruned_list_source() {
    // Export lane crossed with the list source shape whose second element
    // reaches no file.
    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    let hot = dir.path().join("hot.ndjson");
    let out = dir.path().join("export.parquet");
    write_foreign_struct_corpus(&full, &hot);

    let exec = Executor::new().expect("executor should initialize");
    let source = format!(
        "['{}/*.parquet', '{}/*.parquet']",
        full.display(),
        empty.display()
    );
    let err = exec
        .export_parquet_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            1000,
        )
        .expect_err("a nonconformant corpus behind a pruned list must fail the export");
    assert!(
        matches!(err, EngineError::Database(_)),
        "expected a loud database error, got: {err}"
    );
}

#[test]
fn hot_cold_malformed_timestamp_keeps_cold_data() {
    // A malformed timestamp in the hot buffer must not throw the hot+cold
    // union (ADR-0008: the partition key is never hard-CAST). With TRY_CAST
    // on the union's hot side both rows survive; a hard cast raises a
    // Conversion error that reads as a schema conflict and drops the entire
    // parquet history.
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
    // The envelope timestamp pins ride along, as in production: they must
    // not double up on the union's unconditional timestamp TRY_CASTs.
    let mut pins = FieldTypes::new();
    pins.insert("_time", trawl_core::schema::CanonicalType::Timestamp);
    pins.insert("_ingested", trawl_core::schema::CanonicalType::Timestamp);
    let result = exec
        .run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &pins,
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

// ── pin-aware comparisons survive every lane (ADR-0011) ────────────────

/// Cold parquet with a VARCHAR `status` column holding mixed
/// numeric-looking and word values — the write-time-conformant shape a
/// VARCHAR pin guarantees.
fn write_varchar_status_parquet(dir: &std::path::Path) {
    use duckdb::Connection;
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                      'svc' AS service, unnest(['200', '404', 'accepted']) AS status) \
         TO '{}' (FORMAT PARQUET)",
        dir.join("status.parquet").display()
    ))
    .unwrap();
}

fn status_varchar_pins() -> FieldTypes {
    let mut pins = FieldTypes::new();
    pins.insert("status", trawl_core::schema::CanonicalType::Varchar);
    pins
}

#[test]
fn pinned_comparison_applies_on_cold_only_run_query() {
    // The cold-only branch (pool cold path): run_query itself must consult
    // the pins. `status=200` binds text and matches only the stored "200";
    // `status>=400` TRY_CASTs and excludes "accepted" without erroring
    // (pin-blind emission Conversion-errors here).
    let dir = tempfile::tempdir().unwrap();
    write_varchar_status_parquet(dir.path());
    let exec = Executor::new().unwrap();
    let source = format!("{}/*.parquet", dir.path().display());
    let pins = status_varchar_pins();

    let eq = exec
        .run_query("status=200", &source, &pins, usize::MAX, 0)
        .expect("text equality must not error on the VARCHAR column");
    assert_eq!(eq.row_count(), 1);

    let ordered = exec
        .run_query("status>=400", &source, &pins, usize::MAX, 0)
        .expect("ordered numeric comparison must not error over 'accepted'");
    assert_eq!(ordered.row_count(), 1, "only '404' is >= 400");
}

#[test]
fn pinned_comparison_survives_pruned_list_source() {
    // One list element points at an hour dir holding no file, so resolution
    // narrows the list before the read. The narrowed source must carry the
    // same comparison pins: a pin dropped there turns `status>=400` into a
    // Conversion error over 'accepted'.
    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    write_varchar_status_parquet(&full);
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"status\":\"500\"}\n",
    )
    .unwrap();

    let exec = Executor::new().unwrap();
    let source = format!(
        "['{}/*.parquet', '{}/*.parquet']",
        full.display(),
        empty.display()
    );
    let pins = status_varchar_pins();
    let result = exec
        .run_query_with_hot(
            "status>=400",
            &source,
            hot.to_str().unwrap(),
            &pins,
            &pins,
            usize::MAX,
            0,
        )
        .expect("the pruned retry must keep the comparison pins");
    // cold '404' + hot '500'; cold 'accepted'/'200' excluded, no error.
    assert_eq!(result.row_count(), 2);
}

#[test]
fn pinned_comparison_survives_hot_only_fallback() {
    // Genuine cold start: the glob matches nothing, so the executor falls
    // back to the hot-only read — which must keep the SAME comparison
    // interpretation. The hot snapshot holds both a numeric-looking and a
    // word value; pin-blind emission would Conversion-error the ordered
    // comparison over the VARCHAR-inferred column.
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        concat!(
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"status\":\"404\"}\n",
            "{\"_time\":\"2024-01-15T10:00:02Z\",\"_ingested\":\"2024-01-15T10:00:02Z\",\"service\":\"svc\",\"status\":\"accepted\"}\n",
        ),
    )
    .unwrap();

    let exec = Executor::new().unwrap();
    let source = format!("{}/nothing/*.parquet", dir.path().display());
    let pins = status_varchar_pins();
    let result = exec
        .run_query_with_hot(
            "status>=400",
            &source,
            hot.to_str().unwrap(),
            &pins,
            &pins,
            usize::MAX,
            0,
        )
        .expect("the hot-only fallback must keep the comparison pins");
    assert_eq!(result.row_count(), 1, "only '404' matches, no error");
}

#[test]
fn pinned_comparison_survives_export_retry() {
    // The export lane: a narrowed list source with comparison pins carried.
    use duckdb::Connection;
    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full");
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::create_dir_all(&empty).unwrap();
    write_varchar_status_parquet(&full);
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"status\":\"500\"}\n",
    )
    .unwrap();
    let out = dir.path().join("export.parquet");

    let exec = Executor::new().unwrap();
    let source = format!(
        "['{}/*.parquet', '{}/*.parquet']",
        full.display(),
        empty.display()
    );
    let pins = status_varchar_pins();
    exec.export_parquet_with_hot(
        "status>=400",
        &source,
        hot.to_str().unwrap(),
        &pins,
        &pins,
        &out,
        1000,
    )
    .expect("the export retry must keep the comparison pins");

    let conn = Connection::open_in_memory().unwrap();
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
    assert_eq!(rows, 2, "cold '404' + hot '500'");
}

/// A hot snapshot whose `status` values are JSON NUMBERS — the shape that
/// makes `read_json` infer BIGINT for a field the catalog pins VARCHAR.
fn write_numeric_status_hot(hot: &std::path::Path) {
    std::fs::write(
        hot,
        "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"status\":200}\n",
    )
    .unwrap();
}

#[test]
fn hot_only_fallback_conforms_hot_columns_to_the_pin() {
    // Cold start (the glob matches no file), so the executor reads hot-only.
    // That changes the SOURCE, never the TYPES: the hot column must arrive
    // conformed to the VARCHAR pin exactly as the union's hot branch
    // conforms it, or the answer would flip the moment the first parquet
    // landed — in the returned VALUE (a JSON-inferred BIGINT comes back as
    // an Integer) and in what a non-numeric literal does (text compares,
    // a BIGINT column throws a Conversion error).
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    write_numeric_status_hot(&hot);

    let exec = Executor::new().unwrap();
    let source = format!("{}/nothing/*.parquet", dir.path().display());
    let pins = status_varchar_pins();

    let exact = exec
        .run_query_with_hot(
            "status=200",
            &source,
            hot.to_str().unwrap(),
            &pins,
            &pins,
            usize::MAX,
            0,
        )
        .expect("text equality must not error on the hot-only lane");
    assert_eq!(exact.row_count(), 1, "the text '200' must match");
    let status = exact
        .columns
        .iter()
        .position(|c| c.name == "status")
        .expect("status column must be present");
    assert_eq!(
        exact.rows[0][status],
        Value::String("200".to_owned()),
        "the hot value must arrive as the pinned VARCHAR — an Integer here \
         means the hot-only lane read JSON-inferred types"
    );

    // A VARCHAR pin does not fix how a NUMBER is spelled on disk —
    // `read_json`'s inference does, and this same event stores as '200.0'
    // the moment a fractional sibling shares its batch. So the equality
    // rule carries the value's numeric reading and `status=200.0` matches
    // the stored '200', exactly as the SSE matcher answers for the same
    // wire value (ADR-0011, `trawl-core/tests/filter_parity.rs`).
    let decimal = exec
        .run_query_with_hot(
            "status=200.0",
            &source,
            hot.to_str().unwrap(),
            &pins,
            &pins,
            usize::MAX,
            0,
        )
        .expect("text equality must not error on the hot-only lane");
    assert_eq!(
        decimal.row_count(),
        1,
        "'200.0' is the same number as the stored '200'"
    );

    // And the column really is text: a non-numeric literal compares
    // cleanly, which a JSON-inferred BIGINT column could not do.
    let word = exec
        .run_query_with_hot(
            "status!=accepted",
            &source,
            hot.to_str().unwrap(),
            &pins,
            &pins,
            usize::MAX,
            0,
        )
        .expect("a non-numeric literal must not throw on the hot-only lane");
    assert_eq!(word.row_count(), 1, "'200' is not the text 'accepted'");
}

#[test]
fn export_hot_only_fallback_conforms_hot_columns_to_the_pin() {
    // The export lane of the same cold start: the exported parquet must
    // carry the catalog's type, not `read_json`'s inference — otherwise an
    // export taken before the first compaction disagrees with one taken
    // after.
    use duckdb::Connection;

    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    let out = dir.path().join("export.parquet");
    write_numeric_status_hot(&hot);

    let exec = Executor::new().unwrap();
    let source = format!("{}/nothing/*.parquet", dir.path().display());
    let pins = status_varchar_pins();
    exec.export_parquet_with_hot(
        "*",
        &source,
        hot.to_str().unwrap(),
        &pins,
        &pins,
        &out,
        1000,
    )
    .expect("the hot-only export must succeed on a cold start");

    let conn = Connection::open_in_memory().unwrap();
    let ty: String = conn
        .query_row(
            &format!(
                "SELECT column_type FROM (DESCRIBE SELECT * FROM read_parquet('{}')) \
                 WHERE column_name = 'status'",
                out.display()
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        ty, "VARCHAR",
        "the exported hot column must carry the catalog pin, not the \
         JSON-inferred type"
    );
}

// ── pinned rust_stages tail (ADR-0011) ────────────────────────────────

/// A source with a VARCHAR `status` column and kv-free messages, plus the
/// VARCHAR pin for it.
fn setup_pinned_kv() -> (Executor, String, FieldTypes, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pinned.parquet");
    {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT unnest(['200', '404', '500', 'accepted', '1.5']) AS status, \
             'plain text line' AS message) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }
    let mut ft = FieldTypes::new();
    ft.insert("status", trawl_core::schema::CanonicalType::Varchar);
    let exec = Executor::new().expect("executor should initialize");
    (exec, format!("{}", path.display()), ft, dir)
}

/// The `rust_stages` lane is pin-aware: `… | extract kv | where <pinned
/// cmp>` returns the same rows as the equivalent query without the kv
/// split — the tail's `where` runs under the scope stamped at the split,
/// not pin-blind.
#[test]
fn extract_kv_tail_where_is_pin_aware() {
    let (exec, src, ft, _dir) = setup_pinned_kv();
    let with_kv = exec
        .run_query("* | extract kv | where status > 400", &src, &ft, 1000, 0)
        .unwrap();
    let without_kv = exec
        .run_query("* | where status > 400", &src, &ft, 1000, 0)
        .unwrap();
    // '404' and '500' compare in the DECIMAL space; 'accepted' and '1.5'
    // vs 400 are UNKNOWN/false; '200' is below.
    assert_eq!(without_kv.row_count(), 2);
    assert_eq!(
        with_kv.row_count(),
        without_kv.row_count(),
        "the kv tail must answer exactly as the split-free query"
    );

    // The equality rung too: '200' matches == 200 as text.
    let eq_kv = exec
        .run_query("* | extract kv | where status == 200", &src, &ft, 1000, 0)
        .unwrap();
    assert_eq!(eq_kv.row_count(), 1);
}

/// A rename BEFORE the kv split remaps the pin in the stamped scope, so
/// the tail's `where` under the new name stays pin-aware.
#[test]
fn extract_kv_tail_after_rename_carries_the_remapped_pin() {
    let (exec, src, ft, _dir) = setup_pinned_kv();
    let result = exec
        .run_query(
            "* | rename status as st | extract kv | where st > 400",
            &src,
            &ft,
            1000,
            0,
        )
        .unwrap();
    assert_eq!(result.row_count(), 2, "the pin travels under the new name");
}

/// The pin-blind door stays pin-blind: the same tail without pins keeps
/// literal-driven evaluation (a string status has no numeric reading in
/// the streaming evaluator, so nothing matches).
#[test]
fn extract_kv_tail_without_pins_stays_literal_driven() {
    let (exec, src, _ft, _dir) = setup_pinned_kv();
    let result = exec
        .run_query(
            "* | extract kv | where status > 400",
            &src,
            &FieldTypes::new(),
            1000,
            0,
        )
        .unwrap();
    assert_eq!(result.row_count(), 0, "embedded mode keeps today's answer");
}

/// A source holding one row at `_time = 2026-01-01 05:30:00` UTC, plus
/// the TIMESTAMP pin the envelope seed gives `_time` on every install.
fn setup_pinned_time() -> (Executor, String, FieldTypes, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timed.parquet");
    {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT TIMESTAMP '2026-01-01 05:30:00' AS _time, \
             'plain text line' AS message) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }
    let mut ft = FieldTypes::new();
    ft.insert("_time", trawl_core::schema::CanonicalType::Timestamp);
    let exec = Executor::new().expect("executor should initialize");
    (exec, format!("{}", path.display()), ft, dir)
}

/// The tail's pin-aware TIMESTAMP comparison reads the STORED instant, not
/// the caller's display shift: `utc_offset_secs` is a rendering knob, so
/// the same query must answer the same rows in every display zone — and
/// exactly what the split-free query answers.
#[test]
fn extract_kv_tail_timestamp_compare_ignores_the_display_offset() {
    let (exec, src, ft, _dir) = setup_pinned_time();
    let dsl = "* | extract kv | where _time > \"2026-01-01T05:00:00Z\"";
    for offset in [0, -18_000, 19_800] {
        let result = exec.run_query(dsl, &src, &ft, 1000, offset).unwrap();
        assert_eq!(
            result.row_count(),
            1,
            "05:30Z is after 05:00Z whatever zone the client displays in (offset {offset})"
        );
    }
    // The threshold that genuinely excludes the row excludes it everywhere.
    let after = "* | extract kv | where _time > \"2026-01-01T06:00:00Z\"";
    for offset in [0, -18_000, 19_800] {
        assert_eq!(
            exec.run_query(after, &src, &ft, 1000, offset)
                .unwrap()
                .row_count(),
            0,
            "offset {offset}"
        );
    }
    // And it agrees with the same predicate without the kv split.
    assert_eq!(
        exec.run_query(
            "* | where _time > \"2026-01-01T05:00:00Z\"",
            &src,
            &ft,
            1000,
            -18_000,
        )
        .unwrap()
        .row_count(),
        1,
    );
}

/// Running the tail over UTC does not cost the caller its display zone:
/// the surviving TIMESTAMP columns are shifted back afterwards, so the
/// rendered cell is the same one the split-free query prints.
#[test]
fn extract_kv_tail_still_renders_timestamps_in_the_display_zone() {
    let (exec, src, ft, _dir) = setup_pinned_time();
    let with_kv = exec
        .run_query("* | extract kv | head 5", &src, &ft, 1000, -18_000)
        .unwrap();
    let without_kv = exec
        .run_query("* | head 5", &src, &ft, 1000, -18_000)
        .unwrap();
    let cell = |result: &QueryResult| {
        let idx = result
            .columns
            .iter()
            .position(|c| c.name == "_time")
            .expect("_time column");
        result.rows[0][idx].clone()
    };
    assert_eq!(cell(&with_kv), Value::String("2026-01-01 00:30:00".into()));
    assert_eq!(cell(&with_kv), cell(&without_kv));
}

/// The display shift follows the tail's own lineage: a timestamp column
/// renamed INSIDE the tail still renders in the caller's zone under its
/// new name — the value is untransformed, only the label moved.
#[test]
fn extract_kv_tail_rename_keeps_the_display_zone() {
    let (exec, src, ft, _dir) = setup_pinned_time();
    let result = exec
        .run_query(
            "* | extract kv | rename _time as t | table t",
            &src,
            &ft,
            1000,
            -18_000,
        )
        .unwrap();
    assert_eq!(result.columns.len(), 1);
    assert_eq!(result.columns[0].name, "t");
    assert_eq!(
        result.rows[0][0],
        Value::String("2026-01-01 00:30:00".into()),
        "the renamed column is still the stored instant and shifts with it"
    );
}

/// `let t2 = _time` copies the value verbatim, so both columns must show
/// the same rendering in the caller's zone, never one local and one UTC
/// side by side.
#[test]
fn extract_kv_tail_alias_copy_keeps_the_display_zone() {
    let (exec, src, ft, _dir) = setup_pinned_time();
    let result = exec
        .run_query(
            "* | extract kv | let t2 = _time | table _time, t2",
            &src,
            &ft,
            1000,
            -18_000,
        )
        .unwrap();
    let cell = |name: &str| {
        let idx = result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} column"));
        result.rows[0][idx].clone()
    };
    assert_eq!(cell("_time"), Value::String("2026-01-01 00:30:00".into()));
    assert_eq!(
        cell("t2"),
        cell("_time"),
        "an alias copy is the same instant and must display in the same zone"
    );
}

/// A COMPUTED value is the tail's own, not the stored rendering: the
/// lineage kills it and it stays exactly as the tail produced it (UTC
/// text here) — the honest reading for a transformed value, never a
/// re-shifted guess.
#[test]
fn extract_kv_tail_computed_value_stays_as_rendered() {
    let (exec, src, ft, _dir) = setup_pinned_time();
    let result = exec
        .run_query(
            "* | extract kv | let t3 = coalesce(_time, _time) | table _time, t3",
            &src,
            &ft,
            1000,
            -18_000,
        )
        .unwrap();
    let cell = |name: &str| {
        let idx = result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} column"));
        result.rows[0][idx].clone()
    };
    assert_eq!(cell("_time"), Value::String("2026-01-01 00:30:00".into()));
    assert_eq!(
        cell("t3"),
        Value::String("2026-01-01 05:30:00".into()),
        "a derived value keeps the tail's own rendering"
    );
}

// ── parameter ordering across an aggregate SELECT list ────────────────
//
// `DuckDB` binds `?` positionally, so the parameter list has to run in the
// order the placeholders appear in the rendered SQL. A built SELECT renders
// `SELECT … FROM … WHERE …`, but the emitter walks the search stage first,
// so a parameter the search predicate pushed sits first in the list while
// its placeholder sits last in the text. Any parameter an aggregating stage
// pushes into the SELECT list therefore collides with it: the predicate's
// value is fed to the SELECT's placeholder. Flushing to a CTE is what keeps
// them apart, because a CTE renders before the outer SELECT.
//
// The failure is worst when the two values happen to be type-compatible, a
// timestamp-shaped window bound against `now()`'s TIMESTAMP anchor, because
// then nothing errors and the query answers the wrong values in both
// places.
//
// These execute against real `DuckDB` rather than asserting on SQL text:
// the defect is the binding, and only the engine can show it.

/// The anchor these emit under, so an assertion can name the exact
/// instant `now()` has to answer.
fn fixed_anchor() -> trawl_core::context::EvalContext {
    trawl_core::context::EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-02-03T04:05:06.789012Z")
            .expect("a valid RFC 3339 instant")
            .into(),
    )
}

/// How `DuckDB` renders [`fixed_anchor`] once bound and cast.
const ANCHOR_TEXT: &str = "2026-02-03 04:05:06.789012";

/// Emit under the fixed anchor and execute through the production
/// emit-and-bind seam (including the `_raw`-free retry).
fn run_at_anchor(exec: &Executor, dsl: &str, source: &str) -> Result<QueryResult, EngineError> {
    let query = trawl_core::parser::parse(dsl).expect("dsl parses");
    let emitted =
        trawl_core::emitter::emit(&query, source, fixed_anchor()).expect("emit should succeed");
    exec.execute_emitted(&emitted, usize::MAX, 0)
}

fn integer_at(result: &QueryResult, row: usize, name: &str) -> i64 {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("missing {name} column: {:?}", result.columns));
    match &result.rows[row][index] {
        Value::Integer(value) => *value,
        other => panic!("{name} must be an integer, got {other:?}"),
    }
}

fn text_cell(result: &QueryResult, row: usize, name: &str) -> String {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("missing {name} column: {:?}", result.columns));
    match &result.rows[row][index] {
        Value::String(text) => text.clone(),
        other => panic!("{name} must be text, got {other:?}"),
    }
}

/// (a) The loud half: a plain filter's value cannot be cast to a
/// TIMESTAMP, so a swap takes the whole query down.
#[test]
fn an_aggregate_now_binds_after_the_search_predicate() {
    let (exec, glob) = setup();
    let result = run_at_anchor(&exec, "service=nginx | stats max(now()) as n", &glob)
        .expect("the search predicate's value must not reach the anchor's cast");
    assert_eq!(result.row_count(), 1);
    assert_eq!(text_cell(&result, 0, "n"), ANCHOR_TEXT);
}

/// (b) The silent half: a timestamp-shaped window bound and the anchor
/// are type-compatible, so a swapped query succeeds and answers the
/// window bound as `max(now())` while filtering on the anchor.
#[test]
fn a_time_window_bound_cannot_swap_with_an_aggregate_now() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        r#"earliest="2024-01-15T10:01:00Z" | stats max(now()) as n, count() as c"#,
        &glob,
    )
    .expect("the windowed aggregate must run");
    assert_eq!(
        text_cell(&result, 0, "n"),
        ANCHOR_TEXT,
        "max(now()) must answer the anchor, not the window bound"
    );
    let count = integer_column(&result, "c");
    assert_eq!(
        count,
        vec![7],
        "and the window must filter on ITS bound: the 7 rows at or after 10:01:00"
    );
}

/// (c) `timechart` builds its aggregate SELECT the same way `stats` does.
#[test]
fn a_timechart_aggregate_now_binds_after_the_search_predicate() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | timechart span=1m max(now()) as n",
        &glob,
    )
    .expect("the search predicate's value must not reach the anchor's cast");
    assert!(result.row_count() >= 1);
    for row in 0..result.row_count() {
        assert_eq!(text_cell(&result, row, "n"), ANCHOR_TEXT);
    }
}

/// (d) The `_raw`-free twin pushes the same parameters in the same order,
/// so the realignment has to reach both passes: asserted over a source
/// that has `_raw` (the first pass runs) and one that does not (the retry
/// runs).
#[test]
fn a_bare_text_search_keeps_aggregate_params_aligned_in_both_raw_variants() {
    let (exec, glob) = setup();
    let with_raw = run_at_anchor(&exec, "gateway | stats max(now()) as n", &glob)
        .expect("the `_raw`-bound pass must run");
    assert_eq!(text_cell(&with_raw, 0, "n"), ANCHOR_TEXT);

    let dir = tempfile::tempdir().unwrap();
    let raw_free_source = foreign_parquet(dir.path());
    let raw_free = run_at_anchor(&exec, "boom | stats max(now()) as n", &raw_free_source)
        .expect("the `_raw`-free retry must run");
    assert_eq!(text_cell(&raw_free, 0, "n"), ANCHOR_TEXT);
}

/// The trap is not `now()`'s: any parameter an aggregate argument pushes
/// hits it, which is why the guard keys on "the emission pushed a
/// parameter" rather than on the anchor. Here the parameter is an integer
/// literal inside an aggregate.
#[test]
fn an_aggregate_literal_argument_binds_after_the_search_predicate() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | stats max(substr(message, 1, 3)) as m",
        &glob,
    )
    .expect("the search predicate's value must not reach substr's offsets");
    assert_eq!(result.row_count(), 1);
    assert_eq!(text_cell(&result, 0, "m"), "POS");
}

// ── the same guard after a PIVOT ──────────────────────────────────────
//
// `DuckDB` cannot parameterize PIVOT, so `flush_pivot_to_cte` inlines every
// accumulated placeholder and empties the parameter list. The ordering
// guard reads "has this level pushed anything yet" as a comparison against
// how many parameters are already inside a CTE, so the inlining must reset
// that count: left stale, the guard reads "nothing pushed here" as soon as
// the post-pivot count climbs back to it and skips the flush it exists to
// perform.

/// The pivot's own output columns, so a `where` downstream has something
/// real to bind: `host` plus one column per status value.
const PIVOT_PREFIX: &str = "service=nginx | pivot count() on status by host";

/// (a) The loud half after a pivot: the predicate's text reaches the
/// anchor's TIMESTAMP cast.
#[test]
fn an_aggregate_now_after_a_pivot_binds_after_the_predicate() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        &format!(r#"{PIVOT_PREFIX} | where host == "web01" | stats max(now()) as n"#),
        &glob,
    )
    .expect("the predicate's value must not reach the anchor's cast");
    assert_eq!(result.row_count(), 1);
    assert_eq!(text_cell(&result, 0, "n"), ANCHOR_TEXT);
}

/// (b) The silent half after a pivot: three string parameters, all
/// type-compatible, so nothing errors and every one of them lands in the
/// wrong placeholder.
#[test]
fn an_aggregate_literal_after_a_pivot_binds_after_the_predicate() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        &format!(
            r#"{PIVOT_PREFIX} | where host == "web01" | stats max(replace(host, "w", "W")) as m"#
        ),
        &glob,
    )
    .expect("the pivoted aggregate must run");
    assert_eq!(result.row_count(), 1);
    assert_eq!(
        text_cell(&result, 0, "m"),
        "Web01",
        "the predicate must filter on 'web01' and replace must rewrite 'w' as 'W'"
    );
}

/// (c) `timechart` after a pivot takes the same guard. The pivot groups
/// BY `_time` so the bucket column survives into the downstream stage.
#[test]
fn a_timechart_aggregate_after_a_pivot_binds_after_the_predicate() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        r#"service=nginx | pivot count() on status by _time \
           | where _time >= "2024-01-15 10:00:00" | timechart span=1m max(now()) as n"#
            .replace(" \\\n           ", " ")
            .as_str(),
        &glob,
    )
    .expect("the pivoted timechart must run");
    assert!(
        result.row_count() >= 1,
        "the window must keep rows: {result:?}"
    );
    for row in 0..result.row_count() {
        assert_eq!(text_cell(&result, row, "n"), ANCHOR_TEXT);
    }
}

// ── an aggregating stage's placement ──────────────────────────────────
//
// SQL applies a LIMIT and an ORDER BY after the aggregation on the same
// SELECT, so a pending one has to be flushed to a CTE first or
// `head 2 | stats count()` counts the whole input and then limits a
// one-row result.
//
// Placement must also not key off parameter accounting, or the answer
// depends on a sibling: `stats count() as c` would give the un-limited
// count while `stats count() as c, max(now()) as n` gives the limited one,
// because only the second pushes a parameter. An aggregating stage
// therefore flushes whenever a LIMIT or ORDER BY is pending, and both
// spellings agree.

/// `head` before `stats` limits the INPUT to the aggregation.
#[test]
fn a_limit_before_stats_applies_before_the_aggregation() {
    let (exec, glob) = setup();
    let result = run_at_anchor(&exec, "service=nginx | head 2 | stats count() as c", &glob)
        .expect("the limited aggregate must run");
    assert_eq!(
        integer_column(&result, "c"),
        vec![2],
        "the aggregation must see the 2 limited rows, not all 6 nginx rows"
    );
}

/// …and the answer does not depend on whether a SIBLING aggregate
/// happens to bind a parameter.
#[test]
fn a_limit_before_stats_answers_the_same_beside_a_bound_sibling() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | head 2 | stats count() as c, max(now()) as n",
        &glob,
    )
    .expect("the limited aggregate must run");
    assert_eq!(integer_column(&result, "c"), vec![2]);
    assert_eq!(text_cell(&result, 0, "n"), ANCHOR_TEXT);
}

/// A pending ORDER BY is the same trap: it has to order the AGGREGATION'S
/// INPUT, so `sort -duration | head 2 | stats sum(duration)` sums the two
/// LARGEST durations rather than every row.
#[test]
fn a_pending_sort_orders_the_aggregations_input() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | sort -duration | head 2 | stats sum(duration) as s",
        &glob,
    )
    .expect("the sorted-and-limited aggregate must run");
    let sum = match &result.rows[0][result
        .columns
        .iter()
        .position(|column| column.name == "s")
        .expect("missing s column")]
    {
        Value::Float(value) => *value,
        other => panic!("s must be a float, got {other:?}"),
    };
    assert!(
        (sum - 6.234).abs() < 1e-9,
        "the two largest nginx durations are 5.000 and 1.234, got {sum}"
    );
}

/// `timechart` places the same way.
#[test]
fn a_limit_before_timechart_applies_before_the_aggregation() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | head 2 | timechart span=1h count() as c",
        &glob,
    )
    .expect("the limited timechart must run");
    assert_eq!(
        integer_column(&result, "c"),
        vec![2],
        "the bucket must count the 2 limited rows"
    );
}

// ── `top`/`rare` place the same way ───────────────────────────────────
//
// `top`/`rare` desugar to `stats count() by field | sort count | limit N`
// through `process_frequency`, so they take the same placement rule. A
// pending LIMIT absorbed into the aggregation's own SELECT would make
// `head 2 | top 3 host` count all six matching rows and then keep two
// groups, which is a different question from the one the pipeline asks.

/// Every `count` a frequency stage produced, summed: the rows the
/// aggregation actually saw. Deliberately order-independent, because
/// `head` with no sort takes whichever rows the scan yields, so the
/// assertion is the number of survivors and not their identity.
fn counted_rows(result: &QueryResult) -> i64 {
    integer_column(result, "count").iter().sum()
}

/// (a) `head` before `top` limits the INPUT to the frequency count.
#[test]
fn a_limit_before_top_applies_before_the_aggregation() {
    let (exec, glob) = setup();
    let result = run_at_anchor(&exec, "service=nginx | head 2 | top 3 host", &glob)
        .expect("the limited frequency stage must run");
    assert_eq!(
        counted_rows(&result),
        2,
        "top must count the 2 limited rows, not all 6 nginx rows: {result:?}"
    );
}

/// (b) `rare` shares the desugaring, so it shares the placement.
#[test]
fn a_limit_before_rare_applies_before_the_aggregation() {
    let (exec, glob) = setup();
    let result = run_at_anchor(&exec, "service=nginx | head 2 | rare 3 host", &glob)
        .expect("the limited frequency stage must run");
    assert_eq!(
        counted_rows(&result),
        2,
        "rare must count the 2 limited rows, not all 6 nginx rows: {result:?}"
    );
}

/// (c) A pending `sort` orders the frequency stage's INPUT rather than
/// being appended to its grouped output. Sorting `host` descending puts
/// both `web02` rows first, so `head 2` leaves exactly them.
#[test]
fn a_pending_sort_orders_the_frequency_stages_input() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | sort -host | head 2 | top 3 host",
        &glob,
    )
    .expect("the sorted-and-limited frequency stage must run");
    assert_eq!(
        result.row_count(),
        1,
        "only the rows the sort+limit kept reach the count: {result:?}"
    );
    assert_eq!(text_cell(&result, 0, "host"), "web02");
    assert_eq!(integer_column(&result, "count"), vec![2]);
}

// ── a pivot follows a pivot ───────────────────────────────────────────
//
// A pending pivot is flushed to a CTE before any following stage,
// including another pivot. `process_pivot` opens with an ordinary
// `flush_to_cte`, whose `build_select` does not render a pending `PIVOT`,
// and then overwrites the spec, so without the pivot-aware flush the first
// pivot is silently dropped and the second runs over pre-pivot input.

/// The second pivot reads the FIRST one's dynamic output columns.
///
/// Over the six nginx fixture rows the first pivot counts statuses per
/// host — `web01` has two 200s, `web02` none — so its `200` column holds
/// `{2, 0}`, and pivoting on THAT column is only expressible if the first
/// pivot actually ran.
#[test]
fn a_second_pivot_reads_the_first_pivots_output() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | pivot count() on status by host | pivot count() on `200` by host",
        &glob,
    )
    .expect("the second pivot must read the first's output columns");

    let columns: Vec<&str> = result
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(
        columns,
        vec!["host", "0", "2"],
        "the pivoted-on values are the first pivot's own 200-counts"
    );
    assert_eq!(result.row_count(), 2);
    for row in 0..result.row_count() {
        let (zero, two) = (integer_at(&result, row, "0"), integer_at(&result, row, "2"));
        match text_cell(&result, row, "host").as_str() {
            "web01" => assert_eq!((zero, two), (0, 1), "web01 served two 200s"),
            "web02" => assert_eq!((zero, two), (1, 0), "web02 served none"),
            other => panic!("unexpected host {other}"),
        }
    }
}

/// A column the first pivot consumed into dynamic columns is gone, and
/// naming it is a loud error rather than a quietly different answer: a
/// dropped first pivot would let this "succeed", returning `sum(status)`
/// per status per host with the first pivot's `count()` nowhere in it.
#[test]
fn a_second_pivot_cannot_see_a_column_the_first_consumed() {
    let (exec, glob) = setup();
    let error = run_at_anchor(
        &exec,
        "service=nginx | pivot count() on status by host | pivot sum(status) on status by host",
        &glob,
    )
    .expect_err("`status` no longer exists after the first pivot consumed it");
    assert!(
        error.to_string().contains("unknown field: status"),
        "the refusal must name the missing column, got: {error}"
    );
}

/// A terminal pivot is never flushed by the loop: no stage follows it, so
/// `finalize` renders it, inlining every parameter.
#[test]
fn a_terminal_pivot_is_unchanged() {
    let (exec, glob) = setup();
    let result = run_at_anchor(
        &exec,
        "service=nginx | pivot count() on status by host",
        &glob,
    )
    .expect("the terminal pivot must run");
    let columns: Vec<&str> = result
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(columns, vec!["host", "200", "301", "404", "500", "502"]);
}
