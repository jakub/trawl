// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`RowCap`] against real `DuckDB`: a refusing cap refuses, a cutting cap
//! cuts, and a cutting cap never cuts the rows a kv tail aggregates.
//!
//! A row count cannot tell a `LIMIT` `DuckDB` applied from a cut made after
//! it built the whole result, so the pushdown is read off the statement
//! the lane actually prepared ([`prepare_probe::last_statement`]).

use std::io::Write as _;

use trawl_core::schema::FieldTypes;
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::executor::{Executor, RowCap, prepare_probe};
use trawl_engine::value::{QueryResult, Value};

/// Six events, `k=a` three times and `k=b` three times, where `read_json`
/// can reach them.
fn six_events() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.ndjson");
    let mut file = std::fs::File::create(&path).unwrap();
    for i in 0..6 {
        let k = if i % 2 == 0 { "a" } else { "b" };
        writeln!(file, r#"{{"message":"k={k}","n":{i}}}"#).unwrap();
    }
    file.flush().unwrap();
    let glob = path.display().to_string();
    (dir, glob)
}

fn run(dsl: &str, glob: &str, cap: RowCap) -> Result<QueryResult, EngineError> {
    let exec = Executor::new().unwrap();
    exec.cancellable(&CancelLatch::never())
        .run_query(dsl, glob, &FieldTypes::new(), cap, 0)
}

#[test]
fn refuse_refuses_a_result_past_its_cap_and_passes_one_at_it() {
    let (_dir, glob) = six_events();

    let refused = run("*", &glob, RowCap::Refuse(5));
    assert!(
        matches!(refused, Err(EngineError::ResultTooLarge(5))),
        "six rows past a cap of five: {refused:?}"
    );
    let whole = run("*", &glob, RowCap::Refuse(6)).expect("six rows fit a cap of six");
    assert_eq!(whole.rows.len(), 6);
}

#[test]
fn truncate_keeps_the_first_rows_of_a_sql_pipeline() {
    let (_dir, glob) = six_events();
    let cap = RowCap::Truncate {
        rows: 2,
        tail_input: 3,
    };

    let cut = run("* | sort n | table n", &glob, cap).expect("a cut is not a refusal");
    assert_eq!(
        cut.rows,
        vec![vec![Value::Integer(0)], vec![Value::Integer(1)]],
        "the first two rows in the pipeline's order"
    );

    // `tail_input` bounds only what a kv tail reads: a SQL pipeline past
    // it is still cut, not refused.
    let past_tail_input = run("*", &glob, cap).expect("a SQL pipeline is never refused");
    assert_eq!(past_tail_input.rows.len(), 2);

    let none = run(
        "*",
        &glob,
        RowCap::Truncate {
            rows: 0,
            tail_input: 6,
        },
    )
    .expect("a cut to nothing");
    assert!(none.rows.is_empty());
}

#[test]
fn truncate_cuts_a_kv_tail_answer_but_never_its_input() {
    let (_dir, glob) = six_events();

    // A count over every event, not over the first one.
    let counted = run(
        "* | extract kv | stats count() as c",
        &glob,
        RowCap::Truncate {
            rows: 1,
            tail_input: 6,
        },
    )
    .expect("six events fit the tail's input bound");
    assert_eq!(counted.rows, vec![vec![Value::Integer(6)]]);

    // Two groups out, one kept.
    let grouped = run(
        "* | extract kv | stats count() by k",
        &glob,
        RowCap::Truncate {
            rows: 1,
            tail_input: 6,
        },
    )
    .expect("the tail's answer is cut");
    assert_eq!(grouped.rows.len(), 1);

    // Cutting the input would publish a wrong count, so it is refused.
    let refused = run(
        "* | extract kv | stats count() as c",
        &glob,
        RowCap::Truncate {
            rows: 1,
            tail_input: 5,
        },
    );
    assert!(
        matches!(refused, Err(EngineError::ResultTooLarge(5))),
        "six events past a tail input bound of five: {refused:?}"
    );
}

/// Run `dsl` under `cap` and return the SQL the lane handed `DuckDB`.
///
/// The lane runs on this thread and the probe keeps each thread's last
/// statement, so nothing another test binds can land in between.
fn prepared(dsl: &str, glob: &str, cap: RowCap) -> String {
    run(dsl, glob, cap).unwrap_or_else(|e| panic!("{dsl:?} under {cap:?}: {e}"));
    prepare_probe::last_statement().expect("the lane prepared a statement")
}

#[test]
fn truncate_reaches_duckdb_as_a_limit_and_refuse_does_not() {
    let (_dir, glob) = six_events();
    let dsl = "n>=0 | sort n | table n";

    let refused = prepared(dsl, &glob, RowCap::Refuse(6));
    assert!(
        !refused.contains("LIMIT"),
        "the interactive lane runs the emitted SQL untouched: {refused}"
    );

    // The same emitted SQL, wrapped: the cut happens inside DuckDB, and the
    // wrapper adds no placeholder to the parameters the search binds.
    let truncated = prepared(
        dsl,
        &glob,
        RowCap::Truncate {
            rows: 2,
            tail_input: 6,
        },
    );
    assert_eq!(truncated, format!("SELECT * FROM ({refused}) LIMIT 2"));
    assert!(
        refused.contains('?'),
        "the search binds a parameter: {refused}"
    );
    assert_eq!(truncated.matches('?').count(), refused.matches('?').count());
}

#[test]
fn a_kv_tail_under_truncate_prepares_no_limit() {
    let (_dir, glob) = six_events();
    let dsl = "* | extract kv | stats count() as c";

    let tail = prepared(
        dsl,
        &glob,
        RowCap::Truncate {
            rows: 1,
            tail_input: 6,
        },
    );
    assert!(
        !tail.contains("LIMIT"),
        "the tail's input is read whole, never cut in SQL: {tail}"
    );
    assert_eq!(tail, prepared(dsl, &glob, RowCap::Refuse(6)));
}
