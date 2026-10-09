// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every server 5xx emits exactly one `http_failure` event that names its
//! request, route, stage and cause, and nothing the caller or an error
//! wrote (ADR-0040).
//!
//! Its own test binary because `.init()` installs the global subscriber
//! and the global `log` bridge, exactly as `trawld` does: the production
//! subscriber from `telemetry::build_subscriber`, with a stdout capture and
//! a WAL layer. Tests tell their events apart by request id.
//!
//! Two kinds of request: through a booted server (real TLS, postgres,
//! `DuckDB` and data directory), and through [`with_edge_layers`] around a
//! route of the test's own, for failures no production route can be made
//! to produce on demand (a bare 500, a handler panic, a 502).

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{any, get};
use tower::ServiceExt as _;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_server::config::RateLimitConfig;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::seam::{INJECTED_PANIC_PAYLOAD, Seam};
use trawl_server::state::HttpConfig;
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};
use trawl_server::transport::failure::{FAILURE_TARGET, UNMETERED_FAILURE_TARGET, mark_handler};
use trawl_server::transport::http::with_edge_layers;

const ENV: &str = "default";

// -- the production subscriber, captured ------------------------------------

/// Everything the global subscriber writes to stdout.
#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Stdout {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct Sinks {
    stdout: Stdout,
    wal: WalLayer,
    wal_dir: tempfile::TempDir,
}

/// The production subscriber under the default directives, installed once
/// for the whole binary.
fn sinks() -> &'static Sinks {
    static SINKS: OnceLock<Sinks> = OnceLock::new();
    SINKS.get_or_init(|| {
        let wal_dir = tempfile::tempdir().unwrap();
        let handle = WalHandle::new();
        handle.set(Arc::new(WalWriter::new(wal_dir.path().to_path_buf())), ENV);
        let wal = WalLayer::new_with_buffer_cap(
            handle,
            &[ENV.to_owned()],
            ENV,
            Arc::new(Derivation::defaults()),
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
        );
        let stdout = Stdout::default();
        let (subscriber, _) = telemetry::build_subscriber(
            telemetry::DEFAULT_LOG_FILTER,
            LogSinks {
                stdout_ansi: false,
                stdout: Some(stdout.clone()),
                wal: Some(wal.clone()),
                file_log: false,
            },
        );
        subscriber.init();
        Sinks {
            stdout,
            wal,
            wal_dir,
        }
    })
}

/// Every stdout line so far.
fn stdout_lines() -> Vec<String> {
    let bytes = sinks().stdout.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A field's value on a formatted stdout line: `name="quoted"` or
/// `name=bare`.
fn field(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!(" {name}="))? + name.len() + 2;
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| quoted[..end].to_owned())
    } else {
        Some(rest.split_whitespace().next().unwrap_or("").to_owned())
    }
}

/// Every `http_failure` stdout line.
fn stdout_failures() -> Vec<String> {
    stdout_lines()
        .into_iter()
        .filter(|line| field(line, "event_type").as_deref() == Some("http_failure"))
        .collect()
}

/// The `http_failure` stdout lines for one request.
fn stdout_failures_for(request_id: &str) -> Vec<String> {
    stdout_failures()
        .into_iter()
        .filter(|line| field(line, "request_id").as_deref() == Some(request_id))
        .collect()
}

/// Every record the telemetry WAL holds so far.
fn wal_records() -> Vec<serde_json::Value> {
    // Tests flush concurrently. Without this lock one test could find the
    // queue already drained by another whose WAL write has not landed yet.
    static FLUSH: Mutex<()> = Mutex::new(());
    let _flush = FLUSH.lock().unwrap();
    sinks().wal.flush();
    read_wal(&sinks().wal_dir.path().join(ENV))
}

fn read_wal(dir: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ndjson"))
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Every persisted `http_failure` record.
fn wal_failures() -> Vec<serde_json::Value> {
    wal_records()
        .into_iter()
        .filter(|record| record["event_type"] == "http_failure")
        .collect()
}

/// The persisted `http_failure` records for one request.
fn wal_failures_for(request_id: &str) -> Vec<serde_json::Value> {
    wal_failures()
        .into_iter()
        .filter(|record| record["request_id"] == request_id)
        .collect()
}

/// Assert no `http_failure` line or record, on either sink, holds any of
/// `sentinels`.
fn assert_no_failure_carries(sentinels: &[&str]) {
    let lines = stdout_failures();
    let records: Vec<String> = wal_failures().iter().map(ToString::to_string).collect();
    assert!(!lines.is_empty(), "no http_failure reached stdout at all");
    for sentinel in sentinels {
        for line in &lines {
            assert!(
                !line.contains(sentinel),
                "{sentinel} reached a stdout http_failure: {line}"
            );
        }
        for record in &records {
            assert!(
                !record.contains(sentinel),
                "{sentinel} reached a persisted http_failure: {record}"
            );
        }
    }
}

// -- requests -----------------------------------------------------------------

/// A client that adds the shared test certificate, whose SAN covers
/// `127.0.0.1` and `localhost`, to its trusted roots, with validation on.
fn raw_client() -> reqwest::Client {
    let (cert_path, _) = common::ensure_test_cert();
    let certificate = reqwest::Certificate::from_pem(&std::fs::read(cert_path).unwrap()).unwrap();
    reqwest::Client::builder()
        .add_root_certificate(certificate)
        .build()
        .unwrap()
}

fn request_id_of(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get("x-request-id")
        .expect("every response carries X-Request-Id")
        .to_str()
        .unwrap()
        .to_owned()
}

/// The HTTP config the fixture server runs with, for the edge-layer tests.
fn http_config() -> HttpConfig {
    HttpConfig {
        max_request_body_bytes: 128 * 1024,
        max_concurrent_requests: 256,
        shutdown_drain_secs: 5,
        cors_allowed_origins: vec![],
        ingest_max_body_bytes: None,
        rate_limit: RateLimitConfig::default(),
    }
}

const HANDLER_PANIC_SENTINEL: &str = "zz-handler-panic-sentinel";

/// A handler that panics with a sentinel payload.
async fn panicking_handler() -> StatusCode {
    panic!("{HANDLER_PANIC_SENTINEL}")
}

/// Test routes under the production edge layers. Every route but
/// `/bare-unmarked` is marked the way production marks its handlers;
/// `/bare-unmarked` is added after the marker, so a 5xx from it never got
/// past admission.
fn edge_app() -> Router {
    let app = Router::new()
        .route("/bare", any(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
        .route(
            "/status/{code}",
            get(
                |axum::extract::Path(code): axum::extract::Path<u16>| async move {
                    StatusCode::from_u16(code).unwrap()
                },
            ),
        )
        .route(
            "/items/{id}",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        )
        .route("/panic", get(panicking_handler))
        .route_layer(axum::middleware::from_fn(mark_handler))
        .route(
            "/bare-unmarked",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
    with_edge_layers(app, &http_config())
}

/// Send one request through the edge app; answer its response.
async fn edge_call(request: Request<Body>) -> axum::response::Response {
    sinks();
    edge_app().oneshot(request).await.unwrap()
}

fn edge_request_id(response: &axum::response::Response) -> String {
    response
        .headers()
        .get("x-request-id")
        .expect("every response carries X-Request-Id")
        .to_str()
        .unwrap()
        .to_owned()
}

/// The level a formatted stdout line was logged at.
fn level(line: &str) -> &'static str {
    ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
        .into_iter()
        .find(|level| line.split_whitespace().any(|word| word == *level))
        .unwrap_or("none")
}

/// Plant an unreadable parquet file over the `nginx` fixture, so a query
/// over it fails inside `DuckDB`.
fn corrupt_nginx(dir: &Path) {
    let file = dir
        .join("data")
        .join("prod")
        .join("2024-01-15")
        .join("10")
        .join("nginx.parquet");
    assert!(file.is_file(), "the fixture moved: {}", file.display());
    std::fs::write(&file, b"zz-not-a-parquet-file").unwrap();
}

// -- AC1 ------------------------------------------------------------------------

/// A real `DuckDB` failure on `POST /api/v1/query` is one persisted
/// `http_failure` that names the request, the route template, the
/// handler-error stage, the engine's class and cause, and the query id the
/// request's own `query_failed` carries.
#[tokio::test(flavor = "multi_thread")]
async fn duckdb_failure_names_request_route_stage_cause_and_query_id() {
    sinks();
    let tmp = tempfile::tempdir().unwrap();
    let server = common::setup_in_dir(tmp.path(), RateLimitConfig::default()).await;
    corrupt_nginx(tmp.path());

    let response = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "service=nginx" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500, "{:?}", response.text().await);
    let request_id = request_id_of(response.headers());

    let failures = wal_failures_for(&request_id);
    assert_eq!(failures.len(), 1, "one persisted failure: {failures:?}");
    let failure = &failures[0];
    assert_eq!(failure["target"], FAILURE_TARGET);
    assert_eq!(failure["level"], "error");
    assert_eq!(failure["method"], "POST");
    assert_eq!(failure["route"], "/api/v1/query");
    assert_eq!(failure["status"], 500);
    assert_eq!(failure["stage"], "handler_error");
    assert!(failure.get("reached").is_none(), "{failure}");
    assert_eq!(failure["error_class"], "database");
    assert_eq!(failure["cause_kind"], "duckdb_failure");
    assert!(failure["latency_ms"].is_number(), "{failure}");
    assert!(failure["key_id"].is_number(), "{failure}");
    assert!(
        failure.get("peer_addr").is_none(),
        "a metered failure names its key, not its peer: {failure}"
    );
    // Explicit fields only: nothing inherited from the request span.
    for inherited in ["path", "user_agent"] {
        assert!(failure.get(inherited).is_none(), "{inherited}: {failure}");
    }

    let query_failed: Vec<_> = wal_records()
        .into_iter()
        .filter(|record| record["event_type"] == "query_failed")
        .filter(|record| record["request_id"] == request_id.as_str())
        .collect();
    assert_eq!(query_failed.len(), 1, "{query_failed:?}");
    assert!(query_failed[0]["query_id"].is_number());
    assert_eq!(failure["query_id"], query_failed[0]["query_id"]);

    assert_eq!(stdout_failures_for(&request_id).len(), 1);
}

// -- AC2 ------------------------------------------------------------------------

/// A 5xx whose producer recorded nothing still emits one `http_failure`,
/// with the request id, the route template, the stage that says so, and
/// how far the request got: here, the handler.
#[tokio::test]
async fn a_bare_500_is_logged_as_unrecorded() {
    let response = edge_call(Request::get("/bare").body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let request_id = edge_request_id(&response);

    let lines = stdout_failures_for(&request_id);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert_eq!(level(line), "ERROR", "{line}");
    assert!(line.contains(UNMETERED_FAILURE_TARGET), "{line}");
    assert_eq!(field(line, "route").as_deref(), Some("/bare"), "{line}");
    assert_eq!(field(line, "method").as_deref(), Some("GET"), "{line}");
    assert_eq!(field(line, "status").as_deref(), Some("500"), "{line}");
    assert_eq!(
        field(line, "stage").as_deref(),
        Some("unrecorded"),
        "{line}"
    );
    assert_eq!(
        field(line, "error_class").as_deref(),
        Some("unknown"),
        "{line}"
    );
    assert_eq!(field(line, "cause_kind").as_deref(), Some("none"), "{line}");
    assert_eq!(field(line, "reached").as_deref(), Some("handler"), "{line}");
    assert_eq!(field(line, "query_id"), None, "{line}");

    // Unmetered, and well under the cap: persisted with the same fields.
    let records = wal_failures_for(&request_id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["target"], UNMETERED_FAILURE_TARGET);
    assert_eq!(records[0]["stage"], "unrecorded");
    assert_eq!(records[0]["reached"], "handler");
}

/// An unrecorded 5xx from a route no marker reached says so: it never got
/// past admission.
#[tokio::test]
async fn an_unrecorded_500_before_the_handler_reached_pre_admission() {
    let response = edge_call(Request::get("/bare-unmarked").body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let request_id = edge_request_id(&response);

    let lines = stdout_failures_for(&request_id);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert_eq!(
        field(line, "stage").as_deref(),
        Some("unrecorded"),
        "{line}"
    );
    assert_eq!(
        field(line, "reached").as_deref(),
        Some("pre_admission"),
        "{line}"
    );
}

// -- AC3 ------------------------------------------------------------------------

/// A handler panic caught by the outer catcher answers the same 500 as
/// ever, under the same hardening headers, and emits one `http_failure`
/// with the panic stage. The payload reaches neither sink.
#[tokio::test]
async fn a_caught_handler_panic_records_the_panic_stage() {
    let response = edge_call(Request::get("/panic").body(Body::empty()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let request_id = edge_request_id(&response);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(headers.contains_key("strict-transport-security"));
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"Service panicked");

    let lines = stdout_failures_for(&request_id);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert_eq!(level(line), "ERROR", "{line}");
    assert_eq!(field(line, "route").as_deref(), Some("/panic"), "{line}");
    assert_eq!(field(line, "stage").as_deref(), Some("panicked"), "{line}");
    assert_eq!(
        field(line, "reached"),
        None,
        "only unrecorded events: {line}"
    );
    assert_eq!(
        field(line, "error_class").as_deref(),
        Some("panic"),
        "{line}"
    );

    assert!(
        stdout_lines()
            .iter()
            .all(|line| !line.contains(HANDLER_PANIC_SENTINEL)),
        "the panic payload reached stdout"
    );
    assert!(
        wal_records()
            .iter()
            .all(|record| !record.to_string().contains(HANDLER_PANIC_SENTINEL)),
        "the panic payload reached the WAL"
    );
}

/// A panic caught inside the executor pool reaches the query handler as a
/// typed error, and the request's one `http_failure` records the panic
/// stage. The payload reaches neither sink.
#[tokio::test(flavor = "multi_thread")]
async fn a_pool_panic_records_the_panic_stage() {
    sinks();
    let server = common::setup().await;
    let seams = server.state.query.pool.seams();
    let _panic = seams.panic_at(Seam::Started);

    let response = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "*" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    let request_id = request_id_of(response.headers());

    let failures = wal_failures_for(&request_id);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["route"], "/api/v1/query");
    assert_eq!(failures[0]["stage"], "panicked");
    assert_eq!(failures[0]["error_class"], "panic");
    assert!(failures[0]["query_id"].is_number(), "{}", failures[0]);
    assert_eq!(stdout_failures_for(&request_id).len(), 1);

    assert!(
        stdout_lines()
            .iter()
            .all(|line| !line.contains(INJECTED_PANIC_PAYLOAD)),
        "the panic payload reached stdout"
    );
    assert!(
        wal_records()
            .iter()
            .all(|record| !record.to_string().contains(INJECTED_PANIC_PAYLOAD)),
        "the panic payload reached the WAL"
    );
}

// -- AC7 and one-per-5xx -----------------------------------------------------------

/// 503 and 504 are expected pressure outcomes and log at WARN; 500 and
/// every other 5xx log at ERROR.
#[tokio::test]
async fn five_xx_levels_map_503_504_to_warn() {
    for (code, expected) in [
        (500, "ERROR"),
        (502, "ERROR"),
        (503, "WARN"),
        (504, "WARN"),
        (507, "ERROR"),
    ] {
        let response = edge_call(
            Request::get(format!("/status/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status().as_u16(), code);
        let request_id = edge_request_id(&response);
        let lines = stdout_failures_for(&request_id);
        assert_eq!(lines.len(), 1, "{code}: {lines:?}");
        assert_eq!(level(&lines[0]), expected, "{code}: {}", lines[0]);
        assert_eq!(
            field(&lines[0], "route").as_deref(),
            Some("/status/{code}"),
            "{}",
            lines[0]
        );
    }
}

/// Exactly one `http_failure` per 5xx, and none for anything else.
#[tokio::test]
async fn exactly_one_http_failure_per_5xx() {
    for code in [200, 400, 404, 413, 429, 499] {
        let response = edge_call(
            Request::get(format!("/status/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let request_id = edge_request_id(&response);
        assert!(
            stdout_failures_for(&request_id).is_empty(),
            "a {code} logged a failure"
        );
    }
    let unmatched = edge_call(Request::get("/nowhere").body(Body::empty()).unwrap()).await;
    assert_eq!(unmatched.status(), StatusCode::NOT_FOUND);
    assert!(stdout_failures_for(&edge_request_id(&unmatched)).is_empty());

    for code in [500, 501, 502, 503, 504, 599] {
        let response = edge_call(
            Request::get(format!("/status/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let request_id = edge_request_id(&response);
        assert_eq!(
            stdout_failures_for(&request_id).len(),
            1,
            "a {code} must log exactly one failure"
        );
    }
}

/// `/api/v1/health` is in scope: a 503 from it is one WARN failure on the
/// unmetered target, carrying the peer address, since no key was metered.
/// A panic the pool caught under the `DuckDB` probe is recorded as one: the
/// panic stage and class, the payload nowhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_health_503_is_one_unmetered_warn_failure() {
    sinks();
    let server = common::setup().await;
    let seams = server.state.query.pool.seams();
    let _panic = seams.panic_at(Seam::Started);

    let response = raw_client()
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let request_id = request_id_of(response.headers());

    let lines = stdout_failures_for(&request_id);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert_eq!(level(line), "WARN", "{line}");
    assert!(line.contains(UNMETERED_FAILURE_TARGET), "{line}");
    assert_eq!(field(line, "route").as_deref(), Some("/api/v1/health"));
    assert_eq!(field(line, "stage").as_deref(), Some("panicked"), "{line}");
    assert_eq!(field(line, "reached"), None, "{line}");
    assert_eq!(
        field(line, "error_class").as_deref(),
        Some("panic"),
        "{line}"
    );
    assert_eq!(field(line, "cause_kind").as_deref(), Some("none"), "{line}");
    let peer = field(line, "peer_addr").expect("an unmetered failure names its peer");
    assert!(peer.starts_with("127.0.0.1:"), "{line}");

    // Unmetered, and well under the cap: persisted, peer included.
    let records = wal_failures_for(&request_id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["target"], UNMETERED_FAILURE_TARGET);
    assert_eq!(records[0]["level"], "warn");
    assert_eq!(records[0]["stage"], "panicked");
    assert_eq!(records[0]["error_class"], "panic");
    assert_eq!(records[0]["peer_addr"], peer.as_str());

    assert!(
        stdout_lines()
            .iter()
            .all(|line| !line.contains(INJECTED_PANIC_PAYLOAD)),
        "the panic payload reached stdout"
    );
    assert!(
        wal_records()
            .iter()
            .all(|record| !record.to_string().contains(INJECTED_PANIC_PAYLOAD)),
        "the panic payload reached the WAL"
    );
}

/// A `DuckDB` probe that outlives its budget answers the same 503, and the
/// failure names what the probe returned: a timeout in the handler.
#[tokio::test(flavor = "multi_thread")]
async fn a_health_probe_timeout_records_its_class() {
    sinks();
    let server = common::setup().await;
    let seams = server.state.query.pool.seams();
    let _held = seams.hold(Seam::Started);

    let response = raw_client()
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let request_id = request_id_of(response.headers());
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "unavailable", "{body}");
    assert_eq!(body["checks"]["duckdb"], "error", "{body}");

    let records = wal_failures_for(&request_id);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["target"], UNMETERED_FAILURE_TARGET);
    assert_eq!(record["level"], "warn");
    assert_eq!(record["route"], "/api/v1/health");
    assert_eq!(record["status"], 503);
    assert_eq!(record["stage"], "handler_error");
    assert!(record.get("reached").is_none(), "{record}");
    assert_eq!(record["error_class"], "timeout");
    assert_eq!(record["cause_kind"], "none");
    assert_eq!(stdout_failures_for(&request_id).len(), 1);
}

// -- AC4 ------------------------------------------------------------------------

/// The auth backend down is a server fault before any key was metered: the
/// 503 persists on the unmetered target, staged before admission, with the
/// peer address as its only lead.
#[tokio::test(flavor = "multi_thread")]
async fn an_auth_backend_503_persists_before_admission_with_its_peer() {
    sinks();
    let server = common::setup().await;
    server.kill_fleet_database().await;

    let response = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "*" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let request_id = request_id_of(response.headers());

    let records = wal_failures_for(&request_id);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["target"], UNMETERED_FAILURE_TARGET);
    assert_eq!(record["level"], "warn");
    assert_eq!(record["route"], "/api/v1/query");
    assert_eq!(record["status"], 503);
    assert_eq!(record["stage"], "pre_admission");
    assert_eq!(record["error_class"], "service_unavailable");
    assert!(record.get("key_id").is_none(), "{record}");
    assert!(
        record["peer_addr"]
            .as_str()
            .is_some_and(|peer| peer.starts_with("127.0.0.1:")),
        "{record}"
    );
    assert_eq!(stdout_failures_for(&request_id).len(), 1);
}

/// A 401 and a 403 are the caller's fault, decided before any limiter: the
/// rejections reach stdout, and nothing on an unmetered target reaches the
/// WAL.
#[tokio::test(flavor = "multi_thread")]
async fn unmetered_401_403_never_reach_the_wal() {
    sinks();
    let server = common::setup().await;

    // Positive control: a metered request's own record persists, so the
    // absences below are the filter's doing and not a dead WAL.
    let metered = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "*" }))
        .send()
        .await
        .unwrap();
    assert_eq!(metered.status(), 200);
    let metered_request = request_id_of(metered.headers());
    let completed: Vec<_> = wal_records()
        .into_iter()
        .filter(|record| record["event_type"] == "query_complete")
        .filter(|record| record["request_id"] == metered_request.as_str())
        .collect();
    assert_eq!(completed.len(), 1, "the WAL is live: {completed:?}");

    let unauthorized = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth("zz-not-a-key")
        .json(&serde_json::json!({ "query": "*" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let forbidden = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.coastwatch_only_token)
        .json(&serde_json::json!({ "query": "*" }))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);

    let lines = stdout_lines();
    for target in ["fleet_auth", telemetry::UNMETERED_POLICY_TARGET] {
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&format!(" {target}"))),
            "the {target} rejection reaches stdout: {lines:#?}"
        );
    }
    let unmetered: Vec<_> = wal_records()
        .into_iter()
        .filter(|record| {
            record["target"]
                .as_str()
                .is_some_and(|target| !telemetry::is_persisted_target(target))
        })
        .collect();
    assert!(unmetered.is_empty(), "{unmetered:#?}");
    for request_id in [
        request_id_of(unauthorized.headers()),
        request_id_of(forbidden.headers()),
    ] {
        assert!(wal_failures_for(&request_id).is_empty());
    }
}

// -- AC5 ------------------------------------------------------------------------

/// Sentinels planted in the DSL, a database error's text, request headers,
/// the request path and the method reach no `http_failure`, on stdout or in
/// the WAL.
#[tokio::test(flavor = "multi_thread")]
async fn sentinels_never_reach_http_failure() {
    const DSL: &str = "zzdslsentinel";
    const DB_ERROR: &str = "zz-dberror-sentinel";
    const HEADER: &str = "zz-header-sentinel";
    const USER_AGENT: &str = "zz-useragent-sentinel";
    const PATH: &str = "zz-path-sentinel";
    const METHOD: &str = "ZZMETHODSENTINEL";

    sinks();
    // `DuckDB`'s error for an unreadable file names the file, so the data
    // root's path puts a sentinel into the database error's text.
    let tmp = tempfile::Builder::new()
        .prefix(&format!("{DB_ERROR}-"))
        .tempdir()
        .unwrap();
    let server = common::setup_in_dir(tmp.path(), RateLimitConfig::default()).await;
    corrupt_nginx(tmp.path());

    let response = raw_client()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .header("user-agent", USER_AGENT)
        .header("x-zz-sentinel", HEADER)
        .json(&serde_json::json!({ "query": format!("service=nginx host={DSL}") }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    let query_request = request_id_of(response.headers());
    assert_eq!(wal_failures_for(&query_request).len(), 1);

    let response = edge_call(
        Request::get(format!("/items/{PATH}"))
            .header("user-agent", USER_AGENT)
            .header("x-zz-sentinel", HEADER)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let path_request = edge_request_id(&response);
    let lines = stdout_failures_for(&path_request);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(field(&lines[0], "route").as_deref(), Some("/items/{id}"));

    let response = edge_call(
        Request::builder()
            .method(METHOD)
            .uri("/bare")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let method_request = edge_request_id(&response);
    let lines = stdout_failures_for(&method_request);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(field(&lines[0], "method").as_deref(), Some("OTHER"));

    assert_no_failure_carries(&[DSL, DB_ERROR, HEADER, USER_AGENT, PATH, METHOD]);
}

// -- #293: the request-limit refusal --------------------------------------------

/// The regular refusal's message, word for word.
const REQUEST_LIMIT_MESSAGE: &str = "trawld is at its HTTP request limit \
     ([server] max_concurrent_requests); the request was not processed; retry later with backoff";

/// The probe refusal's message, word for word.
const PROBE_MESSAGE: &str = "trawld is at its HTTP probe allowance; the request was not processed; retry later with backoff";

/// A request through the production router, carrying the peer the accept
/// loop would insert.
fn production_request(uri: &str, token: &str, body: Body) -> Request<Body> {
    let mut request = Request::get(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(body)
        .unwrap();
    request
        .extensions_mut()
        .insert(std::net::SocketAddr::from(([127, 0, 0, 1], 40_293)));
    request
}

/// Every `trawl_auth_failures_total` sample a recorder renders.
fn auth_failure_samples(handle: &metrics_exporter_prometheus::PrometheusHandle) -> Vec<String> {
    handle
        .render()
        .lines()
        .filter(|line| line.starts_with("trawl_auth_failures_total{"))
        .map(str::to_owned)
        .collect()
}

/// Assert `request_id` emitted exactly one request-limit `http_failure`,
/// on stdout and persisted, naming `route` when the request matched one.
fn assert_one_request_limit_failure(request_id: &str, route: Option<&str>) {
    let lines = stdout_failures_for(request_id);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert_eq!(level(line), "WARN", "{line}");
    assert!(line.contains(UNMETERED_FAILURE_TARGET), "{line}");
    for (name, value) in [
        ("status", "503"),
        ("error_class", "service_unavailable"),
        ("cause_kind", "request_limit_reached"),
        ("stage", "pre_admission"),
    ] {
        assert_eq!(field(line, name).as_deref(), Some(value), "{line}");
    }
    assert_eq!(field(line, "key_id"), None, "{line}");
    if let Some(route) = route {
        assert_eq!(field(line, "route").as_deref(), Some(route), "{line}");
    }

    let records = wal_failures_for(request_id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["target"], UNMETERED_FAILURE_TARGET);
    assert_eq!(records[0]["level"], "warn");
    assert_eq!(records[0]["cause_kind"], "request_limit_reached");
    assert_eq!(records[0]["stage"], "pre_admission");
}

/// Each request-limit refusal is exactly one `http_failure`: class
/// `service_unavailable`, cause `request_limit_reached`, stage
/// `pre_admission`, at WARN, on the unmetered target, and persisted under
/// the unmetered cap. On an authenticated route with a bad token the
/// refusal is not rewritten to an auth answer, and
/// `trawl_auth_failures_total` does not move.
///
/// ONE production router serves every request, so the request holding the
/// count and the refused ones share it. The auth-failure counter is read
/// from a recorder of this thread's own: the requests whose metrics it
/// reads are driven on the test's own thread, and no other test in the
/// process can move it. The same bad token with the count free is the
/// control that the counter counts.
#[tokio::test(flavor = "multi_thread")]
async fn request_limit_refusal_is_one_unmetered_warn() {
    const BAD_TOKEN: &str = "flt_zzzzzzzz_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
    sinks();
    let server = common::setup().await;
    let app = server.router_with(|http| http.max_concurrent_requests = 1);

    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let _local = metrics::set_default_local_recorder(&recorder);

    // Control: with the count free, the bad token is a counted 401.
    let unauthorized = app
        .clone()
        .oneshot(production_request(
            "/api/v1/whoami",
            BAD_TOKEN,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let counted = auth_failure_samples(&handle);
    assert_eq!(counted.len(), 1, "{counted:?}");
    assert!(counted[0].ends_with(" 1"), "{counted:?}");

    // Hold the count: an admitted request whose body the client holds open.
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::clone(&release);
    let held_body = Body::from_stream(async_stream::stream! {
        let _ = entered_tx.send(());
        gate.notified().await;
        yield Ok::<_, std::io::Error>(axum::body::Bytes::from_static(br#"{"query":"*"}"#));
    });
    let mut held_request = production_request("/api/v1/validate", &server.admin_token, held_body);
    *held_request.method_mut() = axum::http::Method::POST;
    held_request
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    let held = tokio::spawn(app.clone().oneshot(held_request));
    tokio::time::timeout(std::time::Duration::from_secs(20), entered_rx)
        .await
        .expect("the held body was polled before the deadline")
        .unwrap();

    // Two refusals: one unauthenticated, one with a bad token on an
    // authenticated route.
    let mut refused_ids = Vec::new();
    for (uri, token) in [("/zz-nowhere", ""), ("/api/v1/whoami", BAD_TOKEN)] {
        let response = app
            .clone()
            .oneshot(production_request(uri, token, Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        let request_id = edge_request_id(&response);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["error"]["code"], "request_limit_reached",
            "{uri}: {body}"
        );
        assert_eq!(
            body["error"]["message"], REQUEST_LIMIT_MESSAGE,
            "{uri}: {body}"
        );
        refused_ids.push((uri, request_id));
    }

    assert_one_request_limit_failure(&refused_ids[0].1, None);
    assert_one_request_limit_failure(&refused_ids[1].1, Some("/api/v1/whoami"));

    // The refusals counted no auth failure: still the control's one.
    assert_eq!(auth_failure_samples(&handle), counted);

    release.notify_one();
    let held = tokio::time::timeout(std::time::Duration::from_secs(20), held)
        .await
        .expect("the held request finished")
        .unwrap()
        .unwrap();
    assert_eq!(held.status(), StatusCode::OK);
}

/// A probe refusal is one `http_failure` like every request-limit refusal:
/// three held `GET /api/v1/health` requests fill the probe allowance under
/// the production edge layers, and the fourth is refused before its
/// handler with the probe message, at WARN on the unmetered target, stage
/// `pre_admission`, the route named, and persisted under the unmetered
/// cap.
///
/// Health is a test handler at the production route template: the
/// production handler cannot be held without a seam, because its `DuckDB`
/// ping gives up after its own budget.
#[tokio::test(flavor = "multi_thread")]
async fn probe_refusal_is_one_unmetered_warn() {
    sinks();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let app = with_edge_layers(
        Router::new()
            .route(
                "/api/v1/health",
                get(move || {
                    let entered = entered_tx.clone();
                    let gate = Arc::clone(&handler_gate);
                    async move {
                        let _ = entered.send(());
                        // A closed gate releases every held request.
                        let _ = gate.acquire().await;
                        StatusCode::OK
                    }
                }),
            )
            .route_layer(axum::middleware::from_fn(mark_handler)),
        &http_config(),
    );
    let health = || Request::get("/api/v1/health").body(Body::empty()).unwrap();

    let held: Vec<_> = (0..3)
        .map(|_| tokio::spawn(app.clone().oneshot(health())))
        .collect();
    for _ in 0..3 {
        tokio::time::timeout(std::time::Duration::from_secs(20), entered_rx.recv())
            .await
            .expect("a held probe entered its handler before the deadline")
            .expect("the entry channel is open");
    }

    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        app.clone().oneshot(health()),
    )
    .await
    .expect("the fourth probe was answered without waiting on a held one")
    .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    let request_id = edge_request_id(&refused);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(refused.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["code"], "request_limit_reached", "{body}");
    assert_eq!(body["error"]["message"], PROBE_MESSAGE, "{body}");
    assert!(
        entered_rx.try_recv().is_err(),
        "the refused probe entered its handler"
    );
    assert_one_request_limit_failure(&request_id, Some("/api/v1/health"));

    gate.close();
    for handle in held {
        let response = tokio::time::timeout(std::time::Duration::from_secs(20), handle)
            .await
            .expect("the held probe finished")
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
