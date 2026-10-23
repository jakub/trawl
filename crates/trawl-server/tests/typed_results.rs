// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed result columns and exact integers over a real server (#214).
//!
//! The query wire types every column from execution, so a result with no
//! rows still says what its columns hold, and it carries integers as
//! decimal strings so none loses digits in a JavaScript reader. Exports
//! are files, not the wire: NDJSON keeps JSON numbers, CSV keeps the
//! digits as text, and parquet keeps the column's exact type.
//!
//! Stored integers conform to `BIGINT` (ADR-0009 has no unsigned type),
//! so the `u64` here comes from an aggregate: `sum` is a HUGEINT, and one
//! that fits `u64` lands as one.

mod common;

use common::{TestServer, seed_data_root, setup_in_dir_with_data};
use trawl_server::config::RateLimitConfig;

/// The planted service; no seed row carries it.
const SERVICE: &str = "counters";

/// 2^53 + 1, the first integer an IEEE double cannot hold.
const PAST_DOUBLE: i64 = 9_007_199_254_740_993;

/// One stored integer an IEEE double cannot hold.
const PAST_DOUBLE_DSL: &str = "service=counters kind=past | fields n";

/// `i64::MAX + i64::MAX + 1`: a HUGEINT sum that is exactly `u64::MAX`.
const U64_MAX_DSL: &str = "service=counters kind=big | stats sum(n) as total";

/// Plant `{data}/prod/2024-01-15/10/counters.parquet`: a BIGINT `n`
/// holding [`PAST_DOUBLE`] under `kind=past`, and `i64::MAX`, `i64::MAX`
/// and 1 under `kind=big`.
fn plant_counters(data: &str) {
    let path = std::path::Path::new(data)
        .join("prod")
        .join("2024-01-15")
        .join("10")
        .join(format!("{SERVICE}.parquet"));
    let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
    conn.execute_batch(&format!(
        "COPY (
            SELECT
                TIMESTAMP '2024-01-15 10:00:00' + to_seconds(i) AS _time,
                TIMESTAMP '2024-01-15 10:00:10' AS _ingested,
                'raw' || i AS _raw,
                NULL::VARCHAR AS _repairs,
                'prod' AS env,
                '{SERVICE}' AS service,
                'meter01' AS host,
                kind,
                n::BIGINT AS n
            FROM (VALUES
                (0, 'past', {PAST_DOUBLE}),
                (1, 'big', {max}),
                (2, 'big', {max}),
                (3, 'big', 1)
            ) AS t(i, kind, n)
        ) TO '{path}' (FORMAT PARQUET)",
        max = i64::MAX,
        path = path.display(),
    ))
    .expect("write the counters parquet");
}

/// A server over a private data root holding the seed tree and the
/// planted counters.
async fn server(dir: &std::path::Path) -> TestServer {
    let data = seed_data_root(dir);
    plant_counters(&data);
    setup_in_dir_with_data(dir, data, RateLimitConfig::default()).await
}

/// POST `body` to `route` as the analyst; the status and the body bytes.
async fn post(server: &TestServer, route: &str, dsl: &str) -> (u16, Vec<u8>) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/{route}", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": dsl }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{route}: {e}"));
    let status = response.status().as_u16();
    (status, response.bytes().await.unwrap().to_vec())
}

/// POST `dsl` to `/query`; the body as JSON, after a 200.
async fn query(server: &TestServer, dsl: &str) -> serde_json::Value {
    let (status, body) = post(server, "query", dsl).await;
    assert_eq!(status, 200, "{dsl}: {}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

/// POST `dsl` to `/export` in `format`; the body as text, after a 200.
async fn export(server: &TestServer, format: &str, dsl: &str) -> Vec<u8> {
    let (status, body) = post(server, &format!("export?format={format}"), dsl).await;
    assert_eq!(
        status,
        200,
        "{format} {dsl}: {}",
        String::from_utf8_lossy(&body)
    );
    body
}

/// The `{"name","type"}` object a result names `column` with.
fn column<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    body["columns"]
        .as_array()
        .expect("a columns array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no column {name:?} in {body}"))
}

/// Every row's cell under `name`.
fn cells<'a>(body: &'a serde_json::Value, name: &str) -> Vec<&'a serde_json::Value> {
    let idx = body["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no column {name:?} in {body}"));
    body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row[idx])
        .collect()
}

fn client(server: &TestServer) -> trawl_client::HttpClient {
    trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap()
}

/// A filter that matches nothing over files that exist still answers
/// with every column typed by `DuckDB`, not by the cells it has none of
/// (AC1).
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_result_over_present_files_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path()).await;
    let dsl = "service=nginx host=zz_no_such_host";

    let body = query(&server, dsl).await;
    assert_eq!(
        body["rows"],
        serde_json::json!([]),
        "the filter matches nothing"
    );
    for (name, ty) in [
        ("_time", "timestamp"),
        ("severity", "integer"),
        ("message", "string"),
    ] {
        assert_eq!(
            column(&body, name),
            &serde_json::json!({ "name": name, "type": ty }),
            "{body}"
        );
    }

    // The client decodes the same body into a typed result.
    let response = client(&server)
        .query_paginated(dsl, None, None)
        .await
        .unwrap();
    let severity = response
        .result
        .columns
        .iter()
        .find(|c| c.name == "severity")
        .expect("severity column");
    assert_eq!(severity.ty, trawl_api::value::ColumnType::Integer);
}

/// The query wire carries integers past 2^53 as decimal strings in an
/// `integer` column, and the client lands them exactly: a stored BIGINT
/// as `Integer`, a HUGEINT sum of `u64::MAX` as `UInt`.
#[tokio::test(flavor = "multi_thread")]
async fn the_query_wire_carries_integers_as_decimal_strings() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path()).await;

    for (dsl, name, digits, landed) in [
        (
            PAST_DOUBLE_DSL,
            "n",
            PAST_DOUBLE.to_string(),
            trawl_api::value::Value::Integer(PAST_DOUBLE),
        ),
        (
            U64_MAX_DSL,
            "total",
            u64::MAX.to_string(),
            trawl_api::value::Value::UInt(u64::MAX),
        ),
    ] {
        let body = query(&server, dsl).await;
        assert_eq!(
            column(&body, name),
            &serde_json::json!({ "name": name, "type": "integer" }),
            "{dsl}"
        );
        assert_eq!(cells(&body, name), [&serde_json::json!(digits)], "{dsl}");

        let response = client(&server)
            .query_paginated(dsl, None, None)
            .await
            .unwrap();
        let idx = response
            .result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap();
        let cells: Vec<&trawl_api::value::Value> =
            response.result.rows.iter().map(|row| &row[idx]).collect();
        assert_eq!(cells, [&landed], "{dsl}");
    }
}

/// Export files are not the wire: NDJSON keeps JSON numbers, exact past
/// 2^53 and at `u64::MAX`, and CSV keeps every digit (AC9).
#[tokio::test(flavor = "multi_thread")]
async fn ndjson_and_csv_exports_keep_numbers_and_digits() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path()).await;

    for (dsl, name, number, digits) in [
        (
            PAST_DOUBLE_DSL,
            "n",
            serde_json::json!(PAST_DOUBLE),
            PAST_DOUBLE.to_string(),
        ),
        (
            U64_MAX_DSL,
            "total",
            serde_json::json!(u64::MAX),
            u64::MAX.to_string(),
        ),
    ] {
        let ndjson = String::from_utf8(export(&server, "json", dsl).await).unwrap();
        let numbers: Vec<serde_json::Value> = ndjson
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()[name].clone())
            .collect();
        assert_eq!(numbers, [number], "{dsl}: {ndjson}");
        assert!(
            ndjson.contains(&format!("\"{name}\":{digits}")),
            "a bare JSON number, not a string or a float: {ndjson}"
        );

        let csv = String::from_utf8(export(&server, "csv", dsl).await).unwrap();
        let mut lines = csv.lines();
        let header: Vec<&str> = lines.next().expect("a header").split(',').collect();
        let idx = header.iter().position(|h| *h == name).expect("the column");
        let exported: Vec<&str> = lines
            .map(|line| line.split(',').nth(idx).expect("a full row"))
            .collect();
        assert_eq!(exported, [digits.as_str()], "{dsl}: {csv}");
    }
}

/// The parquet export writes a stored integer past 2^53 as an exact
/// `BIGINT`, not a double (AC9).
#[tokio::test(flavor = "multi_thread")]
async fn the_parquet_export_keeps_an_exact_bigint() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path()).await;

    let parquet = export(&server, "parquet", PAST_DOUBLE_DSL).await;
    let file = dir.path().join("export.parquet");
    std::fs::write(&file, &parquet).unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let (ty, n): (String, i64) = conn
        .query_row(
            &format!(
                "SELECT (SELECT column_type FROM (DESCRIBE SELECT * FROM read_parquet('{file}')) \
                         WHERE column_name = 'n'), \
                        (SELECT n FROM read_parquet('{file}'))",
                file = file.display()
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((ty.as_str(), n), ("BIGINT", PAST_DOUBLE));
}
