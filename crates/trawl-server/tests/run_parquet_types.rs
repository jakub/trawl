// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A report run's result is stored in exact types (ADR-0035, amended
//! 2026-10-22), over the real server: real postgres, real scheduler runs,
//! real parquet files on disk, read back through `GET run` and
//! `| from saved`.
//!
//! The parquet file is the ordinary representation, a zero-row run
//! included. The typed blob is kept for a zero-column result, a `mixed`
//! column and a failed write; `from saved` reads a zero-column blob as an
//! empty source and refuses any other blob by the run's name.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestServer, setup};
use trawl_api::value::{Column, ColumnType, QueryResult, Value};
use trawl_client::{ClientError, HttpClient};

/// Each of the fixture's two nginx events contributes this much, so their
/// `sum` is 12,000,000,000,000,000,000: past `i64::MAX`, under `u64::MAX`.
const BIG_SUM: &str = "service=nginx | let big = 6000000000000000000 | stats sum(big) as total";
const BIG_TOTAL: u64 = 12_000_000_000_000_000_000;

/// A saved query with a schedule, ready to be triggered.
async fn net(client: &HttpClient, name: &str, query: &str) -> i64 {
    let saved = client
        .create_saved(name, query)
        .await
        .expect("the saved query is created");
    client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .expect("the schedule is attached");
    saved.id
}

/// Trigger one run and return it once it has finished, with the run
/// before it if there was one: newest first.
async fn run_once(
    client: &HttpClient,
    saved_id: i64,
    n: usize,
) -> Vec<trawl_api::ReportRunSummary> {
    client
        .trigger_run(saved_id)
        .await
        .expect("the run is accepted");
    for _ in 0..100 {
        let runs = client
            .list_report_runs(saved_id, None, None)
            .await
            .expect("the runs list answers")
            .runs;
        if runs.len() >= n && runs.iter().all(|r| r.status != "running") {
            return runs;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{n} finished runs never appeared for saved query {saved_id}");
}

/// The absolute path of a run's stored parquet result.
fn stored_path(server: &TestServer, run: &trawl_api::ReportRunSummary) -> PathBuf {
    let relative = run
        .result_path
        .as_deref()
        .unwrap_or_else(|| panic!("run {} must have a parquet result: {run:?}", run.id));
    Path::new(server.state.query.pool.base_dir()).join(relative)
}

/// The `DuckDB` type of every column in a parquet file, in order.
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

fn pairs(described: &[(&str, &str)]) -> Vec<(String, String)> {
    described
        .iter()
        .map(|(name, ty)| ((*name).to_owned(), (*ty).to_owned()))
        .collect()
}

/// The result `GET run` serves, decoded through the typed codec.
async fn served(client: &HttpClient, saved_id: i64, run_id: i64) -> QueryResult {
    client
        .get_report_run(saved_id, run_id)
        .await
        .expect("the run endpoint answers")
        .result
        .expect("a successful run carries its result")
}

fn refusal<T: std::fmt::Debug>(outcome: Result<T, ClientError>) -> (u16, String) {
    match outcome {
        Err(ClientError::Server { status, error }) => (status, error.message),
        other => panic!("expected a refusal, got: {other:?}"),
    }
}

/// AC 7: a `sum` past `i64::MAX` is stored as `UBIGINT`. The footer says
/// `UINT_64`, never a double, and the run reads back with every digit.
#[tokio::test(flavor = "multi_thread")]
async fn a_value_above_i64_max_is_stored_unsigned_and_reads_back_exactly() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(&client, "big_sum", BIG_SUM).await;
    let run = run_once(&client, saved_id, 1).await.remove(0);
    assert_eq!(run.status, "success", "{run:?}");

    let path = stored_path(&server, &run);
    assert_eq!(described(&path), pairs(&[("total", "UBIGINT")]));
    let (physical, converted): (String, Option<String>) = duckdb::Connection::open_in_memory()
        .unwrap()
        .query_row(
            &format!(
                "SELECT type, converted_type FROM parquet_schema('{}') WHERE name = 'total'",
                path.display()
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (physical.as_str(), converted.as_deref()),
        ("INT64", Some("UINT_64"))
    );

    assert_eq!(
        served(&client, saved_id, run.id).await,
        QueryResult {
            columns: vec![Column::new("total", ColumnType::Integer)],
            rows: vec![vec![Value::UInt(BIG_TOTAL)]],
        }
    );
}

/// The other stored types a run produces: `BIGINT`, a UTC `TIMESTAMP`, a
/// `BIGINT[]`, a `DOUBLE` holding NaN, and a column of digit strings that
/// stays `VARCHAR` because nothing is inferred.
#[tokio::test(flavor = "multi_thread")]
async fn a_runs_columns_are_stored_in_their_exact_types() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(
        &client,
        "typed",
        "service=nginx | let nan = 0.0 / 0, code = \"123\" \
         | stats count() as n, min(_time) as first, list(severity) as sevs, \
           max(nan) as nan, max(code) as code",
    )
    .await;
    let run = run_once(&client, saved_id, 1).await.remove(0);
    assert_eq!(run.status, "success", "{run:?}");

    assert_eq!(
        described(&stored_path(&server, &run)),
        pairs(&[
            ("n", "BIGINT"),
            ("first", "TIMESTAMP"),
            ("sevs", "BIGINT[]"),
            ("nan", "DOUBLE"),
            ("code", "VARCHAR"),
        ])
    );

    let result = served(&client, saved_id, run.id).await;
    assert_eq!(
        result.columns,
        vec![
            Column::new("n", ColumnType::Integer),
            Column::new("first", ColumnType::Timestamp),
            Column::new("sevs", ColumnType::list(ColumnType::Integer)),
            Column::new("nan", ColumnType::Float),
            Column::new("code", ColumnType::String),
        ]
    );
    let row = &result.rows[0];
    assert_eq!(row[0], Value::Integer(2));
    assert_eq!(row[1], Value::String("2024-01-15T10:00:00.000000Z".into()));
    let Value::Array(sevs) = &row[2] else {
        panic!("a list cell: {row:?}");
    };
    let mut sevs = sevs.clone();
    sevs.sort_by_key(|v| match v {
        Value::Integer(i) => *i,
        other => panic!("an integer element: {other:?}"),
    });
    assert_eq!(sevs, vec![Value::Integer(9), Value::Integer(17)]);
    assert!(matches!(row[3], Value::Float(f) if f.is_nan()), "{row:?}");
    assert_eq!(row[4], Value::String("123".into()));
}

/// AC 1: a run whose window holds no event stores a typed empty parquet
/// file. `GET run` serves its typed columns with no rows, and
/// `| from saved` reads it as a typed source.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_row_run_stores_a_typed_empty_file() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    // The filter is on `host`, not `service`: the files are read and no
    // row survives, so the columns are typed by execution.
    let saved_id = net(
        &client,
        "quiet",
        "service=nginx host=no_such_host | table host, severity",
    )
    .await;
    let run = run_once(&client, saved_id, 1).await.remove(0);
    assert_eq!(run.status, "success", "{run:?}");
    assert_eq!(run.row_count, Some(0), "{run:?}");

    assert_eq!(
        described(&stored_path(&server, &run)),
        pairs(&[("host", "VARCHAR"), ("severity", "BIGINT")])
    );
    let typed = vec![
        Column::new("host", ColumnType::String),
        Column::new("severity", ColumnType::Integer),
    ];
    assert_eq!(
        served(&client, saved_id, run.id).await,
        QueryResult {
            columns: typed.clone(),
            rows: vec![],
        }
    );

    let counted = client
        .query_paginated("| from saved quiet run=latest | stats count()", None, None)
        .await
        .expect("a zero-row run is a working source");
    assert_eq!(counted.result.rows, vec![vec![Value::Integer(0)]]);
    let listed = client
        .query_paginated(
            "| from saved quiet run=latest | table host, severity",
            None,
            None,
        )
        .await
        .expect("its columns bind with their types");
    assert_eq!(
        listed.result,
        QueryResult {
            columns: typed,
            rows: vec![],
        }
    );
}

/// A `mixed` column has no parquet form, so the run keeps a typed blob.
/// `GET run` serves it cell for cell, and `| from saved` refuses it by the
/// run's name.
#[tokio::test(flavor = "multi_thread")]
async fn a_mixed_column_run_is_a_blob_that_from_saved_refuses() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(
        &client,
        "mixed",
        "service=nginx | let kvs = if(severity > 10, \"x=abc\", \"x=1\") \
         | extract kv from kvs | table x",
    )
    .await;
    let run = run_once(&client, saved_id, 1).await.remove(0);
    assert_eq!(run.status, "success", "{run:?}");
    assert!(run.result_path.is_none(), "{run:?}");

    let result = served(&client, saved_id, run.id).await;
    assert_eq!(result.columns, vec![Column::new("x", ColumnType::Mixed)]);
    let mut cells: Vec<String> = result
        .rows
        .iter()
        .map(|row| format!("{:?}", row[0]))
        .collect();
    cells.sort();
    assert_eq!(cells, vec!["Integer(1)", "String(\"abc\")"]);

    let (status, message) = refusal(
        client
            .query_paginated("| from saved mixed run=latest", None, None)
            .await,
    );
    assert_eq!(status, 409, "{message}");
    assert!(
        message.contains(&format!("report run {}", run.id))
            && message.contains("no parquet result"),
        "the refusal names the run: {message}"
    );
}

/// A run whose source matched no file at all has no columns, which parquet
/// cannot hold. It keeps a blob, and `| from saved` reads it as a source
/// with no rows.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_column_run_is_a_blob_that_counts_zero() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(&client, "nothing", "service=no_such_service | table host").await;
    let run = run_once(&client, saved_id, 1).await.remove(0);
    assert_eq!(run.status, "success", "{run:?}");
    assert!(run.result_path.is_none(), "{run:?}");
    assert_eq!(
        served(&client, saved_id, run.id).await,
        QueryResult::empty()
    );

    let counted = client
        .query_paginated(
            "| from saved nothing run=latest | stats count()",
            None,
            None,
        )
        .await
        .expect("a zero-column run is a source with no rows");
    assert_eq!(counted.result.rows, vec![vec![Value::Integer(0)]]);
}

/// `run=all` over a `BIGINT` run and a `UBIGINT` run: the union widens the
/// column to `HUGEINT` in the query, and both totals answer exactly.
#[tokio::test(flavor = "multi_thread")]
async fn run_all_unions_a_bigint_run_and_a_ubigint_run_exactly() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(
        &client,
        "widths",
        "service=nginx | stats sum(severity) as total",
    )
    .await;
    let small = run_once(&client, saved_id, 1).await.remove(0);
    assert_eq!(
        described(&stored_path(&server, &small)),
        pairs(&[("total", "BIGINT")])
    );
    client
        .update_saved(saved_id, BIG_SUM)
        .await
        .expect("the saved query is updated");
    let big = run_once(&client, saved_id, 2).await.remove(0);
    assert_eq!(
        described(&stored_path(&server, &big)),
        pairs(&[("total", "UBIGINT")])
    );

    let answered = client
        .query_paginated(
            "| from saved widths run=all | sort total | table total",
            None,
            None,
        )
        .await
        .expect("run=all unions the two widths");
    assert_eq!(
        answered.result,
        QueryResult {
            columns: vec![Column::new("total", ColumnType::Integer)],
            rows: vec![vec![Value::Integer(26)], vec![Value::UInt(BIG_TOTAL)]],
        }
    );
}

/// A parquet write that fails falls back to the typed blob, which `GET run`
/// serves. The scheduled directory is made read-only, so the writer cannot
/// create its temp file.
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn a_failed_parquet_write_records_a_readable_typed_blob() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let scheduled = Path::new(server.state.query.pool.base_dir()).join("scheduled");
    std::fs::create_dir_all(&scheduled).unwrap();
    std::fs::set_permissions(&scheduled, std::fs::Permissions::from_mode(0o555)).unwrap();

    let saved_id = net(&client, "unwritable", "service=nginx | table service").await;
    let run = run_once(&client, saved_id, 1).await.remove(0);
    std::fs::set_permissions(&scheduled, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(run.status, "success", "{run:?}");
    assert!(run.result_path.is_none(), "the write failed: {run:?}");
    assert_eq!(
        served(&client, saved_id, run.id).await,
        QueryResult {
            columns: vec![Column::new("service", ColumnType::String)],
            rows: vec![
                vec![Value::String("nginx".into())],
                vec![Value::String("nginx".into())],
            ],
        }
    );
}
