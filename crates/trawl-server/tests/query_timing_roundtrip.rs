// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A query's `query_timing` event, end to end through trawld's own
//! telemetry (ADR-0046): the slice goal is that an operator finds the slow
//! phase by searching trawld's telemetry with the DSL.
//!
//! The fixture boots no telemetry producer, so the test builds one the way
//! trawld's `main` does: `build_subscriber` with the default filter and a
//! `WalLayer` sink, installed as the process's global subscriber, then fed
//! the server's WAL writer, default env and hot buffer. `flush_cycle`, the
//! production flush task's write path, moves the captured events into the
//! WAL and the hot buffer, where a search reads them back.
//!
//! The subscriber has to be global: the server emits from its tokio
//! workers and blocking threads, which a thread-local `set_default` never
//! reaches. A process has one global subscriber, so this binary holds one
//! test.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::TestServer;
use trawl_api::value::Value;
use trawl_server::config::IngestConfig;
use trawl_server::ingest::producer::Derivation;
use trawl_server::telemetry::{self, WalHandle, WalLayer};

type Row = BTreeMap<String, Value>;

/// The phase fields the issue's recipe reads.
const PHASES: [&str; 3] = ["query_bind_us", "query_execute_us", "query_pool_wait_us"];

fn client() -> reqwest::Client {
    common::harness_client_builder().build().unwrap()
}

/// Install trawld's subscriber shape as the global one: one global filter,
/// a WAL sink and no other sink. Returns the WAL handle to activate once
/// the server exists, and the layer the test flushes.
fn install_telemetry() -> (WalHandle, WalLayer) {
    let defaults = IngestConfig::default();
    let handle = WalHandle::new();
    let layer = WalLayer::new_with_buffer_cap(
        handle.clone(),
        &defaults.effective_envs(),
        &defaults.default_env,
        Arc::new(Derivation::defaults()),
        defaults.telemetry_buffer_max_bytes,
    );
    let (dispatch, _) = telemetry::build_subscriber(
        telemetry::DEFAULT_LOG_FILTER,
        telemetry::LogSinks {
            stdout: None::<fn() -> std::io::Sink>,
            wal: Some(layer.clone()),
            file_log: false,
        },
    );
    tracing::dispatcher::set_global_default(dispatch)
        .expect("this binary installs its only global subscriber");
    (handle, layer)
}

/// Run `dsl` over HTTP, require a 200, and return the request's id with
/// every row as a map from column name to value.
async fn query(server: &TestServer, dsl: &str) -> (String, Vec<Row>) {
    let response = client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&trawl_api::QueryRequest {
            query: dsl.to_owned(),
            limit: Some(10_000),
            offset: None,
            timezone: None,
        })
        .send()
        .await
        .unwrap();
    let request_id = response
        .headers()
        .get("x-request-id")
        .expect("every response carries its request id")
        .to_str()
        .expect("a ULID is ASCII")
        .to_owned();
    assert_eq!(response.status(), 200, "{dsl}: {:?}", response.text().await);
    let answer: trawl_api::QueryResponse = response.json().await.unwrap();
    let columns: Vec<String> = answer
        .result
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let rows = answer
        .result
        .rows
        .into_iter()
        .map(|row| columns.iter().cloned().zip(row).collect())
        .collect();
    (request_id, rows)
}

/// The one value a `stats count()` answers with.
async fn count(server: &TestServer, dsl: &str) -> i64 {
    let (_, rows) = query(server, dsl).await;
    assert_eq!(rows.len(), 1, "{dsl}: {rows:?}");
    match rows[0].values().next() {
        Some(Value::Integer(n)) => *n,
        other => panic!("{dsl}: the count is an integer, got {other:?}"),
    }
}

/// A column's value as an integer, only when the row carries it as one.
fn integer(row: &Row, column: &str) -> Option<u64> {
    match row.get(column)? {
        Value::Integer(n) => u64::try_from(*n).ok(),
        Value::UInt(n) => Some(*n),
        _ => None,
    }
}

fn text<'r>(row: &'r Row, column: &str) -> Option<&'r str> {
    match row.get(column)? {
        Value::String(value) => Some(value),
        _ => None,
    }
}

/// A query's timing lands in trawld's telemetry with numeric phase fields,
/// and only trawld's own door can produce a row the `_producer=trawld`
/// filter matches: a forged HTTP event naming itself trawld does not.
#[tokio::test(flavor = "multi_thread")]
async fn query_timing_round_trips_through_trawld_telemetry() {
    let (handle, telemetry) = install_telemetry();
    let server = common::setup().await;
    let defaults = IngestConfig::default();
    handle.set(
        Arc::clone(
            server
                .state
                .ingest
                .wal_writer
                .as_ref()
                .expect("ingest is enabled"),
        ),
        &defaults.default_env,
    );
    telemetry.set_hot_buffer(Arc::clone(
        server
            .state
            .query
            .hot_buffer
            .as_ref()
            .expect("ingest is enabled"),
    ));

    // 1. A query against the real server, then the WAL flush.
    let (request_id, rows) = query(&server, "service=nginx").await;
    assert!(
        !rows.is_empty(),
        "the seeded corpus answers the probe query"
    );
    telemetry.flush_cycle().await;

    // The request id is the only handle the client holds, so it finds the
    // query id. Every search emits a `query_timing` of its own, under its
    // own request id: filtering on the probe's request id keeps each
    // verification query from matching itself.
    let (_, found) = query(
        &server,
        &format!(
            "service=trawld _producer=trawld event_type=query_timing \
             request_id=\"{request_id}\" | table query_id"
        ),
    )
    .await;
    assert_eq!(found.len(), 1, "one account for the probe: {found:?}");
    let query_id = integer(&found[0], "query_id").expect("query_id is an integer");

    // The flushed WAL holds no `pool_acquired`, which the timing account
    // replaced, while it holds the probe's account: the count of zero is
    // over a capture that saw the query run.
    for (filter, expected) in [
        (
            format!("event_type=query_timing request_id=\"{request_id}\""),
            1,
        ),
        ("event_type=pool_acquired".to_owned(), 0),
    ] {
        let dsl = format!("service=trawld _producer=trawld {filter} | stats count()");
        assert_eq!(count(&server, &dsl).await, expected, "{dsl}");
    }

    // 2. The issue's recipe returns the phases as numbers, not strings.
    let recipe = format!(
        "service=trawld _producer=trawld event_type=query_timing query_id={query_id} \
         request_id=\"{request_id}\" | table {}",
        PHASES.join(", ")
    );
    let (_, rows) = query(&server, &recipe).await;
    assert_eq!(rows.len(), 1, "{recipe}: {rows:?}");
    for phase in PHASES {
        assert!(
            integer(&rows[0], phase).is_some(),
            "{phase} is an integer: {rows:?}"
        );
    }

    // 3. A forged HTTP event carrying the same identity, and a `_producer`
    //    of its own that ingest overwrites with the door it came through.
    let mut forged = serde_json::json!({
        "service": "trawld",
        "event_type": "query_timing",
        "query_id": query_id,
        "request_id": request_id,
        "_producer": "trawld",
        "message": "forged query timing",
    });
    for phase in PHASES {
        forged[phase] = 1.into();
    }
    let response = client()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .header("content-type", "application/x-ndjson")
        .body(format!("{forged}\n"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    let answer: trawl_api::IngestResponse = response.json().await.unwrap();
    assert_eq!((answer.accepted, answer.rejected), (1, 0), "{answer:?}");

    // The forgery landed: without the producer filter both rows match.
    let (_, rows) = query(
        &server,
        &format!(
            "service=trawld event_type=query_timing query_id={query_id} \
             request_id=\"{request_id}\" | table _producer"
        ),
    )
    .await;
    let mut producers: Vec<_> = rows.iter().filter_map(|r| text(r, "_producer")).collect();
    producers.sort_unstable();
    assert_eq!(producers, ["http", "trawld"], "{rows:?}");

    // The producer filter still finds trawld's own event only.
    let (_, rows) = query(&server, &recipe).await;
    assert_eq!(rows.len(), 1, "the forgery does not match: {rows:?}");
}
