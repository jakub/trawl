// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The count on the production router: trawld's own routes, auth, rate
//! limits, body limits, stream caps and executor pool, over a booted
//! fixture's state.
//!
//! Each test builds ONE production router with `router_with` and drives
//! clones of it with `oneshot`, so every request shares that router's
//! count. Nothing here needs a production seam to fill the count: a
//! [`Fill`] request passes auth and the rate limit, then its handler's
//! body extractor waits on a body the test holds open. The count is held
//! from the moment the body is first polled until the test lets it end.
//!
//! Raw URIs go through `oneshot` unchanged, so no client normalizes a path
//! before the router sees it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, Response, StatusCode, header};
use fleet_auth::{KeyStore, PrincipalKind};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::seam::Seam;

use crate::common::{self, TestServer};
use crate::support::{DEADLINE, REGULAR_MESSAGE, assert_refused, body_bytes, body_json, call};

/// The peer every in-process request carries, as the accept loop inserts
/// one for every real connection (ingest reads it).
const PEER: ([u8; 4], u16) = ([127, 0, 0, 1], 40_293);

/// A request through the production router: a raw URI, an optional bearer
/// token, and the peer extension.
fn request(method: &str, uri: &str, token: Option<&str>, body: Body) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut request = builder.body(body).unwrap();
    request.extensions_mut().insert(SocketAddr::from(PEER));
    request
}

/// A request with an empty body.
fn empty(method: &str, uri: &str, token: Option<&str>) -> Request<Body> {
    request(method, uri, token, Body::empty())
}

/// A JSON POST.
fn json(uri: &str, token: &str, body: &serde_json::Value) -> Request<Body> {
    let mut request = request("POST", uri, Some(token), Body::from(body.to_string()));
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    request
}

/// An ndjson POST of one event for `service`.
fn ndjson(uri: &str, token: &str, service: &str) -> Request<Body> {
    let event = serde_json::json!({ "service": service, "message": "request limit probe" });
    let mut request = request("POST", uri, Some(token), Body::from(format!("{event}\n")));
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/x-ndjson"),
    );
    request
}

/// A bearer token that names no key.
const BAD_TOKEN: &str = "flt_zzzzzzzz_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";

/// The production router over `server`'s state with
/// `max_concurrent_requests` at `limit`.
fn router(server: &TestServer, limit: usize) -> Router {
    server.router_with(|http| http.max_concurrent_requests = limit)
}

/// Assert a response's status and answer its JSON body.
async fn status_json(
    response: Response<Body>,
    status: StatusCode,
    what: &str,
) -> serde_json::Value {
    assert_eq!(response.status(), status, "{what}");
    body_json(response).await
}

// -- holding the count ------------------------------------------------------------

/// Requests that hold the regular count with a body the client holds open.
///
/// Each is a JSON POST whose body yields nothing until the test releases
/// it. Its first poll reports entry, so by then the request is past auth,
/// the grant and the rate limit, inside its handler's body extractor, and
/// counted.
struct Fill {
    release: Arc<Semaphore>,
    held: Vec<JoinHandle<Response<Body>>>,
}

impl Fill {
    /// Send one held request per `(route, token)`, and wait until each
    /// body has been polled.
    async fn hold(app: &Router, requests: &[(&str, &str)]) -> Self {
        let release = Arc::new(Semaphore::new(0));
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let mut held = Vec::with_capacity(requests.len());
        for (route, token) in requests {
            let entered = entered_tx.clone();
            let gate = Arc::clone(&release);
            let body = Body::from_stream(async_stream::stream! {
                let _ = entered.send(());
                // A closed gate releases every held body.
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                }
                yield Ok::<_, std::io::Error>(Bytes::from_static(br#"{"query":"*"}"#));
            });
            let mut held_request = request("POST", route, Some(token), body);
            held_request.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            held.push(crate::support::send(app, held_request));
        }
        for _ in requests {
            tokio::time::timeout(DEADLINE, entered_rx.recv())
                .await
                .expect("a held body was polled before the deadline")
                .expect("the entry channel is open");
        }
        Self { release, held }
    }

    /// Let every held body finish, and require each request's 200.
    async fn release(self) {
        self.release.close();
        for handle in self.held {
            let response = crate::support::finish(handle).await;
            let status = response.status();
            assert_eq!(status, StatusCode::OK, "{:?}", body_bytes(response).await);
        }
    }
}

/// Assert the regular count is full: a plain regular request is refused.
async fn assert_regular_full(app: &Router, token: &str) {
    assert_refused(
        call(app, empty("GET", "/api/v1/whoami", Some(token))).await,
        REGULAR_MESSAGE,
    )
    .await;
}

/// Assert the regular count has room: a plain regular request is answered.
async fn assert_regular_free(app: &Router, token: &str) {
    let response = call(app, empty("GET", "/api/v1/whoami", Some(token))).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{:?}",
        body_bytes(response).await
    );
}

// -- AC3, production part ------------------------------------------------------------

/// The refusal on production routes: `/api/v1/ingest/preview` is refused
/// with `Cache-Control: no-store` although its own no-store layer never
/// runs, and a refused HEAD on a production route has an empty body.
#[tokio::test(flavor = "multi_thread")]
async fn request_limit_refusal_contract_production() {
    let server = common::setup().await;
    let app = router(&server, 1);
    let fill = Fill::hold(&app, &[("/api/v1/validate", &server.admin_token)]).await;

    // The preview, from the one key allowed to call it.
    let preview = call(
        &app,
        ndjson("/api/v1/ingest/preview", &server.admin_token, "rl-preview"),
    )
    .await;
    let headers = preview.headers().clone();
    assert_refused(preview, REGULAR_MESSAGE).await;
    assert_eq!(headers[header::CACHE_CONTROL], "no-store", "{headers:?}");
    assert!(headers.get(header::RETRY_AFTER).is_none(), "{headers:?}");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(headers.contains_key("x-request-id"), "{headers:?}");

    // HEAD on two regular production routes, one of them unauthenticated.
    for (uri, token) in [
        ("/api/v1/schema", Some(server.analyst_token.as_str())),
        ("/api/v1/whoami", None),
    ] {
        let refused = call(&app, empty("HEAD", uri, token)).await;
        assert_eq!(
            refused.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "HEAD {uri}"
        );
        assert_eq!(
            refused.headers()[header::CACHE_CONTROL],
            "no-store",
            "HEAD {uri}"
        );
        assert!(
            body_bytes(refused).await.is_empty(),
            "HEAD {uri} got a body"
        );
    }

    fill.release().await;
    // Control: the same HEAD is served once the count has room.
    let head = call(
        &app,
        empty("HEAD", "/api/v1/whoami", Some(&server.analyst_token)),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
}

// -- AC4 ------------------------------------------------------------------------------

/// The WAL directory of `server`'s ingest writer.
fn wal_dir(server: &TestServer) -> PathBuf {
    server
        .state
        .ingest
        .wal_writer
        .as_ref()
        .expect("ingest is enabled")
        .dir()
        .to_path_buf()
}

/// Every path under `dir`, recursively, with its size.
fn listing(dir: &Path) -> std::collections::BTreeSet<(PathBuf, u64)> {
    let mut out = std::collections::BTreeSet::new();
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
            }
            out.insert((entry.path(), meta.len()));
        }
    }
    out
}

/// The probes `request_limit_precedes_auth_body_rate_and_routing` sends,
/// each with the status it gets when the count has room.
fn precedence_probes(
    server: &TestServer,
    max_body: usize,
) -> Vec<(&'static str, Request<Body>, StatusCode)> {
    let mut oversized = json(
        "/api/v1/validate",
        &server.analyst_token,
        &serde_json::json!({ "query": "x".repeat(max_body) }),
    );
    let length = axum::body::HttpBody::size_hint(oversized.body())
        .exact()
        .unwrap();
    oversized
        .headers_mut()
        .insert(header::CONTENT_LENGTH, length.into());
    vec![
        (
            "a bad bearer token",
            empty("GET", "/api/v1/whoami", Some(BAD_TOKEN)),
            StatusCode::UNAUTHORIZED,
        ),
        (
            "an oversized body",
            oversized,
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            "an exhausted key rate",
            empty("GET", "/api/v1/whoami", Some(&server.reader_token)),
            StatusCode::TOO_MANY_REQUESTS,
        ),
        (
            "an unknown path under the API",
            empty("GET", "/api/v1/zz-nowhere", Some(&server.analyst_token)),
            StatusCode::NOT_FOUND,
        ),
        (
            "an unknown path",
            empty("GET", "/zz-nowhere", None),
            StatusCode::NOT_FOUND,
        ),
        (
            "a wrong method on a top-level route",
            empty("PUT", "/api/v1/health", None),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            "a wrong method on an API route",
            empty("DELETE", "/api/v1/whoami", Some(&server.schema_admin_token)),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
    ]
}

/// While the count is full, a bad token, an oversized body, an exhausted
/// key rate, an unknown path and a wrong method all get
/// `request_limit_reached`: none of those checks has run yet. With room,
/// each gets its own answer. A refused ingest request writes nothing to
/// the WAL.
///
/// The interactive rate is one request a minute, so every key here spends
/// its one token on purpose: the reader key is the exhausted one, the
/// admin key's token goes to the request that fills the count, and the
/// analyst and schema-admin keys each reach one route once with room.
#[tokio::test(flavor = "multi_thread")]
async fn request_limit_precedes_auth_body_rate_and_routing() {
    let server = common::setup().await;
    let mut max_body = 0;
    let app = server.router_with(|http| {
        http.max_concurrent_requests = 1;
        http.rate_limit = RateLimitConfig {
            default_rpm: 1,
            ..http.rate_limit.clone()
        };
        max_body = http.max_request_body_bytes;
    });
    let wal = wal_dir(&server);

    // Spend the reader key's one token.
    assert_regular_free(&app, &server.reader_token).await;

    // With room: each probe gets its own answer.
    for (what, probe, status) in precedence_probes(&server, max_body) {
        let response = call(&app, probe).await;
        assert_eq!(response.status(), status, "{what}: {response:?}");
    }
    // ...and an ingest request is written: the WAL is live.
    let before = listing(&wal);
    let response = call(
        &app,
        ndjson("/api/v1/ingest", &server.ingest_token, "rl-admitted"),
    )
    .await;
    let answer = status_json(response, StatusCode::OK, "admitted ingest").await;
    assert_eq!(answer["accepted"], 1, "{answer}");
    assert_ne!(
        listing(&wal),
        before,
        "an admitted ingest request writes the WAL"
    );

    // Full: every probe is the refusal.
    let fill = Fill::hold(&app, &[("/api/v1/validate", &server.admin_token)]).await;
    for (what, probe, _) in precedence_probes(&server, max_body) {
        let response = call(&app, probe).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{what}: {response:?}"
        );
        let body = body_json(response).await;
        assert_eq!(
            body["error"]["code"], "request_limit_reached",
            "{what}: {body}"
        );
        assert_eq!(body["error"]["message"], REGULAR_MESSAGE, "{what}: {body}");
    }
    // A refused ingest request writes nothing.
    let before = listing(&wal);
    assert_refused(
        call(
            &app,
            ndjson("/api/v1/ingest", &server.ingest_token, "rl-refused"),
        )
        .await,
        REGULAR_MESSAGE,
    )
    .await;
    assert_eq!(
        listing(&wal),
        before,
        "a refused ingest request wrote the WAL"
    );
    fill.release().await;

    // With room again, the exhausted key is still exhausted: its refusal
    // spent nothing and refilled nothing.
    let response = call(
        &app,
        empty("GET", "/api/v1/whoami", Some(&server.reader_token)),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "{response:?}"
    );
}

// -- AC7 ------------------------------------------------------------------------------

/// Serializes the `/metrics` scrapes in this file. The recorder is global
/// under the plain `cargo test` harness, and a scrape sets
/// `trawl_query_permits_retained` from its own server's pool just before
/// it renders, so two scrapes from different fixtures must not interleave.
static SCRAPE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// With the regular count full, every control route and method reaches
/// its handler. Each handler is recognised by what only it produces:
/// health's `DuckDB` probe arrives at the pool, `/metrics` answers
/// Prometheus text, the query list answers its three lists, and a
/// cancellation answers a cancel result. HEAD carries the same
/// representation without the body. Health is not required to be 200.
///
/// Regular never borrows control: a regular request is refused while all
/// four control places are free. The allowance grants nothing: a key that
/// does not own a query still cannot cancel it.
///
/// The count is full with one query parked in the pool at its work-start
/// seam and one request holding its body open, on two routes. Four held
/// control handlers are not built here: none of them can be held without
/// a production seam, and the edge test
/// `control_allowance_saturates_at_four` covers that case.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one saturated window, every control route read in it
async fn control_allowance_survives_regular_saturation() {
    let _scrape = SCRAPE.lock().await;
    let server = common::setup().await;
    let app = router(&server, 2);
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let twin = store
        .create_key(
            "rl-twin",
            PrincipalKind::Service,
            &common::roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let twin = twin.plaintext_token.to_string();

    // One query, parked past its work-start transition, owned by the
    // analyst key.
    let seams = server.state.query.pool.seams();
    let parked = seams.hold(Seam::Started);
    let query = crate::support::send(
        &app,
        json(
            "/api/v1/query",
            &server.analyst_token,
            &serde_json::json!({ "query": "*" }),
        ),
    );
    wait_for(|| parked.arrivals() == 1, "the query's worker parks").await;
    // Later workers, health's probe among them, pass and are counted.
    let probes = seams.watch(Seam::Started);
    let fill = Fill::hold(&app, &[("/api/v1/validate", &server.admin_token)]).await;
    assert_regular_full(&app, &server.analyst_token).await;

    // Health, GET and HEAD: its `DuckDB` probe reached the pool each time.
    let health = call(&app, empty("GET", "/api/v1/health", None)).await;
    let status = health.status();
    let body = body_json(health).await;
    assert!(
        body["status"].is_string() && body["checks"].is_object(),
        "{status}: {body}"
    );
    assert_eq!(probes.arrivals(), 1, "GET health ran its probe");
    let head = call(&app, empty("HEAD", "/api/v1/health", None)).await;
    assert!(body_bytes(head).await.is_empty());
    assert_eq!(probes.arrivals(), 2, "HEAD health ran its probe");

    // `/metrics`, GET and HEAD: Prometheus text.
    let metrics = call(&app, empty("GET", "/metrics", None)).await;
    assert_eq!(metrics.status(), StatusCode::OK);
    assert!(
        metrics.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4"),
        "{metrics:?}"
    );
    let text = String::from_utf8(body_bytes(metrics).await.to_vec()).unwrap();
    assert!(
        text.contains("# TYPE trawl_http_requests_in_progress gauge"),
        "{text}"
    );
    let head = call(&app, empty("HEAD", "/metrics", None)).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert!(
        head.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4"),
        "{head:?}"
    );
    assert!(body_bytes(head).await.is_empty());

    // The query list, GET and HEAD: the parked query is the owner's one
    // active entry, and HEAD's length is the same representation's.
    let listed = call(
        &app,
        empty("GET", "/api/v1/queries", Some(&server.analyst_token)),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = body_bytes(listed).await;
    let list: serde_json::Value = serde_json::from_slice(&listed).unwrap();
    assert!(
        list["recent"].is_array() && list["retained"].is_array(),
        "{list}"
    );
    let active = list["active"].as_array().expect("an active list");
    assert_eq!(active.len(), 1, "{list}");
    let id = active[0]["id"].as_u64().expect("the parked query's id");
    let head = call(
        &app,
        empty("HEAD", "/api/v1/queries", Some(&server.analyst_token)),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers()[header::CONTENT_LENGTH],
        listed.len().to_string().as_str(),
        "HEAD carries the list's own length"
    );
    assert!(body_bytes(head).await.is_empty());

    // Cancellation keeps its ownership check: the twin key is refused by
    // the handler's own check, then the owner's cancel is a cancel result.
    let refused = call(
        &app,
        empty("DELETE", &format!("/api/v1/queries/{id}"), Some(&twin)),
    )
    .await;
    let body = status_json(refused, StatusCode::FORBIDDEN, "the twin's cancel").await;
    assert_eq!(
        body["error"]["message"], "cannot cancel this query",
        "{body}"
    );
    let cancelled = call(
        &app,
        empty(
            "DELETE",
            &format!("/api/v1/queries/{id}"),
            Some(&server.analyst_token),
        ),
    )
    .await;
    let body = status_json(cancelled, StatusCode::OK, "the owner's cancel").await;
    assert_eq!(
        body,
        serde_json::json!({ "cancelled": true, "query_id": id })
    );

    // Still full: every control request above came out of the allowance.
    assert_regular_full(&app, &server.analyst_token).await;

    // Whatever the cancelled query answers, it answers once released.
    parked.release();
    crate::support::finish(query).await;
    fill.release().await;
    assert_regular_free(&app, &server.analyst_token).await;
}

/// Wait until `done` holds, under the deadline. For state a test cannot
/// be told about on a channel (a pool seam's arrival count).
async fn wait_for(mut done: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(DEADLINE, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} before the deadline"));
}

// -- AC8 ------------------------------------------------------------------------------

/// Control membership is the matched route and method. With the regular
/// count full, a path that only spells a control route, a forged header
/// and a control path under another method are all charged to regular and
/// refused with its message. A query string on a real control route is
/// still control and reaches the handler.
#[tokio::test(flavor = "multi_thread")]
async fn control_allowance_is_keyed_by_matched_route() {
    let server = common::setup().await;
    let app = router(&server, 1);
    let fill = Fill::hold(&app, &[("/api/v1/validate", &server.admin_token)]).await;
    let admin = server.admin_token.as_str();

    let spellings = [
        ("GET", "/api/v1/health/../query"),
        ("GET", "/api/v1/health/"),
        ("GET", "/api/v1/%68ealth"),
        ("GET", "/api%2Fv1/health"),
        ("GET", "/api/v1/health%2F..%2Fquery"),
        ("GET", "/%6Detrics"),
        ("GET", "/METRICS"),
        ("GET", "/api/v1/%71ueries"),
        ("DELETE", "/api/v1/queries%2F7"),
        ("DELETE", "/api/v1/queries/7/"),
    ];
    for (method, uri) in spellings {
        let response = call(&app, empty(method, uri, Some(admin))).await;
        assert_refused(response, REGULAR_MESSAGE).await;
    }

    for (name, value) in [
        ("x-original-url", "/api/v1/health"),
        ("x-original-uri", "/api/v1/health"),
        ("x-rewrite-url", "/metrics"),
        ("x-forwarded-uri", "/api/v1/queries"),
        ("x-forwarded-prefix", "/api/v1/health"),
        ("x-matched-path", "/api/v1/health"),
        ("x-http-method-override", "GET"),
    ] {
        let mut forged = empty("GET", "/api/v1/whoami", Some(admin));
        forged.headers_mut().insert(name, value.parse().unwrap());
        assert_refused(call(&app, forged).await, REGULAR_MESSAGE).await;
    }
    // A method override on a control path does not make a POST control.
    let mut overridden = empty("POST", "/api/v1/health", None);
    overridden
        .headers_mut()
        .insert("x-http-method-override", "GET".parse().unwrap());
    assert_refused(call(&app, overridden).await, REGULAR_MESSAGE).await;

    for (method, uri) in [
        ("POST", "/api/v1/health"),
        ("PUT", "/metrics"),
        ("POST", "/api/v1/queries"),
        ("GET", "/api/v1/queries/7"),
        ("PUT", "/api/v1/queries/7"),
    ] {
        let response = call(&app, empty(method, uri, Some(admin))).await;
        assert_refused(response, REGULAR_MESSAGE).await;
    }

    // A query string on a real control route: still control, so the
    // handler answers.
    let health = call(&app, empty("GET", "/api/v1/health?zz=/api/v1/query", None)).await;
    let body = body_json(health).await;
    assert!(body["checks"].is_object(), "{body}");
    let metrics = call(&app, empty("GET", "/metrics?zz=1", None)).await;
    assert_eq!(metrics.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(metrics).await.to_vec()).unwrap();
    assert!(text.contains("# TYPE "), "{text}");
    let cancel = call(
        &app,
        empty("DELETE", "/api/v1/queries/987654?zz=1", Some(admin)),
    )
    .await;
    let body = status_json(cancel, StatusCode::OK, "admin cancel").await;
    assert_eq!(
        body,
        serde_json::json!({ "cancelled": false, "query_id": 987_654 })
    );

    fill.release().await;
}

// -- AC10, stream routes ------------------------------------------------------------------

/// Assert `response` is an open event stream, and answer it so its body
/// stays open.
fn open_stream(response: Response<Body>, what: &str) -> Response<Body> {
    assert_eq!(response.status(), StatusCode::OK, "{what}: {response:?}");
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "{what}: {response:?}"
    );
    assert!(
        !axum::body::HttpBody::is_end_stream(response.body()),
        "{what}: the stream ended"
    );
    response
}

/// With a limit of 1, an open `/api/v1/stream` and an open
/// `/api/v1/dashboard/stream` leave the count free once their heads are
/// sent, both at once. Each route still answers 429 `too_many_streams`
/// past its own cap, set to one stream each here.
#[tokio::test(flavor = "multi_thread")]
async fn response_bodies_and_streams_do_not_hold_the_count_streams() {
    let server = common::setup().await;
    let mut state = server.state.clone();
    state.query.sse_semaphore = Arc::new(Semaphore::new(1));
    state.query.dashboard_sse_semaphore = Arc::new(Semaphore::new(1));
    let app = server.router_over(state, |http| http.max_concurrent_requests = 1);
    let analyst = server.analyst_token.as_str();
    let admin = server.admin_token.as_str();

    let stream = open_stream(
        call(
            &app,
            empty("GET", "/api/v1/stream?query=%2A", Some(analyst)),
        )
        .await,
        "the live stream",
    );
    assert_regular_free(&app, analyst).await;
    let second = call(
        &app,
        empty("GET", "/api/v1/stream?query=%2A", Some(analyst)),
    )
    .await;
    let body = status_json(
        second,
        StatusCode::TOO_MANY_REQUESTS,
        "a second live stream",
    )
    .await;
    assert_eq!(body["error"]["code"], "too_many_streams", "{body}");

    let dashboard = open_stream(
        call(&app, empty("GET", "/api/v1/dashboard/stream", Some(admin))).await,
        "the dashboard stream",
    );
    assert_regular_free(&app, admin).await;
    let second = call(&app, empty("GET", "/api/v1/dashboard/stream", Some(admin))).await;
    let body = status_json(
        second,
        StatusCode::TOO_MANY_REQUESTS,
        "a second dashboard stream",
    )
    .await;
    assert_eq!(body["error"]["code"], "too_many_streams", "{body}");

    // Both streams are still open, and the count is still free.
    assert!(!axum::body::HttpBody::is_end_stream(stream.body()));
    assert!(!axum::body::HttpBody::is_end_stream(dashboard.body()));
    assert_regular_free(&app, analyst).await;
    drop((stream, dashboard));
}

// -- AC13 -----------------------------------------------------------------------------

/// The one `trawl_query_permits_retained` sample a production scrape
/// renders.
async fn scraped_retained(app: &Router) -> u64 {
    let metrics = call(app, empty("GET", "/metrics", None)).await;
    assert_eq!(metrics.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(metrics).await.to_vec()).unwrap();
    let value: f64 = text
        .lines()
        .find_map(|line| line.strip_prefix("trawl_query_permits_retained "))
        .unwrap_or_else(|| panic!("no trawl_query_permits_retained in:\n{text}"))
        .trim()
        .parse()
        .unwrap();
    value.to_string().parse().unwrap()
}

/// Retained executor work holds no HTTP count. With a limit of 1, a query
/// that times out while its worker is parked past the work-start seam
/// answers, and the count is free again although the work still holds its
/// executor permit and `trawl_query_permits_retained` reads 1. A manual
/// run holds no count while its worker runs after the triggering response
/// returned.
///
/// Every wait is on the pool's state (a seam's arrivals, the retained
/// count), never on elapsed time.
#[tokio::test(flavor = "multi_thread")]
async fn executor_retained_work_does_not_hold_the_count() {
    let _scrape = SCRAPE.lock().await;
    let permissive = RateLimitConfig {
        default_rpm: 1_000_000,
        ..RateLimitConfig::default()
    };
    let server = common::setup_with_query_timeout(permissive, 1).await;
    let app = router(&server, 1);
    let pool = server.state.query.pool.clone();
    let seams = pool.seams();
    let analyst = server.analyst_token.as_str();

    // A query that times out with its worker parked: a 504, and the work
    // is retained.
    let parked = seams.hold(Seam::Started);
    let response = call(
        &app,
        json(
            "/api/v1/query",
            analyst,
            &serde_json::json!({ "query": "*" }),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::GATEWAY_TIMEOUT,
        "{response:?}"
    );
    let body = body_json(response).await;
    assert_eq!(parked.arrivals(), 1, "{body}");
    assert_eq!(pool.retained(), 1, "the timed-out query keeps its permit");
    // The count is back while the permit is still retained.
    assert_regular_free(&app, analyst).await;
    assert_eq!(scraped_retained(&app).await, 1);
    assert_eq!(pool.retained(), 1);

    parked.release();
    wait_for(|| pool.retained() == 0, "the retained permit comes back").await;

    // A manual run, its worker parked after the trigger answered.
    let client = trawl_client::HttpClient::new_insecure(&server.url, analyst).unwrap();
    let saved = client
        .create_saved("rl-manual-run", "* | head 3")
        .await
        .unwrap();
    client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .unwrap();
    let parked = seams.hold(Seam::Started);
    let triggered = call(
        &app,
        empty(
            "POST",
            &format!("/api/v1/saved/{}/run", saved.id),
            Some(analyst),
        ),
    )
    .await;
    let summary = status_json(triggered, StatusCode::OK, "the manual run").await;
    assert_eq!(summary["status"], "running", "{summary}");
    wait_for(|| parked.arrivals() == 1, "the run's worker parks").await;
    assert_regular_free(&app, analyst).await;
    assert_eq!(parked.arrivals(), 1, "the run is still parked");
    parked.release();
}
