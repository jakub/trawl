// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A captured result's parquet file (ADR-0057): columns named by position,
//! rows in the result's order, every column exact or kept as its wire
//! JSON, and pages and cells read back by row number. Each test writes a
//! real file with `Executor::write_capture_parquet`, asks `DuckDB` what the
//! file holds, and reads it back with `read_capture_page` and
//! `read_capture_cell`.

use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use trawl_engine::error::EngineError;
use trawl_engine::executor::{
    CAPTURE_ROW_GROUP_ROWS, CaptureFile, Executor, QueryBudgets, StoredAs,
};
use trawl_engine::scratch::{ScratchLock, is_spill_dir_name};
use trawl_engine::value::{Column, ColumnType, QueryResult, Value};

fn text(s: &str) -> Value {
    Value::String(s.to_owned())
}

/// A 0600 file for the capture, made the way the results root makes one.
fn dest_in(dir: &Path) -> PathBuf {
    let path = dir.join(".stage-a1B2c3.parquet");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("the staged file");
    path
}

/// Write `result` as a capture into a fresh directory.
fn capture(result: &QueryResult) -> (tempfile::TempDir, PathBuf, CaptureFile) {
    let dir = tempfile::tempdir().unwrap();
    let path = dest_in(dir.path());
    let file = Executor::new()
        .unwrap()
        .write_capture_parquet(result, &path)
        .expect("the capture writes");
    assert_eq!(file.rows, result.rows.len() as u64);
    assert_eq!(file.bytes, std::fs::metadata(&path).unwrap().len());
    (dir, path, file)
}

/// Every row of the capture at `path`, every column in position order.
fn read_all(path: &Path, result: &QueryResult, stored_as: &[StoredAs]) -> Vec<Vec<Value>> {
    let types = types(result);
    let positions: Vec<usize> = (0..types.len()).collect();
    Executor::new()
        .unwrap()
        .read_capture_page(path, &types, stored_as, &positions, 0, u64::MAX)
        .expect("the capture reads back")
}

fn types(result: &QueryResult) -> Vec<ColumnType> {
    result.columns.iter().map(|c| c.ty.clone()).collect()
}

/// The name and `DuckDB` type of every column in the file, in order.
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

fn described_as(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, ty)| ((*name).to_owned(), (*ty).to_owned()))
        .collect()
}

/// The texts a `VARCHAR` column of the file holds, in file order.
fn stored_texts(path: &Path, column: &str) -> Vec<Option<String>> {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {column} FROM read_parquet('{}', file_row_number = true) \
             ORDER BY file_row_number",
            path.display()
        ))
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Whether two cells are the same value: floats by their bits, since NaN
/// never equals itself and `-0.0` equals `0.0`.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        _ => a == b,
    }
}

fn assert_rows(got: &[Vec<Value>], want: &[Vec<Value>]) {
    assert_eq!(got.len(), want.len(), "row count");
    for (row, (got, want)) in got.iter().zip(want).enumerate() {
        assert_eq!(got.len(), want.len(), "row {row}: cell count");
        for (column, (got, want)) in got.iter().zip(want).enumerate() {
            assert!(
                same(got, want),
                "row {row} column {column}: {got:?} != {want:?}"
            );
        }
    }
}

/// Display names never reach the file: its columns are `c0`, `c1`, ..., so
/// duplicates, quotes and a name that spells another column's file name
/// all read back under their positions.
#[test]
fn duplicate_and_odd_display_names_reach_no_file_column() {
    let written = QueryResult {
        columns: vec![
            Column::new("x", ColumnType::Integer),
            Column::new("x", ColumnType::String),
            Column::new(r#"we"ird, "name"#, ColumnType::Float),
            Column::new("c0", ColumnType::Boolean),
        ],
        rows: vec![
            vec![
                Value::Integer(1),
                text("one"),
                Value::Float(1.5),
                Value::Boolean(true),
            ],
            vec![Value::Integer(-2), Value::Null, Value::Null, Value::Null],
        ],
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(file.stored_as, vec![StoredAs::Native; 4]);
    assert_eq!(
        described(&path),
        described_as(&[
            ("c0", "BIGINT"),
            ("c1", "VARCHAR"),
            ("c2", "DOUBLE"),
            ("c3", "BOOLEAN"),
        ])
    );
    assert_rows(&read_all(&path, &written, &file.stored_as), &written.rows);
}

/// A `mixed` column has no parquet type, so each cell is kept as its wire
/// JSON text in a `VARCHAR`, null cells as `NULL`, and lands back through
/// the wire's decoder exactly: tags, `u64` digits, a non-finite float and
/// a list of mixed items included.
#[test]
fn a_mixed_column_is_kept_as_wire_text_and_lands_back() {
    let written = QueryResult {
        columns: vec![
            Column::new("status", ColumnType::Mixed),
            Column::new("n", ColumnType::Integer),
        ],
        rows: vec![
            vec![Value::Integer(200), Value::Integer(1)],
            vec![text("abc"), Value::Integer(2)],
            vec![Value::Null, Value::Integer(3)],
            vec![Value::UInt(u64::MAX), Value::Integer(4)],
            vec![Value::Float(f64::NAN), Value::Integer(5)],
            vec![Value::Boolean(false), Value::Integer(6)],
            vec![
                Value::Array(vec![Value::Integer(1), text("x")]),
                Value::Integer(7),
            ],
        ],
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(file.stored_as, vec![StoredAs::WireText, StoredAs::Native]);
    assert_eq!(
        described(&path),
        described_as(&[("c0", "VARCHAR"), ("c1", "BIGINT")])
    );
    let texts = stored_texts(&path, "c0");
    assert_eq!(
        texts[..4],
        [
            Some(r#"{"type":"integer","value":"200"}"#.to_owned()),
            Some(r#"{"type":"string","value":"abc"}"#.to_owned()),
            None,
            Some(r#"{"type":"integer","value":"18446744073709551615"}"#.to_owned()),
        ]
    );
    assert_eq!(
        texts[4].as_deref(),
        Some(r#"{"type":"float","value":"NaN"}"#)
    );
    assert_rows(&read_all(&path, &written, &file.stored_as), &written.rows);
}

/// Integers that need both a negative and the range above `i64::MAX` have
/// no exact parquet type (`HUGEINT` is stored as `DOUBLE`), so the column
/// is kept as wire text, its digits exact; a list of them likewise.
#[test]
fn integers_of_both_signs_past_i64_are_kept_exactly_as_wire_text() {
    let written = QueryResult {
        columns: vec![
            Column::new("total", ColumnType::Integer),
            Column::new("totals", ColumnType::list(ColumnType::Integer)),
        ],
        rows: vec![
            vec![Value::Integer(-1), Value::Array(vec![Value::Integer(-1)])],
            vec![
                Value::UInt(u64::MAX),
                Value::Array(vec![Value::UInt(u64::MAX), Value::Null]),
            ],
            vec![Value::Integer(i64::MIN), Value::Array(vec![])],
            vec![Value::Null, Value::Null],
        ],
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(file.stored_as, vec![StoredAs::WireText; 2]);
    assert_eq!(
        described(&path),
        described_as(&[("c0", "VARCHAR"), ("c1", "VARCHAR")])
    );
    assert_eq!(
        stored_texts(&path, "c0"),
        vec![
            Some(r#""-1""#.to_owned()),
            Some(r#""18446744073709551615""#.to_owned()),
            Some(r#""-9223372036854775808""#.to_owned()),
            None,
        ]
    );
    assert_rows(&read_all(&path, &written, &file.stored_as), &written.rows);
}

/// A value above `i64::MAX` with no negative beside it fits `UBIGINT`, so
/// the column stays native and unsigned in the file.
#[test]
fn an_integer_above_i64_alone_stays_native() {
    let written = QueryResult {
        columns: vec![Column::new("bytes", ColumnType::Integer)],
        rows: vec![
            vec![Value::UInt(u64::MAX)],
            vec![Value::Integer(0)],
            vec![Value::Null],
        ],
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(file.stored_as, vec![StoredAs::Native]);
    assert_eq!(described(&path), described_as(&[("c0", "UBIGINT")]));
    assert_rows(&read_all(&path, &written, &file.stored_as), &written.rows);
}

/// The wire spells a non-finite float as `"NaN"`, `"Infinity"` or
/// `"-Infinity"`; the file holds them as `DOUBLE` values, and every float
/// reads back to the bit, the extremes and a negative zero included.
#[test]
fn non_finite_floats_are_stored_as_doubles_and_read_back_to_the_bit() {
    let floats = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.1,
        0.1 + 0.2,
        -0.0,
        f64::MAX,
        5e-324,
    ];
    let mut rows: Vec<Vec<Value>> = floats.iter().map(|f| vec![Value::Float(*f)]).collect();
    rows.push(vec![Value::Null]);
    let written = QueryResult {
        columns: vec![Column::new("f", ColumnType::Float)],
        rows,
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(file.stored_as, vec![StoredAs::Native]);
    assert_eq!(described(&path), described_as(&[("c0", "DOUBLE")]));
    let (nan, inf, neg_inf): (bool, bool, bool) = duckdb::Connection::open_in_memory()
        .unwrap()
        .query_row(
            &format!(
                "SELECT bool_or(isnan(c0)), bool_or(c0 = 'Infinity'::DOUBLE), \
                 bool_or(c0 = '-Infinity'::DOUBLE) FROM read_parquet('{}')",
                path.display()
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(
        nan && inf && neg_inf,
        "the file holds the non-finite doubles"
    );
    assert_rows(&read_all(&path, &written, &file.stored_as), &written.rows);
}

/// Lists nest as their element types, at any depth: native when the items
/// have a stored type, wire text when they are mixed.
#[test]
fn nested_lists_round_trip() {
    let nested = ColumnType::list(ColumnType::list(ColumnType::Integer));
    let written = QueryResult {
        columns: vec![
            Column::new("ll", nested),
            Column::new("ls", ColumnType::list(ColumnType::String)),
            Column::new("lt", ColumnType::list(ColumnType::Timestamp)),
            Column::new("lm", ColumnType::list(ColumnType::Mixed)),
        ],
        rows: vec![
            vec![
                Value::Array(vec![
                    Value::Array(vec![Value::Integer(1), Value::Integer(-2)]),
                    Value::Array(vec![]),
                ]),
                Value::Array(vec![text("a"), Value::Null, text("")]),
                Value::Array(vec![text("2026-10-23T10:00:00.000001Z")]),
                Value::Array(vec![Value::Integer(1), text("x"), Value::Null]),
            ],
            vec![
                Value::Array(vec![Value::Array(vec![Value::Integer(i64::MIN)])]),
                Value::Array(vec![]),
                Value::Array(vec![]),
                Value::Array(vec![]),
            ],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
        ],
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(
        file.stored_as,
        vec![
            StoredAs::Native,
            StoredAs::Native,
            StoredAs::Native,
            StoredAs::WireText
        ]
    );
    assert_eq!(
        described(&path),
        described_as(&[
            ("c0", "BIGINT[][]"),
            ("c1", "VARCHAR[]"),
            ("c2", "TIMESTAMP[]"),
            ("c3", "VARCHAR"),
        ])
    );
    assert_rows(&read_all(&path, &written, &file.stored_as), &written.rows);
}

/// A timestamp is stored as a UTC `TIMESTAMP` to the microsecond and
/// reads back as the same instant in UTC, whatever offset it was written
/// in; `infinity` stays `infinity`.
#[test]
fn timestamps_keep_their_microseconds() {
    let written = QueryResult {
        columns: vec![Column::new("_time", ColumnType::Timestamp)],
        rows: vec![
            vec![text("2026-10-23T10:00:00.123456Z")],
            vec![text("2026-10-23T15:30:00.000001+05:30")],
            vec![text("1969-12-31T23:59:59.999999Z")],
            vec![text("infinity")],
            vec![Value::Null],
        ],
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(file.stored_as, vec![StoredAs::Native]);
    assert_eq!(described(&path), described_as(&[("c0", "TIMESTAMP")]));
    let micros: Vec<Option<i64>> = duckdb::Connection::open_in_memory()
        .unwrap()
        .prepare(&format!(
            "SELECT CASE WHEN isfinite(c0) THEN epoch_us(c0) END \
             FROM read_parquet('{}', file_row_number = true) ORDER BY file_row_number",
            path.display()
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        micros,
        vec![
            Some(1_792_749_600_123_456),
            Some(1_792_749_600_000_001),
            Some(-1),
            None,
            None,
        ]
    );
    assert_rows(
        &read_all(&path, &written, &file.stored_as),
        &[
            vec![text("2026-10-23T10:00:00.123456Z")],
            vec![text("2026-10-23T10:00:00.000001Z")],
            vec![text("1969-12-31T23:59:59.999999Z")],
            vec![text("infinity")],
            vec![Value::Null],
        ],
    );
}

/// A result with columns and no rows writes a file that carries its
/// schema, a mixed column as `VARCHAR`, and reads back as no rows.
#[test]
fn zero_rows_write_a_typed_empty_file() {
    let written = QueryResult {
        columns: vec![
            Column::new("n", ColumnType::Integer),
            Column::new("_time", ColumnType::Timestamp),
            Column::new("tags", ColumnType::list(ColumnType::String)),
            Column::new("status", ColumnType::Mixed),
        ],
        rows: vec![],
    };
    let (_dir, path, file) = capture(&written);
    assert_eq!(file.rows, 0);
    assert!(file.bytes > 0, "an empty result still writes its schema");
    assert_eq!(
        file.stored_as,
        vec![
            StoredAs::Native,
            StoredAs::Native,
            StoredAs::Native,
            StoredAs::WireText
        ]
    );
    assert_eq!(
        described(&path),
        described_as(&[
            ("c0", "BIGINT"),
            ("c1", "TIMESTAMP"),
            ("c2", "VARCHAR[]"),
            ("c3", "VARCHAR"),
        ])
    );
    assert!(read_all(&path, &written, &file.stored_as).is_empty());
    let cell = Executor::new()
        .unwrap()
        .read_capture_cell(&path, &ColumnType::Integer, StoredAs::Native, 0, 0)
        .expect("the cell read runs");
    assert_eq!(cell, None);
}

/// A parquet file holds at least one column, so a result without columns
/// is refused before anything is written: the caller keeps it as metadata
/// alone. A cell its column does not admit, or a ragged row, fails the
/// wire encoding; none of the refusals leaves a staging file behind.
#[test]
fn the_writer_refuses_what_has_no_file() {
    let scratch_parent = tempfile::tempdir().unwrap();
    let scratch_path = scratch_parent.path().join("data.query-scratch");
    let executor = query_database(&scratch_path);
    let dir = tempfile::tempdir().unwrap();
    let path = dest_in(dir.path());

    let err = executor
        .write_capture_parquet(&QueryResult::empty(), &path)
        .expect_err("a zero-column result has no file");
    assert!(
        matches!(err, EngineError::NotParquetStorable { .. }),
        "{err:?}"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "dest untouched");

    let disagreeing = QueryResult {
        columns: vec![Column::new("n", ColumnType::Integer)],
        rows: vec![vec![text("7")]],
    };
    let err = executor
        .write_capture_parquet(&disagreeing, &path)
        .expect_err("a string cell under an integer column does not encode");
    assert!(err.to_string().contains("\"n\""), "{err}");

    let ragged = QueryResult {
        columns: vec![
            Column::new("a", ColumnType::Integer),
            Column::new("b", ColumnType::Integer),
        ],
        rows: vec![vec![Value::Integer(1)]],
    };
    let err = executor
        .write_capture_parquet(&ragged, &path)
        .expect_err("a ragged row does not encode");
    assert!(err.to_string().contains("row 0 has 1 cells"), "{err}");

    assert_eq!(
        scratch_entries(&scratch_path).len(),
        1,
        "only the spill dir"
    );
}

/// 10,000 rows span several row groups and read back in order, a page at
/// a time, at every row-group boundary, and past the end.
#[test]
fn rows_page_in_order_across_row_groups() {
    const ROWS: usize = 10_000;
    let written = QueryResult {
        columns: vec![
            Column::new("i", ColumnType::Integer),
            Column::new("s", ColumnType::String),
            Column::new("f", ColumnType::Float),
        ],
        rows: (0..ROWS)
            .map(|i| {
                let n = i64::try_from(i).unwrap();
                vec![
                    Value::Integer(n),
                    text(&format!("row {i}")),
                    #[allow(clippy::cast_precision_loss)]
                    Value::Float(n as f64 / 4.0),
                ]
            })
            .collect(),
    };
    let (_dir, path, file) = capture(&written);
    let groups: i64 = duckdb::Connection::open_in_memory()
        .unwrap()
        .query_row(
            &format!(
                "SELECT count(DISTINCT row_group_id) FROM parquet_metadata('{}')",
                path.display()
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        usize::try_from(groups).unwrap(),
        ROWS.div_ceil(CAPTURE_ROW_GROUP_ROWS)
    );
    assert!(groups >= 3, "{groups} row groups");

    let executor = Executor::new().unwrap();
    let types = types(&written);
    let page = |offset: u64, limit: u64| {
        executor
            .read_capture_page(&path, &types, &file.stored_as, &[0, 1, 2], offset, limit)
            .expect("a page reads")
    };
    let mut paged = Vec::new();
    for offset in (0..ROWS as u64).step_by(200) {
        paged.extend(page(offset, 200));
    }
    assert_rows(&paged, &written.rows);

    let group = CAPTURE_ROW_GROUP_ROWS as u64;
    for offset in [
        0,
        1,
        group - 1,
        group,
        group + 1,
        2 * group - 1,
        2 * group,
        ROWS as u64 - 2,
        ROWS as u64 - 1,
        ROWS as u64,
        ROWS as u64 + 1,
        i64::MAX as u64,
        u64::MAX,
    ] {
        let start = usize::try_from(offset).map_or(ROWS, |o| o.min(ROWS));
        let end = (start + 3).min(ROWS);
        assert_rows(&page(offset, 3), &written.rows[start..end]);
    }
    assert!(page(0, 0).is_empty(), "a zero limit reads no row");
    assert_rows(&page(9_990, u64::MAX), &written.rows[9_990..]);
}

/// A page returns the columns at the given positions in the given order,
/// a repeated position repeated, and with no position its rows still
/// count. A position past the last column, or metadata that does not
/// describe every column, is refused.
#[test]
fn a_page_projects_by_position() {
    let written = QueryResult {
        columns: vec![
            Column::new("a", ColumnType::Integer),
            Column::new("b", ColumnType::Mixed),
            Column::new("c", ColumnType::String),
        ],
        rows: vec![
            vec![Value::Integer(1), Value::Integer(10), text("x")],
            vec![Value::Integer(2), text("ten"), text("y")],
        ],
    };
    let (_dir, path, file) = capture(&written);
    let executor = Executor::new().unwrap();
    let types = types(&written);
    let read = |positions: &[usize]| {
        executor.read_capture_page(&path, &types, &file.stored_as, positions, 0, 10)
    };

    assert_rows(
        &read(&[2, 0, 1, 1]).expect("projection"),
        &[
            vec![
                text("x"),
                Value::Integer(1),
                Value::Integer(10),
                Value::Integer(10),
            ],
            vec![text("y"), Value::Integer(2), text("ten"), text("ten")],
        ],
    );
    assert_eq!(read(&[]).expect("no column"), vec![Vec::<Value>::new(); 2]);

    let err = read(&[3]).expect_err("past the last column");
    assert!(matches!(&err, EngineError::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput));
    let err = executor
        .read_capture_page(&path, &types, &file.stored_as[..2], &[0], 0, 10)
        .expect_err("storage for two of three columns");
    assert!(matches!(&err, EngineError::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput));
}

/// One cell by row and position, native or wire text; `None` past the
/// last row.
#[test]
fn a_cell_reads_by_row_and_position() {
    let written = QueryResult {
        columns: vec![
            Column::new("n", ColumnType::Integer),
            Column::new("m", ColumnType::Mixed),
        ],
        rows: (0..5)
            .map(|i| vec![Value::Integer(i), text(&format!("m{i}"))])
            .collect(),
    };
    let (_dir, path, file) = capture(&written);
    let executor = Executor::new().unwrap();
    let cell = |ty: &ColumnType, stored_as: StoredAs, row: u64, position: usize| {
        executor
            .read_capture_cell(&path, ty, stored_as, row, position)
            .expect("the cell read runs")
    };
    assert_eq!(file.stored_as, vec![StoredAs::Native, StoredAs::WireText]);
    assert_eq!(
        cell(&ColumnType::Integer, StoredAs::Native, 0, 0),
        Some(Value::Integer(0))
    );
    assert_eq!(
        cell(&ColumnType::Integer, StoredAs::Native, 4, 0),
        Some(Value::Integer(4))
    );
    assert_eq!(
        cell(&ColumnType::Mixed, StoredAs::WireText, 3, 1),
        Some(text("m3"))
    );
    for row in [5, 6, i64::MAX as u64, u64::MAX] {
        assert_eq!(cell(&ColumnType::Integer, StoredAs::Native, row, 0), None);
    }
}

/// The file the caller made 0600 stays that file: `COPY` writes into it in
/// place, so its inode and mode survive under any umask, and a second
/// write truncates it. `bytes` is its synced length.
#[test]
fn a_pre_created_0600_file_keeps_its_inode_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dest_in(dir.path());
    let inode = std::fs::metadata(&path).unwrap().ino();
    let executor = Executor::new().unwrap();
    let long = QueryResult {
        columns: vec![Column::new("s", ColumnType::String)],
        rows: (0..3_000)
            .map(|i| vec![text(&format!("{i:0>64}"))])
            .collect(),
    };
    let first = executor
        .write_capture_parquet(&long, &path)
        .expect("the first write");
    let short = QueryResult {
        columns: vec![Column::new("s", ColumnType::String)],
        rows: vec![vec![text("only")]],
    };
    let second = executor
        .write_capture_parquet(&short, &path)
        .expect("the second write");

    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.ino(), inode, "COPY replaced the file");
    assert_eq!(meta.permissions().mode() & 0o7777, 0o600);
    assert_eq!(second.bytes, meta.len());
    assert!(second.bytes < first.bytes, "the second write truncated");
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no `tmp_` sibling or staging file beside it"
    );
    assert_rows(&read_all(&path, &short, &second.stored_as), &short.rows);
}

/// The query database stages a capture's rows in the scratch directory,
/// under a name the boot clearing recognizes, and removes them: a scratch
/// directory spelled with a glob character shows where the stage goes,
/// since `read_json` would expand it, and the write refuses it.
#[test]
fn the_query_database_stages_rows_in_query_scratch() {
    let parent = tempfile::tempdir().unwrap();
    let result = QueryResult {
        columns: vec![Column::new("n", ColumnType::Integer)],
        rows: vec![vec![Value::Integer(1)]],
    };
    let out = tempfile::tempdir().unwrap();

    let scratch = parent.path().join("data.query-scratch");
    let executor = query_database(&scratch);
    let path = dest_in(out.path());
    executor
        .write_capture_parquet(&result, &path)
        .expect("the capture writes");
    let entries = scratch_entries(&scratch);
    assert_eq!(entries.len(), 1, "the stage was removed: {entries:?}");
    assert!(is_spill_dir_name(entries[0].file_name().unwrap()));

    let globbed = parent.path().join("data[1].query-scratch");
    let executor = query_database(&globbed);
    let err = executor
        .write_capture_parquet(&result, &path)
        .expect_err("a staging path read_json would expand");
    assert!(
        err.to_string()
            .contains("capture staging path carries the glob character '['"),
        "{err}"
    );
    assert_eq!(scratch_entries(&globbed).len(), 1, "the stage was removed");
}

/// A capture is read from exactly its path: `read_parquet` expands a glob
/// even in a bound path, so a path with a glob character is refused.
#[test]
fn a_read_path_with_a_glob_character_is_refused() {
    let written = QueryResult {
        columns: vec![Column::new("n", ColumnType::Integer)],
        rows: vec![vec![Value::Integer(1)]],
    };
    let (dir, path, file) = capture(&written);
    let globbed = dir.path().join("a*.parquet");
    std::fs::copy(&path, &globbed).unwrap();
    let err = Executor::new()
        .unwrap()
        .read_capture_page(&globbed, &types(&written), &file.stored_as, &[0], 0, 1)
        .expect_err("a glob in the path");
    assert!(err.to_string().contains("glob character '*'"), "{err}");
}

/// A query database whose scratch directory is `path`, as trawld opens it.
fn query_database(path: &Path) -> Executor {
    let scratch = Arc::new(ScratchLock::open(path).expect("the scratch directory locks"));
    let budgets = QueryBudgets {
        spill_max_bytes: NonZeroU64::new(64 << 20).unwrap(),
        memory_limit: NonZeroU64::new(256 << 20).unwrap(),
        threads: NonZeroU32::new(2).unwrap(),
    };
    Executor::open_query_database(scratch, &budgets).expect("the query database opens")
}

fn scratch_entries(path: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}
