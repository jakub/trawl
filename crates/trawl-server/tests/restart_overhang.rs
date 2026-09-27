// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A restart whose surviving WAL the hot buffer cannot hold (ADR-0041
//! slice 2), through a real `AppState` and the HTTPS server.
//!
//! Each test plants writer-format WAL above the hot-buffer caps before the
//! fixture boots, so hydration leaves **overhang**. The fixture never
//! spawns compaction, and that is the barrier that holds overhang in
//! place: a barrier inside a publish would hold the publication write
//! guard, and readers would wait on it instead of being refused. A test
//! releases overhang by running the production compaction loop until its
//! coverage proof settles the gate.
//!
//! The binary installs one global capture subscriber, so exact counts of
//! events no request id correlates (`corpus_settled`, the scheduler's
//! transitions) rely on nextest running each test in its own process.

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use common::audit_capture::{Capture, Captured};
use common::{HotBufferKnobs, TestServer, seed_data_root, setup_observing_boot};
use trawl_api::value::{Column, QueryResult, Value};
use trawl_server::publication::CorpusUnsettled;
use trawl_server::report_window::truncate_to_micros;
use trawl_server::scheduler::{poll_and_execute, spawn_scheduler};
use trawl_server::state::AppState;
use trawl_server::store::{FinishOutcome, RunClaim, RunStatus};

/// Every planted event's service, so counts see no seed row.
const SERVICE: &str = "restartoverhang";

/// Caps a handful of events can overflow. The compaction interval only
/// matters to the loop a test spawns to release overhang; its first pass
/// runs at once regardless.
const KNOBS: HotBufferKnobs = HotBufferKnobs {
    max_events: 20,
    max_bytes: 1 << 20,
    compaction_interval_secs: 3600,
};

/// Events in the planted file that fits the caps and hydrates.
const FITS: usize = 5;

/// Events in the planted file over the full event cap: never resident.
const OVERSIZED: usize = 30;

/// The saved query whose recorded run `| from saved` reads.
const REPORT: &str = "overhang_report";

// -- capture -------------------------------------------------------------------

/// The one global subscriber, under the production default directives.
fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        use tracing_subscriber::prelude::*;
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone().with_filter(
            tracing_subscriber::EnvFilter::new(trawl_server::telemetry::DEFAULT_LOG_FILTER),
        ));
        // Global: the server logs from every tokio worker and blocking thread.
        tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");
        capture
    })
}

/// Every captured event whose `event_type` is exactly `name`.
fn events_of(name: &str) -> Vec<Captured> {
    let quoted = format!("{name:?}");
    capture()
        .events()
        .into_iter()
        .filter(|event| event.fields.get("event_type") == Some(&quoted))
        .collect()
}

// -- planting and boot ---------------------------------------------------------

/// A WAL file in the live writer's name and line format, planted under
/// `dir/wal/prod`, holding `count` events of [`SERVICE`] tagged `tag`.
fn plant_wal(dir: &Path, tag: &str, count: usize) -> PathBuf {
    let time = (Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut ndjson = Vec::new();
    for seq in 0..count {
        let event = serde_json::json!({
            "_time": time,
            "_ingested": time,
            "env": "prod",
            "service": SERVICE,
            "seq": seq,
            "message": format!("{tag} {seq}"),
        });
        serde_json::to_writer(&mut ndjson, &event).unwrap();
        ndjson.push(b'\n');
    }
    let writer = trawl_server::ingest::wal::WalWriter::new(dir.join("wal"));
    writer.ensure_dir().unwrap();
    writer.write("prod", SERVICE, &ndjson).unwrap()
}

/// Boot over a WAL the caps cannot hold: a file of `fits` events that
/// hydrates (none when zero) and one oversized file. Returns the server
/// and its data root.
async fn boot_with_overhang(dir: &Path, fits: usize) -> (TestServer, String) {
    let data = seed_data_root(dir);
    if fits > 0 {
        plant_wal(dir, "fits", fits);
    }
    plant_wal(dir, "oversized", OVERSIZED);
    let server = setup_observing_boot(dir, data.clone(), Some(KNOBS), &mut |_| {}).await;
    assert_eq!(
        unsettled(&server.state),
        Some(CorpusUnsettled::RestartBacklog),
        "the oversized file is overhang"
    );
    assert_eq!(
        server
            .state
            .query
            .hot_buffer
            .as_ref()
            .unwrap()
            .event_count(),
        fits,
        "the file that fits hydrated"
    );
    (server, data)
}

/// The gate the server's reads go through.
fn unsettled(state: &AppState) -> Option<CorpusUnsettled> {
    state.query.pool.publication().unsettled()
}

/// Release overhang the way production does: run the compaction loop
/// `main` runs over this server's WAL and data root until its coverage
/// proof settles the gate, then stop it.
async fn settle(server: &TestServer, dir: &Path, data: &str) {
    let state = &server.state;
    let ingest = trawl_config::IngestConfig::default();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let compaction = trawl_server::ingest::compaction::spawn_compaction(
        dir.join("wal"),
        PathBuf::from(data),
        Duration::from_secs(KNOBS.compaction_interval_secs),
        false,
        ingest.compaction_chunk_size,
        ingest.compaction_memory_limit,
        state.query.hot_buffer.clone(),
        state.ingest.compaction_stats.clone(),
        Some(trawl_server::catalog::CatalogContext {
            store: state.storage.catalog.clone(),
            cache: state.query.field_catalog.clone(),
        }),
        state.ingest.repin_coordinator.clone(),
        stopped,
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while unsettled(state).is_some() {
        assert!(
            Instant::now() < deadline,
            "compaction did not settle the corpus within 60s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(true).unwrap();
    compaction.await.unwrap();
}

// -- requests ------------------------------------------------------------------

fn raw_client() -> reqwest::Client {
    common::harness_client_builder().build().unwrap()
}

fn client(server: &TestServer) -> trawl_client::HttpClient {
    trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap()
}

/// `stats count()` over `dsl` through the query API.
async fn count(server: &TestServer, dsl: &str) -> i64 {
    let dsl = format!("{dsl} | stats count()");
    let result = client(server)
        .query_paginated(&dsl, None, None)
        .await
        .unwrap_or_else(|e| panic!("{dsl}: {e}"));
    match &result.result.rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("{dsl}: count must be an integer, got {other:?}"),
    }
}

/// One authenticated request: its status, its `Retry-After`, its request
/// id and its body as text.
struct Answer {
    status: u16,
    retry_after: Option<String>,
    request_id: String,
    body: String,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

async fn answer(request: reqwest::RequestBuilder) -> Answer {
    let response = request.send().await.unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    let retry_after = header("retry-after");
    let request_id = header("x-request-id").expect("every response carries X-Request-Id");
    let status = response.status().as_u16();
    let body = response.text().await.unwrap();
    Answer {
        status,
        retry_after,
        request_id,
        body,
    }
}

async fn get(server: &TestServer, token: &str, path: &str) -> Answer {
    answer(
        raw_client()
            .get(format!("{}{path}", server.url))
            .bearer_auth(token),
    )
    .await
}

async fn post(server: &TestServer, token: &str, path: &str, body: serde_json::Value) -> Answer {
    answer(
        raw_client()
            .post(format!("{}{path}", server.url))
            .bearer_auth(token)
            .json(&body),
    )
    .await
}

/// `/api/v1/health`: HTTP 200 with `status` and `checks.corpus`.
async fn health(server: &TestServer) -> (String, String) {
    let answer = answer(raw_client().get(format!("{}/api/v1/health", server.url))).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let body = answer.json();
    (
        body["status"].as_str().unwrap().to_owned(),
        body["checks"]["corpus"].as_str().unwrap().to_owned(),
    )
}

/// The value `/metrics` renders for `trawl_corpus_unsettled{reason}`, as
/// the exposition text spells it (`1` or `0`).
async fn corpus_gauge(server: &TestServer, reason: CorpusUnsettled) -> String {
    let answer = answer(raw_client().get(format!("{}/metrics", server.url))).await;
    assert_eq!(answer.status, 200);
    let series = format!(
        "{}{{reason=\"{}\"}} ",
        trawl_server::metrics::CORPUS_UNSETTLED,
        reason.label()
    );
    answer
        .body
        .lines()
        .find_map(|line| Some(line.strip_prefix(&series)?.trim().to_owned()))
        .unwrap_or_else(|| panic!("no {series} in the scrape:\n{}", answer.body))
}

/// A saved query with a schedule attached, owned by the analyst key.
/// Returns the saved query's id and the schedule's.
async fn scheduled_saved_query(server: &TestServer, name: &str) -> (i64, i64) {
    let client = client(server);
    let saved = client
        .create_saved(name, "service=nginx | stats count() by host")
        .await
        .unwrap();
    let schedule = client
        .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
        .await
        .unwrap();
    (saved.id, schedule.id)
}

/// The app-state database, for what no API reports byte for byte.
async fn app_db(server: &TestServer) -> sqlx::PgPool {
    common::fixture_pool(&server.app_db_url, 1).await
}

/// The schedule row, as Postgres renders it: every column, byte for byte.
async fn schedule_row(db: &sqlx::PgPool, schedule_id: i64) -> String {
    sqlx::query_scalar("SELECT row_to_json(s)::text FROM schedules s WHERE id = $1")
        .bind(schedule_id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn run_count(db: &sqlx::PgPool, saved_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM report_runs WHERE saved_query_id = $1")
        .bind(saved_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Assert `answer` is the overhang refusal: 503 `corpus_recovering`, no
/// `Retry-After`, and exactly one `http_failure` for its request, naming
/// `restart_backlog`.
fn assert_restart_backlog_refusal(what: &str, answer: &Answer) {
    assert_eq!(answer.status, 503, "{what}: {}", answer.body);
    assert_eq!(answer.retry_after, None, "{what}: no retry hint");
    assert_eq!(
        answer.json()["error"]["code"],
        "corpus_recovering",
        "{what}: {}",
        answer.body
    );
    let failures = capture().of_type("http_failure", "request_id", &answer.request_id);
    assert_eq!(failures.len(), 1, "{what}: one http_failure: {failures:?}");
    let failure = &failures[0];
    assert_eq!(failure.field("status"), "503", "{what}: {failure:?}");
    assert_eq!(
        failure.field("cause_kind"),
        "\"restart_backlog\"",
        "{what}: {failure:?}"
    );
    assert_eq!(
        failure.field("error_class"),
        "\"service_unavailable\"",
        "{what}: {failure:?}"
    );
}

/// A dry-run repin of a pinned field while the corpus is unsettled.
/// Asserts the job it claimed ended `blocked` with the text that names the
/// reason, and returns the request's answer.
async fn refused_repin(server: &TestServer) -> Answer {
    // Admission reaches the corpus check only for a pinned field. The
    // fixture compacts nothing, so pin the planted one here.
    let seq = [("seq".to_owned(), trawl_core::schema::CanonicalType::BigInt)];
    server
        .state
        .storage
        .catalog
        .pin_missing(&[trawl_server::store::PinProposal {
            field: seq[0].0.clone(),
            ty: seq[0].1,
            pinned_from: SERVICE.to_owned(),
        }])
        .await
        .unwrap();
    server.state.query.field_catalog.merge(seq);

    let answer = post(
        server,
        &server.schema_admin_token,
        "/api/v1/schema/repin",
        serde_json::json!({ "field": "seq", "to": "VARCHAR", "dry_run": true }),
    )
    .await;
    let job = server
        .state
        .repin
        .as_ref()
        .unwrap()
        .store()
        .latest()
        .await
        .unwrap()
        .expect("the refused repin left its job row");
    assert_eq!(
        (job.status, job.error.as_deref()),
        (
            trawl_server::store::RepinJobStatus::Blocked,
            Some(
                "repin is blocked while the server finishes loading data from before its \
                 restart; retry once it settles"
            )
        ),
        "the job row keeps the reason-specific text"
    );
    answer
}

// -- AC6: refusal and release --------------------------------------------------

/// With WAL above the caps and compaction held (the fixture runs none),
/// every corpus read answers 503 `corpus_recovering` with one failure
/// record naming `restart_backlog` and no retry hint, health degrades at
/// HTTP 200, and the gauge reads 1. Compaction's coverage proof then
/// settles the gate: every planted event counts once, the gauge reads 0,
/// and `corpus_settled` is logged once.
#[tokio::test(flavor = "multi_thread")]
async fn overhang_refuses_corpus_reads_until_compaction_settles_it() {
    capture();
    let dir = tempfile::tempdir().unwrap();
    let (server, data) = boot_with_overhang(dir.path(), FITS).await;
    let token = server.analyst_token.clone();
    let (saved_id, _) = scheduled_saved_query(&server, "overhang_manual").await;
    let db = app_db(&server).await;

    let query = serde_json::json!({ "query": format!("service={SERVICE} | stats count()") });
    let refusals = [
        (
            "query",
            post(&server, &token, "/api/v1/query", query.clone()).await,
        ),
        (
            "CSV export",
            post(&server, &token, "/api/v1/export?format=csv", query.clone()).await,
        ),
        (
            "Parquet export",
            post(&server, &token, "/api/v1/export?format=parquet", query).await,
        ),
        (
            "uncached field values",
            get(&server, &token, "/api/v1/schema/values/message").await,
        ),
        (
            "manual run",
            post(
                &server,
                &token,
                &format!("/api/v1/saved/{saved_id}/run"),
                serde_json::json!({}),
            )
            .await,
        ),
        ("repin", refused_repin(&server).await),
    ];
    for (what, answer) in &refusals {
        assert_restart_backlog_refusal(what, answer);
    }
    assert_eq!(
        run_count(&db, saved_id).await,
        0,
        "the manual run made no row"
    );
    assert_eq!(
        health(&server).await,
        ("degraded".to_owned(), "restart_backlog".to_owned())
    );
    assert_eq!(
        corpus_gauge(&server, CorpusUnsettled::RestartBacklog).await,
        "1"
    );
    assert!(
        events_of("corpus_settled").is_empty(),
        "nothing settled while compaction was held"
    );

    settle(&server, dir.path(), &data).await;

    assert_eq!(
        count(&server, &format!("service={SERVICE}")).await,
        i64::try_from(FITS + OVERSIZED).unwrap(),
        "every planted event, once"
    );
    assert_eq!(
        corpus_gauge(&server, CorpusUnsettled::RestartBacklog).await,
        "0"
    );
    assert_eq!(health(&server).await, ("ok".to_owned(), "ok".to_owned()));
    assert_eq!(
        events_of("corpus_settled").len(),
        1,
        "the gate logs its one transition"
    );
}

// -- AC7: lanes the gate does not hold -----------------------------------------

/// The result of the report both servers record.
fn report() -> QueryResult {
    QueryResult {
        columns: vec![
            Column {
                name: "host".into(),
            },
            Column { name: "n".into() },
        ],
        rows: vec![
            vec![Value::String("web-1".into()), Value::Integer(3)],
            vec![Value::String("web-2".into()), Value::Integer(4)],
        ],
    }
}

/// Record a finished run of `name` whose result is [`report`], through the
/// stores the scheduler records with, and the result file where it writes
/// one: the report a `| from saved` read finds after a restart. No run can
/// execute during overhang, so this is how the overhang server gets one.
async fn record_report(server: &TestServer, data: &str, name: &str) {
    let (saved_id, schedule_id) = scheduled_saved_query(server, name).await;
    let store = &server.state.storage.schedule;
    let RunClaim::Started(run_id) = store
        .claim_run(schedule_id, saved_id, "service=nginx", None, None)
        .await
        .unwrap()
    else {
        panic!("the schedule has no run yet");
    };
    let relative = format!("scheduled/run_{run_id}.parquet");
    std::fs::create_dir_all(Path::new(data).join("scheduled")).unwrap();
    let rows = trawl_engine::executor::Executor::new()
        .unwrap()
        .write_query_result_to_parquet(&report(), &Path::new(data).join(&relative))
        .unwrap();
    assert_eq!(
        store
            .finish_run(
                run_id,
                RunStatus::Success,
                1,
                Some(rows),
                None,
                None,
                Some(&relative)
            )
            .await
            .unwrap(),
        FinishOutcome::Persisted
    );
}

/// Read an SSE response until `needle` and the end of its frame arrive.
async fn read_sse_until(response: &mut reqwest::Response, needle: &str) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let chunk = response.chunk().await.unwrap().expect("stream ended early");
            bytes.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&bytes);
            if let Some(at) = text.find(needle)
                && text[at..].contains("\n\n")
            {
                return text.into_owned();
            }
        }
    })
    .await;
    read.unwrap_or_else(|_| {
        panic!(
            "{needle:?} never arrived: {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

/// Tail `service=livetail` over SSE, ingest one event, and return the
/// payload the tail delivered for it.
async fn live_tail_delivers(server: &TestServer) -> serde_json::Value {
    let mut stream = raw_client()
        .get(format!("{}/api/v1/stream", server.url))
        .query(&[("query", "service=livetail | table message")])
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let ingest = trawl_client::HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let accepted = ingest
        .ingest(&[serde_json::json!({"service": "livetail", "message": "tail-needle"})])
        .await
        .unwrap()
        .accepted;
    assert_eq!(accepted, 1);
    let buffer = read_sse_until(&mut stream, "tail-needle").await;
    buffer
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap())
        .find(|payload| payload.to_string().contains("tail-needle"))
        .unwrap()
}

/// Wait for the schema refresh to fill `/schema/services`.
async fn schema_services(server: &TestServer) -> Answer {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let answer = get(server, &server.analyst_token, "/api/v1/schema/services").await;
        if answer.status != 503 || Instant::now() >= deadline {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The columns and rows of `| from saved` [`REPORT`], which the query
/// response flattens beside its execution facts.
async fn from_saved(server: &TestServer) -> (serde_json::Value, serde_json::Value) {
    let query = serde_json::json!({ "query": format!("| from saved {REPORT} | stats sum(n)") });
    let answer = post(server, &server.analyst_token, "/api/v1/query", query).await;
    assert_eq!(answer.status, 200, "| from saved: {}", answer.body);
    let body = answer.json();
    (body["columns"].clone(), body["rows"].clone())
}

/// Fill `from`'s field-value cache for `field` with a real read, and copy
/// the entry that read produced into `to`'s cache.
async fn copy_field_values_cache(from: &TestServer, to: &TestServer, field: &str) {
    let path = format!("/api/v1/schema/values/{field}");
    let filled = get(from, &from.analyst_token, &path).await;
    assert_eq!(filled.status, 200, "{}", filled.body);
    assert_eq!(filled.json()["cached"], false, "a real read of the corpus");
    let entry = from
        .state
        .query
        .field_values_cache
        .lock()
        .await
        .get(field)
        .cloned()
        .expect("the read filled the cache");
    to.state
        .query
        .field_values_cache
        .lock()
        .await
        .insert(field.to_owned(), entry);
}

/// Assert the overhang server answered `what` as the settled one did:
/// the same status and the same JSON body, and a 200.
fn assert_same_answer(what: &str, settled: &Answer, during: &Answer) {
    assert_eq!(settled.status, 200, "{what}: {}", settled.body);
    assert_eq!(
        (during.status, during.json()),
        (settled.status, settled.json()),
        "{what}"
    );
}

/// During overhang, the lanes that take no publication read guard answer
/// exactly as a settled server does: the SSE live tail, `| from saved`,
/// `/schema`, `/schema/services`, `/metrics` and a cached field-values hit.
///
/// The settled server is a control booted beside it, over the same seed,
/// the same caps and the same recorded report, with no WAL to hydrate. The
/// field-value cache is empty at boot and filling it takes a corpus read,
/// which overhang refuses. So the control's real read fills its cache, and
/// the entry it produced is copied into the overhang server's cache: the
/// hit then serves what a settled read put there.
#[tokio::test(flavor = "multi_thread")]
async fn unaffected_lanes_answer_during_overhang_as_on_a_settled_server() {
    capture();
    let control_dir = tempfile::tempdir().unwrap();
    let control_data = seed_data_root(control_dir.path());
    let control = setup_observing_boot(
        control_dir.path(),
        control_data.clone(),
        Some(KNOBS),
        &mut |_| {},
    )
    .await;
    assert_eq!(
        unsettled(&control.state),
        None,
        "no WAL: the control settles"
    );
    // Nothing hydrates, so both hot buffers start empty and the responses
    // that report hot-buffer occupancy agree.
    let dir = tempfile::tempdir().unwrap();
    let (server, data) = boot_with_overhang(dir.path(), 0).await;

    record_report(&control, &control_data, REPORT).await;
    record_report(&server, &data, REPORT).await;
    copy_field_values_cache(&control, &server, "host").await;
    let _refresh = [
        trawl_server::schema_refresh::spawn_schema_refresh(control.state.clone()),
        trawl_server::schema_refresh::spawn_schema_refresh(server.state.clone()),
    ];

    for (what, path) in [
        ("/schema", "/api/v1/schema"),
        ("cached field values", "/api/v1/schema/values/host"),
    ] {
        let settled = get(&control, &control.analyst_token, path).await;
        let during = get(&server, &server.analyst_token, path).await;
        assert_same_answer(what, &settled, &during);
    }
    assert_same_answer(
        "/schema/services",
        &schema_services(&control).await,
        &schema_services(&server).await,
    );

    let settled = from_saved(&control).await;
    assert_eq!(settled.1, serde_json::json!([[7]]), "the recorded report");
    assert_eq!(from_saved(&server).await, settled, "| from saved");

    assert_eq!(
        corpus_gauge(&control, CorpusUnsettled::RestartBacklog).await,
        "0"
    );
    assert_eq!(
        corpus_gauge(&server, CorpusUnsettled::RestartBacklog).await,
        "1",
        "/metrics answers during overhang, and reports it"
    );

    let settled = live_tail_delivers(&control).await;
    assert_eq!(live_tail_delivers(&server).await, settled, "the live tail");

    assert_eq!(
        unsettled(&server.state),
        Some(CorpusUnsettled::RestartBacklog),
        "every lane above answered while the corpus was unsettled"
    );
}

// -- AC11: nets ----------------------------------------------------------------

/// The schedule's first planned fire.
async fn first_fire(db: &sqlx::PgPool, schedule_id: i64) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT next_fire_at FROM schedules WHERE id = $1")
        .bind(schedule_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// One scheduler poll at `now` over the server's stores, awaiting the
/// executions it spawned. Returns how many it spawned.
async fn poll(server: &TestServer, now: DateTime<Utc>) -> usize {
    let key_store = fleet_auth::KeyStore::from_pool(server.fleet_pool.clone());
    let handles = poll_and_execute(
        &server.state.storage.schedule,
        &key_store,
        &server.state.query.pool,
        &trawl_server::config::SchedulerConfig {
            max_catchup_intervals: 24,
            ..trawl_server::config::SchedulerConfig::default()
        },
        common::DEFAULT_TEST_TIMEOUT_SECS,
        truncate_to_micros(now),
    )
    .await;
    let spawned = handles.len();
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }
    spawned
}

/// While the corpus is unsettled a scheduler poll claims nothing: the
/// schedule row, fire cursor and watermark included, is byte-identical
/// before and after, and no run exists. Once compaction settles the
/// corpus, one claim covers every boundary that passed in between.
#[tokio::test(flavor = "multi_thread")]
async fn nets_scheduler_poll_claims_nothing_while_unsettled() {
    capture();
    let dir = tempfile::tempdir().unwrap();
    let (server, data) = boot_with_overhang(dir.path(), FITS).await;
    let (saved_id, schedule_id) = scheduled_saved_query(&server, "overhang_net").await;
    let db = app_db(&server).await;
    let fire = first_fire(&db, schedule_id).await;
    // Three more boundaries pass while the corpus is unsettled.
    let now = fire + chrono::Duration::hours(3) + chrono::Duration::seconds(5);

    let before = schedule_row(&db, schedule_id).await;
    assert_eq!(poll(&server, now).await, 0, "no run spawned");
    assert_eq!(poll(&server, now).await, 0, "nor on the next poll");
    assert_eq!(schedule_row(&db, schedule_id).await, before);
    assert_eq!(run_count(&db, saved_id).await, 0);

    settle(&server, dir.path(), &data).await;

    assert_eq!(poll(&server, now).await, 1, "one claim after settling");
    let runs = server
        .state
        .storage
        .schedule
        .list_runs(saved_id, runs_owner(&server, saved_id).await, 10, 0)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(run.status, RunStatus::Success, "{run:?}");
    assert_eq!(
        (run.window_start, run.window_end, run.window_truncated),
        (
            Some(fire - chrono::Duration::hours(1)),
            Some(fire + chrono::Duration::hours(3)),
            Some(false)
        ),
        "one window from the first uncovered boundary through the last one passed"
    );
    assert_eq!(
        first_fire(&db, schedule_id).await,
        fire + chrono::Duration::hours(4),
        "the cursor resumes one interval past the covered gap"
    );
}

/// The key that owns `saved_id`, which the run listing is scoped by.
async fn runs_owner(server: &TestServer, saved_id: i64) -> i64 {
    sqlx::query_scalar("SELECT key_id FROM saved_queries WHERE id = $1")
        .bind(saved_id)
        .fetch_one(&app_db(server).await)
        .await
        .unwrap()
}

/// While the corpus is unsettled a manual run answers 503
/// `corpus_recovering` before it claims: no run row, and the schedule row
/// is unchanged. After settling the same request starts a run.
#[tokio::test(flavor = "multi_thread")]
async fn nets_manual_run_is_refused_before_it_claims() {
    capture();
    let dir = tempfile::tempdir().unwrap();
    let (server, data) = boot_with_overhang(dir.path(), FITS).await;
    let (saved_id, schedule_id) = scheduled_saved_query(&server, "overhang_manual").await;
    let db = app_db(&server).await;
    let run = format!("/api/v1/saved/{saved_id}/run");

    let before = schedule_row(&db, schedule_id).await;
    let refused = post(&server, &server.analyst_token, &run, serde_json::json!({})).await;
    assert_restart_backlog_refusal("manual run", &refused);
    assert_eq!(run_count(&db, saved_id).await, 0, "no run row");
    assert_eq!(schedule_row(&db, schedule_id).await, before);

    settle(&server, dir.path(), &data).await;

    let started = post(&server, &server.analyst_token, &run, serde_json::json!({})).await;
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(run_count(&db, saved_id).await, 1);
}

/// The scheduler loop logs the corpus state once per change, never once
/// per poll: one pause naming the reason while the corpus is unsettled,
/// one resume after it settles. The schedule is due from the start, and
/// the loop claims it only after the resume.
#[tokio::test(flavor = "multi_thread")]
async fn nets_scheduler_loop_logs_each_corpus_transition_once() {
    capture();
    let dir = tempfile::tempdir().unwrap();
    let (server, data) = boot_with_overhang(dir.path(), FITS).await;
    let (saved_id, _) = scheduled_saved_query(&server, "overhang_loop").await;
    let db = app_db(&server).await;

    let (stop, stopped) = tokio::sync::watch::channel(false);
    let scheduler = spawn_scheduler(
        server.state.storage.schedule.clone(),
        fleet_auth::KeyStore::from_pool(server.fleet_pool.clone()),
        server.state.query.pool.clone(),
        trawl_server::config::SchedulerConfig {
            poll_interval_secs: 1,
            ..trawl_server::config::SchedulerConfig::default()
        },
        common::DEFAULT_TEST_TIMEOUT_SECS,
        stopped,
    );
    // The first tick is immediate; wait out two more.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let paused = events_of("scheduler_paused");
    assert_eq!(paused.len(), 1, "{paused:?}");
    assert_eq!(paused[0].field("reason"), "\"restart_backlog\"");
    assert!(events_of("scheduler_resumed").is_empty());
    assert_eq!(run_count(&db, saved_id).await, 0, "three polls, no claim");

    settle(&server, dir.path(), &data).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while events_of("scheduler_resumed").is_empty() || run_count(&db, saved_id).await == 0 {
        assert!(
            Instant::now() < deadline,
            "the scheduler never resumed and claimed the due run"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(events_of("scheduler_paused").len(), 1);
    assert_eq!(events_of("scheduler_resumed").len(), 1);
    assert_eq!(run_count(&db, saved_id).await, 1, "one claim, then not due");

    stop.send(true).unwrap();
    scheduler.await.unwrap();
}
