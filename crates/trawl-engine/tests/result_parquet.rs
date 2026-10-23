// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The run parquet writer stores every column in an exact type and reads
//! it back as the same typed result (ADR-0035, amended 2026-10-22). Each
//! test writes a real file, then asks `DuckDB` what the file holds and
//! reads it back through `read_parquet_to_result`.

use std::path::Path;

use trawl_engine::error::EngineError;
use trawl_engine::executor::Executor;
use trawl_engine::value::{Column, ColumnType, QueryResult, Value};

fn text(s: &str) -> Value {
    Value::String(s.to_owned())
}

/// The `DuckDB` type of every column in the file, in order.
fn described(path: &Path) -> Vec<(String, String)> {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_parquet('{}'))",
            path.display()
        ))
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// A leaf column's parquet physical and converted type, from the footer.
fn footer(path: &Path, name: &str) -> (String, Option<String>) {
    duckdb::Connection::open_in_memory()
        .unwrap()
        .query_row(
            &format!(
                "SELECT type, converted_type FROM parquet_schema('{}') WHERE name = ?",
                path.display()
            ),
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn write(result: &QueryResult) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.parquet");
    let written = Executor::new()
        .unwrap()
        .write_query_result_to_parquet(result, &path)
        .expect("the result writes");
    assert_eq!(written, result.rows.len());
    (dir, path)
}

fn read(path: &Path) -> QueryResult {
    Executor::new()
        .unwrap()
        .read_parquet_to_result(path, 1000)
        .expect("the file reads back")
}

/// AC 7, and every other type the writer stores: the file holds exact
/// types, never inferred ones, and reads back cell for cell.
#[test]
#[allow(clippy::too_many_lines)] // one row per stored type, kept in one table
fn every_column_type_is_stored_exactly_and_reads_back() {
    let timestamps = ColumnType::list(ColumnType::Timestamp);
    let nested = ColumnType::list(ColumnType::list(ColumnType::Integer));
    let written = QueryResult {
        columns: vec![
            Column::new("big", ColumnType::Integer),
            Column::new("n", ColumnType::Integer),
            Column::new("neg", ColumnType::Integer),
            Column::new("f", ColumnType::Float),
            Column::new("g", ColumnType::Float),
            Column::new("t", ColumnType::Timestamp),
            Column::new("d", ColumnType::Date),
            Column::new("tm", ColumnType::Time),
            Column::new("b", ColumnType::Boolean),
            Column::new("s", ColumnType::String),
            Column::new("l", ColumnType::list(ColumnType::Integer)),
            Column::new("lt", timestamps),
            Column::new("ll", nested),
        ],
        rows: vec![
            vec![
                Value::UInt(u64::MAX),
                Value::Integer(i64::MIN),
                Value::Integer(-1),
                Value::Float(f64::NAN),
                Value::Float(f64::INFINITY),
                text("2026-10-22T12:34:56.123456+01:00"),
                text("2026-10-22"),
                text("12:34:56.123456"),
                Value::Boolean(true),
                text("18446744073709551615"),
                Value::Array(vec![Value::Integer(1), Value::Integer(-2)]),
                Value::Array(vec![text("2026-10-22T00:00:00.000001-07:00")]),
                Value::Array(vec![Value::Array(vec![Value::UInt(u64::MAX)])]),
            ],
            vec![
                Value::Integer(1),
                Value::Integer(9_007_199_254_740_993),
                Value::Integer(i64::MAX),
                Value::Float(0.1),
                Value::Float(f64::NEG_INFINITY),
                text("infinity"),
                text("1969-12-31"),
                text("00:00:00"),
                Value::Boolean(false),
                text("007"),
                Value::Array(vec![]),
                Value::Array(vec![]),
                Value::Array(vec![Value::Array(vec![])]),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Float(f64::MAX),
                Value::Float(5e-324),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ],
    };
    let (_dir, path) = write(&written);

    assert_eq!(
        described(&path),
        [
            ("big", "UBIGINT"),
            ("n", "BIGINT"),
            ("neg", "BIGINT"),
            ("f", "DOUBLE"),
            ("g", "DOUBLE"),
            ("t", "TIMESTAMP"),
            ("d", "DATE"),
            ("tm", "TIME"),
            ("b", "BOOLEAN"),
            ("s", "VARCHAR"),
            ("l", "BIGINT[]"),
            ("lt", "TIMESTAMP[]"),
            ("ll", "UBIGINT[][]"),
        ]
        .map(|(name, ty)| (name.to_owned(), ty.to_owned()))
    );
    assert_eq!(
        footer(&path, "big"),
        ("INT64".to_owned(), Some("UINT_64".to_owned())),
        "a value above i64::MAX is stored unsigned, not as a double"
    );

    let back = read(&path);
    assert_eq!(
        back.columns, written.columns,
        "every column reads back typed"
    );
    // A timestamp is stored in UTC and reads back as the same instant in
    // UTC; every other cell reads back as written. NaN never equals
    // itself, so the float cells are compared by their bits.
    let mut expected = written.rows.clone();
    expected[0][5] = text("2026-10-22T11:34:56.123456Z");
    expected[0][11] = Value::Array(vec![text("2026-10-22T07:00:00.000001Z")]);
    expected[1][7] = text("00:00:00");
    for (row, (got, want)) in back.rows.iter().zip(&expected).enumerate() {
        for (column, (got, want)) in got.iter().zip(want).enumerate() {
            match (got, want) {
                (Value::Float(a), Value::Float(b)) => assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "row {row} column {column}: {a} != {b}"
                ),
                _ => assert_eq!(got, want, "row {row} column {column}"),
            }
        }
    }
    assert_eq!(back.rows.len(), expected.len());
}

/// The writer declares every column's text and never infers one, so a
/// column of digit strings is still a string column in the file. Before
/// this, `read_json(..., auto_detect=true)` read it as `BIGINT`.
#[test]
fn a_column_of_digit_strings_stays_a_string() {
    let written = QueryResult {
        columns: vec![Column::new("code", ColumnType::String)],
        rows: vec![vec![text("123")], vec![text("456")]],
    };
    let (_dir, path) = write(&written);
    assert_eq!(
        described(&path),
        vec![("code".to_owned(), "VARCHAR".to_owned())]
    );
    assert_eq!(read(&path), written);
}

/// AC 1: a zero-row result writes a parquet file that carries its schema,
/// and reads back as typed columns with no rows. An `integer` column with
/// no cell to ask is `BIGINT`.
#[test]
fn a_zero_row_result_writes_a_typed_empty_file() {
    let written = QueryResult {
        columns: vec![
            Column::new("n", ColumnType::Integer),
            Column::new("_time", ColumnType::Timestamp),
            Column::new("tags", ColumnType::list(ColumnType::String)),
        ],
        rows: vec![],
    };
    let (_dir, path) = write(&written);
    assert!(path.exists(), "an empty result still writes its file");
    assert_eq!(
        described(&path),
        [
            ("n", "BIGINT"),
            ("_time", "TIMESTAMP"),
            ("tags", "VARCHAR[]")
        ]
        .map(|(name, ty)| (name.to_owned(), ty.to_owned()))
    );
    assert_eq!(read(&path), written);
}

/// A timestamp list a Rust tail passed through is a list of timestamps in
/// its result, so a run of it stores `TIMESTAMP[]` and reads back as the
/// same instants, in UTC.
#[test]
fn a_timestamp_list_through_a_rust_tail_is_stored_as_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("events.parquet");
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY (SELECT 'k=v' AS message, TIMESTAMP '2026-01-15 09:00:00.123456' AS _time) \
             TO '{}' (FORMAT PARQUET)",
            source.display()
        ))
        .unwrap();
    let written = Executor::new()
        .unwrap()
        .run_query(
            "* | stats list(_time) as seen, max(message) as message | extract kv | table seen",
            &source.display().to_string(),
            &trawl_core::schema::FieldTypes::new(),
            100,
            19_800,
        )
        .expect("the kv tail runs");
    assert_eq!(
        written.rows,
        vec![vec![Value::Array(vec![text(
            "2026-01-15T14:30:00.123456+05:30"
        )])]]
    );

    let (_dir, path) = write(&written);
    assert_eq!(
        described(&path),
        vec![("seen".to_owned(), "TIMESTAMP[]".to_owned())]
    );
    assert_eq!(
        read(&path),
        QueryResult {
            columns: vec![Column::new("seen", ColumnType::list(ColumnType::Timestamp))],
            rows: vec![vec![Value::Array(vec![text(
                "2026-01-15T09:00:00.123456Z"
            )])]],
        }
    );
}

/// Column names are the result's own, quotes and duplicates of a key
/// spelling included: the ndjson keys are positional.
#[test]
fn column_names_are_kept_verbatim() {
    let written = QueryResult {
        columns: vec![
            Column::new(r#"we"ird"#, ColumnType::Integer),
            Column::new("c0", ColumnType::String),
        ],
        rows: vec![vec![Value::Integer(7), text("x")]],
    };
    let (_dir, path) = write(&written);
    assert_eq!(read(&path), written);
}

/// What the writer refuses, and that a refusal leaves no file behind:
/// parquet holds no zero-column file and no union type, and a cell its
/// column's type does not admit fails the wire encoding.
#[test]
fn the_writer_refuses_what_parquet_cannot_hold_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.parquet");
    let executor = Executor::new().unwrap();

    let err = executor
        .write_query_result_to_parquet(&QueryResult::empty(), &path)
        .expect_err("a zero-column result has no parquet form");
    assert!(
        matches!(err, EngineError::NotParquetStorable { .. }),
        "{err:?}"
    );

    let mixed = QueryResult {
        columns: vec![Column::new("status", ColumnType::Mixed)],
        rows: vec![vec![Value::Integer(200)], vec![text("abc")]],
    };
    let err = executor
        .write_query_result_to_parquet(&mixed, &path)
        .expect_err("a mixed column has no parquet form");
    assert!(
        matches!(&err, EngineError::NotParquetStorable { reason } if reason.contains("status")),
        "the refusal names the column: {err:?}"
    );

    // Both signs and the unsigned range: only HUGEINT holds them, and
    // DuckDB's parquet writer stores HUGEINT as DOUBLE (see the probe
    // below), so the column is refused rather than rounded.
    let wide = QueryResult {
        columns: vec![Column::new("total", ColumnType::Integer)],
        rows: vec![vec![Value::Integer(-1)], vec![Value::UInt(u64::MAX)]],
    };
    let err = executor
        .write_query_result_to_parquet(&wide, &path)
        .expect_err("no exact parquet type holds -1 and u64::MAX");
    assert!(
        matches!(&err, EngineError::NotParquetStorable { reason } if reason.contains("total")),
        "the refusal names the column: {err:?}"
    );

    let disagreeing = QueryResult {
        columns: vec![Column::new("n", ColumnType::Integer)],
        rows: vec![vec![text("7")]],
    };
    let err = executor
        .write_query_result_to_parquet(&disagreeing, &path)
        .expect_err("a string cell under an integer column does not encode");
    assert!(err.to_string().contains("\"n\""), "{err}");

    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no file and no temp file is left behind"
    );
}

/// The premise behind refusing a column that needs `HUGEINT`: `DuckDB`'s
/// parquet writer stores a `HUGEINT` column as `DOUBLE`, so a value past
/// 2^53 would come back rounded. If a `DuckDB` upgrade makes this fail,
/// the writer can store such a column instead of refusing it.
#[test]
fn duckdb_stores_a_hugeint_column_as_double() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.parquet");
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY (SELECT CAST('18446744073709551615' AS HUGEINT) AS h) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    assert_eq!(
        described(&path),
        vec![("h".to_owned(), "DOUBLE".to_owned())]
    );
}
