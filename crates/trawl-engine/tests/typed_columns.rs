// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Result columns are typed from execution (ADR-0035, amended
//! 2026-10-22): `DuckDB`'s logical types for the SQL read, the emitted
//! cells for the Rust tail. Each test runs a real query and reads the
//! types off the result, then crosses the wire codec where the claim is
//! about the wire.

use std::io::Write as _;

use trawl_core::schema::FieldTypes;
use trawl_engine::error::EngineError;
use trawl_engine::executor::Executor;
use trawl_engine::value::{ColumnType, QueryResult, Value};

/// A one-file parquet source `DuckDB` writes from `select`, so the column
/// types are exactly the ones the `SELECT` names.
fn parquet(select: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.parquet");
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY ({select}) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    let source = path.display().to_string();
    (dir, source)
}

/// An ndjson source, one row per line.
fn ndjson(rows: &[&str]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.ndjson");
    let mut file = std::fs::File::create(&path).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    file.flush().unwrap();
    let source = path.display().to_string();
    (dir, source)
}

fn run(dsl: &str, source: &str) -> Result<QueryResult, EngineError> {
    run_at(dsl, source, 0)
}

/// [`run`] with timestamps rendered at a display offset of
/// `utc_offset_secs`.
fn run_at(dsl: &str, source: &str, utc_offset_secs: i32) -> Result<QueryResult, EngineError> {
    Executor::new()
        .unwrap()
        .run_query(dsl, source, &FieldTypes::new(), usize::MAX, utc_offset_secs)
}

fn types(result: &QueryResult) -> Vec<(String, ColumnType)> {
    result
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.ty.clone()))
        .collect()
}

fn type_of(result: &QueryResult, name: &str) -> ColumnType {
    result
        .columns
        .iter()
        .find(|column| column.name == name)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", result.columns))
        .ty
        .clone()
}

/// AC 3: a `sum` past `u64::MAX` is a `HUGEINT` no wire integer holds,
/// so the query is refused by the result column's name, never answered
/// with digits as text or a rounded float.
#[test]
fn a_hugeint_past_u64_max_is_refused_by_column_name() {
    let (_dir, source) = parquet(
        "SELECT * FROM (VALUES (18446744073709551615::UBIGINT), \
         (18446744073709551615::UBIGINT)) t(bytes)",
    );

    let err = run("* | stats sum(bytes) as total", &source)
        .expect_err("2 * u64::MAX has no integer cell");
    assert!(
        matches!(&err, EngineError::IntegerOutOfRange { column } if column == "total"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "column \"total\" holds an integer outside the 64-bit range; \
         cast it explicitly with tonumber() or tostring()"
    );

    // The same HUGEINT column inside the range narrows exactly.
    let fits = run("* | head 1 | stats sum(bytes) as total", &source).expect("one u64::MAX fits");
    assert_eq!(type_of(&fits, "total"), ColumnType::Integer);
    assert_eq!(fits.rows, vec![vec![Value::UInt(u64::MAX)]]);
}

/// AC 1: a query whose filter matches nothing still returns its columns,
/// typed from `DuckDB`'s logical types before any row is read.
#[test]
fn a_zero_row_result_is_typed_from_execution() {
    let (_dir, source) = parquet(
        "SELECT TIMESTAMP '2026-01-15 09:00:00' AS _time, 'nginx' AS service, \
         18446744073709551615::UBIGINT AS bytes, 1.5::DOUBLE AS rate, \
         true AS ok, ['a', 'b'] AS tags, DATE '2026-01-15' AS day",
    );

    let empty = run("service=nonexistent", &source).expect("an empty answer is an answer");
    assert!(empty.rows.is_empty());
    let mut typed = types(&empty);
    typed.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        typed,
        vec![
            ("_time".to_owned(), ColumnType::Timestamp),
            ("bytes".to_owned(), ColumnType::Integer),
            ("day".to_owned(), ColumnType::Date),
            ("ok".to_owned(), ColumnType::Boolean),
            ("rate".to_owned(), ColumnType::Float),
            ("service".to_owned(), ColumnType::String),
            ("tags".to_owned(), ColumnType::list(ColumnType::String)),
        ]
    );

    // The types survive the wire with no row to infer them from.
    let wire = serde_json::to_value(&empty).unwrap();
    let back: QueryResult = serde_json::from_value(wire).unwrap();
    assert_eq!(back, empty);
}

/// A column the tail computes as all nulls carries no input column's
/// type and has no evidence of its own: it is `string`, and its null cells
/// encode under it.
#[test]
fn a_tail_column_with_no_evidence_is_string() {
    let (_dir, source) = parquet("SELECT 'k=v' AS message");
    let result = run("* | extract kv | let nothing = null", &source).expect("a null let runs");
    assert_eq!(type_of(&result, "nothing"), ColumnType::String);
    let index = result
        .columns
        .iter()
        .position(|column| column.name == "nothing")
        .unwrap();
    assert_eq!(result.rows.len(), 1, "{result:?}");
    assert_eq!(
        serde_json::to_value(&result).unwrap()["rows"][0][index],
        serde_json::Value::Null
    );
}

/// AC 8: an `extract kv` tail whose cells in one column are not one type
/// makes a `mixed` column, and every tagged cell decodes back to exactly
/// the variant it left as.
#[test]
fn a_kv_tail_with_mixed_cells_is_a_mixed_column_that_round_trips() {
    let (_dir, source) = ndjson(&[
        r#"{"message":"status=200 user=alice"}"#,
        r#"{"message":"status=abc user=bob"}"#,
        r#"{"message":"status=1.5 user=carol"}"#,
        r#"{"message":"user=dave"}"#,
    ]);

    let result = run("* | extract kv | table status, user", &source).expect("the kv tail runs");
    assert_eq!(
        types(&result),
        vec![
            ("status".to_owned(), ColumnType::Mixed),
            ("user".to_owned(), ColumnType::String),
        ]
    );
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        vec![
            Value::Integer(200),
            Value::String("abc".into()),
            Value::Float(1.5),
            Value::Null,
        ]
    );

    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(
        wire["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        vec![
            serde_json::json!({"type": "integer", "value": "200"}),
            serde_json::json!({"type": "string", "value": "abc"}),
            serde_json::json!({"type": "float", "value": 1.5}),
            serde_json::Value::Null,
        ]
    );
    let back: QueryResult = serde_json::from_str(&wire.to_string()).unwrap();
    assert_eq!(back, result);
}

/// A timestamp a stage after `extract kv` computes is text in the shared
/// value model, so a `mixed` column tags it `string`: the limit api.md
/// names. A column holding only such timestamps is `timestamp`, because
/// the column fold reads the pipeline cell, which is where the two part.
#[test]
fn a_computed_timestamp_in_a_mixed_column_is_tagged_string() {
    let (_dir, source) = ndjson(&[r#"{"message":"n=1"}"#, r#"{"message":"n=2"}"#]);

    let result = run(
        r#"* | extract kv | let x = if(n == 1, now(), "unknown") | table x"#,
        &source,
    )
    .expect("the kv tail runs");
    assert_eq!(type_of(&result, "x"), ColumnType::Mixed);

    let wire = serde_json::to_value(&result).unwrap();
    let cells: Vec<serde_json::Value> = wire["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[0].clone())
        .collect();
    assert_eq!(cells[0]["type"], "string");
    let instant = cells[0]["value"].as_str().unwrap();
    chrono::DateTime::parse_from_rfc3339(instant)
        .unwrap_or_else(|e| panic!("the computed instant is RFC 3339: {instant}: {e}"));
    assert!(
        instant.ends_with('Z') && instant.len() == "2026-10-22T12:34:56.123456Z".len(),
        "the computed instant is written in UTC with six fraction digits: {instant}"
    );
    assert_eq!(
        cells[1],
        serde_json::json!({"type": "string", "value": "unknown"})
    );

    let alone = run("* | extract kv | let x = now() | table x", &source).expect("the kv tail runs");
    assert_eq!(type_of(&alone, "x"), ColumnType::Timestamp);
}

/// The tail types its columns from the cells it emits, and a stored
/// TIMESTAMP the tail passes through (renamed or not) stays `timestamp`,
/// though the tail itself saw it as text.
#[test]
fn a_kv_tail_keeps_the_types_its_cells_and_lineage_carry() {
    let (_dir, source) = parquet(
        "SELECT TIMESTAMP '2026-01-15 09:00:00' AS _time, 'n=3 rate=1.5 ok=true' AS message, \
         NULL::DOUBLE AS latency",
    );

    let result = run(
        "* | extract kv | rename _time as at | table at, n, rate, ok, latency",
        &source,
    )
    .expect("the kv tail runs");
    let mut typed = types(&result);
    typed.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        typed,
        vec![
            ("at".to_owned(), ColumnType::Timestamp),
            // No evidence in the tail's output: the type the input
            // column it passed through carries.
            ("latency".to_owned(), ColumnType::Float),
            ("n".to_owned(), ColumnType::Integer),
            ("ok".to_owned(), ColumnType::Boolean),
            ("rate".to_owned(), ColumnType::Float),
        ]
    );
}

/// A Rust tail that filters every row away still answers with the
/// columns its stages project: a filter removes rows, not the schema. The
/// `latency` the input typed `float` keeps that type with no row left to
/// show it.
#[test]
fn a_rust_tail_that_removes_every_row_keeps_the_projected_schema() {
    let (_dir, source) = parquet("SELECT 'n=1' AS message, 1.5::DOUBLE AS latency");
    let result =
        run("* | extract kv | where n == 999 | table latency", &source).expect("the kv tail runs");
    assert!(result.rows.is_empty(), "{result:?}");
    assert_eq!(
        types(&result),
        vec![("latency".to_owned(), ColumnType::Float)],
        "a filter removes rows, not the projected schema"
    );
}

/// A stored `TIMESTAMP[]` the Rust tail passes through is still a list of
/// timestamps: typed `list` of `timestamp`, each item in the display
/// offset, exactly as the same query answers without the tail.
#[test]
fn a_timestamp_list_keeps_its_type_and_display_offset_through_a_rust_tail() {
    let (_dir, source) =
        parquet("SELECT 'k=v' AS message, [TIMESTAMP '2026-01-15 09:00:00.123456'] AS seen");
    let baseline = run_at("* | table seen", &source, 19800).expect("the SQL lane runs");
    let tail = run_at("* | extract kv | table seen", &source, 19800).expect("the kv tail runs");
    assert_eq!(
        baseline.columns[0].ty,
        ColumnType::list(ColumnType::Timestamp),
        "{baseline:?}"
    );
    assert_eq!(
        tail, baseline,
        "an unrelated kv stage must retain the timestamp list"
    );
}

/// The same through an aggregate the SQL prefix computes: `list(_time)`
/// is a `TIMESTAMP[]`, and a kv tail after it keeps it one.
#[test]
fn a_listed_aggregate_keeps_its_timestamp_items_through_kv() {
    let (_dir, source) =
        parquet("SELECT 'k=v' AS message, TIMESTAMP '2026-01-15 09:00:00.123456' AS _time");
    let base = "* | stats list(_time) as seen, max(message) as message";
    let baseline =
        run_at(&format!("{base} | table seen"), &source, 19800).expect("the SQL lane runs");
    let tail = run_at(&format!("{base} | extract kv | table seen"), &source, 19800)
        .expect("the kv tail runs");
    assert_eq!(tail, baseline);
}

/// With no row left the lineage still types the list: the tail's own
/// schema says `list` of `string`, and the lineage says those strings are
/// stored timestamps.
#[test]
fn a_timestamp_list_with_no_row_left_is_still_typed() {
    let (_dir, source) =
        parquet("SELECT 'k=v' AS message, [TIMESTAMP '2026-01-15 09:00:00.123456'] AS seen");
    let result = run_at(
        "* | extract kv | where k == \"nope\" | table seen",
        &source,
        19800,
    )
    .expect("the kv tail runs");
    assert!(result.rows.is_empty(), "{result:?}");
    assert_eq!(
        types(&result),
        vec![("seen".to_owned(), ColumnType::list(ColumnType::Timestamp))]
    );
}
