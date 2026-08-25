// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `extract kv` batch tail against real `DuckDB`: what the SQL prefix
//! computes is what the tail receives.
//!
//! The tail used to reach the Rust stages through `serde_json`, which has
//! no spelling for a non-finite double — so a prefix computing `1.0/0`
//! handed the tail a NULL, and the SAME query without the kv split
//! answered differently. These tests run both shapes of each query
//! against one another rather than against a hand-written expectation, so
//! agreement is the assertion.

use std::io::Write as _;

use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_engine::executor::Executor;
use trawl_engine::value::{QueryResult, Value};

/// One ndjson row, written where `read_json` can reach it.
fn source(rows: &[&str]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.ndjson");
    let mut file = std::fs::File::create(&path).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
    file.flush().unwrap();
    let glob = path.display().to_string();
    (dir, glob)
}

fn run(exec: &Executor, dsl: &str, glob: &str) -> QueryResult {
    exec.run_query(dsl, glob, &FieldTypes::new(), usize::MAX, 0)
        .unwrap_or_else(|error| panic!("{dsl:?} must run: {error}"))
}

fn cell<'r>(result: &'r QueryResult, row: usize, name: &str) -> &'r Value {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", result.columns));
    &result.rows[row][index]
}

/// (a) The specials arrive as themselves, and (b) the same query without
/// the kv split answers identically.
#[test]
fn the_kv_tail_receives_the_specials_the_sql_prefix_computed() {
    let (_dir, glob) = source(&[r#"{"message":"k=v","service":"nginx"}"#]);
    let exec = Executor::new().unwrap();

    let with_tail = run(
        &exec,
        "* | let pos = 1.0 / 0, neg = -1.0 / 0, undef = 0.0 / 0 \
         | extract kv from message | table pos, neg, undef",
        &glob,
    );
    let Value::Float(pos) = cell(&with_tail, 0, "pos") else {
        panic!("pos must be a float: {:?}", cell(&with_tail, 0, "pos"));
    };
    let Value::Float(neg) = cell(&with_tail, 0, "neg") else {
        panic!("neg must be a float");
    };
    let Value::Float(undef) = cell(&with_tail, 0, "undef") else {
        panic!("undef must be a float");
    };
    assert!(pos.is_infinite() && pos.is_sign_positive(), "{pos}");
    assert!(neg.is_infinite() && neg.is_sign_negative(), "{neg}");
    assert!(undef.is_nan(), "{undef}");

    // The same query WITHOUT the kv split — pure SQL, no tail at all.
    let without_tail = run(
        &exec,
        "* | let pos = 1.0 / 0, neg = -1.0 / 0, undef = 0.0 / 0 | table pos, neg, undef",
        &glob,
    );
    for name in ["pos", "neg"] {
        assert_eq!(
            cell(&with_tail, 0, name),
            cell(&without_tail, 0, name),
            "the kv split changed {name}"
        );
    }
    // NaN is never equal to itself, so the pair is compared by rendering.
    let (Value::Float(tail_nan), Value::Float(sql_nan)) = (
        cell(&with_tail, 0, "undef"),
        cell(&without_tail, 0, "undef"),
    ) else {
        panic!("both lanes must answer a float");
    };
    assert_eq!(
        (tail_nan.is_nan(), sql_nan.is_nan()),
        (true, true),
        "the kv split changed undef"
    );
}

/// A special that survives the bridge is also usable BY the tail — it
/// filters, aggregates and sorts as the value it is, where a NULL would
/// have dropped the row and emptied the aggregate.
#[test]
fn the_tail_computes_over_the_specials_it_received() {
    let (_dir, glob) = source(&[
        r#"{"message":"k=1","service":"nginx"}"#,
        r#"{"message":"k=2","service":"nginx"}"#,
    ]);
    let exec = Executor::new().unwrap();

    // `where` over an infinity: `inf > 1000000.0` is TRUE, so both rows
    // stay. (A plain decimal literal: the DSL float grammar has no
    // exponent form.)
    let kept = run(
        &exec,
        "* | let big = 1.0 / 0 | extract kv from message | where big > 1000000.0 | table k",
        &glob,
    );
    assert_eq!(
        kept.row_count(),
        2,
        "an infinity compares, it does not null"
    );

    // …and `max`/`count` see it.
    let stats = run(
        &exec,
        "* | let big = 1.0 / 0 | extract kv from message | stats count() as n, max(big) as m",
        &glob,
    );
    assert_eq!(cell(&stats, 0, "n"), &Value::Integer(2));
    let Value::Float(max) = cell(&stats, 0, "m") else {
        panic!("max must be a float: {:?}", cell(&stats, 0, "m"));
    };
    assert!(max.is_infinite() && max.is_sign_positive(), "{max}");

    // A NaN sorts GREATEST, the way DuckDB orders it — not "wherever
    // arrival order left it". `v` stays in the projection because the
    // tail defers every sort to the very end, after `table` has already
    // chosen the columns.
    let sorted = run(
        &exec,
        "* | extract kv from message | let v = if(k == 1, 0.0 / 0, 5.0) | sort v | table k, v",
        &glob,
    );
    assert_eq!(
        (cell(&sorted, 0, "k"), cell(&sorted, 1, "k")),
        (&Value::Integer(2), &Value::Integer(1)),
        "the NaN row sorts last"
    );
}

/// (c) The silent-regression guard: a TIMESTAMP column still display-shifts
/// after a tail runs.
///
/// The shift re-parses the rendered TEXT (`shift_timestamp_columns`), so a
/// bridge that retyped timestamp cells would break it without any test
/// noticing — the value would simply stay in UTC.
#[test]
fn a_timestamp_column_still_shifts_after_the_tail() {
    let (_dir, glob) = source(&[
        r#"{"message":"k=v","service":"nginx","_time":"2026-01-15T09:00:00Z","_ingested":"2026-01-15T09:00:00Z"}"#,
    ]);
    let exec = Executor::new().unwrap();
    let dsl = "* | extract kv from message | table _time, k";
    let offset = 5 * 3600 + 1800; // +05:30

    let shifted = exec
        .run_query(dsl, &glob, &FieldTypes::new(), usize::MAX, offset)
        .unwrap();
    let utc = exec
        .run_query(dsl, &glob, &FieldTypes::new(), usize::MAX, 0)
        .unwrap();

    let Value::String(shifted_time) = cell(&shifted, 0, "_time") else {
        panic!("_time must stay a string through the tail");
    };
    let Value::String(utc_time) = cell(&utc, 0, "_time") else {
        panic!("_time must stay a string through the tail");
    };
    assert_eq!(utc_time, "2026-01-15 09:00:00");
    assert_eq!(
        shifted_time, "2026-01-15 14:30:00",
        "the display offset must still reach a cell that crossed the tail"
    );
}

/// A timestamp the TAIL computes — an infinity included — survives a
/// display-shifted run.
///
/// The shift re-parses the rendered text of the columns it tracks; an
/// infinity renders as a WORD, which is not that rendering, so it passes
/// through untouched while a real `_time` beside it still shifts. Both
/// halves are asserted, because "nothing moved" is also what a broken
/// shift looks like.
#[test]
fn a_tail_computed_infinity_survives_a_shifted_run() {
    let (_dir, glob) = source(&[
        r#"{"message":"k=v","service":"nginx","_time":"2026-01-15T09:00:00Z","_ingested":"2026-01-15T09:00:00Z"}"#,
    ]);
    let exec = Executor::new().unwrap();
    let dsl =
        r#"* | extract kv from message | let t = date_trunc("day", "infinity") | table _time, t"#;
    let offset = 5 * 3600 + 1800; // +05:30

    let shifted = exec
        .run_query(dsl, &glob, &FieldTypes::new(), usize::MAX, offset)
        .unwrap();
    assert_eq!(
        cell(&shifted, 0, "t"),
        &Value::String("infinity".into()),
        "a computed infinity must cross the bridge and the shift unchanged"
    );
    assert_eq!(
        cell(&shifted, 0, "_time"),
        &Value::String("2026-01-15 14:30:00".into()),
        "…while the real timestamp column beside it still shifts"
    );
}

/// (d) A DOUBLE-pinned infinity answers a tail's `| where d > 1` the way
/// the batch lane does.
///
/// The pinned read used to project a non-finite as JSON null — UNKNOWN,
/// so the row vanished — while the SQL lane compared it happily. The
/// column is written as a real DOUBLE, which is what a conformed corpus
/// holds (both `inf` spellings survive the pin's round-trip guard).
#[test]
fn a_double_pinned_infinity_compares_in_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("metric.parquet");
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT 'inf'::DOUBLE AS d, 'k=v' AS message, 'nginx' AS service) \
         TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();
    let glob = file.display().to_string();

    let exec = Executor::new().unwrap();
    let mut pins = FieldTypes::new();
    pins.insert("d", CanonicalType::Double);

    let batch = exec
        .run_query(
            "* | where d > 1 | table service",
            &glob,
            &pins,
            usize::MAX,
            0,
        )
        .unwrap();
    assert_eq!(
        batch.row_count(),
        1,
        "premise: the SQL lane compares a stored infinity"
    );

    let tail = exec
        .run_query(
            "* | extract kv from message | where d > 1 | table k",
            &glob,
            &pins,
            usize::MAX,
            0,
        )
        .unwrap();
    assert_eq!(
        tail.row_count(),
        1,
        "the tail must answer as the SQL lane does"
    );
}

// ── the now() anchor across the kv split (ADR-0017 §3, #106) ──────────

/// One row of real parquet, written by `DuckDB` itself — the shape the
/// production cold lane reads.
fn parquet_source() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.parquet");
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT TIMESTAMP '2024-01-15 10:00:00' AS \"_time\", \
         'nginx' AS service, 'k=v' AS message) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .unwrap();
    (dir, path.display().to_string())
}

/// AC1: the SQL prefix and the `rust_stages` tail read ONE instant.
///
/// `now()` appears three times in one statement across the kv split —
/// twice in the SQL prefix (a `let` and a `where`) and once in the tail.
/// The prefix's cell and the tail's cell must be the same string: one
/// unit of output, one instant, whichever side of the split computed it.
#[test]
fn the_kv_tail_and_the_sql_prefix_read_one_now() {
    let (_dir, glob) = parquet_source();
    let exec = Executor::new().unwrap();

    let result = run(
        &exec,
        "* | let sql_now = now() | where now() >= _time \
         | extract kv from message | let tail_now = now() \
         | table sql_now, tail_now",
        &glob,
    );

    assert_eq!(result.row_count(), 1, "the where must keep the row");
    let prefix = cell(&result, 0, "sql_now");
    let tail = cell(&result, 0, "tail_now");
    assert!(
        matches!(prefix, Value::String(_)),
        "the prefix cell arrives as rendered timestamp text: {prefix:?}"
    );
    assert_eq!(
        prefix, tail,
        "the SQL prefix and the kv tail must render one instant byte for byte"
    );
}

/// The same guarantee where the tail's anchor could most easily drift:
/// the COLD-START lane, where the union attempt finds no parquet and the
/// query is RE-EMITTED hot-only. The tail keeps evaluating under the
/// original emission's anchor, so a re-capture at the re-emission would
/// split the two cells apart.
#[test]
fn the_cold_start_fallback_keeps_the_statements_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\
         \"service\":\"svc\",\"message\":\"k=v\"}\n",
    )
    .unwrap();
    let exec = Executor::new().unwrap();
    // A glob that reaches no parquet at all: the union errors and the
    // executor falls back to reading the hot buffer alone.
    let cold = format!("{}/nonexistent/*.parquet", dir.path().display());

    let result = exec
        .run_query_with_hot(
            "* | let sql_now = now() | extract kv from message \
             | let tail_now = now() | table sql_now, tail_now",
            &cold,
            hot.to_str().unwrap(),
            &FieldTypes::new(),
            &FieldTypes::new(),
            usize::MAX,
            0,
        )
        .expect("the cold-start fallback must answer");

    assert_eq!(result.row_count(), 1);
    assert_eq!(
        cell(&result, 0, "sql_now"),
        cell(&result, 0, "tail_now"),
        "the re-emitted hot-only SQL must inherit the statement's anchor"
    );
}
