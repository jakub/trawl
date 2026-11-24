// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Formatting never changes what a query means, in the SQL lane
//! (ADR-0061).
//!
//! The comparison stays inside this lane. Whether the live lane agrees
//! with the SQL lane is a different property (`filter_parity`), and
//! mixing the two here would hide a formatter defect behind a lane gap.
//!
//! Every row also names the row count it must return, so a query that
//! matches nothing before and after formatting cannot pass by accident.

use trawl_core::format::checked_format_query;
use trawl_core::parser;
use trawl_core::schema::FieldTypes;
use trawl_engine::executor::Executor;
use trawl_engine::value::{QueryResult, Value};

/// Two parquet files under one glob. The second does not carry `opt`,
/// so `union_by_name` reads that field as missing on its rows, beside a
/// row in the first file that holds an explicit NULL.
fn source() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let with_opt = dir.path().join("a.parquet");
    let without_opt = dir.path().join("b.parquet");
    conn.execute_batch(&format!(
        r#"
        COPY (
            SELECT * FROM (VALUES
                (TIMESTAMP '2024-01-15 10:00:00', 'web', 'web01', 'GET /a ok',            'x', 1, 'p',  'keep'),
                (TIMESTAMP '2024-01-15 10:00:01', 'web', 'web02', 'GET /b connection refused', 'y', 2, 'q',  NULL),
                (TIMESTAMP '2024-01-15 10:00:02', 'db',  '*',     'star * in the message',  'x', 1, 'p',  'keep'),
                (TIMESTAMP '2024-01-15 10:00:03', 'db',  'db01',  'bad gateway upstream',  '',  3, 'r',  'drop')
            ) AS t(_time, service, host, message, "OR.a", "true.a", "not.a", opt)
        ) TO '{a}' (FORMAT PARQUET);
        COPY (
            SELECT * FROM (VALUES
                (TIMESTAMP '2024-01-15 10:05:00', 'web', 'web01', 'POST /c timeout',        'x', 2, 'q'),
                (TIMESTAMP '2024-01-15 10:05:01', 'cache', 'c01', 'connection refused again', 'z', 1, 'p')
            ) AS t(_time, service, host, message, "OR.a", "true.a", "not.a")
        ) TO '{b}' (FORMAT PARQUET);
        "#,
        a = with_opt.display(),
        b = without_opt.display(),
    ))
    .unwrap();
    let glob = format!("{}/*.parquet", dir.path().display());
    (dir, glob)
}

const FIXTURES: &[(&str, usize)] = &[
    ("`OR.a`=x", 3),
    ("`OR.a`=\"\"", 1),
    ("`true.a`=1", 3),
    ("`not.a`=p service=db", 1),
    ("`OR.a`=x OR `true.a`=2", 4),
    ("service=web `OR.a`=x OR service=cache", 3),
    ("NOT `OR.a`=x", 3),
    ("* | where `OR.a` == \"x\" and `true.a` == 1", 2),
    ("* | where `true.a` in (2, 3)", 3),
    (
        "* | let t = `true.a` + 1 | where t > 2 | stats count() as n by service",
        3,
    ),
    ("NOT refused", 4),
    ("NOT \"bad gateway\"", 5),
    ("NOT refused NOT timeout", 3),
    ("connection NOT again", 1),
    ("refused OR NOT service=web", 4),
    ("*", 6),
    ("\"*\"", 1),
    ("host=*", 6),
    ("host=\"*\"", 1),
    ("service=db \"*\"", 1),
    ("opt=*", 3),
    ("NOT opt=drop", 2),
    ("opt=keep", 2),
    ("* | where isnull(opt)", 3),
    ("* | where opt != \"keep\" | stats count() as n", 1),
    (
        "earliest=\"2024-01-15T10:00:01Z\" latest=\"2024-01-15T10:05:00Z\"",
        3,
    ),
    (
        "service=web OR service=cache earliest=\"2024-01-15T10:00:01Z\"",
        3,
    ),
    ("latest=\"2024-01-15T10:00:02Z\" NOT refused", 1),
    (
        "earliest=\"2024-01-15T10:00:00Z\" `OR.a`=x | stats count() as n by service | sort service",
        3,
    ),
];

fn run(executor: &Executor, dsl: &str, source: &str) -> QueryResult {
    executor
        .run_query(dsl, source, &FieldTypes::new(), usize::MAX)
        .unwrap_or_else(|error| panic!("{dsl:?} runs: {error}"))
}

fn sorted_rows(result: &QueryResult) -> Vec<String> {
    let mut rows: Vec<String> = result.rows.iter().map(|row| format!("{row:?}")).collect();
    rows.sort();
    rows
}

#[test]
fn formatting_preserves_sql_results() {
    let (_dir, source) = source();
    let executor = Executor::new().unwrap();
    let mut failures = Vec::new();
    for &(query, expected_rows) in FIXTURES {
        let parsed = parser::parse(query).unwrap_or_else(|error| panic!("{query:?}: {error:?}"));
        let formatted = checked_format_query(&parsed)
            .unwrap_or_else(|error| panic!("{query:?} formats: {error:?}"));

        let before = run(&executor, query, &source);
        let after = run(&executor, &formatted, &source);

        let counted = selected(query, &before);
        if counted != expected_rows {
            failures.push(format!(
                "{query:?} selects {counted} rows, expected {expected_rows}"
            ));
        }
        if before.columns != after.columns {
            failures.push(format!(
                "{query:?} formatted as {formatted:?} changes the columns"
            ));
        }
        if sorted_rows(&before) != sorted_rows(&after) {
            failures.push(format!(
                "{query:?} formatted as {formatted:?} changes the rows"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn selected(query: &str, result: &QueryResult) -> usize {
    let Some(n) = result.columns.iter().position(|column| column.name == "n") else {
        return result.rows.len();
    };
    result
        .rows
        .iter()
        .map(|row| match &row[n] {
            Value::Integer(count) => usize::try_from(*count).unwrap(),
            Value::UInt(count) => usize::try_from(*count).unwrap(),
            other => panic!("{query:?}: count cell {other:?}"),
        })
        .sum()
}
