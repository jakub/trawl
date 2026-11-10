// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The kv batch tail compares a column the SQL step returns as a TIMESTAMP
//! as an instant, unless a pin for its name says otherwise (ADR-0011,
//! amended 2026-11-09), against real `DuckDB` and real parquet.
//!
//! The SQL step hands the tail `DuckDB`'s cast text, such as
//! `2026-11-01 06:30:00`. Read as a string, that text sorts below an RFC
//! 3339 bound like `2026-11-01T06:00:00Z`, because a space sorts below
//! `T`. Every parity test here compares the tail query against the same
//! query without `extract kv`, and against a hard-coded answer, by the
//! sorted `seq` of the rows kept, never by scan order.

use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_engine::executor::Executor;
use trawl_engine::value::{ColumnType, QueryResult, Value};

const BOUND: &str = "\"2026-11-01T06:00:00Z\"";

/// `select` written to one parquet file by `DuckDB`, in the session zone
/// the corpus is written in.
fn parquet(select: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.parquet");
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    conn.execute_batch(&format!(
        "COPY ({select}) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .unwrap();
    (dir, path.display().to_string())
}

/// Six rows around the bound, written out of `seq` order: one before it,
/// one on it, one a microsecond past it, one well past it, one at
/// `infinity` and one NULL.
fn timestamp_source() -> (tempfile::TempDir, String) {
    parquet(
        "SELECT * FROM (VALUES \
         (4, TIMESTAMP '2026-11-01 06:30:00', 'k=d'), \
         (1, TIMESTAMP '2026-11-01 05:59:59', 'k=a'), \
         (6, NULL::TIMESTAMP, 'k=f'), \
         (3, TIMESTAMP '2026-11-01 06:00:00.000001', 'k=c'), \
         (5, TIMESTAMP 'infinity', 'k=e'), \
         (2, TIMESTAMP '2026-11-01 06:00:00', 'k=b')) \
         AS t(seq, _time, message)",
    )
}

fn run_with(dsl: &str, glob: &str, pins: &FieldTypes) -> QueryResult {
    Executor::new()
        .unwrap()
        .run_query(dsl, glob, pins, usize::MAX)
        .unwrap_or_else(|error| panic!("{dsl:?} must run: {error}"))
}

fn run(dsl: &str, glob: &str) -> QueryResult {
    run_with(dsl, glob, &FieldTypes::new())
}

/// The `seq` of every row kept, sorted.
fn kept(result: &QueryResult) -> Vec<i64> {
    let at = result
        .columns
        .iter()
        .position(|column| column.name == "seq")
        .unwrap_or_else(|| panic!("missing seq: {:?}", result.columns));
    let mut seqs: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match &row[at] {
            Value::Integer(seq) => *seq,
            other => panic!("seq must be an integer: {other:?}"),
        })
        .collect();
    seqs.sort_unstable();
    seqs
}

/// The tail query keeps `expected`, and so does the same query without
/// `extract kv`.
fn assert_parity(tail: &str, sql: &str, glob: &str, expected: &[i64]) {
    let sql_kept = kept(&run(sql, glob));
    assert_eq!(sql_kept, expected, "the all-SQL lane: {sql}");
    assert_eq!(kept(&run(tail, glob)), expected, "the kv tail: {tail}");
}

#[test]
fn a_tail_compares_a_sql_timestamp_as_an_instant() {
    let (_dir, glob) = timestamp_source();
    assert_parity(
        &format!("* | extract kv | where _time > {BOUND} | table seq"),
        &format!("* | where _time > {BOUND} | table seq"),
        &glob,
        &[3, 4, 5],
    );
    assert_parity(
        &format!("* | extract kv | rename _time as t | where t > {BOUND} | table seq"),
        &format!("* | rename _time as t | where t > {BOUND} | table seq"),
        &glob,
        &[3, 4, 5],
    );
    assert_parity(
        &format!("* | extract kv | let t = _time | where t > {BOUND} | table seq"),
        &format!("* | let t = _time | where t > {BOUND} | table seq"),
        &glob,
        &[3, 4, 5],
    );
    // 06:00:00.000001 and 06:30 truncate to 06:00, which is not past the
    // bound; infinity stays infinity.
    assert_parity(
        &format!(
            "* | let t = date_trunc(\"hour\", _time) | extract kv | where t > {BOUND} | table seq"
        ),
        &format!("* | let t = date_trunc(\"hour\", _time) | where t > {BOUND} | table seq"),
        &glob,
        &[5],
    );
}

#[test]
fn null_infinity_and_fractions_match_the_sql_lane() {
    let (_dir, glob) = timestamp_source();
    let sql = run(&format!("* | where _time > {BOUND} | table seq"), &glob);
    let tail = run(
        &format!("* | extract kv | where _time > {BOUND} | table seq"),
        &glob,
    );
    for (lane, result) in [("all-SQL", &sql), ("kv tail", &tail)] {
        let seqs = kept(result);
        assert!(!seqs.contains(&6), "{lane}: a NULL _time is not kept");
        assert!(seqs.contains(&5), "{lane}: infinity is past the bound");
        assert!(
            seqs.contains(&3),
            "{lane}: a microsecond past the bound is past it"
        );
    }
    assert_eq!(kept(&tail), kept(&sql));
}

#[test]
fn a_zero_row_tail_keeps_its_columns() {
    let (_dir, glob) = timestamp_source();
    let result = run(
        &format!(
            "earliest=\"2020-01-01T00:00:00Z\" latest=\"2020-01-02T00:00:00Z\" \
             | extract kv | where _time > {BOUND} | table _time, message, seq"
        ),
        &glob,
    );
    assert_eq!(result.row_count(), 0);
    let columns: Vec<(&str, &ColumnType)> = result
        .columns
        .iter()
        .map(|column| (column.name.as_str(), &column.ty))
        .collect();
    assert_eq!(
        columns,
        [
            ("_time", &ColumnType::Timestamp),
            ("message", &ColumnType::String),
            ("seq", &ColumnType::Integer),
        ]
    );
}

#[test]
fn a_varchar_time_stays_literal() {
    // As text both values sort below the bound: a space sorts below `T`,
    // and `05` below `06`. Read as instants, seq 2 would be past it.
    let (_dir, glob) = parquet(
        "SELECT * FROM (VALUES \
         (2, '2026-11-01 06:30:00', 'k=b'), \
         (1, '2026-11-01T05:00:00Z', 'k=a')) \
         AS t(seq, _time, message)",
    );
    let premise = run("* | table _time", &glob);
    assert_eq!(
        premise.columns[0].ty,
        ColumnType::String,
        "premise: the fixture's _time is VARCHAR"
    );
    assert_parity(
        &format!("* | extract kv | where _time > {BOUND} | table seq"),
        &format!("* | where _time > {BOUND} | table seq"),
        &glob,
        &[],
    );
}

#[test]
fn a_caller_pin_wins_over_the_physical_type() {
    let (_dir, glob) = timestamp_source();
    let tail = format!("* | extract kv | where _time > {BOUND} | table seq");
    let mut pins = FieldTypes::new();
    pins.insert("_time", CanonicalType::Varchar);
    // Under the VARCHAR pin the tail compares the cast text: only
    // `infinity` sorts above the bound (`i` above `2`), and every dated
    // row's space sorts below its `T`. The all-SQL lane is no oracle
    // here, because `DuckDB` still compares a TIMESTAMP column as an
    // instant under a VARCHAR pin.
    assert_eq!(kept(&run_with(&tail, &glob, &pins)), [5]);
    assert_eq!(
        kept(&run(&tail, &glob)),
        [3, 4, 5],
        "without the pin the tail compares instants"
    );
}

#[test]
fn a_mixed_case_timestamp_column_compares_as_an_instant() {
    let (_dir, glob) = parquet(
        "SELECT * FROM (VALUES \
         (2, TIMESTAMP '2026-11-01 06:30:00', 'k=b'), \
         (1, TIMESTAMP '2026-11-01 05:00:00', 'k=a')) \
         AS t(seq, \"Stamp\", message)",
    );
    assert_parity(
        &format!("* | extract kv | where Stamp > {BOUND} | table seq"),
        &format!("* | where Stamp > {BOUND} | table seq"),
        &glob,
        &[2],
    );
    assert_parity(
        &format!("* | extract kv | where stamp > {BOUND} | table seq"),
        &format!("* | where stamp > {BOUND} | table seq"),
        &glob,
        &[2],
    );
}

/// A TIMESTAMPTZ and a `TIMESTAMP_NS` column behind `extract kv` compare
/// their cast text as a string, so neither row passes the bound. These
/// types are outside the fill because the all-SQL lane types them
/// differently from a Timestamp pin (a TIMESTAMPTZ applies a literal's
/// offset, a `TIMESTAMP_NS` keeps nanoseconds the pin truncates).
#[test]
fn timestamptz_and_timestamp_ns_columns_get_no_fill() {
    let (_dir, glob) = parquet(
        "SELECT * FROM (VALUES \
         (2, TIMESTAMPTZ '2026-11-01 06:30:00+00', TIMESTAMP_NS '2026-11-01 06:30:00', 'k=b'), \
         (1, TIMESTAMPTZ '2026-11-01 05:00:00+00', TIMESTAMP_NS '2026-11-01 05:00:00', 'k=a')) \
         AS t(seq, tz, ns, message)",
    );
    let tz = run(
        &format!("* | extract kv | where tz > {BOUND} | table seq"),
        &glob,
    );
    let ns = run(
        &format!("* | extract kv | where ns > {BOUND} | table seq"),
        &glob,
    );
    assert_eq!(kept(&tz), [0_i64; 0], "TIMESTAMPTZ behind extract kv");
    assert_eq!(kept(&ns), [0_i64; 0], "TIMESTAMP_NS behind extract kv");
}
