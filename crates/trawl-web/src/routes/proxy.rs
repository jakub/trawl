// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generic `/api/v1/*` → trawld pass-through.

use axum::body::{Body, Bytes, HttpBody as _};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use futures::StreamExt as _;
use trawl_api::ErrorCode;

use crate::body_budget::{BODY_BUDGET_NAME, BodyBudget, BodyReservation};
use crate::config::ProxyLimits;
use crate::error::{ProxyError, envelope};
use crate::middleware::body_deadline::BodyDeadlineElapsed;
use crate::middleware::session_extractor::Auth;
use crate::state::AppState;

pub const BODY_LIMIT_SETTING: &str = "[server] max_request_body_bytes";

/// Headers that are hop-by-hop per RFC 7230 §6.1 and MUST NOT be forwarded
/// end-to-end. Plus `host` which we always rewrite.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
];

pub async fn not_found() -> Response {
    envelope(StatusCode::NOT_FOUND, ErrorCode::NotFound, "not found")
}

pub async fn block_ingest() -> Response {
    not_found().await
}

pub async fn forward(
    State(state): State<AppState>,
    auth: Auth,
    req: Request<Body>,
) -> Result<Response, ProxyError> {
    do_forward(&state, auth, req).await
}

async fn do_forward(
    state: &AppState,
    auth: Auth,
    req: Request<Body>,
) -> Result<Response, ProxyError> {
    let (parts, body) = req.into_parts();

    // CSRF defense already ran: `auth` is here, which means the request
    // either carried a valid bearer header (no cookie, so not a CSRF
    // target) or passed the origin guard in the `Session` extractor.
    let upstream_uri = build_upstream_uri(state.upstream_url(), &parts.uri)?;

    let body_bytes =
        collect_body(&parts.headers, body, state.limits(), state.body_budget()).await?;

    // After every local check, so a request refused here never waits on
    // the pin file.
    let http = state.upstream_client().await?;

    let mut upstream_req = http
        .request(reqwest_method(&parts.method), upstream_uri)
        .bearer_auth(auth.token());

    for (name, value) in &parts.headers {
        if HOP_BY_HOP
            .iter()
            .any(|h| name.as_str().eq_ignore_ascii_case(h))
            || name == header::COOKIE
            || name == header::AUTHORIZATION
        {
            continue;
        }
        upstream_req = upstream_req.header(name.as_str(), value);
    }

    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes);
    }

    let upstream_resp = state.send_to_upstream(upstream_req).await?;
    refuse_redirect(upstream_resp.status())?;
    relay_response(upstream_resp, &auth)
}

pub(crate) fn relay_response(
    upstream_resp: reqwest::Response,
    auth: &Auth,
) -> Result<Response, ProxyError> {
    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let upstream_headers = upstream_resp.headers().clone();
    let body_stream = upstream_resp.bytes_stream();

    let mut out = Response::builder().status(status);
    let out_headers = out.headers_mut().expect("fresh response has headers map");
    copy_response_headers(&upstream_headers, out_headers);

    if let Some(clear) = clear_cookie_for_proxied_response(status, auth) {
        out_headers.insert(header::SET_COOKIE, clear);
    }

    out.body(Body::from_stream(body_stream))
        .map_err(|e| ProxyError::Internal(format!("response build: {e}")))
}

#[must_use]
pub(crate) fn clear_cookie_for_proxied_response(
    _status: StatusCode,
    _auth: &Auth,
) -> Option<HeaderValue> {
    None
}

/// trawld never redirects, and the client follows no redirect. Mirroring the
/// 3xx would let the browser follow it instead: a `Location` naming another
/// port on the browser's host would carry the host-scoped session cookie
/// there, since a cookie's scope ignores the port. Every 3xx is refused, 304
/// included: trawld sends no validators, so a 304 is not an answer it
/// gives.
pub(crate) fn refuse_redirect(status: reqwest::StatusCode) -> Result<(), ProxyError> {
    if status.is_redirection() {
        return Err(ProxyError::Upstream(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        ));
    }
    Ok(())
}

async fn collect_body(
    headers: &HeaderMap,
    body: Body,
    limits: &ProxyLimits,
    budget: &BodyBudget,
) -> Result<Bytes, ProxyError> {
    let limit = limits.request_body_bytes;
    let (cap, cap_name) = if limits.body_budget_bytes < limit {
        (limits.body_budget_bytes, BODY_BUDGET_NAME)
    } else {
        (limit, BODY_LIMIT_SETTING)
    };
    let too_large = |setting, limit| ProxyError::RequestTooLarge { setting, limit };
    let announced: Option<usize> = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok());
    if announced.is_some_and(|length| length > limit) {
        return Err(too_large(BODY_LIMIT_SETTING, limit));
    }
    // The length hyper framed the body with: exact for a `Content-Length`
    // body and for no body at all, unknown for a chunked one. hyper never
    // yields more than it framed.
    let hint = body.size_hint();
    let framed = hint
        .exact()
        .or(hint.upper())
        .map(|length| usize::try_from(length).unwrap_or(usize::MAX));
    let reserved = framed.or(announced).unwrap_or(cap);
    if reserved > cap {
        return Err(too_large(cap_name, cap));
    }
    let reservation = budget
        .try_reserve(reserved)
        .ok_or(ProxyError::BodyBudgetFull)?;

    let mut stream = body.into_data_stream();
    let mut buffer: Vec<u8> = Vec::with_capacity(reserved);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            let e = e.into_inner();
            if e.is::<BodyDeadlineElapsed>() {
                ProxyError::BodyTimeout
            } else {
                ProxyError::BadRequest(format!("request body: {e}"))
            }
        })?;
        if buffer.len().saturating_add(chunk.len()) > reserved {
            // Past the reservation, so past the buffer. A body of unknown
            // length was reserved the cap, so this is the byte that
            // crosses it; a declared body that runs past its declaration
            // is malformed.
            return Err(if reserved == cap {
                too_large(cap_name, cap)
            } else {
                ProxyError::BadRequest("request body longer than its declared length".into())
            });
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from_owner(ReservedBody {
        bytes: buffer,
        _reservation: reservation,
    }))
}

/// A buffered request body and the budget it was charged, as one owner
/// for the `Bytes` reqwest sends (ADR-0054).
struct ReservedBody {
    bytes: Vec<u8>,
    _reservation: BodyReservation,
}

impl AsRef<[u8]> for ReservedBody {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

fn build_upstream_uri(base: &str, orig: &Uri) -> Result<String, ProxyError> {
    let path_and_query = orig
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .ok_or_else(|| ProxyError::Internal("request URI missing path".into()))?;
    Ok(format!("{}{path_and_query}", base.trim_end_matches('/')))
}

fn reqwest_method(m: &Method) -> reqwest::Method {
    reqwest::Method::from_bytes(m.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

/// Mirror upstream response headers, minus hop-by-hop headers,
/// `Set-Cookie` (trawld owns no browser cookie) and `Location` (the proxy
/// never points the browser anywhere on trawld's say-so; see
/// [`refuse_redirect`]).
fn copy_response_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    for (name, value) in src {
        if HOP_BY_HOP
            .iter()
            .any(|h| name.as_str().eq_ignore_ascii_case(h))
            || name == reqwest::header::SET_COOKIE
            || name == reqwest::header::LOCATION
        {
            continue;
        }
        if let (Ok(h_name), Ok(h_value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            dst.append(h_name, h_value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;
    use trawl_config::WebConfig;
    use wiremock::matchers::{bearer_token, body_bytes, method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    use crate::body_budget::PROXY_BODY_BUDGET;
    use crate::config::ResolvedConfig;
    use crate::middleware::request_count::{PROXY_MAX_REQUESTS, RequestCount};
    use crate::routes;
    use crate::test_support::{HandUpstream, TlsUpstream, wedge_body_bytes, wedge_body_bytes_for};
    use crate::test_timing::{frozen, until};

    const TEST_ORIGIN: &str = "https://trawl.fleet.test";

    fn state_pointing_at(upstream: &TlsUpstream) -> AppState {
        let web = WebConfig {
            allow_insecure_cookies: true,
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..upstream.web_config()
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    fn build_app(state: AppState) -> Router {
        routes::build(state)
    }

    fn counted_app(upstream: &HandUpstream, count: usize) -> Router {
        let web = WebConfig {
            allow_insecure_cookies: true,
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..upstream.web_config()
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        routes::build_with_count(
            AppState::from_config(cfg).unwrap(),
            RequestCount::new(count),
        )
    }

    fn limited_app(
        upstream: &HandUpstream,
        count: usize,
        adjust: impl FnOnce(&mut ResolvedConfig),
    ) -> Router {
        limited_app_under(upstream, RequestCount::new(count), adjust)
    }

    fn limited_app_under(
        upstream: &HandUpstream,
        count: RequestCount,
        adjust: impl FnOnce(&mut ResolvedConfig),
    ) -> Router {
        let web = WebConfig {
            allow_insecure_cookies: true,
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..upstream.web_config()
        };
        let mut cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        adjust(&mut cfg);
        routes::build_with_count(AppState::from_config(cfg).unwrap(), count)
    }

    fn limited_state(
        upstream: &HandUpstream,
        adjust: impl FnOnce(&mut ResolvedConfig),
    ) -> AppState {
        let web = WebConfig {
            allow_insecure_cookies: true,
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..upstream.web_config()
        };
        let mut cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        adjust(&mut cfg);
        AppState::from_config(cfg).unwrap()
    }

    fn stalled_post(path: &str) -> Request<Body> {
        let stalled = futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from("{")) })
            .chain(futures::stream::pending());
        bearer_post(path, Body::from_stream(stalled))
    }

    fn stalled_login() -> Request<Body> {
        let stalled = futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from("{")) })
            .chain(futures::stream::pending());
        Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("origin", TEST_ORIGIN)
            .header("content-type", "application/json")
            .body(Body::from_stream(stalled))
            .unwrap()
    }

    fn bearer_post(path: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", "Bearer flt_direct")
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    async fn assert_envelope_refusal(
        resp: Response,
        status: StatusCode,
        code: trawl_api::ErrorCode,
        message: &str,
    ) {
        assert_eq!(resp.status(), status);
        let headers = resp.headers().clone();
        assert_eq!(
            headers
                .get_all(header::CACHE_CONTROL)
                .iter()
                .collect::<Vec<_>>(),
            ["no-store"]
        );
        assert!(headers.get(header::SET_COOKIE).is_none());
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let envelope: trawl_api::ErrorResponse = serde_json::from_slice(&body)
            .unwrap_or_else(|e| panic!("not an envelope ({e}): {body:?}"));
        assert_eq!(envelope.error.code, code);
        assert_eq!(envelope.error.message, message);
    }

    fn bearer_get(path: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(path)
            .header("authorization", "Bearer flt_direct")
            .body(Body::empty())
            .unwrap()
    }

    async fn assert_proxy_request_limit_refusal(resp: Response) {
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let headers = resp.headers().clone();
        assert_eq!(
            headers
                .get_all(header::CACHE_CONTROL)
                .iter()
                .collect::<Vec<_>>(),
            ["no-store"]
        );
        assert!(headers.get(header::RETRY_AFTER).is_none());
        assert!(headers.get(header::SET_COOKIE).is_none());
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "request_limit_reached", "{body}");
        assert_eq!(
            body["error"]["message"],
            crate::error::REQUEST_LIMIT_MESSAGE,
            "{body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("trawl-web"),
            "the message names the hop: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_requests_over_the_count_get_503_request_limit_reached() {
        let upstream = HandUpstream::start().await;
        upstream.hold();
        let app = counted_app(&upstream, 2);

        let held: Vec<_> = (0..2)
            .map(|_| tokio::spawn(app.clone().oneshot(bearer_get("/api/v1/schema"))))
            .collect();
        upstream.wait_for_requests(2).await;

        // Under a deadline: a proxy without the count would forward this
        // to the held upstream and wait there instead of failing.
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(bearer_get("/api/v1/stats")),
        )
        .await
        .expect("the refusal is immediate")
        .unwrap();
        assert_proxy_request_limit_refusal(refused).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            upstream.seen(),
            vec!["GET /api/v1/schema", "GET /api/v1/schema"],
            "the upstream saw only the held requests"
        );

        upstream.release();
        for request in held {
            let resp = request.await.unwrap().unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        let later = app.oneshot(bearer_get("/api/v1/stats")).await.unwrap();
        assert_eq!(later.status(), StatusCode::OK, "the count is free again");
        assert_eq!(upstream.seen().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_healthz_is_never_counted() {
        let upstream = HandUpstream::start().await;
        upstream.hold();
        let app = counted_app(&upstream, 1);

        let held = tokio::spawn(app.clone().oneshot(bearer_get("/api/v1/schema")));
        upstream.wait_for_requests(1).await;

        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(bearer_get("/api/v1/stats")),
        )
        .await
        .expect("the refusal is immediate")
        .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the count is full"
        );
        for _ in 0..3 {
            let probe = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/healthz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(probe.status(), StatusCode::OK);
            let body = axum::body::to_bytes(probe.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(&body[..], b"ok");
        }

        upstream.release();
        assert_eq!(held.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_count_is_released_at_the_response_head() {
        let upstream = HandUpstream::start().await;
        let app = counted_app(&upstream, 1);

        let stream = app
            .clone()
            .oneshot(bearer_get("/api/v1/stream?query=_severity%3Derror"))
            .await
            .unwrap();
        assert_eq!(stream.status(), StatusCode::OK);
        assert_eq!(
            stream.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let mut body = stream.into_body().into_data_stream();
        let first =
            tokio::time::timeout(Duration::from_secs(5), futures::StreamExt::next(&mut body))
                .await
                .expect("an event before the deadline")
                .expect("the stream is open")
                .unwrap();
        assert!(first.starts_with(b"data: "), "{first:?}");

        let next = app.oneshot(bearer_get("/api/v1/schema")).await.unwrap();
        assert_eq!(
            next.status(),
            StatusCode::OK,
            "an open stream held the count past its head"
        );
        let later =
            tokio::time::timeout(Duration::from_secs(5), futures::StreamExt::next(&mut body))
                .await
                .expect("another event before the deadline")
                .expect("the stream is still open")
                .unwrap();
        assert!(later.starts_with(b"data: "), "{later:?}");
        drop(body);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_over_server_limit_gets_413_without_upstream_call() {
        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 4, |cfg| cfg.limits.request_body_bytes = 64);
        let sentence = "request body exceeds [server] max_request_body_bytes (64 bytes)";

        let announced = Request::builder()
            .method("POST")
            .uri("/api/v1/search")
            .header("authorization", "Bearer flt_direct")
            .header("content-type", "application/json")
            .header("content-length", "65")
            .body(Body::from(vec![b'x'; 65]))
            .unwrap();
        let resp = app.clone().oneshot(announced).await.unwrap();
        assert_envelope_refusal(
            resp,
            StatusCode::PAYLOAD_TOO_LARGE,
            trawl_api::ErrorCode::RequestTooLarge,
            sentence,
        )
        .await;

        let chunks = futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 60])),
            Ok(Bytes::from(vec![b'x'; 5])),
        ]);
        let resp = app
            .clone()
            .oneshot(bearer_post("/api/v1/search", Body::from_stream(chunks)))
            .await
            .unwrap();
        assert_envelope_refusal(
            resp,
            StatusCode::PAYLOAD_TOO_LARGE,
            trawl_api::ErrorCode::RequestTooLarge,
            sentence,
        )
        .await;
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");

        let resp = app
            .oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; 64])))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        upstream.wait_for_requests(1).await;
        assert_eq!(upstream.seen(), ["POST /api/v1/search"]);
    }

    #[test]
    fn proxy_body_limit_defaults_to_trawld_default() {
        let limits = crate::config::ProxyLimits::from_server(None);
        assert_eq!(
            limits.request_body_bytes,
            trawl_config::DEFAULT_MAX_REQUEST_BODY_BYTES
        );
        assert_eq!(limits.body_read_timeout, Duration::from_secs(30));
        assert_eq!(limits.upstream_head_timeout, Duration::from_secs(40));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_returns_504_timeout_when_upstream_head_is_late() {
        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 1, |cfg| {
            cfg.limits.upstream_head_timeout = Duration::from_millis(200);
        });
        upstream.hold();

        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the deadline answered the request")
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(160),
            "answered before the deadline: {:?}",
            started.elapsed()
        );
        assert_envelope_refusal(
            resp,
            StatusCode::GATEWAY_TIMEOUT,
            trawl_api::ErrorCode::Timeout,
            crate::error::UPSTREAM_HEAD_TIMEOUT_MESSAGE,
        )
        .await;

        upstream.release();
        let next = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the next request was answered")
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the count was not released");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_sse_outlives_the_head_deadline() {
        let upstream = HandUpstream::start().await;
        let deadline = Duration::from_millis(200);
        let app = limited_app(&upstream, 4, |cfg| {
            cfg.limits.upstream_head_timeout = deadline;
        });

        let stream = app
            .oneshot(bearer_get("/api/v1/stream?query=_severity%3Derror"))
            .await
            .unwrap();
        assert_eq!(stream.status(), StatusCode::OK);
        let mut body = stream.into_body().into_data_stream();
        let started = std::time::Instant::now();
        let mut events = 0_usize;
        while started.elapsed() < deadline * 3 {
            let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
                .await
                .expect("an event before the deadline")
                .expect("the stream is still open")
                .unwrap();
            assert!(chunk.starts_with(b"data: "), "{chunk:?}");
            events += 1;
        }
        assert!(
            events > 3,
            "only {events} events in {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_body_over_limit_is_refused() {
        let upstream = HandUpstream::start().await;
        let app = counted_app(&upstream, 4);

        let padding = "x".repeat(16 * 1024);
        let body = format!(r#"{{"api_key":"flt_direct","padding":"{padding}"}}"#);
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("origin", TEST_ORIGIN)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_envelope_refusal(
            resp,
            StatusCode::PAYLOAD_TOO_LARGE,
            trawl_api::ErrorCode::RequestTooLarge,
            "request body exceeds the login body limit (16384 bytes)",
        )
        .await;
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_declared_length_over_limit_is_refused_for_any_content_type() {
        let upstream = HandUpstream::start().await;
        let app = counted_app(&upstream, 4);

        let login = |content_type: &str, len: usize| {
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("origin", TEST_ORIGIN)
                .header("content-type", content_type)
                .header("content-length", len.to_string())
                .body(Body::from(vec![b'x'; len]))
                .unwrap()
        };

        for content_type in ["text/plain", "application/json", "application/xml"] {
            let resp = app
                .clone()
                .oneshot(login(content_type, 16 * 1024 + 1))
                .await
                .unwrap();
            assert_envelope_refusal(
                resp,
                StatusCode::PAYLOAD_TOO_LARGE,
                trawl_api::ErrorCode::RequestTooLarge,
                "request body exceeds the login body limit (16384 bytes)",
            )
            .await;
        }

        let at_limit = app.oneshot(login("text/plain", 16 * 1024)).await.unwrap();
        assert_eq!(
            at_limit.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "a body at the limit is not refused for its size"
        );
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_chunked_body_over_limit_is_refused() {
        let upstream = HandUpstream::start().await;
        let app = counted_app(&upstream, 4);

        let chunked = |content_type: &str| {
            let chunks = (0..17).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 1024])));
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("origin", TEST_ORIGIN)
                .header("content-type", content_type)
                .body(Body::from_stream(futures::stream::iter(chunks)))
                .unwrap()
        };

        for content_type in ["application/json", "text/plain"] {
            let resp = app.clone().oneshot(chunked(content_type)).await.unwrap();
            assert_envelope_refusal(
                resp,
                StatusCode::PAYLOAD_TOO_LARGE,
                trawl_api::ErrorCode::RequestTooLarge,
                "request body exceeds the login body limit (16384 bytes)",
            )
            .await;
        }
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
    }

    /// `Router::oneshot` skips the framing rules that decide which headers
    /// the router sees.
    async fn serve_on_loopback(
        app: Router,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(crate::listener::serve(
            listener,
            app,
            crate::listener::ConnectionTimeouts::DEFAULT,
            async {
                let _ = stop.await;
            },
        ));
        (addr, shutdown, task)
    }

    async fn raw_chunked_login(
        addr: std::net::SocketAddr,
        head_lines: &[&str],
        chunks: &[&[u8]],
    ) -> (u16, String) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut request = format!(
            "POST /api/auth/login HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\
             origin: {TEST_ORIGIN}\r\n"
        );
        for line in head_lines {
            request.push_str(line);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        let mut bytes = request.into_bytes();
        for chunk in chunks {
            bytes.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            bytes.extend_from_slice(chunk);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"0\r\n\r\n");

        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream.write_all(&bytes).await.expect("write the request");
        let mut raw = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
            .await
            .expect("the answer before the timeout")
            .expect("read the answer");
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").expect("a complete head");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status in {head:?}"));
        (status, body.to_owned())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_chunked_plain_text_over_limit_is_413_on_the_wire() {
        let upstream = HandUpstream::start().await;
        let (addr, _shutdown, task) = serve_on_loopback(counted_app(&upstream, 4)).await;

        let chunk = vec![b'x'; 1024];
        let chunks: Vec<&[u8]> = (0..17).map(|_| chunk.as_slice()).collect();
        for content_type in ["text/plain", "application/json"] {
            let (status, body) = raw_chunked_login(
                addr,
                &[
                    &format!("content-type: {content_type}"),
                    "transfer-encoding: chunked",
                ],
                &chunks,
            )
            .await;
            assert_eq!(status, 413, "{content_type}: {body}");
            assert!(body.contains("request_too_large"), "{content_type}: {body}");
            assert!(
                body.contains("the login body limit (16384 bytes)"),
                "{content_type}: {body}"
            );
        }
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
        task.abort();
    }

    /// hyper gives the chunked framing precedence and hands the router no
    /// `Content-Length`, so the declared length cannot be what refuses it.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_chunked_with_content_length_over_limit_is_413_in_either_order() {
        let upstream = HandUpstream::start().await;
        let (addr, _shutdown, task) = serve_on_loopback(counted_app(&upstream, 4)).await;

        let chunk = vec![b'x'; 1024];
        let chunks: Vec<&[u8]> = (0..17).map(|_| chunk.as_slice()).collect();
        let orders: [[&str; 3]; 2] = [
            [
                "content-type: text/plain",
                "transfer-encoding: chunked",
                "content-length: 16385",
            ],
            [
                "content-type: text/plain",
                "content-length: 16385",
                "transfer-encoding: chunked",
            ],
        ];
        for head in orders {
            let (status, body) = raw_chunked_login(addr, &head, &chunks).await;
            assert_eq!(status, 413, "{head:?}: {body}");
            assert!(body.contains("request_too_large"), "{head:?}: {body}");
        }
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_chunked_json_within_limit_succeeds_on_the_wire() {
        let upstream = HandUpstream::start().await;
        let (addr, _shutdown, task) = serve_on_loopback(counted_app(&upstream, 4)).await;

        let (status, body) = raw_chunked_login(
            addr,
            &[
                "content-type: application/json",
                "transfer-encoding: chunked",
            ],
            &[br#"{"api_key":"#, br#""flt_direct"}"#],
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(upstream.seen(), ["GET /api/v1/whoami"]);
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_stalled_body_is_408_on_the_wire() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 4, |cfg| {
            cfg.limits.body_read_timeout = Duration::from_millis(300);
        });
        let (addr, _shutdown, task) = serve_on_loopback(app).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let head = format!(
            "POST /api/auth/login HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\
             origin: {TEST_ORIGIN}\r\ncontent-type: text/plain\r\n\
             transfer-encoding: chunked\r\n\r\n5\r\nhello\r\n"
        );
        stream.write_all(head.as_bytes()).await.expect("write");
        let started = std::time::Instant::now();
        let mut raw = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
            .await
            .expect("the deadline answered the stalled login")
            .expect("read the answer");
        let raw = String::from_utf8_lossy(&raw);
        assert!(raw.starts_with("HTTP/1.1 408"), "{raw}");
        assert!(raw.contains("\"timeout\""), "{raw}");
        assert!(
            started.elapsed() >= Duration::from_millis(240),
            "answered before the deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_login_within_limit_still_succeeds() {
        let upstream = HandUpstream::start().await;
        let app = counted_app(&upstream, 4);

        let body = r#"{"api_key":"flt_direct"}"#;
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("origin", TEST_ORIGIN)
            .header("content-type", "application/json")
            .header("content-length", body.len().to_string())
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get(header::SET_COOKIE).is_some());
        assert_eq!(upstream.seen(), ["GET /api/v1/whoami"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_collection_deadline_releases_slot() {
        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 1, |cfg| {
            cfg.limits.body_read_timeout = Duration::from_millis(200);
        });

        let stalled = futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from("{")) })
            .chain(futures::stream::pending());
        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone()
                .oneshot(bearer_post("/api/v1/search", Body::from_stream(stalled))),
        )
        .await
        .expect("the deadline answered the request")
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(160),
            "answered before the deadline: {:?}",
            started.elapsed()
        );
        assert_envelope_refusal(
            resp,
            StatusCode::REQUEST_TIMEOUT,
            trawl_api::ErrorCode::Timeout,
            crate::error::BODY_TIMEOUT_MESSAGE,
        )
        .await;
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");

        let next = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the next request was answered")
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the count was not released");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_stalled_login_body_releases_slot_and_answers_408() {
        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 1, |cfg| {
            cfg.limits.body_read_timeout = Duration::from_millis(200);
        });

        let started = std::time::Instant::now();
        let resp =
            tokio::time::timeout(Duration::from_secs(5), app.clone().oneshot(stalled_login()))
                .await
                .expect("the deadline answered the stalled login")
                .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(160),
            "answered before the deadline: {:?}",
            started.elapsed()
        );
        assert_envelope_refusal(
            resp,
            StatusCode::REQUEST_TIMEOUT,
            trawl_api::ErrorCode::Timeout,
            crate::error::BODY_TIMEOUT_MESSAGE,
        )
        .await;
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");

        let next = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the next request was answered")
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the count was not released");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn proxy_stalled_logins_release_the_whole_count_at_the_deadline() {
        const DEADLINE: Duration = Duration::from_millis(500);
        let upstream = HandUpstream::start().await;
        let count = RequestCount::new(PROXY_MAX_REQUESTS);
        let app = limited_app_under(&upstream, count.clone(), |cfg| {
            cfg.limits.body_read_timeout = DEADLINE;
        });

        let stalled: Vec<_> = (0..PROXY_MAX_REQUESTS)
            .map(|_| tokio::spawn(app.clone().oneshot(stalled_login())))
            .collect();
        until("every stalled login took a count", || {
            count.available() == 0
        })
        .await;

        let refused = frozen(
            "the refusal",
            app.clone().oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .unwrap();
        assert_proxy_request_limit_refusal(refused).await;

        tokio::time::advance(DEADLINE + Duration::from_millis(10)).await;
        for login in stalled {
            let resp = frozen("the deadline answered the stalled login", login)
                .await
                .unwrap()
                .unwrap();
            assert_envelope_refusal(
                resp,
                StatusCode::REQUEST_TIMEOUT,
                trawl_api::ErrorCode::Timeout,
                crate::error::BODY_TIMEOUT_MESSAGE,
            )
            .await;
        }
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
        assert_eq!(
            count.available(),
            PROXY_MAX_REQUESTS,
            "a stalled login kept its count"
        );

        let next = frozen(
            "the request after the stalled logins",
            app.oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the count was not released");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn proxy_body_budget_bounds_buffered_bodies_at_an_8_mib_limit() {
        const LIMIT: usize = 8 * 1024 * 1024;
        const ADMITTED: usize = PROXY_BODY_BUDGET / LIMIT;
        const DEADLINE: Duration = Duration::from_millis(500);
        assert_eq!(ADMITTED, 16);
        let upstream = HandUpstream::start().await;
        let count = RequestCount::new(PROXY_MAX_REQUESTS);
        let state = limited_state(&upstream, |cfg| {
            cfg.limits.request_body_bytes = LIMIT;
            cfg.limits.body_read_timeout = DEADLINE;
        });
        let budget = state.body_budget().clone();
        let app = routes::build_with_count(state, count.clone());

        let stalled: Vec<_> = (0..ADMITTED)
            .map(|_| tokio::spawn(app.clone().oneshot(stalled_post("/api/v1/search"))))
            .collect();
        until("every stalled body reserved the limit", || {
            budget.available() == 0
        })
        .await;
        assert_eq!(
            count.available(),
            PROXY_MAX_REQUESTS - ADMITTED,
            "the count still has room: the budget is what is full"
        );

        for request in [
            stalled_post("/api/v1/search"),
            bearer_post("/api/v1/search", Body::from("{}")),
        ] {
            let refused = frozen("the refusal", app.clone().oneshot(request))
                .await
                .unwrap();
            assert_proxy_request_limit_refusal(refused).await;
        }
        let bodiless = frozen(
            "the bodiless request",
            app.clone().oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .unwrap();
        assert_eq!(
            bodiless.status(),
            StatusCode::OK,
            "a bodiless request was charged"
        );

        tokio::time::advance(DEADLINE + Duration::from_millis(10)).await;
        for post in stalled {
            let resp = frozen("the deadline answered the stalled body", post)
                .await
                .unwrap()
                .unwrap();
            assert_envelope_refusal(
                resp,
                StatusCode::REQUEST_TIMEOUT,
                trawl_api::ErrorCode::Timeout,
                crate::error::BODY_TIMEOUT_MESSAGE,
            )
            .await;
        }
        assert_eq!(
            budget.available(),
            PROXY_BODY_BUDGET,
            "a stalled body kept its reservation"
        );
        assert_eq!(count.available(), PROXY_MAX_REQUESTS);

        let next = frozen(
            "the request after the deadline",
            app.oneshot(bearer_post(
                "/api/v1/search",
                Body::from(vec![b'x'; 1024 * 1024]),
            )),
        )
        .await
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the budget was not released");
        assert_eq!(
            upstream.seen(),
            ["GET /api/v1/schema", "POST /api/v1/search"],
            "trawld saw a refused body"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_budget_charges_the_declared_length_until_the_body_is_sent_and_answered() {
        let upstream = HandUpstream::start().await;
        upstream.hold();
        let state = limited_state(&upstream, |cfg| {
            cfg.limits.request_body_bytes = 1024;
            cfg.limits.body_budget_bytes = 1024;
        });
        let budget = state.body_budget().clone();
        let app = routes::build_with_count(state, RequestCount::new(PROXY_MAX_REQUESTS));

        let first = tokio::spawn(
            app.clone()
                .oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; 600]))),
        );
        upstream.wait_for_requests(1).await;
        assert_eq!(
            budget.available(),
            1024 - 600,
            "a declared body reserved its length"
        );

        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone()
                .oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; 500]))),
        )
        .await
        .expect("the refusal is immediate")
        .unwrap();
        assert_proxy_request_limit_refusal(refused).await;
        assert_eq!(
            budget.available(),
            1024 - 600,
            "a refused body kept a reservation"
        );

        let second = tokio::spawn(
            app.clone()
                .oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; 400]))),
        );
        upstream.wait_for_requests(2).await;
        assert_eq!(budget.available(), 24);

        upstream.release();
        for held in [first, second] {
            assert_eq!(held.await.unwrap().unwrap().status(), StatusCode::OK);
        }
        assert_eq!(
            budget.available(),
            1024,
            "a reservation outlived its response head"
        );
        assert_eq!(
            upstream.seen(),
            ["POST /api/v1/search", "POST /api/v1/search"]
        );
    }

    #[test]
    fn wedge_body_is_sized_from_the_hosts_send_buffer_maximum() {
        const MIB: usize = 1024 * 1024;
        assert_eq!(
            wedge_body_bytes_for(Some("4096\t16384\t4194304\n")),
            16 * MIB
        );
        assert_eq!(wedge_body_bytes_for(Some("4096 131072 6291456")), 24 * MIB);
        assert_eq!(wedge_body_bytes_for(Some("4096 87380 33554432")), 128 * MIB);
        assert_eq!(wedge_body_bytes_for(Some("4096 87380 5242881")), 21 * MIB);
        assert_eq!(wedge_body_bytes_for(Some("4096 16384 1048576")), 16 * MIB);
        for unreadable in [None, Some(""), Some("4096 16384"), Some("4096 16384 many")] {
            assert_eq!(wedge_body_bytes_for(unreadable), 16 * MIB, "{unreadable:?}");
        }
        assert!(wedge_body_bytes() >= 16 * MIB);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_budget_is_held_while_the_sent_body_is_still_queued() {
        let size = wedge_body_bytes();
        let budget_bytes = size + size / 2;
        let upstream = HandUpstream::start_narrow().await;
        let state = limited_state(&upstream, |cfg| {
            cfg.limits.request_body_bytes = size;
            cfg.limits.body_budget_bytes = budget_bytes;
        });
        let budget = state.body_budget().clone();
        let app = routes::build_with_count(state, RequestCount::new(PROXY_MAX_REQUESTS));

        let wedged = tokio::time::timeout(
            Duration::from_secs(10),
            app.clone().oneshot(bearer_post(
                HandUpstream::FLOOD_PATH,
                Body::from(vec![b'x'; size]),
            )),
        )
        .await
        .expect("the head arrived")
        .unwrap();
        assert_eq!(wedged.status(), StatusCode::OK);
        assert_eq!(
            budget.available(),
            budget_bytes - size,
            "the body is still queued, yet the budget shows it released"
        );

        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone()
                .oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; size]))),
        )
        .await
        .expect("the refusal is immediate")
        .unwrap();
        assert_proxy_request_limit_refusal(refused).await;

        let admitted = tokio::time::timeout(
            Duration::from_secs(10),
            app.clone().oneshot(bearer_post(
                "/api/v1/search",
                Body::from(vec![b'x'; size / 2]),
            )),
        )
        .await
        .expect("the admitted body was answered")
        .unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);
        assert_eq!(
            upstream.seen(),
            [
                format!("POST {}", HandUpstream::FLOOD_PATH),
                "POST /api/v1/search".to_owned()
            ],
            "trawld saw the refused body"
        );
        assert_eq!(
            budget.available(),
            budget_bytes - size,
            "the wedged body gave its reservation back, or the sent one kept it"
        );
        drop(wedged);
    }

    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_wedged_send_is_torn_down_at_the_head_deadline() {
        const DEADLINE: Duration = Duration::from_secs(2);
        let size = wedge_body_bytes();
        let upstream = HandUpstream::start_narrow().await;
        let state = limited_state(&upstream, |cfg| {
            cfg.limits.request_body_bytes = size;
            cfg.limits.body_budget_bytes = size;
            cfg.limits.upstream_head_timeout = DEADLINE;
        });
        let budget = state.body_budget().clone();
        let app = routes::build_with_count(state, RequestCount::new(PROXY_MAX_REQUESTS));

        let wedged = tokio::time::timeout(
            Duration::from_secs(10),
            app.clone().oneshot(bearer_post(
                HandUpstream::FLOOD_PATH,
                Body::from(vec![b'x'; size]),
            )),
        )
        .await
        .expect("the head arrived")
        .unwrap();
        let head_at = std::time::Instant::now();
        assert_eq!(wedged.status(), StatusCode::OK);
        assert_eq!(
            budget.available(),
            0,
            "the body is still queued, yet the budget shows it released"
        );

        while budget.available() < size {
            assert!(
                head_at.elapsed() < Duration::from_secs(8),
                "the wedged connection kept its body {:?} after the head, past the {DEADLINE:?} deadline",
                head_at.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            head_at.elapsed() >= Duration::from_millis(1_800),
            "the body was freed {:?} after the head, before the deadline could have run out",
            head_at.elapsed()
        );

        let mut body = wedged.into_body().into_data_stream();
        let ending = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match body.next().await {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Some(error),
                    None => return None,
                }
            }
        })
        .await
        .expect("the held response ended");
        assert!(
            ending.is_some(),
            "the held response ended cleanly, as if trawld had finished it"
        );

        let next = tokio::time::timeout(
            Duration::from_secs(10),
            app.oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; size]))),
        )
        .await
        .expect("the request after the teardown was answered")
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the budget was not released");
        assert_eq!(
            upstream.seen(),
            [
                format!("POST {}", HandUpstream::FLOOD_PATH),
                "POST /api/v1/search".to_owned()
            ]
        );
    }

    /// The count middleware takes a count before the body reserves the
    /// budget, so the test waits on the budget itself.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn proxy_body_budget_never_refuses_before_the_count_at_the_default_limit() {
        const DEADLINE: Duration = Duration::from_millis(500);
        let upstream = HandUpstream::start().await;
        let count = RequestCount::new(PROXY_MAX_REQUESTS);
        let state = limited_state(&upstream, |cfg| {
            cfg.limits.body_read_timeout = DEADLINE;
        });
        let limit = state.limits().request_body_bytes;
        assert_eq!(limit, trawl_config::DEFAULT_MAX_REQUEST_BODY_BYTES);
        assert_eq!(state.limits().body_budget_bytes, PROXY_BODY_BUDGET);
        assert!(
            PROXY_BODY_BUDGET / limit >= PROXY_MAX_REQUESTS,
            "the budget admits fewer full bodies than the count does"
        );
        let budget = state.body_budget().clone();
        let app = routes::build_with_count(state, count.clone());

        let stalled: Vec<_> = (0..PROXY_MAX_REQUESTS)
            .map(|_| tokio::spawn(app.clone().oneshot(stalled_post("/api/v1/search"))))
            .collect();
        until("every stalled body reserved the limit", || {
            budget.available() == PROXY_BODY_BUDGET - PROXY_MAX_REQUESTS * limit
        })
        .await;
        assert_eq!(count.available(), 0, "a stalled body took no count");

        let refused = frozen(
            "the refusal",
            app.clone().oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .unwrap();
        assert_proxy_request_limit_refusal(refused).await;

        tokio::time::advance(DEADLINE + Duration::from_millis(10)).await;
        for post in stalled {
            let resp = frozen("the deadline answered the stalled body", post)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
        }
        assert_eq!(budget.available(), PROXY_BODY_BUDGET);
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_past_the_budget_gets_413_naming_the_budget() {
        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 4, |cfg| {
            cfg.limits.request_body_bytes = 1024;
            cfg.limits.body_budget_bytes = 64;
        });
        let sentence = "request body exceeds the proxy body budget (64 bytes)";

        let announced = Request::builder()
            .method("POST")
            .uri("/api/v1/search")
            .header("authorization", "Bearer flt_direct")
            .header("content-type", "application/json")
            .header("content-length", "65")
            .body(Body::from(vec![b'x'; 65]))
            .unwrap();
        let resp = app.clone().oneshot(announced).await.unwrap();
        assert_envelope_refusal(
            resp,
            StatusCode::PAYLOAD_TOO_LARGE,
            trawl_api::ErrorCode::RequestTooLarge,
            sentence,
        )
        .await;

        let chunks = futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 60])),
            Ok(Bytes::from(vec![b'x'; 5])),
        ]);
        let resp = app
            .clone()
            .oneshot(bearer_post("/api/v1/search", Body::from_stream(chunks)))
            .await
            .unwrap();
        assert_envelope_refusal(
            resp,
            StatusCode::PAYLOAD_TOO_LARGE,
            trawl_api::ErrorCode::RequestTooLarge,
            sentence,
        )
        .await;
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");

        let resp = app
            .oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; 64])))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        upstream.wait_for_requests(1).await;
        assert_eq!(upstream.seen(), ["POST /api/v1/search"]);
    }

    #[tokio::test]
    async fn retired_api_namespace_is_not_routed_or_served_by_the_spa() {
        // Bare `/api` is here on purpose: the catch-all `/api/{*path}`
        // needs at least one segment, so without its own route the exact
        // path falls through to the SPA fallback and answers 200 with
        // index.html while every path below it answers the JSON 404.
        for retired_path in ["/api/intel/v1/stories", "/api", "/api/v2/query"] {
            let upstream = TlsUpstream::start().await;
            let app = build_app(state_pointing_at(&upstream));

            let request = Request::builder()
                .uri(retired_path)
                .body(Body::empty())
                .unwrap();
            let response = app.oneshot(request).await.unwrap();

            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{retired_path} must answer the API 404, not the SPA"
            );
            assert!(
                upstream
                    .mock()
                    .received_requests()
                    .await
                    .unwrap()
                    .is_empty(),
                "{retired_path} must not relay to the configured upstream"
            );
        }
    }

    async fn login_and_get_cookie(app: Router, upstream: &TlsUpstream) -> String {
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
        let sc = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        sc.split(';').next().unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn forward_injects_bearer_and_mirrors_status() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .and(bearer_token("flt_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [{"name": "timestamp", "type": "TIMESTAMP"}]
            })))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn forward_sends_a_chunked_body_whole() {
        let upstream = TlsUpstream::start().await;
        let app = build_app(state_pointing_at(&upstream));
        let expected: Vec<u8> = (0..3_000_u32).flat_map(u32::to_le_bytes).collect();

        Mock::given(method("POST"))
            .and(path("/api/v1/search"))
            .and(bearer_token("flt_direct"))
            .and(body_bytes(expected.clone()))
            .respond_with(ResponseTemplate::new(200))
            .mount(upstream.mock())
            .await;

        let chunks: Vec<Result<Bytes, std::io::Error>> = expected
            .chunks(1_000)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let resp = app
            .oneshot(bearer_post(
                "/api/v1/search",
                Body::from_stream(futures::stream::iter(chunks)),
            ))
            .await
            .unwrap();
        // wiremock answers 404 to a body that matches no mock.
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "the body did not arrive whole"
        );
    }

    #[tokio::test]
    async fn forward_rejects_missing_cookie() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ingest_endpoint_is_always_404() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        for method_name in &["GET", "POST", "PUT", "DELETE"] {
            let req = Request::builder()
                .method(*method_name)
                .uri("/api/v1/ingest")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "/ingest must never be exposed to browsers (method={method_name})"
            );
        }

        let relayed: Vec<String> = upstream
            .mock()
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.url.path().to_owned())
            .filter(|p| p == "/api/v1/ingest")
            .collect();
        assert!(
            relayed.is_empty(),
            "/ingest must not relay to the upstream: {relayed:?}"
        );
    }

    #[tokio::test]
    async fn blocked_and_unknown_api_paths_answer_the_not_found_envelope() {
        let upstream = TlsUpstream::start().await;
        let app = build_app(state_pointing_at(&upstream));

        for (method_name, uri) in [
            ("POST", "/api/v1/ingest"),
            ("GET", "/api/v1/ingest"),
            ("GET", "/api"),
            ("GET", "/api/v2/query"),
            ("DELETE", "/api/admin/keys"),
        ] {
            let req = Request::builder()
                .method(method_name)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method_name} {uri}");
            assert_eq!(
                resp.headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some("no-store"),
                "{method_name} {uri}"
            );
            assert!(
                !resp.headers().contains_key(header::SET_COOKIE),
                "{method_name} {uri}"
            );
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                body,
                json!({"error": {"code": "not_found", "message": "not found"}}),
                "{method_name} {uri}"
            );
        }
        let relayed = upstream.mock().received_requests().await.unwrap();
        assert!(
            relayed.is_empty(),
            "a 404 stub reached upstream: {relayed:?}"
        );
    }

    #[tokio::test]
    async fn ingest_preview_forwards_with_session_bearer() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        let request_body: &[u8] = br#"{"message":"hello","host":"web-01"}"#;
        let response_body: &[u8] =
            br#"{"events":[{"_time":"2026-01-01T00:00:00Z","message":"hello"}],"rejected":[]}"#;
        Mock::given(method("POST"))
            .and(path("/api/v1/ingest/preview"))
            .and(bearer_token("flt_token"))
            .and(query_param("peer_ip", "192.0.2.7"))
            .and(body_bytes(request_body))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "no-store")
                    .set_body_raw(response_body, "application/json"),
            )
            .expect(1)
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/ingest/preview?peer_ip=192.0.2.7")
            .header("cookie", &cookie)
            .header("origin", TEST_ORIGIN)
            .header("host", "trawl.fleet.test")
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let relayed = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&relayed[..], response_body);
    }

    #[tokio::test]
    async fn forward_preserves_upstream_4xx() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn proxy_forwards_request_limit_refusal() {
        let upstream = TlsUpstream::start().await;
        let app = build_app(state_pointing_at(&upstream));
        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        let refusal: &[u8] = br#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP request limit ([server] max_concurrent_requests); the request was not processed; retry later with backoff"}}"#;
        let request_id = "0b5f3c1e-request-limit-id";
        Mock::given(method("POST"))
            .and(path("/api/v1/query"))
            .and(bearer_token("flt_token"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("cache-control", "no-store")
                    .insert_header("x-request-id", request_id)
                    .set_body_raw(refusal, "application/json"),
            )
            .expect(1)
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/query")
            .header("cookie", &cookie)
            .header("origin", TEST_ORIGIN)
            .header("host", "trawl.fleet.test")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"*"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let headers = resp.headers();
        assert_eq!(
            headers
                .get_all(header::CACHE_CONTROL)
                .iter()
                .collect::<Vec<_>>(),
            ["no-store"]
        );
        assert_eq!(headers.get("x-request-id").unwrap(), request_id);
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert!(headers.get(header::RETRY_AFTER).is_none());
        let relayed = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&relayed[..], refusal);

        let sent = upstream
            .mock()
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/api/v1/query")
            .count();
        assert_eq!(sent, 1, "one upstream request, no retry");
    }

    #[tokio::test]
    async fn forward_strips_upstream_set_cookie_headers() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/saved"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("set-cookie", "one=1; Path=/")
                    .append_header("set-cookie", "two=2; Path=/")
                    .set_body_string("[]"),
            )
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/saved")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let set_cookies: Vec<_> = resp.headers().get_all(header::SET_COOKIE).iter().collect();
        assert!(
            set_cookies.is_empty(),
            "upstream Set-Cookie headers must be stripped, got {set_cookies:?}"
        );
    }

    #[tokio::test]
    async fn forward_strips_upstream_location_outside_a_redirect() {
        // A 3xx never gets this far (`refuse_redirect`), but a navigation
        // that lands on a proxied 201 still sees its headers, so
        // `Location` is dropped whatever the status.
        let upstream = TlsUpstream::start().await;
        let app = build_app(state_pointing_at(&upstream));
        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("POST"))
            .and(path("/api/v1/saved"))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header("location", "http://127.0.0.1:1/steal")
                    .set_body_string("{}"),
            )
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/saved")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert!(
            resp.headers().get(header::LOCATION).is_none(),
            "upstream Location must be stripped, got {:?}",
            resp.headers().get(header::LOCATION)
        );
    }

    #[test]
    fn upstream_uri_preserves_query_string() {
        let orig: Uri = "/api/v1/saved?limit=50&cursor=abc".parse().unwrap();
        let built = build_upstream_uri("https://trawld:5514/", &orig).unwrap();
        assert_eq!(
            built,
            "https://trawld:5514/api/v1/saved?limit=50&cursor=abc"
        );
    }

    #[tokio::test]
    async fn forward_upstream_401_with_session_preserves_cookie() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(401))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "a proxied 401 must NOT clear the shared fleet_session cookie"
        );
    }

    #[tokio::test]
    async fn forward_upstream_403_preserves_cookie() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(403))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "upstream 403 must NOT clear the shared cookie"
        );
    }

    #[tokio::test]
    async fn forward_upstream_401_with_bearer_does_not_clear() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(401))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("authorization", "Bearer flt_dead")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "bearer-authed 401 must not emit a cookie clear"
        );
    }

    #[tokio::test]
    async fn forward_accepts_bearer_header() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .and(bearer_token("flt_direct"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [{"name": "host", "type": "VARCHAR"}]
            })))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("authorization", "Bearer flt_direct")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn forward_bearer_wins_over_cookie() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .and(bearer_token("flt_explicit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("cookie", &cookie)
            .header("authorization", "Bearer flt_explicit")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn forward_rejects_empty_bearer() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/schema")
            .header("authorization", "Bearer ")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn forward_rejects_cookie_authed_sibling_origin_mutation() {
        // No upstream mock is mounted for the route: a 403
        // (not a forwarded 404) proves the request was blocked at the proxy.
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/saved")
            .header("cookie", &cookie)
            .header("origin", "https://sibling.fleet.test")
            .header("host", "trawl.fleet.test")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"x","dsl":"*"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "a rejected cross-origin mutation must not touch the cookie"
        );
    }

    #[tokio::test]
    async fn forward_allows_cookie_authed_same_origin_mutation() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("POST"))
            .and(path("/api/v1/saved"))
            .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/saved")
            .header("cookie", &cookie)
            .header("origin", "https://trawl.fleet.test")
            .header("host", "trawl.fleet.test")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"x","dsl":"*"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn forward_allows_cookie_authed_mutation_without_origin() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("POST"))
            .and(path("/api/v1/saved"))
            .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/saved")
            .header("cookie", &cookie)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"x","dsl":"*"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn forward_bearer_cross_origin_mutation_is_allowed() {
        let upstream = TlsUpstream::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        Mock::given(method("POST"))
            .and(path("/api/v1/saved"))
            .and(bearer_token("flt_direct"))
            .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/saved")
            .header("authorization", "Bearer flt_direct")
            .header("origin", "https://evil.example.com")
            .header("host", "trawl.fleet.test")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"x","dsl":"*"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }
}
