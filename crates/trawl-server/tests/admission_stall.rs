// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hot-buffer admission under a drain stall, end to end (ADR-0043).
//!
//! `stalled_drain_refuses_ingest_and_keeps_reads_exact` runs a real
//! compaction loop (`spawn_compaction`, wired as trawld's `main` wires it)
//! against a real server, then stalls it by blocking the catalog database.
//! That database is a separate one minted for the test. The server's own
//! app database is left alone, because trawld's `main` terminates when that
//! database's advisory-lock session dies.
//!
//! How a dead catalog stalls the drain: `compact_service_batch` makes the
//! batch's new pins durable (`resolve_pins_durable` → `pin_missing`) in
//! phase 2, before phase 3's `conform_and_publish` reaches `publish_output`.
//! A failed `pin_missing` returns the chunk as an error, so the WAL stays
//! where it is and nothing drains. Phase 2 only touches postgres when the
//! batch proposes a pin the in-process cache does not already hold, so
//! every event written after the block carries a field no earlier batch
//! had.
//!
//! `self_telemetry_is_admitted_where_a_trawld_named_request_is_refused`
//! drives a real `WalLayer` flush against the same fixture shape.
//!
//! `overhang_stall_stays_bounded` boots over WAL planted above the caps, so
//! the corpus is overhang (ADR-0041 slice 2), and stalls compaction the same
//! way while refused reads feed self-telemetry.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{
    HotBufferKnobs, TestServer, seed_data_root, setup_observing_boot, setup_with_hot_buffer,
    setup_with_hot_buffer_in,
};
use sqlx::{Connection as _, Executor as _};
use tracing_subscriber::layer::SubscriberExt as _;
use trawl_server::catalog::CatalogContext;
use trawl_server::config::IngestConfig;
use trawl_server::hot_buffer::{AdmissionState, HotBuffer};
use trawl_server::ingest::producer::Derivation;
use trawl_server::publication::CorpusUnsettled;
use trawl_server::telemetry::{WalHandle, WalLayer};

/// 64 events, so the HTTP ceiling is 60 and self-telemetry keeps 4. Bytes
/// are large enough that only the event dimension binds.
const MAX_EVENTS: usize = 64;
const HTTP_CEILING: usize = 60;

fn knobs(compaction_interval_secs: u64) -> HotBufferKnobs {
    HotBufferKnobs {
        max_events: MAX_EVENTS,
        max_bytes: 16 * 1024 * 1024,
        compaction_interval_secs,
    }
}

// -- requests -----------------------------------------------------------------

fn client() -> reqwest::Client {
    common::harness_client_builder().build().unwrap()
}

/// `count` ndjson events for `service` with ids `{prefix}-{n}`, each
/// carrying `extra` as additional fields.
fn events(
    service: &str,
    prefix: &str,
    count: usize,
    extra: &serde_json::Value,
) -> (Vec<u8>, Vec<String>) {
    let mut body = Vec::new();
    let mut ids = Vec::new();
    for n in 0..count {
        let id = format!("{prefix}-{n}");
        let mut event = serde_json::json!({
            "service": service,
            "id": id,
            "message": "admission stall probe",
        });
        if let (Some(event), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
            event.extend(extra.clone());
        }
        serde_json::to_writer(&mut body, &event).unwrap();
        body.push(b'\n');
        ids.push(id);
    }
    (body, ids)
}

async fn post(server: &TestServer, body: Vec<u8>) -> reqwest::Response {
    client()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// POST and require a 200 that accepted all `expected` events.
async fn acknowledge(server: &TestServer, body: Vec<u8>, expected: usize) {
    let response = post(server, body).await;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    let answer: trawl_api::IngestResponse = response.json().await.unwrap();
    assert_eq!(answer.accepted, expected, "{answer:?}");
    assert_eq!(answer.rejected, 0, "{answer:?}");
}

/// A refusal's status and error code.
async fn refusal(response: reqwest::Response) -> (u16, String) {
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.unwrap();
    (
        status,
        body["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

/// Run `dsl` over HTTP, require a 200, and return every row as a map from
/// column name to value.
async fn search(server: &TestServer, dsl: &str) -> Vec<BTreeMap<String, trawl_api::value::Value>> {
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
    assert_eq!(response.status(), 200, "{dsl}: {:?}", response.text().await);
    let answer: trawl_api::QueryResponse = response.json().await.unwrap();
    let columns: Vec<String> = answer
        .result
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    answer
        .result
        .rows
        .into_iter()
        .map(|row| columns.iter().cloned().zip(row).collect())
        .collect()
}

fn text(row: &BTreeMap<String, trawl_api::value::Value>, column: &str) -> Option<String> {
    match row.get(column)? {
        trawl_api::value::Value::String(value) => Some(value.clone()),
        _ => None,
    }
}

/// Every `id` a search for `service` returns, with its multiplicity.
async fn id_counts(server: &TestServer, service: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for row in search(server, &format!("service={service} last=1h")).await {
        let id = text(&row, "id").expect("every row carries a string id");
        *counts.entry(id).or_insert(0) += 1;
    }
    counts
}

/// Assert that a search returns exactly `acked`, each id once.
async fn assert_exactly_once(server: &TestServer, service: &str, acked: &[String]) {
    let counts = id_counts(server, service).await;
    let repeated: Vec<_> = counts.iter().filter(|(_, n)| **n != 1).collect();
    assert!(
        repeated.is_empty(),
        "ids returned more than once: {repeated:?}"
    );
    let found: BTreeSet<&String> = counts.keys().collect();
    let expected: BTreeSet<&String> = acked.iter().collect();
    assert_eq!(acked.len(), expected.len(), "acknowledged ids are distinct");
    assert_eq!(found, expected, "every acknowledged event, and only those");
}

/// GET /api/v1/health: its status code and body.
async fn health(server: &TestServer) -> (u16, serde_json::Value) {
    let response = client()
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

/// The value of one exact series (name and labels) on `/metrics`, `0` when
/// absent.
async fn metric(server: &TestServer, series: &str) -> u64 {
    let body = client()
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    body.lines()
        .find(|line| line.starts_with(series) && line[series.len()..].starts_with(' '))
        .map_or(0, |line| {
            line.rsplit(' ').next().unwrap().trim().parse().unwrap()
        })
}

fn hot(server: &TestServer) -> &Arc<HotBuffer> {
    server
        .state
        .query
        .hot_buffer
        .as_ref()
        .expect("ingest is enabled")
}

/// Every file under `dir`, recursively, with its size and mtime.
fn listing(dir: &Path) -> BTreeSet<(PathBuf, u64, std::time::SystemTime)> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                out.insert((entry.path(), meta.len(), meta.modified().unwrap()));
            }
        }
    }
    out
}

/// Poll `condition` every 50 ms until it holds, or panic after `limit`.
async fn eventually(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + limit;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {limit:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// -- the catalog database -------------------------------------------------------

/// The database name a DSN points at.
fn database_name(url: &str) -> String {
    url.rsplit('/')
        .next()
        .and_then(|last| last.split('?').next())
        .expect("database name in url")
        .to_owned()
}

/// Stop the database accepting connections and end every session on it.
/// Every catalog call then fails at connect.
async fn block_database(url: &str) {
    set_allow_connections(url, false).await;
    common::terminate_backends(url).await;
}

async fn unblock_database(url: &str) {
    set_allow_connections(url, true).await;
}

async fn set_allow_connections(url: &str, allow: bool) {
    let mut admin = sqlx::postgres::PgConnection::connect(&common::admin_database_url())
        .await
        .expect("connect to the admin database");
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            r#"ALTER DATABASE "{}" WITH ALLOW_CONNECTIONS {allow}"#,
            database_name(url)
        )))
        .await
        .expect("ALTER DATABASE ... ALLOW_CONNECTIONS");
}

// -- AC14 -----------------------------------------------------------------------

/// A drain that stalls behind a dead catalog ends in 503s, never in lost or
/// doubled events: reads stay exact and `/health` says `degraded` at 200
/// with `ingest_capacity` `refusing`. Once the catalog is back, the drain
/// resumes and ingest is admitted again.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, stated in order
async fn stalled_drain_refuses_ingest_and_keeps_reads_exact() {
    const SERVICE: &str = "adm-stall";
    const INTERVAL: Duration = Duration::from_secs(1);
    let dir = tempfile::tempdir().unwrap();
    let server = setup_with_hot_buffer_in(dir.path(), knobs(INTERVAL.as_secs())).await;
    let wal_dir = dir.path().join("wal");
    let data_dir = dir.path().join("data");
    let buffer = Arc::clone(hot(&server));

    // The catalog store on a database of its own, booted by the real path
    // (advisory lock, then migrate). Its lock session dies with the block;
    // nothing awaits that here, as nothing may for the server's own.
    let catalog_db = common::create_app_database().await;
    let catalog_storage =
        trawl_server::store::StorageState::from_pool(common::app_pool(&catalog_db).await)
            .await
            .expect("boot the catalog database");
    let catalog = CatalogContext {
        store: catalog_storage.catalog.clone(),
        cache: server.state.query.field_catalog.clone(),
    };

    // The compaction loop, as trawld's `main` spawns it, with the rollup
    // off: the seeded data root is not this test's subject.
    let defaults = IngestConfig::default();
    let (stop_compaction, compaction_stop_rx) = tokio::sync::watch::channel(false);
    let compaction = trawl_server::ingest::compaction::spawn_compaction(
        wal_dir.clone(),
        data_dir.clone(),
        INTERVAL,
        false,
        defaults.compaction_chunk_size,
        defaults.compaction_memory_limit.clone(),
        Some(Arc::clone(&buffer)),
        server.state.ingest.compaction_stats.clone(),
        Some(catalog),
        server.state.ingest.repin_coordinator.clone(),
        compaction_stop_rx,
    );

    // 1. Identifiable events, drained while the catalog is up.
    let mut acked = Vec::new();
    let (body, ids) = events(SERVICE, "before", 10, &serde_json::json!({}));
    acknowledge(&server, body, 10).await;
    acked.extend(ids);
    eventually("the first batch to drain", Duration::from_secs(30), || {
        buffer.event_count() == 0 && buffer.drained_batches() > 0
    })
    .await;
    assert_exactly_once(&server, SERVICE, &acked).await;

    // 2. Block the catalog. Nothing written from here on can drain.
    block_database(&catalog_db).await;
    let drained_at_block = buffer.drained_batches();
    let data_at_block = listing(&data_dir);
    let chunk_failures = "trawl_compaction_operation_failures_total{operation=\"chunk\"}";
    let failures_at_block = metric(&server, chunk_failures).await;

    // 3. Fill until HTTP refuses. Every event carries a field no drained
    //    batch had, so compaction must reach the catalog to pin it.
    let stalled = serde_json::json!({"stall_probe": "blocked"});
    let mut round = 0;
    let refused_ids = loop {
        let (body, ids) = events(SERVICE, &format!("fill{round}"), 7, &stalled);
        let response = post(&server, body).await;
        if response.status() == 200 {
            let answer: trawl_api::IngestResponse = response.json().await.unwrap();
            assert_eq!(answer.accepted, 7, "{answer:?}");
            acked.extend(ids);
        } else {
            let (status, code) = refusal(response).await;
            assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));
            break ids;
        }
        round += 1;
        assert!(round < 20, "HTTP never refused: {:?}", buffer.charged());
    };
    assert_eq!(
        acked.len(),
        10 + 7 * 8,
        "eight rounds of seven fit under 60"
    );
    assert_eq!(buffer.admission_state(), AdmissionState::Refusing);
    // What still fits is admitted, up to exactly the HTTP ceiling.
    let (body, ids) = events(SERVICE, "top", 4, &stalled);
    acknowledge(&server, body, 4).await;
    acked.extend(ids);
    assert_eq!(buffer.charged().events, HTTP_CEILING);

    // Failing passes: pressure passes (the refusal and every insert while
    // not `Open` wake one) and normal ones. Wait for the first failed
    // chunk, then give several more intervals the chance to drain.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while metric(&server, chunk_failures).await <= failures_at_block {
        assert!(
            tokio::time::Instant::now() < deadline,
            "compaction never failed a chunk against the blocked catalog"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(INTERVAL * 4).await;
    assert_eq!(
        buffer.drained_batches(),
        drained_at_block,
        "nothing drained while the catalog was blocked"
    );
    assert_eq!(
        listing(&data_dir),
        data_at_block,
        "no parquet was published: the chunk failed before publish_output"
    );
    assert_eq!(buffer.event_count(), HTTP_CEILING);
    // With no free space, the refusal comes before parsing.
    let (status, code) = refusal(post(&server, events(SERVICE, "late", 1, &stalled).0).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));

    // 4. Reads stay exact: parquet and hot rows, each acknowledged event
    //    once, nothing from a refused request.
    assert_exactly_once(&server, SERVICE, &acked).await;
    let found = id_counts(&server, SERVICE).await;
    for id in &refused_ids {
        assert!(!found.contains_key(id), "refused {id} became searchable");
    }

    // 5. Health: degraded at 200, only because ingest is refusing.
    let (status, body) = health(&server).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "degraded", "{body}");
    assert_eq!(body["checks"]["ingest_capacity"], "refusing", "{body}");
    for check in ["duckdb", "auth_db", "storage_db", "data_path"] {
        assert_eq!(body["checks"][check], "ok", "{check}: {body}");
    }

    // 6. Unblock the catalog: the drain resumes, admission reopens and
    //    ingest is admitted again.
    unblock_database(&catalog_db).await;
    eventually(
        "the stalled batches to drain once the catalog is back",
        Duration::from_secs(60),
        || buffer.event_count() == 0,
    )
    .await;
    assert!(buffer.drained_batches() > drained_at_block);
    assert_eq!(buffer.admission_state(), AdmissionState::Open);
    let (body, ids) = events(SERVICE, "after", 7, &stalled);
    acknowledge(&server, body, 7).await;
    acked.extend(ids);
    assert_exactly_once(&server, SERVICE, &acked).await;
    let (status, body) = health(&server).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["checks"]["ingest_capacity"], "ok", "{body}");
    assert_eq!(body["status"], "ok", "{body}");

    let _ = stop_compaction.send(true);
    tokio::time::timeout(Duration::from_secs(30), compaction)
        .await
        .expect("compaction stops")
        .expect("compaction task does not panic");
    drop(catalog_storage);
    // Leak the tempdir, as the fixtures do: the server's detached tasks
    // still hold its paths until the process exits.
    std::mem::forget(dir);
}

// -- AC9 ------------------------------------------------------------------------

/// With HTTP at its ceiling, a real self-telemetry flush is admitted into
/// the reserve while an HTTP request calling itself `service=trawld` is
/// refused: the entitlement follows the code path, not the event.
///
/// The fixture boots no telemetry producer, so the test builds one the way
/// trawld's `main` does: a `WalLayer` with the server's WAL writer, default
/// env and hot buffer, fed by a subscriber and flushed by `flush_cycle`,
/// the production flush task's write path.
#[tokio::test(flavor = "multi_thread")]
async fn self_telemetry_is_admitted_where_a_trawld_named_request_is_refused() {
    let server = setup_with_hot_buffer(knobs(37)).await;
    let buffer = Arc::clone(hot(&server));
    let refusals = |producer: &str| {
        format!(
            "trawl_hot_buffer_admission_refusals_total{{producer=\"{producer}\",kind=\"full\"}}"
        )
    };
    let trawld_refused = metric(&server, &refusals("trawld")).await;
    let http_refused = metric(&server, &refusals("http")).await;

    // Fill HTTP to exactly its ceiling.
    let mut acked = 0;
    let mut round = 0;
    while acked < HTTP_CEILING {
        let count = 20.min(HTTP_CEILING - acked);
        let (body, _) = events(
            "adm-ac9",
            &format!("fill{round}"),
            count,
            &serde_json::json!({}),
        );
        acknowledge(&server, body, count).await;
        acked += count;
        round += 1;
    }
    assert_eq!(buffer.charged().events, HTTP_CEILING);

    // An HTTP request naming itself trawld is refused.
    let spoof = serde_json::json!({});
    let (status, code) = refusal(post(&server, events("trawld", "spoof", 1, &spoof).0).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));

    // A real self-telemetry flush is admitted.
    let defaults = IngestConfig::default();
    let handle = WalHandle::new();
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
    let telemetry = WalLayer::new_with_buffer_cap(
        handle,
        &defaults.effective_envs(),
        &defaults.default_env,
        Arc::new(Derivation::defaults()),
        defaults.telemetry_buffer_max_bytes,
    );
    telemetry.set_hot_buffer(Arc::clone(&buffer));
    let marker = "ac9-self-telemetry";
    tracing::subscriber::with_default(
        tracing_subscriber::registry().with(telemetry.clone()),
        || tracing::info!(marker, "self-telemetry past the HTTP ceiling"),
    );
    telemetry.flush_cycle().await;
    assert_eq!(
        buffer.charged().events,
        HTTP_CEILING + 1,
        "the telemetry event is resident inside the reserve"
    );
    assert_eq!(buffer.event_count(), HTTP_CEILING + 1);

    // The reserve admits telemetry only: the spoof is still refused.
    let (status, code) =
        refusal(post(&server, events("trawld", "spoof2", 1, &spoof).0).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));

    // What a search for trawld finds is the flushed event, never a spoof.
    let rows = search(&server, "service=trawld last=1h").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        text(&rows[0], "marker").as_deref(),
        Some(marker),
        "{rows:?}"
    );
    assert_eq!(
        text(&rows[0], "_producer").as_deref(),
        Some("trawld"),
        "{rows:?}"
    );

    assert_eq!(
        metric(&server, &refusals("trawld")).await,
        trawld_refused,
        "self-telemetry was never refused"
    );
    assert_eq!(
        metric(&server, &refusals("http")).await - http_refused,
        2,
        "both trawld-named HTTP requests were refused as http"
    );
}

// -- AC10 (#265) ------------------------------------------------------------------

/// A search's status and error code.
async fn search_refusal(server: &TestServer, dsl: &str) -> (u16, String) {
    refusal(
        client()
            .post(format!("{}/api/v1/query", server.url))
            .bearer_auth(&server.analyst_token)
            .json(&serde_json::json!({ "query": dsl }))
            .send()
            .await
            .unwrap(),
    )
    .await
}

/// Captured events whose `event_type` is exactly `name`.
fn captured(
    capture: &common::audit_capture::Capture,
    name: &str,
) -> Vec<common::audit_capture::Captured> {
    let quoted = format!("{name:?}");
    capture
        .events()
        .into_iter()
        .filter(|event| event.fields.get("event_type") == Some(&quoted))
        .collect()
}

/// A stall under overhang stays bounded (ADR-0041 slice 2, ADR-0043).
///
/// The boot hydrates two of three planted WAL files and leaves the third
/// as overhang, so every search is refused. The catalog is down from the
/// first pass, so no planted file can drain and every pass fails its
/// chunks. Each refused search logs one WARN `http_failure`, which real
/// self-telemetry, wired as trawld's `main` wires it, flushes into the WAL
/// and the hot buffer. Those inserts, with the buffer in `Pressure`, wake
/// the compaction loop over and over.
///
/// Over the window, the passes stay at or below the elapsed intervals
/// plus one: a failing pass cools down until the next normal pass, and the
/// boot pass is the one extra. The compaction interval is three telemetry
/// flushes long, so the loop is woken more often than a normal pass falls
/// due, and a loop that let those wakes start passes would break the bound.
/// Every pass logs one `coverage_proof`, which fails with the stuck file not
/// resident. Once the catalog is back, the next pass drains everything, the
/// proof settles the corpus, and a search counts every planted event once.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one scenario, stated in order
async fn overhang_stall_stays_bounded() {
    const SERVICE: &str = "overhang-stall";
    const INTERVAL: Duration = Duration::from_secs(3);
    const WINDOW: Duration = Duration::from_secs(12);
    const FILE_EVENTS: usize = 350;
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let data = seed_data_root(dir.path());

    // Three WAL files in the live writer's name and line format. Two fit
    // under the caps; the third is overhang. Every event carries a field
    // no pin covers yet, so compacting it must reach the catalog.
    let time = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let writer = trawl_server::ingest::wal::WalWriter::new(dir.path().join("wal"));
    writer.ensure_dir().unwrap();
    let mut planted = Vec::new();
    for file in 0..3 {
        let mut ndjson = Vec::new();
        for n in 0..FILE_EVENTS {
            let id = format!("file{file}-{n}");
            let event = serde_json::json!({
                "_time": time,
                "_ingested": time,
                "env": "prod",
                "service": SERVICE,
                "id": id,
                "stall_probe": "overhang",
                "message": "overhang stall probe",
            });
            serde_json::to_writer(&mut ndjson, &event).unwrap();
            ndjson.push(b'\n');
            planted.push(id);
        }
        writer.write("prod", SERVICE, &ndjson).unwrap();
    }
    let knobs = HotBufferKnobs {
        max_events: 1024,
        max_bytes: 16 * 1024 * 1024,
        compaction_interval_secs: INTERVAL.as_secs(),
    };
    let server = setup_observing_boot(dir.path(), data, Some(knobs), &mut |_| {}).await;
    let buffer = Arc::clone(hot(&server));
    let gate = buffer.publication();
    assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RestartBacklog));
    assert_eq!(buffer.event_count(), 2 * FILE_EVENTS, "two files hydrated");
    assert_eq!(
        buffer.admission_state(),
        AdmissionState::Pressure,
        "hydration above half the cap: every insert wakes the loop"
    );

    // The catalog on a database of its own, down before the first pass.
    let catalog_db = common::create_app_database().await;
    let catalog_storage =
        trawl_server::store::StorageState::from_pool(common::app_pool(&catalog_db).await)
            .await
            .expect("boot the catalog database");
    let catalog = CatalogContext {
        store: catalog_storage.catalog.clone(),
        cache: server.state.query.field_catalog.clone(),
    };
    block_database(&catalog_db).await;

    // Self-telemetry as trawld's `main` activates it after the boot: the
    // WAL layer on the process's subscriber, then the writer, the hot
    // buffer and the flush task. The capture layer beside it counts.
    let defaults = IngestConfig::default();
    let handle = WalHandle::new();
    let telemetry = WalLayer::new_with_buffer_cap(
        handle.clone(),
        &defaults.effective_envs(),
        &defaults.default_env,
        Arc::new(Derivation::defaults()),
        defaults.telemetry_buffer_max_bytes,
    );
    let capture = common::audit_capture::Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(telemetry.clone())
            .with(capture.clone())
            .with(tracing_subscriber::EnvFilter::new(
                trawl_server::telemetry::DEFAULT_LOG_FILTER,
            )),
    )
    .expect("this test owns the process's tracing subscriber");
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
    telemetry.set_hot_buffer(Arc::clone(&buffer));
    let (stop_flush, flush_stop_rx) = tokio::sync::watch::channel(false);
    let flush = trawl_server::telemetry::spawn_flush_task(
        telemetry.clone(),
        Duration::from_secs(defaults.telemetry_flush_interval_secs),
        flush_stop_rx,
    );

    let stats = server
        .state
        .ingest
        .compaction_stats
        .clone()
        .expect("ingest is enabled");
    let pressure = buffer.subscribe_pressure();
    let wakes_before = *pressure.borrow();
    let inserted_before = buffer.inserted_batches();
    let loop_started = tokio::time::Instant::now();
    let (stop_compaction, compaction_stop_rx) = tokio::sync::watch::channel(false);
    let compaction = trawl_server::ingest::compaction::spawn_compaction(
        dir.path().join("wal"),
        data_dir.clone(),
        INTERVAL,
        false,
        defaults.compaction_chunk_size,
        defaults.compaction_memory_limit.clone(),
        Some(Arc::clone(&buffer)),
        Some(Arc::clone(&stats)),
        Some(catalog),
        server.state.ingest.repin_coordinator.clone(),
        compaction_stop_rx,
    );

    // Refused searches for the whole window, ten per interval.
    let dsl = format!("service={SERVICE} last=1h");
    let mut refused = 0;
    while loop_started.elapsed() < WINDOW {
        assert_eq!(
            search_refusal(&server, &dsl).await,
            (503, "corpus_recovering".to_owned())
        );
        refused += 1;
        tokio::time::sleep(INTERVAL / 10).await;
    }
    let proofs = captured(&capture, "coverage_proof");
    let finished = stats.total_runs.load(std::sync::atomic::Ordering::Relaxed);
    let intervals = loop_started.elapsed().as_secs() / INTERVAL.as_secs();

    // What the window offered the loop: refusals, their WARNs, and the
    // self-telemetry inserts that wake it.
    assert_eq!(
        captured(&capture, "http_failure")
            .iter()
            .filter(|event| event.field("cause_kind").contains("restart_backlog"))
            .count(),
        refused,
        "one http_failure per refused search"
    );
    assert!(
        buffer.inserted_batches() > inserted_before,
        "self-telemetry inserted during the stall"
    );
    assert!(
        *pressure.borrow() > wakes_before,
        "the loop was woken during the stall"
    );

    // The bound: the boot pass, then at most one pass per interval.
    assert!(
        proofs.len() as u64 <= intervals + 1,
        "{} passes in {intervals} intervals",
        proofs.len()
    );
    assert!(
        finished <= intervals + 1,
        "{finished} finished passes in {intervals} intervals"
    );
    assert!(
        proofs.len() >= 2,
        "the boot pass and at least one normal pass ran: {proofs:?}"
    );
    for proof in &proofs {
        assert!(
            proof.field("outcome").contains("not_resident"),
            "the stuck file stays uncovered: {proof:?}"
        );
    }
    assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RestartBacklog));
    assert!(
        captured(&capture, "corpus_settled").is_empty(),
        "nothing settled during the stall"
    );

    // Catalog back: the next pass drains everything and settles the corpus.
    unblock_database(&catalog_db).await;
    eventually(
        "the corpus to settle once the catalog is back",
        Duration::from_secs(60),
        || gate.unsettled().is_none(),
    )
    .await;
    assert_eq!(captured(&capture, "corpus_settled").len(), 1);
    assert_exactly_once(&server, SERVICE, &planted).await;

    let _ = stop_compaction.send(true);
    tokio::time::timeout(Duration::from_secs(30), compaction)
        .await
        .expect("compaction stops")
        .expect("compaction task does not panic");
    let _ = stop_flush.send(true);
    tokio::time::timeout(Duration::from_secs(30), flush)
        .await
        .expect("the telemetry flush stops")
        .expect("the flush task does not panic");
    drop(catalog_storage);
    // Leak the tempdir, as the fixtures do: the server's detached tasks
    // still hold its paths until the process exits.
    std::mem::forget(dir);
}
