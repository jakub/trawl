// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A report run's result is stored in exact types (ADR-0035).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestServer, setup};
use trawl_api::value::{Column, ColumnType, QueryResult, Value};
use trawl_client::{ClientError, HttpClient};

const BIG_SUM: &str = "service=nginx | let big = 6000000000000000000 | stats sum(big) as total";
const BIG_TOTAL: u64 = 12_000_000_000_000_000_000;

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

/// Returns only once the manual-run gate has drained too: a run's lease goes
/// back after its terminal row is written, and the key's next press before
/// then answers 429.
async fn run_once(
    server: &TestServer,
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
        if runs.len() >= n
            && runs.iter().all(|r| r.status != "running")
            && server.state.query.pool.manual_leases_in_use() == 0
        {
            return runs;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{n} finished runs and a drained manual-run gate never appeared for saved query \
         {saved_id}"
    );
}

fn stored_path(server: &TestServer, run: &trawl_api::ReportRunSummary) -> PathBuf {
    let relative = run
        .result_path
        .as_deref()
        .unwrap_or_else(|| panic!("run {} must have a parquet result: {run:?}", run.id));
    Path::new(server.state.query.pool.base_dir()).join(relative)
}

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
        Err(ClientError::Server { status, error, .. }) => (status, error.message),
        other => panic!("expected a refusal, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_value_above_i64_max_is_stored_unsigned_and_reads_back_exactly() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(&client, "big_sum", BIG_SUM).await;
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
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
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
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
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_rust_tail_that_removes_every_row_stores_a_typed_empty_file() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(
        &client,
        "quiet_tail",
        "service=nginx | extract kv | where no_such_key == 999 | table host, severity",
    )
    .await;
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
    assert_eq!(run.status, "success", "{run:?}");
    assert_eq!(run.row_count, Some(0), "{run:?}");

    assert_eq!(
        described(&stored_path(&server, &run)),
        pairs(&[("host", "VARCHAR"), ("severity", "BIGINT")])
    );
    assert_eq!(
        served(&client, saved_id, run.id).await,
        QueryResult {
            columns: vec![
                Column::new("host", ColumnType::String),
                Column::new("severity", ColumnType::Integer),
            ],
            rows: vec![],
        }
    );
}

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
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_column_run_is_a_blob_that_counts_zero() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(&client, "nothing", "service=no_such_service | table host").await;
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
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
    let small = run_once(&server, &client, saved_id, 1).await.remove(0);
    assert_eq!(
        described(&stored_path(&server, &small)),
        pairs(&[("total", "BIGINT")])
    );
    client
        .update_saved(saved_id, BIG_SUM)
        .await
        .expect("the saved query is updated");
    let big = run_once(&server, &client, saved_id, 2).await.remove(0);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_field_the_run_does_not_carry_takes_no_live_pin() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved_id = net(&client, "hosts", "service=nginx | table host, severity").await;
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
    assert_eq!(run.status, "success", "{run:?}");
    assert_eq!(
        described(&stored_path(&server, &run)),
        pairs(&[("host", "VARCHAR"), ("severity", "BIGINT")])
    );
    let catalog = &server.state.query.field_catalog;
    catalog.repin("f", trawl_core::schema::CanonicalType::Double);
    catalog.repin("severity", trawl_core::schema::CanonicalType::Severity);

    for run in ["run=latest", "run=all"] {
        let saved = format!("| from saved hosts {run}");
        let tabled = client
            .query_paginated(&format!("{saved} | table f"), None, None)
            .await
            .unwrap_or_else(|e| panic!("{run}: an absent field projects: {e:?}"));
        assert_eq!(
            tabled.result,
            QueryResult {
                columns: vec![Column::new("f", ColumnType::String)],
                rows: vec![vec![Value::Null]; 2],
            },
            "{run}"
        );
        let lowered = client
            .query_paginated(&format!("{saved} | let y = lower(f) | table y"), None, None)
            .await
            .unwrap_or_else(|e| panic!("{run}: a string function binds over it: {e:?}"));
        assert_eq!(
            lowered.result,
            QueryResult {
                columns: vec![Column::new("y", ColumnType::String)],
                rows: vec![vec![Value::Null]; 2],
            },
            "{run}"
        );
        let ranked = client
            .query_paginated(
                &format!(r#"{saved} | where severity == "error" | stats count()"#),
                None,
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("{run}: a carried field keeps its pin: {e:?}"));
        assert_eq!(ranked.result.rows, vec![vec![Value::Integer(1)]], "{run}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn a_failed_parquet_write_records_a_readable_typed_blob() {
    use std::os::unix::fs::PermissionsExt as _;

    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:").map(str::to_owned))
        })
        .and_then(|line| line.split_whitespace().next().map(str::to_owned));
    assert_ne!(
        uid.as_deref(),
        Some("0"),
        "root writes into a mode-555 directory anyway, so this test cannot run as root"
    );

    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let scheduled = Path::new(server.state.query.pool.base_dir()).join("scheduled");
    std::fs::create_dir_all(&scheduled).unwrap();
    std::fs::set_permissions(&scheduled, std::fs::Permissions::from_mode(0o555)).unwrap();

    let saved_id = net(&client, "unwritable", "service=nginx | table service").await;
    let run = run_once(&server, &client, saved_id, 1).await.remove(0);
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
