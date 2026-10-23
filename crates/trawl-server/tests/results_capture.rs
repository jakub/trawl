// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `POST /api/v1/results` end to end (ADR-0057): the guards a capture
//! passes before it takes anything, the bounds it resolves, and what a
//! finished capture leaves behind.
//!
//! Captures resolve their window at the fixture's results clock, which a
//! test moves onto the corpus it seeded: the shared seed sits on
//! 2024-01-15, the capture corpus (`common::captured::corpus`) on
//! 2024-01-16.
//!
//! One test installs trawld's telemetry as the process's global subscriber
//! and reads its own request's events back; no other test here installs
//! one. The binary is in the ADR-0021 `postgres` nextest group.

mod common;

#[path = "request_limit/support.rs"]
mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use chrono::TimeDelta;
use common::TestServer;
use common::captured::{self, at};
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};
use trawl_api::results::{ResultList, ResultPage, ResultShape, ResultSource, ValueCount};
use trawl_api::value::Value;
use trawl_config::CaptureConfig;
use trawl_server::config::{IngestConfig, RateLimitConfig};
use trawl_server::ingest::producer::Derivation;
use trawl_server::pool::seam::Seam;
use trawl_server::publication::CorpusUnsettled;
use trawl_server::store::{FinishOutcome, RunClaim, RunStatus};
use trawl_server::telemetry::{self, WalHandle, WalLayer};

/// The order an event-shaped capture's recorded text ends with.
const IMPOSED_ORDER: &str = "| sort -_time, -_ingested";

/// The corpus query every capture here runs, unless it says otherwise.
fn corpus_query() -> String {
    format!("service={}", captured::SERVICE)
}

async fn boot() -> TestServer {
    captured::boot(RateLimitConfig::default(), CaptureConfig::default()).await
}

fn permits(server: &TestServer) -> usize {
    server.state.query.pool.available_permits()
}

fn code(body: &serde_json::Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

// -- guards before any work ---------------------------------------------------

/// A key without the `query` permission is refused before the capture takes
/// a permit or leaves a record.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_query_is_refused_before_it_takes_anything() {
    let server = boot().await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    let before = permits(&server);

    let body = json!({ "query": corpus_query(), "window": { "last": "1h" } });
    let (status, _, answer) = captured::capture(&server, &server.ingest_token, &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert_eq!(code(&answer), "forbidden", "{answer}");
    captured::assert_nothing_held(&server, before, "a forbidden capture").await;
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .is_empty()
    );
}

/// A capture is one more request under the ADR-0054 count: with the count
/// full, it is refused at the edge before auth.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_request_count_refuses_a_capture() {
    let server = boot().await;
    let app = server.router_with(|http| http.max_concurrent_requests = 1);

    // One capture whose body the test holds open holds the only slot.
    let release = Arc::new(Semaphore::new(0));
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let gate = Arc::clone(&release);
    let held_body = Body::from_stream(async_stream::stream! {
        let _ = entered_tx.send(());
        if let Ok(permit) = gate.acquire().await {
            permit.forget();
        }
        yield Ok::<_, std::io::Error>(Bytes::from(json!({ "query": corpus_query() }).to_string()));
    });
    let held = support::send(&app, capture_request(&server.analyst_token, held_body));
    tokio::time::timeout(support::DEADLINE, entered_rx.recv())
        .await
        .expect("the held body was polled")
        .expect("the entry channel is open");

    let body = Body::from(json!({ "query": corpus_query() }).to_string());
    support::assert_refused(
        support::call(&app, capture_request(&server.analyst_token, body)).await,
        support::REGULAR_MESSAGE,
    )
    .await;

    release.close();
    let response = support::finish(held).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{:?}",
        support::body_bytes(response).await
    );
}

/// A JSON capture request through the in-process router, with the peer the
/// accept loop would insert.
fn capture_request(token: &str, body: Body) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/results")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap();
    request
        .extensions_mut()
        .insert(SocketAddr::from(([127, 0, 0, 1], 40_307)));
    request
}

/// Events of a service no other test reads, for the WAL a restart left.
const OVERHANG_SERVICE: &str = "captureoverhang";

/// A WAL file in the live writer's format holding `count` events.
fn plant_wal(dir: &Path, count: usize) {
    let time = (chrono::Utc::now() - TimeDelta::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut ndjson = Vec::new();
    for seq in 0..count {
        let event = json!({
            "_time": time,
            "_ingested": time,
            "env": "prod",
            "service": OVERHANG_SERVICE,
            "seq": seq,
        });
        serde_json::to_writer(&mut ndjson, &event).unwrap();
        ndjson.push(b'\n');
    }
    let writer = trawl_server::ingest::wal::WalWriter::new(dir.join("wal"));
    writer.ensure_dir().unwrap();
    writer.write("prod", OVERHANG_SERVICE, &ndjson).unwrap();
}

/// While hydration has not finished, a capture is refused as a corpus read:
/// 503 `corpus_recovering` without a retry hint, and nothing taken.
///
/// The WAL a restart left is larger than the hot buffer holds, and the
/// fixture never runs compaction, so hydration stays unfinished for the
/// whole test: the in-process form of `TRAWL_TEST_HOLD_BOOT_PASS`, which
/// holds only a real trawld's compaction loop.
#[tokio::test(flavor = "multi_thread")]
async fn an_unhydrated_corpus_refuses_a_capture_as_recovering() {
    let tmp = tempfile::tempdir().unwrap();
    let data = common::seed_data_root(tmp.path());
    plant_wal(tmp.path(), 30);
    let knobs = common::HotBufferKnobs {
        max_events: 20,
        max_bytes: 1 << 20,
        compaction_interval_secs: 3600,
    };
    let server = common::setup_observing_boot(tmp.path(), data, Some(knobs), &mut |_| {}).await;
    assert_eq!(
        server.state.query.pool.publication().unsettled(),
        Some(CorpusUnsettled::RestartBacklog),
        "the planted WAL is overhang"
    );
    let before = permits(&server);

    let body = json!({ "query": format!("service={OVERHANG_SERVICE}") });
    let (status, headers, answer) = captured::capture(&server, &server.analyst_token, &body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{answer}");
    assert_eq!(code(&answer), "corpus_recovering", "{answer}");
    assert!(headers.get("retry-after").is_none(), "no retry hint");
    captured::assert_nothing_held(&server, before, "a recovering capture").await;
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .is_empty()
    );
}

/// The per-key rate covers captures: over it, 429 with `Retry-After`.
#[tokio::test(flavor = "multi_thread")]
async fn the_per_key_rate_refuses_a_capture_with_retry_after() {
    let server = captured::boot(
        RateLimitConfig {
            default_rpm: 1,
            ..RateLimitConfig::default()
        },
        CaptureConfig::default(),
    )
    .await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    let body = json!({ "query": corpus_query(), "window": { "last": "1h" } });

    let (status, _, answer) = captured::capture(&server, &server.analyst_token, &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the first capture is in the burst: {answer}"
    );
    let (status, headers, answer) = captured::capture(&server, &server.analyst_token, &body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{answer}");
    assert_eq!(code(&answer), "rate_limited", "{answer}");
    let retry_after: u64 = headers
        .get("retry-after")
        .expect("a rate refusal names its retry")
        .to_str()
        .unwrap()
        .parse()
        .expect("Retry-After is whole seconds");
    assert!(retry_after >= 1, "{retry_after}");
}

// -- bounds -------------------------------------------------------------------

/// `last=1h` resolves once, at the results clock, to microsecond bounds,
/// and the recorded text is what ran: the window spliced in, ending with the
/// imposed newest-first order.
#[tokio::test(flavor = "multi_thread")]
async fn last_resolves_to_microsecond_bounds_and_records_the_imposed_order() {
    let server = boot().await;
    server.results_clock.set(at("2024-01-16T10:30:00.250000Z"));

    let query = format!("{} last=1h", corpus_query());
    let captured = captured::captured(&server, &server.analyst_token, &json!({ "query": query }))
        .await
        .result;
    assert_eq!(captured.query, query);
    assert_eq!(captured.source, ResultSource::Live);
    assert_eq!(captured.shape, ResultShape::Events);
    assert_eq!(
        captured.earliest.as_deref(),
        Some("2024-01-16T09:30:00.250000Z")
    );
    assert_eq!(
        captured.latest.as_deref(),
        Some("2024-01-16T10:30:00.250000Z")
    );
    assert!(
        captured.executed_query.ends_with(IMPOSED_ORDER),
        "{:?}",
        captured.executed_query
    );
    assert!(
        !captured.executed_query.contains("last="),
        "the clause is replaced by its bounds: {:?}",
        captured.executed_query
    );
    assert_eq!(captured.captured_rows, captured::ROWS as u64);
    assert_eq!(captured.total_rows, captured::ROWS as u64);
    assert!(!captured.truncated);
}

/// With no window and no clause, a capture covers the 15 minutes ending at
/// the results clock, lower bound included.
#[tokio::test(flavor = "multi_thread")]
async fn no_window_covers_the_default_fifteen_minutes() {
    let server = boot().await;
    server.results_clock.set(at("2024-01-16T10:15:10Z"));

    let captured = captured::captured(
        &server,
        &server.analyst_token,
        &json!({ "query": corpus_query() }),
    )
    .await
    .result;
    assert_eq!(
        captured.earliest.as_deref(),
        Some("2024-01-16T10:00:10.000000Z")
    );
    assert_eq!(
        captured.latest.as_deref(),
        Some("2024-01-16T10:15:10.000000Z")
    );
    assert_eq!(
        captured.captured_rows, 10,
        "events 10 to 19 are inside the window"
    );
}

/// Bounds a capture cannot honour are a 400 before anything is taken: no
/// permit, no publication guard, no record. The window's refusals are the
/// capture's own; a clause the parser refuses is its `validation_error`,
/// as on `/query`.
#[tokio::test(flavor = "multi_thread")]
async fn refused_bounds_take_nothing() {
    let server = boot().await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    let service = corpus_query();
    let cases = [
        (
            "a window and a clause",
            json!({ "query": format!("{service} last=1h"), "window": { "last": "1h" } }),
            "bad_request",
        ),
        (
            "a window over 7 days",
            json!({ "query": service, "window": { "last": "8d" } }),
            "bad_request",
        ),
        (
            "a clause over 7 days",
            json!({ "query": format!("{service} last=8d") }),
            "bad_request",
        ),
        (
            "a window's latest alone",
            json!({ "query": service, "window": { "latest": "2024-01-16T10:00:00Z" } }),
            "bad_request",
        ),
        (
            "latest= alone",
            json!({ "query": format!("{service} latest=2024-01-16T10:00:00Z") }),
            "validation_error",
        ),
        (
            "a window's unparseable earliest",
            json!({ "query": service, "window": { "earliest": "yesterday" } }),
            "bad_request",
        ),
        (
            "an unparseable earliest=",
            json!({ "query": format!("{service} earliest=yesterday") }),
            "validation_error",
        ),
    ];
    for (what, body, expected) in cases {
        let before = permits(&server);
        let (status, _, answer) = captured::capture(&server, &server.analyst_token, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {answer}");
        assert_eq!(code(&answer), expected, "{what}: {answer}");
        captured::assert_nothing_held(&server, before, what).await;
    }
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .is_empty()
    );
}

/// The store's TEXT columns refuse U+0000, and a capture's text is stored
/// and shown back (ADR-0057), so a request whose `query` carries one is a
/// 400 at the boundary, not a 503 from the insert. Nothing is held, no row
/// is kept and no file is left. A NUL in a `window` string already fails
/// as an unparseable timestamp.
#[tokio::test(flavor = "multi_thread")]
async fn a_nul_in_the_query_text_is_refused_at_the_boundary() {
    let server = boot().await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    let service = captured::SERVICE;
    let names = captured::root_names(&server);
    let cases = [
        (
            "a NUL in a quoted value",
            json!({ "query": format!("service={service} \"a\u{0}b\"") }),
            "the query text contains a NUL character",
        ),
        (
            "a NUL in a bare term",
            json!({ "query": format!("service={service} message=a\u{0}b") }),
            "the query text contains a NUL character",
        ),
        (
            "a NUL beside a window",
            json!({ "query": format!("service={service} \u{0}"), "window": { "last": "1h" } }),
            "the query text contains a NUL character",
        ),
        (
            "a NUL in a window string",
            json!({ "query": corpus_query(), "window": { "earliest": "2024-01-16T10:00:00\u{0}Z" } }),
            "is not an RFC 3339 instant",
        ),
    ];
    for (what, body, message) in cases {
        let before = permits(&server);
        let (status, _, answer) = captured::capture(&server, &server.analyst_token, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {answer}");
        assert_eq!(code(&answer), "bad_request", "{what}: {answer}");
        assert!(
            answer["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains(message)),
            "{what}: {answer}"
        );
        captured::assert_nothing_held(&server, before, what).await;
        assert_eq!(captured::root_names(&server), names, "{what}: no file");
    }
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .is_empty(),
        "no row"
    );
}

/// The saved query whose recorded run the `from saved` capture reads.
const REPORT: &str = "capture_report";

/// Record a finished run of [`REPORT`] whose file holds two rows, through
/// the stores the scheduler records with.
async fn record_report(server: &TestServer, data: &str) {
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let saved = client
        .create_saved(REPORT, "service=nginx | stats count() by host")
        .await
        .unwrap();
    let schedule = client
        .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
        .await
        .unwrap();
    let store = &server.state.storage.schedule;
    let RunClaim::Started(run_id) = store
        .claim_run(schedule.id, saved.id, "service=nginx", None, None)
        .await
        .unwrap()
    else {
        panic!("the schedule has no run yet");
    };
    let data = Path::new(data);
    let relative = format!("scheduled/run_{run_id}.parquet");
    std::fs::create_dir_all(data.join("scheduled")).unwrap();
    let report = trawl_api::value::QueryResult {
        columns: vec![
            trawl_api::value::Column::new("host", trawl_api::value::ColumnType::String),
            trawl_api::value::Column::new("n", trawl_api::value::ColumnType::Integer),
        ],
        rows: vec![
            vec![Value::String("web-1".into()), Value::Integer(3)],
            vec![Value::String("web-2".into()), Value::Integer(4)],
        ],
    };
    let rows = trawl_engine::executor::Executor::new()
        .unwrap()
        .write_query_result_to_parquet(&report, &data.join(&relative))
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

/// A `from saved` capture reads stored report rows: source `stored`, no
/// bounds, the whole result.
#[tokio::test(flavor = "multi_thread")]
async fn from_saved_captures_the_stored_rows_without_bounds() {
    let (server, data) =
        captured::boot_with_data(RateLimitConfig::default(), CaptureConfig::default()).await;
    record_report(&server, &data).await;

    let query = format!("| from saved {REPORT}");
    let captured = captured::captured(&server, &server.analyst_token, &json!({ "query": query }))
        .await
        .result;
    assert_eq!(captured.source, ResultSource::Stored);
    assert_eq!(captured.shape, ResultShape::Whole);
    assert_eq!(captured.earliest, None);
    assert_eq!(captured.latest, None);
    assert_eq!(captured.executed_query, query, "nothing is spliced in");
    assert_eq!(captured.captured_rows, 2);
    assert!(!captured.truncated);
}

// -- a finished capture -------------------------------------------------------

type Row = BTreeMap<String, Value>;

/// Install trawld's subscriber shape as the global one, as
/// `query_timing_roundtrip.rs` does: one filter, a WAL sink, no other sink.
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
            stdout_ansi: false,
            stdout: None::<fn() -> std::io::Sink>,
            wal: Some(layer.clone()),
            file_log: false,
        },
    );
    tracing::dispatcher::set_global_default(dispatch)
        .expect("this test installs its process's only global subscriber");
    (handle, layer)
}

/// `/api/v1/query` over `dsl`: its typed result.
async fn query(server: &TestServer, dsl: &str) -> trawl_api::value::QueryResult {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
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
    response
        .json::<trawl_api::QueryResponse>()
        .await
        .unwrap()
        .result
}

/// A query's rows as maps from column name to value.
fn rows(result: trawl_api::value::QueryResult) -> Vec<Row> {
    let names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    result
        .rows
        .into_iter()
        .map(|row| names.iter().cloned().zip(row).collect())
        .collect()
}

/// A capture of the 20-event corpus keeps every row, reads back as
/// `/query` answers its recorded text, shows in history and the query
/// list as the caller's, and books exactly one `query_timing` of kind
/// `capture` with its file write timed.
#[tokio::test(flavor = "multi_thread")]
async fn a_capture_reads_back_as_query_answers_and_is_accounted_once() {
    let (handle, telemetry) = install_telemetry();
    let server = boot().await;
    let defaults = IngestConfig::default();
    handle.set(
        Arc::clone(server.state.ingest.wal_writer.as_ref().unwrap()),
        &defaults.default_env,
    );
    telemetry.set_hot_buffer(Arc::clone(server.state.query.hot_buffer.as_ref().unwrap()));
    server.results_clock.set(at("2024-01-16T10:30:00Z"));

    let body = json!({ "query": corpus_query(), "window": { "last": "1h" } });
    let (status, headers, answer) = captured::capture(&server, &server.analyst_token, &body).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let request_id = headers
        .get("x-request-id")
        .expect("every response carries its request id")
        .to_str()
        .unwrap()
        .to_owned();
    let response: trawl_api::results::CaptureResponse = serde_json::from_value(answer).unwrap();
    let result = response.result;
    assert_eq!(result.captured_rows, captured::ROWS as u64);
    assert_eq!(result.total_rows, captured::ROWS as u64);
    assert!(!result.truncated);
    assert!(result.file_bytes > 0);
    assert!(
        response.execution.is_some(),
        "a live capture reports its run"
    );

    // The page is what `/query` answers for the recorded text.
    let (status, _, page) = captured::page(&server, &server.analyst_token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let page: trawl_api::results::ResultPage = serde_json::from_value(page).unwrap();
    assert_eq!(page.pagination.returned, captured::ROWS);
    assert_eq!(page.pagination.total, captured::ROWS);
    let live = query(&server, &result.executed_query).await;
    assert_eq!(page.page.columns, live.columns);
    assert_eq!(page.page.rows, live.rows);

    // History and the query list record the text that ran, as the caller's.
    let (status, _, history) = captured::get(&server, &server.analyst_token, "/history").await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let history: trawl_api::HistoryResponse = serde_json::from_value(history).unwrap();
    let entry = history
        .entries
        .iter()
        .find(|e| e.query == result.executed_query)
        .unwrap_or_else(|| panic!("the capture is in history: {history:?}"));
    assert_eq!(entry.row_count, captured::ROWS);
    let (status, _, queries) = captured::get(&server, &server.analyst_token, "/queries").await;
    assert_eq!(status, StatusCode::OK, "{queries}");
    let queries: trawl_api::QueriesResponse = serde_json::from_value(queries).unwrap();
    let recent = queries
        .recent
        .iter()
        .find(|q| q.snapshot.query == result.executed_query)
        .unwrap_or_else(|| panic!("the capture is a recent query: {queries:?}"));
    assert!(recent.own, "the caller's own capture");
    assert_eq!(recent.snapshot.rows, Some(captured::ROWS));

    // One timing account, of kind capture, with the write phase booked.
    telemetry.flush_cycle().await;
    let found = rows(
        query(
            &server,
            &format!(
                "service=trawld _producer=trawld event_type=query_timing \
                 request_id=\"{request_id}\" | table kind, query_write_us"
            ),
        )
        .await,
    );
    assert_eq!(found.len(), 1, "one account for the capture: {found:?}");
    assert_eq!(
        found[0].get("kind"),
        Some(&Value::String("capture".into())),
        "{found:?}"
    );
    assert!(
        matches!(
            found[0].get("query_write_us"),
            Some(Value::Integer(_) | Value::UInt(_))
        ),
        "the file write is timed: {found:?}"
    );
}

/// A selection whose stored files pass `source_max_bytes` is refused with
/// 400 `capture_source_too_large` before the work starts; one file of it
/// alone fits.
#[tokio::test(flavor = "multi_thread")]
async fn a_selection_over_the_source_budget_is_refused_before_work_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let data = common::seed_data_root(tmp.path());
    let hour = Path::new(&data).join("prod/2024-01-15/10");
    let sizes: Vec<u64> = ["nginx", "postgres"]
        .iter()
        .map(|service| {
            std::fs::metadata(hour.join(format!("{service}.parquet")))
                .unwrap()
                .len()
        })
        .collect();
    // Each file fits on its own; the two together do not.
    let budget = *sizes.iter().max().unwrap();
    assert!(sizes.iter().sum::<u64>() > budget);
    let server = common::setup_with_capture(
        tmp.path(),
        data,
        RateLimitConfig::default(),
        CaptureConfig {
            source_max_bytes: budget,
            ..CaptureConfig::default()
        },
    )
    .await;
    std::mem::forget(tmp);
    server.results_clock.set(at("2024-01-15T10:30:00Z"));
    let seams = server.state.query.pool.seams();
    let started = seams.watch(Seam::Started);
    let before = permits(&server);

    let body = json!({ "query": "*", "window": { "last": "1h" } });
    let (status, _, answer) = captured::capture(&server, &server.analyst_token, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(code(&answer), "capture_source_too_large", "{answer}");
    assert_eq!(started.arrivals(), 0, "the work never started");
    captured::assert_nothing_held(&server, before, "a capture over its source budget").await;

    let body = json!({ "query": "service=nginx", "window": { "last": "1h" } });
    let captured = captured::captured(&server, &server.analyst_token, &body)
        .await
        .result;
    assert_eq!(captured.captured_rows, 2, "one file fits the budget");
    assert_eq!(started.arrivals(), 1);
}

/// The page of a capture of `window`, and what `/query` answers for its
/// recorded text right after.
async fn captured_and_live(
    server: &TestServer,
    dsl: &str,
    window: &serde_json::Value,
) -> (
    trawl_api::results::ResultPage,
    trawl_api::value::QueryResult,
) {
    let body = json!({ "query": dsl, "window": window });
    let result = captured::captured(server, &server.analyst_token, &body)
        .await
        .result;
    let (status, _, page) = captured::page(server, &server.analyst_token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let page = serde_json::from_value(page).unwrap();
    (page, query(server, &result.executed_query).await)
}

/// A capture reads what `/query` reads for the text it recorded, whatever
/// partition holds the event. Compaction names a partition by when it
/// wrote it, and ingest keeps the `_time` a sender says, so an event's
/// partition does not bound its `_time` (ADR-0057):
///
/// - a sender stamping six hours ahead: its event is compacted now, into
///   this hour's partition, and drained from the hot buffer, and the
///   capture covers the hour around its `_time`;
/// - a sender whose clock ran two days ahead: its partition was written
///   two days ago, and its `_time` is ten minutes ago, inside a capture of
///   the last half hour.
#[tokio::test(flavor = "multi_thread")]
async fn a_capture_reads_what_query_reads_whatever_partition_holds_the_event() {
    let now = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap();
    let ahead = now + TimeDelta::hours(6);
    let skewed = now - TimeDelta::minutes(10);
    let written = (now - TimeDelta::days(2))
        .format("prod/%Y-%m-%d/%H")
        .to_string();
    let rfc3339 = |at: chrono::DateTime<chrono::Utc>| at.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let (fixture, server) = captured::Restartable::boot(
        RateLimitConfig::default(),
        CaptureConfig::default(),
        |data| {
            captured::seed_partition(
                data,
                &written,
                "skewed",
                &[captured::Event {
                    time: skewed,
                    host: "web08".into(),
                    message: "two days ahead".into(),
                }],
            );
        },
    )
    .await;
    captured::ingest(
        &server,
        &[json!({
            "_time": rfc3339(ahead),
            "service": "ahead",
            "host": "web09",
            "message": "six hours ahead",
        })],
    )
    .await;
    captured::compact(&server, &fixture.disk).await;
    assert_eq!(
        server
            .state
            .query
            .hot_buffer
            .as_ref()
            .unwrap()
            .event_count(),
        0,
        "the event is stored, not hot"
    );

    for (dsl, window, message) in [
        (
            "service=ahead",
            json!({
                "earliest": rfc3339(ahead - TimeDelta::minutes(30)),
                "latest": rfc3339(ahead + TimeDelta::minutes(30)),
            }),
            "six hours ahead",
        ),
        (
            "service=skewed",
            json!({
                "earliest": rfc3339(now - TimeDelta::minutes(30)),
                "latest": rfc3339(now),
            }),
            "two days ahead",
        ),
    ] {
        let (page, live) = captured_and_live(&server, dsl, &window).await;
        let found = rows(live.clone());
        assert_eq!(found.len(), 1, "{dsl}: `/query` finds the event");
        assert_eq!(
            found[0].get("message"),
            Some(&Value::String(message.into())),
            "{dsl}: {found:?}"
        );
        assert_eq!(page.pagination.total, 1, "{dsl}: the capture found it");
        assert_eq!(page.page.columns, live.columns, "{dsl}");
        assert_eq!(page.page.rows, live.rows, "{dsl}");
    }
}

/// A one-hour window in year 1 is selected from the partitions on disk,
/// never by walking every hour of the calendar from then to now: the
/// capture answers well inside its deadline and finds nothing, and so does
/// `/query` of the recorded text.
#[tokio::test(flavor = "multi_thread")]
async fn an_ancient_window_is_selected_from_the_partitions_on_disk() {
    let server = boot().await;
    let body = json!({
        "query": corpus_query(),
        "window": {
            "earliest": "0001-01-01T00:00:00Z",
            "latest": "0001-01-01T01:00:00Z",
        },
    });

    let started = std::time::Instant::now();
    let (status, _, answer) = captured::capture(&server, &server.analyst_token, &body).await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK, "after {elapsed:?}: {answer}");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the capture took {elapsed:?}"
    );
    let result: trawl_api::results::CaptureResponse = serde_json::from_value(answer).unwrap();
    assert_eq!(result.result.captured_rows, 0);

    let started = std::time::Instant::now();
    let live = query(&server, &result.result.executed_query).await;
    let elapsed = started.elapsed();
    assert!(live.rows.is_empty(), "{live:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "`/query` took {elapsed:?}"
    );
}

/// A window with no events, over a corpus that knows no field yet,
/// captures no columns: 200, no file, and an empty page.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_corpus_captures_no_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let server = common::setup_with_capture(
        tmp.path(),
        data.to_str().unwrap().to_owned(),
        RateLimitConfig::default(),
        CaptureConfig::default(),
    )
    .await;
    std::mem::forget(tmp);

    let captured = captured::captured(
        &server,
        &server.analyst_token,
        &json!({ "query": corpus_query() }),
    )
    .await
    .result;
    assert!(captured.columns.is_empty(), "{captured:?}");
    assert_eq!(captured.file_bytes, 0);
    assert_eq!(captured.captured_rows, 0);
    assert!(!captured.truncated);

    let (status, _, page) = captured::page(&server, &server.analyst_token, &captured.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let page: trawl_api::results::ResultPage = serde_json::from_value(page).unwrap();
    assert!(page.positions.is_empty());
    assert!(page.page.columns.is_empty());
    assert!(page.page.rows.is_empty());
}

/// An empty window over a corpus with fields answers their columns and no
/// rows, and the capture keeps a file of the columns.
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_window_captures_its_columns_and_no_rows() {
    let server = boot().await;
    server.results_clock.set(at("2024-01-10T00:00:00Z"));

    let captured = captured::captured(
        &server,
        &server.analyst_token,
        &json!({ "query": corpus_query(), "window": { "last": "1h" } }),
    )
    .await
    .result;
    assert!(!captured.columns.is_empty(), "{captured:?}");
    assert_eq!(captured.captured_rows, 0);
    assert!(captured.file_bytes > 0, "the columns are stored");

    let (status, _, page) = captured::page(&server, &server.analyst_token, &captured.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let page: trawl_api::results::ResultPage = serde_json::from_value(page).unwrap();
    assert_eq!(page.page.columns.len(), captured.columns.len());
    assert!(page.page.rows.is_empty());
}

/// A string holding U+0000 is a value like any other. An event whose
/// message carries one is ingested and compacted, then captured: its
/// column facts list the value, the list answers the capture as it was
/// committed, and a page reads the value back.
#[tokio::test(flavor = "multi_thread")]
async fn a_value_holding_nul_is_captured_listed_and_read_back() {
    const MESSAGE: &str = "a\u{0}b";
    let (fixture, server) =
        captured::Restartable::boot(RateLimitConfig::default(), CaptureConfig::default(), |_| {})
            .await;
    captured::ingest(
        &server,
        &[json!({
            "_time": "2024-01-16T10:05:00Z",
            "service": "nulcorpus",
            "host": "web09",
            "message": MESSAGE,
        })],
    )
    .await;
    captured::compact(&server, &fixture.disk).await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));

    let body = json!({
        "query": "service=nulcorpus",
        "window": { "earliest": "2024-01-16T10:00:00Z", "latest": "2024-01-16T10:10:00Z" },
    });
    let result = captured::captured(&server, &server.analyst_token, &body)
        .await
        .result;
    assert_eq!(result.captured_rows, 1, "{result:?}");
    let message = result
        .columns
        .iter()
        .position(|column| column.name == "message")
        .expect("a message column");
    assert_eq!(
        result.column_facts[message].values.as_deref(),
        Some(
            &[ValueCount {
                value: Value::String(MESSAGE.to_owned()),
                count: 1,
            }][..]
        ),
        "the facts list the value"
    );

    let (status, _, list) = captured::get(&server, &server.analyst_token, "/results").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let list: ResultList = serde_json::from_value(list).unwrap();
    assert_eq!(
        list.results,
        std::slice::from_ref(&result),
        "listed as it was committed"
    );

    let (status, _, page) = captured::page(&server, &server.analyst_token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let page: ResultPage = serde_json::from_value(page).unwrap();
    assert_eq!(page.result, result);
    assert_eq!(
        page.page.rows[0][message],
        Value::String(MESSAGE.to_owned())
    );
}

/// A stage that keeps every corpus event and keeps the result events.
const KEEP_STAGE: &str = " | where severity >= 0";

/// The corpus query with `stages` copies of [`KEEP_STAGE`], then `tail`.
fn staged(stages: usize, tail: &str) -> String {
    format!("{}{}{tail}", corpus_query(), KEEP_STAGE.repeat(stages))
}

/// A capture of `query` over the last hour.
fn last_hour(query: &str) -> serde_json::Value {
    json!({ "query": query, "window": { "last": "1h" } })
}

/// `/api/v1/query` over `dsl`, answered with a refusal: its status and body.
async fn query_refusal(server: &TestServer, dsl: &str) -> (StatusCode, serde_json::Value) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&json!({ "query": dsl }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

/// The stage cap (ADR-0024, 128 stages) counts the text that runs, and an
/// events capture runs its query with the server's order appended, so the
/// query may carry 127 stages. One with 128 is the capture's own 400,
/// naming its limit, and one with 129 is the refusal `/query` gives it,
/// from the check of the text as sent. A whole-shaped capture appends
/// nothing and takes 128. No refusal takes a permit.
#[tokio::test(flavor = "multi_thread")]
async fn an_events_capture_keeps_one_stage_for_its_order() {
    let server = boot().await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    let analyst = &server.analyst_token;

    let fits = captured::captured(&server, analyst, &last_hour(&staged(127, "")))
        .await
        .result;
    assert_eq!(fits.shape, ResultShape::Events);
    assert!(fits.executed_query.ends_with(IMPOSED_ORDER));
    assert_eq!(fits.captured_rows, captured::ROWS as u64);

    let before = permits(&server);
    let (status, _, answer) =
        captured::capture(&server, analyst, &last_hour(&staged(128, ""))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(code(&answer), "bad_request", "{answer}");
    assert_eq!(
        answer["error"]["message"],
        "this query has 128 pipeline stages, over the limit of 127 for a capture of events, \
         which ends with its own `| sort -_time, -_ingested` stage; shorten the pipeline",
    );
    captured::assert_nothing_held(&server, before, "128 stages of events").await;

    let over = staged(129, "");
    let (status, _, answer) = captured::capture(&server, analyst, &last_hour(&over)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(code(&answer), "validation_error", "{answer}");
    let (query_status, query_answer) = query_refusal(&server, &over).await;
    assert_eq!(query_status, StatusCode::BAD_REQUEST, "{query_answer}");
    assert_eq!(
        answer["error"]["message"], query_answer["error"]["message"],
        "the refusal /query gives"
    );
    captured::assert_nothing_held(&server, before, "129 stages of events").await;

    let whole = captured::captured(
        &server,
        analyst,
        &last_hour(&staged(127, " | stats count()")),
    )
    .await
    .result;
    assert_eq!(whole.shape, ResultShape::Whole);
    assert!(!whole.executed_query.ends_with(IMPOSED_ORDER));
    assert_eq!(whole.captured_rows, 1);
}
