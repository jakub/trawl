// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A `/query` whose search stage pins several services reads only their
//! files, through the real router (#374).
//!
//! The shared seed holds `prod/2024-01-15/10/{nginx,postgres}.parquet`:
//! two nginx rows and one postgres row. A pinned service with no file in
//! the selection must never reach `DuckDB`, which fails on a missing literal
//! in a list. A garbage `junk.parquet` beside the seed tells a narrowed
//! read from a filtered one: a read that opens it fails.
//!
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use std::path::Path;

use common::TestServer;
use serde_json::Value;
use trawl_server::config::RateLimitConfig;

/// `/api/v1/query` over `dsl`: its status and its body.
async fn query(server: &TestServer, dsl: &str) -> (reqwest::StatusCode, Value) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&trawl_api::QueryRequest {
            query: dsl.to_owned(),
            limit: Some(10_000),
            offset: None,
        })
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{dsl}: {status}: the body is not JSON ({e}): {text}"));
    (status, body)
}

/// The `service` of each row `/query` answers for `dsl`, sorted; the query
/// must answer 200.
async fn services(server: &TestServer, dsl: &str) -> Vec<String> {
    let (status, body) = query(server, dsl).await;
    assert_eq!(status, 200, "{dsl}: {body}");
    let response: trawl_api::QueryResponse = serde_json::from_value(body).unwrap();
    let mut services: Vec<String> = common::result_rows(&response.result)
        .iter()
        .map(|row| row["service"].as_str().unwrap_or_default().to_owned())
        .collect();
    services.sort();
    services
}

/// A server over a private copy of the shared seed. With `junk`, a file
/// that is not parquet sits in the seed's hour directory beside the two
/// services' files.
async fn boot(dir: &Path, junk: bool) -> TestServer {
    let data = common::seed_data_root(dir);
    if junk {
        std::fs::write(
            Path::new(&data).join("prod/2024-01-15/10/junk.parquet"),
            b"these bytes are not a parquet file",
        )
        .unwrap();
    }
    common::setup_in_dir_with_data(dir, data, RateLimitConfig::default()).await
}

/// Two pinned services, one with no file: the list and the `OR` forms both
/// answer the present service's rows, and neither opens the other files in
/// the directory. An unpinned query opens every file there and fails on
/// the junk, so the pinned reads were narrowed, not filtered.
#[tokio::test(flavor = "multi_thread")]
async fn one_present_one_missing_reads_the_present_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let server = boot(tmp.path(), true).await;

    for dsl in ["service=nginx,absent", "(service=nginx OR service=absent)"] {
        assert_eq!(services(&server, dsl).await, ["nginx", "nginx"], "{dsl}");
    }
    assert_eq!(
        services(&server, "service=nginx,postgres,absent").await,
        ["nginx", "nginx", "postgres"]
    );

    let (status, body) = query(&server, "*").await;
    assert_eq!(status, 500, "an unpinned read opens the junk file: {body}");
    assert_eq!(body["error"]["code"], "execution_error", "{body}");
}

/// A value that reads as a number pins nothing, so `service=1,nginx` reads
/// every file and its SQL filter still keeps the nginx rows. No junk here:
/// this read opens every file in the directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_numeric_looking_name_reads_every_file_and_keeps_the_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let server = boot(tmp.path(), false).await;

    assert_eq!(
        services(&server, "service=1,nginx").await,
        ["nginx", "nginx"]
    );
}

/// Two pinned services, neither with a file: 200, no rows, no error.
#[tokio::test(flavor = "multi_thread")]
async fn all_missing_reads_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let server = boot(tmp.path(), true).await;

    let (status, body) = query(&server, "service=absent,gone").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("error").is_none(), "{body}");
    let response: trawl_api::QueryResponse = serde_json::from_value(body).unwrap();
    assert!(response.result.rows.is_empty(), "{:?}", response.result);
}
