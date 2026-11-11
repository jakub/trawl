// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Queries at the tree depth limit run (#447).
//!
//! The parser admits a tree exactly `MAX_TREE_DEPTH` (64) levels deep, so
//! every such query has to reach `DuckDB` (or the kv tail) and come back
//! with its row, not with `DuckDB`'s own `Max expression depth` error or a
//! stack overflow. Each case runs parse, execution and the drop of the
//! result on a thread with a worker's 2 MiB stack, against one parquet
//! row whose `a` is 1. Each shape's truth and parity are chosen so the
//! row comes back, and the test reads the row, not only the absence of an
//! error.

use trawl_core::parser::MAX_TREE_DEPTH;
use trawl_core::schema::FieldTypes;
use trawl_engine::executor::Executor;
use trawl_engine::value::Value;

/// trawld's worker and blocking threads.
const WORKER_STACK: usize = 2 * 1024 * 1024;

/// One parquet row: the base columns, a message that `extract kv` splits
/// into `a=1`, and a BIGINT column `a` of 1.
fn one_row() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("one.parquet");
    let conn = duckdb::Connection::open_in_memory().expect("fixture database");
    conn.execute_batch(&format!(
        "COPY (SELECT TIMESTAMP '2026-03-14 03:00:00' AS _time, \
                      TIMESTAMP '2026-03-14 03:00:00' AS _ingested, \
                      'web-1' AS host, 'nginx' AS service, \
                      'a=1' AS message, 'a=1' AS _raw, \
                      CAST(1 AS BIGINT) AS a) \
         TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .expect("write the one-row parquet");
    let glob = path.display().to_string();
    (dir, glob)
}

/// `n` terms joined by `op`: a chain `n - 1` levels deep.
fn chain(op: &str, n: usize) -> String {
    vec!["1"; n].join(&format!(" {op} "))
}

/// `a == 0 or a == 1 …`: `n` comparisons, `n` levels deep. Only
/// `a == 1` is true, so the chain holds for the row.
fn or_chain(n: usize) -> String {
    (0..n)
        .map(|i| format!("a == {i}"))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The queries the criterion names, each with its deepest tree `d` levels
/// deep. The comments count the levels at `d` = 64.
fn cases(d: usize) -> Vec<(&'static str, String)> {
    vec![
        // 64 `+` terms are 63 levels; the comparison is the 64th.
        (
            "plus chain",
            format!("* | where {} > 0 | table a", chain("+", d)),
        ),
        // 63 minus signs over `a` (an odd count, so -1) and the
        // comparison over them: 64 levels, and -1 < 2 holds.
        (
            "minus run",
            format!("* | where {}a < 2 | table a", "-".repeat(d - 1)),
        ),
        ("or chain", format!("* | where {} | table a", or_chain(d))),
        // An even count of NOTs keeps the match.
        (
            "search NOT run",
            format!("{}a=1 | table a", "NOT ".repeat(d)),
        ),
        (
            "search brackets",
            format!("{}a=1{} | table a", "(".repeat(d), " )".repeat(d)),
        ),
        (
            "where after extract kv",
            format!("* | extract kv | where {} | table a", or_chain(d)),
        ),
    ]
}

/// Every 64-deep query returns the one row, with `a` read back as 1. The
/// same shapes one level deeper are refused, which pins each case at the
/// limit rather than somewhere under it.
#[test]
fn queries_at_the_tree_depth_limit_return_their_row() {
    let (_dir, glob) = one_row();
    std::thread::Builder::new()
        .name("tree-depth-64".into())
        .stack_size(WORKER_STACK)
        .spawn(move || {
            let exec = Executor::new().expect("executor");
            for (name, dsl) in cases(MAX_TREE_DEPTH + 1) {
                let Err(error) = exec.run_query(&dsl, &glob, &FieldTypes::new(), usize::MAX) else {
                    panic!("{name} at depth 65 is refused");
                };
                assert!(
                    error.to_string().contains("deeper than 64 levels"),
                    "{name} at depth 65 gets the depth refusal, got: {error}"
                );
            }
            for (name, dsl) in cases(MAX_TREE_DEPTH) {
                let result = exec
                    .run_query(&dsl, &glob, &FieldTypes::new(), usize::MAX)
                    .unwrap_or_else(|error| panic!("{name} at depth 64 must run: {error}"));
                assert_eq!(result.rows.len(), 1, "{name} returns its row: {result:?}");
                let column = result
                    .columns
                    .iter()
                    .position(|column| column.name == "a")
                    .unwrap_or_else(|| panic!("{name} carries a: {:?}", result.columns));
                assert_eq!(
                    result.rows[0][column],
                    Value::Integer(1),
                    "{name} returns the row's a"
                );
            }
        })
        .expect("spawn the 2 MiB thread")
        .join()
        .expect("every query at the limit runs on a worker's stack");
}
