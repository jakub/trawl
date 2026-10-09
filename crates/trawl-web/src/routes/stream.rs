// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SSE pass-through for `/api/v1/stream` and `/api/v1/dashboard/stream`.
//!
//! The generic `/api/v1/*` forwarder buffers request/response bodies,
//! which is wrong for Server-Sent Events — those are streams of
//! unbounded size and unpredictable duration. These handlers stream the
//! upstream response bytes straight to the browser without parsing. The
//! browser's `EventSource` handles framing on its end; reconnection
//! after a drop is automatic and rides the same session cookie. A non-2xx
//! upstream answer is no stream, and is relayed as the generic forwarder
//! relays it.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures::StreamExt;
use serde::Deserialize;
use tokio::time::{Instant, sleep_until};

use crate::error::ProxyError;
use crate::middleware::session_extractor::Auth;
use crate::routes::proxy::{clear_cookie_for_proxied_response, refuse_redirect, relay_response};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub query: String,
}

pub async fn forward(
    State(state): State<AppState>,
    auth: Auth,
    Query(params): Query<StreamParams>,
) -> Result<Response, ProxyError> {
    let upstream_url = format!(
        "{}/api/v1/stream",
        state.upstream_url().trim_end_matches('/')
    );

    // One client for the request and the whole stream it opens.
    let upstream_resp = state
        .upstream_client()
        .await?
        .get(&upstream_url)
        .bearer_auth(auth.token())
        .query(&[("query", &params.query)])
        .send()
        .await
        .map_err(ProxyError::Network)?;

    forward_sse_response(&state, &auth, upstream_resp)
}

/// SSE pass-through for the admin dashboard-stats stream. No params —
/// trawld's `ServerManage` check is the sole authorization gate.
pub async fn forward_dashboard(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Response, ProxyError> {
    let upstream_url = format!(
        "{}/api/v1/dashboard/stream",
        state.upstream_url().trim_end_matches('/')
    );

    // One client for the request and the whole stream it opens.
    let upstream_resp = state
        .upstream_client()
        .await?
        .get(&upstream_url)
        .bearer_auth(auth.token())
        .send()
        .await
        .map_err(ProxyError::Network)?;

    forward_sse_response(&state, &auth, upstream_resp)
}

/// Turn an upstream SSE response into the browser-facing streaming
/// response: status mirroring, session-expiry cap, SSE headers, and the
/// proxy-wide cookie rule. Shared by both SSE forwarders.
fn forward_sse_response(
    state: &AppState,
    auth: &Auth,
    upstream_resp: reqwest::Response,
) -> Result<Response, ProxyError> {
    // Mirror the upstream status verbatim, as `proxy::forward` does.
    // Routing non-2xx through `ProxyError::Upstream` would collapse
    // everything but 401/403 into 502, hiding trawld's 400 (invalid DSL)
    // and 429 (stream-concurrency limit) behind a vague "upstream error".
    // A 3xx is the exception, for the same reason as there.
    refuse_redirect(upstream_resp.status())?;
    // A non-2xx answer is no stream: it goes out through the generic
    // forwarder's copy, with its own Content-Type, Cache-Control and
    // request id, so a 503 `request_limit_reached` reaches the browser
    // unchanged (ADR-0054). Only a stream gets the SSE treatment below.
    if !upstream_resp.status().is_success() {
        return relay_response(upstream_resp, auth);
    }
    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let byte_stream = upstream_resp.bytes_stream();

    // Cap the stream: a cookie session ends at its `exp`, a bearer client
    // at the configured session TTL. Without a deadline a browser that
    // opens `/api/v1/stream` moments before expiry keeps receiving events
    // long after the cookie stops working for any other request, since the
    // extractor checks expiry once at handler entry and `Body::from_stream`
    // has no deadline of its own. Dropping the upstream stream also closes
    // the TCP connection via reqwest's drop handling.
    let ttl = match auth {
        Auth::Session(s) => remaining_ttl(s.exp(), chrono::Utc::now().timestamp()),
        Auth::Bearer(_) => Duration::from_secs(state.session_ttl_secs()),
    };
    let deadline = Instant::now() + ttl;
    let capped_stream = byte_stream.take_until(sleep_until(deadline));

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        // Disable proxy buffering (e.g. nginx) upstream of us. Trawld sets
        // this too but we re-set it to be resilient to misconfigured
        // intermediate proxies when trawl-web is fronted by another one.
        .header("X-Accel-Buffering", "no");

    // The SSE path shares the proxy-wide 401/403 cookie rule; see
    // `clear_cookie_for_proxied_response`. It never clears, so an
    // EventSource reconnect keeps 401ing until the SPA's next `/me` poll
    // drops the dead cookie, which is the permission-aware place to decide.
    if let Some(clear) = clear_cookie_for_proxied_response(status, auth) {
        builder = builder.header(header::SET_COOKIE, clear);
    }

    builder
        .body(Body::from_stream(capped_stream))
        .map_err(|e| ProxyError::Internal(format!("SSE response build: {e}")))
}

/// Compute the `Duration` between now and the session's expiry.
///
/// The session extractor rejects expired sessions before we get here, so
/// `exp > now` in practice. This function nonetheless clamps to zero for
/// the edge case where clock skew or a race lets an about-to-expire
/// session slip through — returning `Duration::ZERO` makes the stream
/// close immediately via `take_until`, which is the right failure mode.
#[must_use]
pub fn remaining_ttl(exp_secs: i64, now_secs: i64) -> Duration {
    let remaining = exp_secs.saturating_sub(now_secs).max(0);
    Duration::from_secs(u64::try_from(remaining).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;
    use trawl_config::WebConfig;
    use wiremock::matchers::{bearer_token, method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    use crate::config::ResolvedConfig;
    use crate::routes;
    use crate::test_support::TlsUpstream;

    fn state_pointing_at(upstream: &TlsUpstream) -> AppState {
        let web = WebConfig {
            allow_insecure_cookies: true,
            // Required since ADR-0016; these tests send no Origin header,
            // and a present-only guard lets those through.
            public_origins: vec!["https://trawl.fleet.test".to_owned()],
            ..upstream.web_config()
        };
        AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap()
    }

    async fn login_cookie(app: axum::Router, upstream: &TlsUpstream) -> String {
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "prefix": "testtest",
                "name": "alice",
                "kind": "human",
                "roles": ["trawl-analyst"],
                "permissions": ["query", "schema_read", "validate", "saved_query", "export", "stream", "query_cancel"]
            })))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        resp.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn stream_forwards_upstream_body_with_sse_headers() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        let sse_body = "event: data\ndata: {\"foo\":\"bar\"}\n\nevent: data\ndata: {\"x\":1}\n\n";
        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .and(query_param("query", "_severity=error"))
            .and(bearer_token("flt_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=_severity%3Derror")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert_eq!(resp.headers().get("x-accel-buffering").unwrap(), "no");

        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), sse_body.as_bytes());
    }

    #[tokio::test]
    async fn stream_rejects_missing_cookie() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stream_maps_upstream_401_to_401() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(401))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stream_upstream_401_with_session_preserves_cookie() {
        // A stream 401 means the upstream key is dead. The stream proxy still
        // leaves cookie lifecycle to `auth::me`.
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(401))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "a proxied SSE 401 must NOT clear the shared fleet_session cookie"
        );
    }

    #[tokio::test]
    async fn stream_upstream_403_preserves_cookie() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(403))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "stream upstream 403 must NOT clear the shared cookie"
        );
    }

    #[tokio::test]
    async fn stream_preserves_upstream_400_for_bad_query() {
        // Invalid DSL is a user error, not a proxy error — we must
        // preserve trawld's 400 so the client can render the real message.
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid DSL"})),
            )
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=junk")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Content-Type from upstream is preserved (not forced to
        // text/event-stream), so the browser gets a parseable error body.
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), br#"{"error":"invalid DSL"}"#);
    }

    #[tokio::test]
    async fn stream_preserves_upstream_429_for_rate_limit() {
        // Stream concurrency limit is an actionable 429 that the UI
        // should render with backoff messaging — not a generic 502.
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(429))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn stream_upstream_500_is_not_masked_as_502() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(500))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// trawld's request-limit refusal on a stream route (ADR-0054).
    const REFUSAL: &str = r#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP request limit ([server] max_concurrent_requests); the request was not processed; retry later with backoff"}}"#;

    /// The request id trawld put on [`REFUSAL`].
    const REQUEST_ID: &str = "0b5f3c1e-request-limit-id";

    /// Open `uri` through the proxy against an upstream that answers
    /// `upstream_path` with trawld's 503 `request_limit_reached`, and check
    /// the browser gets it as trawld sent it: status, body bytes,
    /// `Content-Type`, `Cache-Control: no-store` and the request id, with no
    /// SSE header added. The upstream sees exactly one request: no retry.
    async fn assert_stream_relays_request_limit_refusal(uri: &str, upstream_path: &str) {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);
        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path(upstream_path))
            .and(bearer_token("flt_token"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("cache-control", "no-store")
                    .insert_header("x-request-id", REQUEST_ID)
                    .set_body_raw(REFUSAL, "application/json"),
            )
            .expect(1)
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        let headers = resp.headers();
        assert_eq!(
            headers
                .get_all(header::CACHE_CONTROL)
                .iter()
                .collect::<Vec<_>>(),
            ["no-store"],
            "{uri}"
        );
        assert_eq!(headers.get("x-request-id").unwrap(), REQUEST_ID, "{uri}");
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json",
            "{uri}"
        );
        assert!(headers.get("x-accel-buffering").is_none(), "{uri}");
        assert!(headers.get(header::RETRY_AFTER).is_none(), "{uri}");
        assert!(!headers.contains_key(header::SET_COOKIE), "{uri}");
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), REFUSAL.as_bytes(), "{uri}");

        let relayed = upstream
            .mock()
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == upstream_path)
            .count();
        assert_eq!(relayed, 1, "{uri}: one upstream request, no retry");
    }

    #[tokio::test]
    async fn stream_forwards_request_limit_refusal() {
        assert_stream_relays_request_limit_refusal("/api/v1/stream?query=*", "/api/v1/stream")
            .await;
    }

    #[tokio::test]
    async fn dashboard_stream_forwards_request_limit_refusal() {
        assert_stream_relays_request_limit_refusal(
            "/api/v1/dashboard/stream",
            "/api/v1/dashboard/stream",
        )
        .await;
    }

    #[test]
    fn remaining_ttl_positive_when_exp_in_future() {
        assert_eq!(remaining_ttl(1100, 1000), Duration::from_secs(100));
        assert_eq!(remaining_ttl(1001, 1000), Duration::from_secs(1));
    }

    #[test]
    fn remaining_ttl_zero_when_exp_at_or_past_now() {
        // Defensive clamp — the extractor rejects expired sessions so
        // reaching here with exp <= now is a sign of clock skew or a
        // race. take_until(Duration::ZERO) ends the stream immediately,
        // which is the correct failure mode.
        assert_eq!(remaining_ttl(1000, 1000), Duration::ZERO);
        assert_eq!(remaining_ttl(999, 1000), Duration::ZERO);
        assert_eq!(remaining_ttl(-1_000_000, 1000), Duration::ZERO);
    }

    #[test]
    fn remaining_ttl_handles_max_exp() {
        // Far-future session (100 years) must not overflow.
        let far_future = 1_000 + 3_153_600_000; // ~100y in seconds
        assert_eq!(
            remaining_ttl(far_future, 1000),
            Duration::from_hours(100 * 365 * 24)
        );
    }

    #[tokio::test]
    async fn stream_deadline_cuts_long_running_body() {
        // End-to-end smoke: session with exp ~1s in the future, upstream
        // returns a body that would otherwise stream indefinitely. The
        // response body must EOF before ~2s elapse.
        use fleet_auth::{SessionExpiry, SessionPayload, encrypt};
        use zeroize::Zeroizing;

        let upstream = TlsUpstream::start().await;
        // Upstream body can be short; the point is that take_until fires
        // regardless of whether the body is still arriving.
        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: data\ndata: first\n\n")
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(upstream.mock())
            .await;

        let state = state_pointing_at(&upstream);
        let app = routes::build(state.clone());

        // Hand-craft a session cookie with exp = now + 1 so we skip the
        // login round-trip and get a deterministic short TTL.
        let now = chrono::Utc::now().timestamp();
        let payload = SessionPayload {
            token: Zeroizing::new("flt_test".to_string()),
            name: "test".into(),
            exp: SessionExpiry::from_unix_seconds(now + 1),
        };
        let cookie_value = encrypt(state.cookie_key(), &payload).unwrap();
        let cookie = format!("fleet_session={cookie_value}");

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();

        let start = std::time::Instant::now();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Fully drain the body — with the cap, this must terminate cleanly.
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "stream should close at session.exp (~1s), took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn dashboard_stream_forwards_body_with_sse_headers() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        let sse_body = "event: stats\ndata: {\"uptime_secs\":42}\n\n";
        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard/stream"))
            .and(bearer_token("flt_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/dashboard/stream")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert_eq!(resp.headers().get("x-accel-buffering").unwrap(), "no");

        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), sse_body.as_bytes());
    }

    #[tokio::test]
    async fn dashboard_stream_rejects_missing_cookie() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/dashboard/stream")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn dashboard_stream_upstream_401_preserves_cookie() {
        // Model a dead upstream key on the admin stream. The proxy preserves
        // the shared cookie until `auth::me` performs its `/whoami` check.
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard/stream"))
            .respond_with(ResponseTemplate::new(401))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/dashboard/stream")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "a proxied dashboard-stream 401 must NOT clear the shared fleet_session cookie"
        );
    }

    #[tokio::test]
    async fn stream_accepts_bearer_header() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let sse_body = "event: data\ndata: {\"x\":1}\n\n";
        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .and(query_param("query", "*"))
            .and(bearer_token("flt_direct"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("authorization", "Bearer flt_direct")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), sse_body.as_bytes());
    }
}
