// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The count at the edge: test routes of their own under
//! [`with_edge_layers`], the exact layers production mounts.
//!
//! Each test builds ONE router and drives clones of it with `oneshot`:
//! the clones share one count, the way connections share the listener's.
//! Requests are held on per-label gates and ordered by their entry
//! channel, never by sleeps.
//!
//! This file uses no API newer than the count itself (string literals for
//! codes, messages and metric names), so it also builds against the
//! per-route limiter the count replaced, where these tests fail.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::http::{Request, StatusCode, header};
use axum::routing::{delete, get};
use trawl_server::transport::http::with_edge_layers;

use crate::support::{
    CONTROL_MESSAGE, Holds, PollRecorder, REGULAR_MESSAGE, assert_refused, body_bytes, call,
    finish, http_config, request, send,
};

/// A handler that panics.
async fn panicking() -> StatusCode {
    panic!("zz-request-limit-panic")
}

// -- AC1 ----------------------------------------------------------------------

/// One count spans routes, methods and router clones. N requests held on
/// two routes and two methods fill it; the next, on a third route, is
/// refused at once; releasing one admits exactly one more.
///
/// Per-route limiters (one semaphore per route and method) would admit the
/// third route's request, and that request would then wait on its own
/// hold until the deadline.
#[tokio::test]
async fn request_count_is_shared_across_routes_and_methods() {
    const N: usize = 4;
    let mut holds = Holds::new();
    let app = with_edge_layers(
        Router::new()
            .route(
                "/a",
                get(holds.handler("get /a")).post(holds.handler("post /a")),
            )
            .route(
                "/b",
                get(holds.handler("get /b")).put(holds.handler("put /b")),
            )
            .route("/c", get(holds.handler("get /c"))),
        &http_config(N),
    );

    let held = [("GET", "/a"), ("POST", "/a"), ("GET", "/b"), ("PUT", "/b")]
        .map(|(method, uri)| send(&app, request(method, uri)));
    assert_eq!(
        holds.entered_n(N).await,
        ["get /a", "get /b", "post /a", "put /b"]
    );

    // The third route: refused at once, its handler never entered.
    assert_refused(call(&app, request("GET", "/c")).await, REGULAR_MESSAGE).await;
    assert!(holds.nothing_entered());

    // Release exactly one, and wait for it to end.
    holds.release("post /a");
    let [get_a, post_a, get_b, put_b] = held;
    assert_eq!(finish(post_a).await.status(), StatusCode::OK);

    // Exactly one more is admitted: the first enters, the second is refused.
    let admitted = send(&app, request("GET", "/c"));
    assert_eq!(holds.entered().await, "get /c");
    assert_refused(call(&app, request("GET", "/c")).await, REGULAR_MESSAGE).await;

    holds.release_all();
    for handle in [get_a, get_b, put_b, admitted] {
        assert_eq!(finish(handle).await.status(), StatusCode::OK);
    }
}

// -- AC2 ----------------------------------------------------------------------

/// The refusal never waits: it answers while every held request is still
/// held, its handler is never entered, and its body is never polled.
#[tokio::test]
async fn request_limit_refuses_without_waiting() {
    const N: usize = 2;
    let mut holds = Holds::new();
    let refused_entries = Arc::new(AtomicUsize::new(0));
    let entries = Arc::clone(&refused_entries);
    let app = with_edge_layers(
        Router::new()
            .route("/a", get(holds.handler("get /a")))
            .route("/b", get(holds.handler("get /b")))
            .route(
                "/upload",
                axum::routing::post(move |body: axum::body::Bytes| {
                    entries.fetch_add(1, Ordering::SeqCst);
                    async move { body.len().to_string() }
                }),
            ),
        &http_config(N),
    );

    let held = [
        send(&app, request("GET", "/a")),
        send(&app, request("GET", "/b")),
    ];
    holds.entered_n(N).await;

    let (body, polled) = PollRecorder::body();
    let upload = Request::builder()
        .method("POST")
        .uri("/upload")
        .body(body)
        .unwrap();
    let response = call(&app, upload).await;
    assert_refused(response, REGULAR_MESSAGE).await;

    // Both are still held: nothing was released for the refusal to answer.
    for handle in &held {
        assert!(!handle.is_finished(), "a held request ended early");
    }
    assert_eq!(refused_entries.load(Ordering::SeqCst), 0, "handler entered");
    assert!(!PollRecorder::was_polled(&polled), "the body was polled");
    assert!(holds.nothing_entered());

    holds.release_all();
    for handle in held {
        assert_eq!(finish(handle).await.status(), StatusCode::OK);
    }
}

// -- AC3, edge part -------------------------------------------------------------

/// The refusal's contract at the edge: 503, `request_limit_reached`, the
/// fixed message with no digits, no `Retry-After`, `no-store`, a request
/// id, `nosniff`, HSTS, the CORS headers when origins are configured, and
/// no body for HEAD.
///
/// The production router's half (`no-store` on `/api/v1/ingest/preview`,
/// HEAD on a production route) is `request_limit_refusal_contract_production`
/// in `routes.rs`.
#[tokio::test]
async fn request_limit_refusal_contract() {
    const ORIGIN: &str = "https://console.example";
    let mut holds = Holds::new();
    let mut config = http_config(1);
    config.cors_allowed_origins = vec![ORIGIN.to_owned()];
    let app = with_edge_layers(
        Router::new()
            .route("/a", get(holds.handler("get /a")))
            .route("/c", get(holds.free("get /c"))),
        &config,
    );

    let held = send(&app, request("GET", "/a"));
    holds.entered().await;

    let refused = call(
        &app,
        Request::builder()
            .uri("/c")
            .header(header::ORIGIN, ORIGIN)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    let headers = refused.headers().clone();
    assert!(headers.get(header::RETRY_AFTER).is_none(), "{headers:?}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(
        headers.contains_key(header::STRICT_TRANSPORT_SECURITY),
        "{headers:?}"
    );
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], ORIGIN);
    let request_id = headers
        .get("x-request-id")
        .expect("the refusal carries a request id")
        .to_str()
        .unwrap();
    assert!(!request_id.is_empty());
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let body = assert_refused(refused, REGULAR_MESSAGE).await;
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("[server] max_concurrent_requests"));
    assert!(
        !message.bytes().any(|b| b.is_ascii_digit()),
        "the message carries a digit: {message}"
    );
    assert!(body["error"].get("details").is_none(), "{body}");

    // HEAD: the same refusal, with no body.
    let refused_head = call(&app, request("HEAD", "/c")).await;
    assert_eq!(refused_head.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused_head.headers()[header::CACHE_CONTROL], "no-store");
    assert!(body_bytes(refused_head).await.is_empty(), "HEAD got a body");
    assert!(holds.nothing_entered());

    holds.release_all();
    assert_eq!(finish(held).await.status(), StatusCode::OK);
}

// -- AC6 ----------------------------------------------------------------------

/// One sample of `name` with `allowance`, as the recorder renders it.
///
/// Every series here is a whole count, so it is read as one: a gauge
/// renders as a float, and `Display` writes a whole float without a
/// fraction.
fn sample(rendered: &str, name: &str, allowance: &str) -> u64 {
    let series = format!("{name}{{allowance=\"{allowance}\"}} ");
    let value: f64 = rendered
        .lines()
        .find_map(|line| line.strip_prefix(&series))
        .unwrap_or_else(|| panic!("no {series:?} in:\n{rendered}"))
        .trim()
        .parse()
        .unwrap();
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_| panic!("{series:?} is not a whole count: {value}"))
}

/// The three series for `allowance`: in progress, refused, size.
fn series(
    handle: &metrics_exporter_prometheus::PrometheusHandle,
    allowance: &str,
) -> (u64, u64, u64) {
    let rendered = handle.render();
    (
        sample(&rendered, "trawl_http_requests_in_progress", allowance),
        sample(&rendered, "trawl_http_requests_refused_total", allowance),
        sample(&rendered, "trawl_http_request_allowance", allowance),
    )
}

/// The gauges and the counter follow the count: the sizes are N and 4, a
/// held request raises its allowance's gauge, a refusal raises its
/// counter, and the gauge is back at its baseline after every way a
/// request ends.
///
/// The recorder is this test's own, installed for this thread only: the
/// test runs on a current-thread runtime, so every request it sends records
/// here, and no other test in the process can move these series.
#[tokio::test]
async fn request_count_metrics_follow_ownership() {
    const N: usize = 2;
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let _local = metrics::set_default_local_recorder(&recorder);

    let mut holds = Holds::new();
    let app = with_edge_layers(
        Router::new()
            .route("/a", get(holds.handler("get /a")))
            .route("/b", get(holds.handler("get /b")))
            .route("/api/v1/health", get(holds.handler("health")))
            .route("/panic", get(panicking))
            .route(
                "/error",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            ),
        &http_config(N),
    );

    // Published at build: both sizes, nothing in progress, nothing refused.
    assert_eq!(series(&handle, "regular"), (0, 0, 2));
    assert_eq!(series(&handle, "control"), (0, 0, 4));

    let a = send(&app, request("GET", "/a"));
    let b = send(&app, request("GET", "/b"));
    let health = send(&app, request("GET", "/api/v1/health"));
    holds.entered_n(3).await;
    assert_eq!(series(&handle, "regular"), (2, 0, 2));
    assert_eq!(series(&handle, "control"), (1, 0, 4));

    assert_refused(call(&app, request("GET", "/a")).await, REGULAR_MESSAGE).await;
    assert_eq!(series(&handle, "regular"), (2, 1, 2));
    assert_eq!(series(&handle, "control").1, 0);

    holds.release("get /a");
    holds.release("health");
    finish(a).await;
    finish(health).await;
    assert_eq!(series(&handle, "regular").0, 1);
    assert_eq!(series(&handle, "control").0, 0);
    holds.release("get /b");
    finish(b).await;
    assert_eq!(series(&handle, "regular").0, 0);

    // A caught panic, an error answer and a dropped request each end at
    // the baseline.
    let panicked = call(&app, request("GET", "/panic")).await;
    assert_eq!(panicked.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(series(&handle, "regular").0, 0);
    let errored = call(&app, request("GET", "/error")).await;
    assert_eq!(errored.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(series(&handle, "regular").0, 0);
    let dropped = send(&app, request("GET", "/a"));
    holds.entered().await;
    assert_eq!(series(&handle, "regular").0, 1);
    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    assert_eq!(series(&handle, "regular"), (0, 1, 2));
    assert_eq!(series(&handle, "control"), (0, 0, 4));
}

// -- AC9 ----------------------------------------------------------------------

/// The count is released when a request ends, on an error answer, on a
/// caught panic and when its future is dropped. With a limit of 1, N+1
/// panicking requests in a row are N+1 500s and no refusal.
#[tokio::test]
async fn request_count_released_on_end_error_panic_and_drop() {
    const N: usize = 1;
    let mut holds = Holds::new();
    let app = with_edge_layers(
        Router::new()
            .route("/hold", get(holds.handler("hold")))
            .route("/free", get(holds.free("free")))
            .route("/panic", get(panicking))
            .route(
                "/error",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            ),
        &http_config(N),
    );

    for _ in 0..=N {
        let response = call(&app, request("GET", "/panic")).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_bytes(response).await, "Service panicked");
    }
    for _ in 0..=N {
        let response = call(&app, request("GET", "/error")).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    for _ in 0..=N {
        assert_eq!(
            call(&app, request("GET", "/free")).await.status(),
            StatusCode::OK
        );
        assert_eq!(holds.entered().await, "free");
    }

    // A request whose future is dropped mid-handler gives its count back.
    let dropped = send(&app, request("GET", "/hold"));
    assert_eq!(holds.entered().await, "hold");
    assert_refused(call(&app, request("GET", "/free")).await, REGULAR_MESSAGE).await;
    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    assert_eq!(
        call(&app, request("GET", "/free")).await.status(),
        StatusCode::OK
    );
    assert_eq!(holds.entered().await, "free");
}

// -- AC10, body part ------------------------------------------------------------

/// A response body is not counted: with a limit of 1, a response whose
/// body never ends leaves the count free once its head is out. The stream
/// routes' half is `response_bodies_and_streams_do_not_hold_the_count_streams`
/// in `routes.rs`.
#[tokio::test]
async fn response_bodies_and_streams_do_not_hold_the_count() {
    let mut holds = Holds::new();
    let app = with_edge_layers(
        Router::new()
            .route(
                "/endless",
                get(|| async {
                    Body::from_stream(tokio_stream::pending::<
                        Result<axum::body::Bytes, std::io::Error>,
                    >())
                }),
            )
            .route("/free", get(holds.free("free"))),
        &http_config(1),
    );

    let endless = call(&app, request("GET", "/endless")).await;
    assert_eq!(endless.status(), StatusCode::OK);
    // The head is out and the body is still open: it never yields.
    let body = endless.into_body();
    assert!(!HttpBody::is_end_stream(&body));

    assert_eq!(
        call(&app, request("GET", "/free")).await.status(),
        StatusCode::OK
    );
    assert_eq!(holds.entered().await, "free");
    drop(body);
}

// -- control allowance -----------------------------------------------------------

/// The control allowance saturates at four, and neither allowance borrows
/// from the other. Test routes are mounted at the control templates, so
/// membership is read off their matched routes as in production.
#[tokio::test]
async fn control_allowance_saturates_at_four() {
    let mut holds = Holds::new();
    let app = with_edge_layers(
        Router::new()
            .route("/api/v1/health", get(holds.handler("health")))
            .route("/metrics", get(holds.handler("metrics")))
            .route("/api/v1/queries", get(holds.handler("queries")))
            .route("/api/v1/queries/{id}", delete(holds.handler("cancel")))
            .route("/a", get(holds.handler("get /a")))
            .route("/b", get(holds.free("get /b"))),
        &http_config(1),
    );

    // Four control requests: every control route, GET and HEAD.
    let health = send(&app, request("GET", "/api/v1/health"));
    let metrics = send(&app, request("HEAD", "/metrics"));
    let queries = send(&app, request("GET", "/api/v1/queries"));
    let cancel = send(&app, request("DELETE", "/api/v1/queries/7"));
    assert_eq!(
        holds.entered_n(4).await,
        ["cancel", "health", "metrics", "queries"]
    );

    // A fifth control request gets the control message, although the
    // regular count is free: control never borrows regular.
    assert_refused(
        call(&app, request("GET", "/metrics?x=1")).await,
        CONTROL_MESSAGE,
    )
    .await;
    assert!(holds.nothing_entered());

    // The regular count is untouched by four control requests.
    let a = send(&app, request("GET", "/a"));
    assert_eq!(holds.entered().await, "get /a");

    // Free one control request. Regular is full and control has one free:
    // a regular request, and a POST to a control path, are refused with
    // the regular message. Regular never borrows control.
    holds.release("health");
    assert_eq!(finish(health).await.status(), StatusCode::OK);
    assert_refused(call(&app, request("GET", "/b")).await, REGULAR_MESSAGE).await;
    assert_refused(
        call(&app, request("POST", "/api/v1/health")).await,
        REGULAR_MESSAGE,
    )
    .await;
    assert!(holds.nothing_entered());

    // The free control request is still there for a control route.
    let health = send(&app, request("HEAD", "/api/v1/health"));
    assert_eq!(holds.entered().await, "health");
    assert_refused(
        call(&app, request("DELETE", "/api/v1/queries/8")).await,
        CONTROL_MESSAGE,
    )
    .await;

    holds.release_all();
    for handle in [health, metrics, queries, cancel, a] {
        assert_eq!(finish(handle).await.status(), StatusCode::OK);
    }
}
