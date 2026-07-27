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
//! after a drop is automatic and rides the same session cookie.

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
use crate::routes::proxy::clear_cookie_for_proxied_response;
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

    let upstream_resp = state
        .http()
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

    let upstream_resp = state
        .http()
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
    // Mirror the upstream status verbatim. The previous implementation
    // routed non-2xx through `ProxyError::Upstream`, whose `IntoResponse`
    // collapses everything that isn't 401/403 into 502 — so trawld's
    // 400 (invalid DSL) or 429 (stream-concurrency limit) would surface
    // as a vague "upstream error" to the browser. The generic
    // `proxy::forward` gets this right; we match its behavior here.
    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let upstream_ct = upstream_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let byte_stream = upstream_resp.bytes_stream();

    // Cap the stream at `session.exp`. Without this, a browser that opens
    // `/api/v1/stream` moments before expiry would keep receiving events
    // indefinitely after the cookie becomes unusable for any other
    // request — the extractor checks expiry once at handler entry, but
    // `Body::from_stream` otherwise has no deadline. Dropping the
    // upstream stream also cleanly closes the TCP connection via
    // reqwest's drop handling.
    let ttl = match auth {
        Auth::Session(s) => remaining_ttl(s.exp(), chrono::Utc::now().timestamp()),
        Auth::Bearer(_) => Duration::from_secs(state.session_ttl_secs()),
    };
    let deadline = Instant::now() + ttl;
    let capped_stream = byte_stream.take_until(sleep_until(deadline));

    // For non-2xx, preserve the upstream Content-Type (typically
    // application/json for structured error bodies) so the browser
    // sees a real error response rather than a truncated SSE stream.
    let content_type = if status.is_success() {
        "text/event-stream".to_string()
    } else {
        upstream_ct.unwrap_or_else(|| "application/json".to_string())
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        // Disable proxy buffering (e.g. nginx) upstream of us. Trawld sets
        // this too but we re-set it to be resilient to misconfigured
        // intermediate proxies when trawl-web is fronted by another one.
        .header("X-Accel-Buffering", "no");

    // The SSE path shares the proxy-wide 401/403 cookie rule; see
    // `clear_cookie_for_proxied_response`. (Today it never clears — an
    // EventSource reconnect keeps 401ing until the SPA's next `/me` poll
    // drops the dead cookie, the permission-aware place to make that call.)
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
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::ResolvedConfig;
    use crate::routes;

    fn state_pointing_at(upstream: &MockServer) -> AppState {
        let web = WebConfig {
            upstream_url: Some(upstream.uri()),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap()
    }

    async fn login_cookie(app: axum::Router, upstream: &MockServer) -> String {
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "prefix": "testtest",
                "name": "alice",
                "kind": "human",
                "roles": ["trawl-analyst"],
                "permissions": ["query", "schema_read", "validate", "saved_query", "export", "stream", "query_cancel"]
            })))
            .mount(upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        let sse_body = "event: data\ndata: {\"foo\":\"bar\"}\n\nevent: data\ndata: {\"x\":1}\n\n";
        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .and(query_param("query", "level=error"))
            .and(bearer_token("flt_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&upstream)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=level%3Derror")
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
        let upstream = MockServer::start().await;
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
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
        // trawld's 401 on the SSE path is just as ambiguous as on any other
        // proxied route — it covers both a dead key and a live key lacking
        // the `stream` permission. Clearing on it would sign valid users out
        // of the whole fleet on a routine authz denial, so the stream path
        // must NOT touch the shared cookie. `auth::me` (permission-free
        // /whoami) remains the sole authority for the cookie lifecycle.
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid DSL"})),
            )
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&upstream)
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

        let upstream = MockServer::start().await;
        // Upstream body can be short; the point is that take_until fires
        // regardless of whether the body is still arriving.
        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: data\ndata: first\n\n")
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
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
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
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
        // trawld 401s this path for every non-admin session (ServerManage
        // check), so the shared-cookie rule matters most here: a routine
        // authz denial must never sign the user out of the whole fleet.
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/dashboard/stream"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
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
            .mount(&upstream)
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
