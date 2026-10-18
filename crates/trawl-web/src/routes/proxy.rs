// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Generic `/api/v1/*` → trawld pass-through.
//!
//! Forwards the browser's request to upstream trawld, replacing the
//! session cookie with `Authorization: Bearer <token>`. The response
//! body, status, and most headers are mirrored back to the browser.
//!
//! Exceptions:
//! - `/api/v1/ingest` is blocked outright (never exposed to browsers).
//!   The block matches that exact path only: `/api/v1/ingest/preview`
//!   forwards like any other call, with the session's bearer token
//!   (ADR-0049).
//! - `/api/v1/stream` is handled by the SSE-specific handler in
//!   `routes::stream`, which streams bytes rather than buffering.
//! - Hop-by-hop headers are stripped (connection, upgrade, te, etc.).
//! - An upstream 3xx becomes the proxy's own 502, and `Location` never
//!   reaches the browser; see `refuse_redirect`.

use axum::body::{Body, Bytes, HttpBody as _};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt as _;

use crate::body_budget::{BODY_BUDGET_NAME, BodyBudget, BodyReservation};
use crate::config::ProxyLimits;
use crate::error::ProxyError;
use crate::middleware::body_deadline::BodyDeadlineElapsed;
use crate::middleware::session_extractor::Auth;
use crate::state::AppState;

/// The setting the proxy's body limit comes from, as trawld's 413 names
/// it.
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

/// Always-404 handler for blocked or unknown browser API paths.
pub async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "not found"
        })),
    )
        .into_response()
}

/// The browser never ingests, so keep that path outside the generic proxy.
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
    // target) or passed the origin guard in the `Session` extractor
    // (ADR-0016). The guard used to be an explicit call right here, which
    // covered this forwarder and nothing else; the SSE handlers take the
    // same `Auth` and had no such call, so a foreign page could stream the
    // victim's logs. Owning the check in the extractor is what makes the
    // rule hold for every cookie route, including the next one.
    let upstream_uri = build_upstream_uri(state.upstream_url(), &parts.uri)?;

    // The reservation is held, with the count, until the response head:
    // the buffered body is alive until trawld has read it.
    let (body_bytes, _reservation) =
        collect_body(&parts.headers, body, state.limits(), state.body_budget()).await?;

    // After every local check, so a request refused here never waits on
    // the pin file. This one client carries the request and its streamed
    // response body.
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
        // `Bytes` into reqwest as it is: no copy of the buffered body.
        upstream_req = upstream_req.body(body_bytes);
    }

    let upstream_resp = state.send_to_upstream(upstream_req).await?;
    refuse_redirect(upstream_resp.status())?;
    relay_response(upstream_resp, &auth)
}

/// Copy an upstream answer to the browser as it came: its status, its
/// end-to-end headers through [`copy_response_headers`], and its body
/// bytes, under the proxy-wide cookie rule. Nothing is retried or
/// rewritten, so a refusal such as trawld's 503 `request_limit_reached`
/// keeps its `Cache-Control` and request id (ADR-0054).
///
/// The generic forwarder relays every answer this way, and the SSE
/// handlers every non-2xx one. A caller refuses a redirect first
/// ([`refuse_redirect`]).
///
/// # Errors
/// [`ProxyError::Internal`] when the response cannot be built.
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

/// Whether a proxied upstream response should clear the shared
/// `fleet_session` cookie, and with what directive; `None` leaves the
/// cookie untouched.
///
/// The one home of the proxy-path 401/403 cookie rule (ADR-0004), consulted
/// by both the generic forwarder and the SSE handler so a newly added
/// proxied path can't silently diverge.
///
/// Always `None`: proxied responses never own cookie lifecycle. A trawld
/// `401` means the upstream credential is invalid, revoked, or expired; a
/// `403` means the valid key lacks a trawl grant or the permission for that
/// route and may still carry grants for sibling apps. `auth::me` owns cookie
/// clearing through the permission-free upstream `/whoami`. `_status` and
/// `_auth` remain available if that policy changes.
#[must_use]
pub(crate) fn clear_cookie_for_proxied_response(
    _status: StatusCode,
    _auth: &Auth,
) -> Option<HeaderValue> {
    None
}

/// Turn an upstream redirect into an upstream error.
///
/// trawld never redirects, and the client follows no redirect (see
/// [`AppState::from_config`]). Mirroring the 3xx would let the browser
/// follow it instead: a `Location` naming another port on the browser's
/// host would carry the host-scoped session cookie there, since a
/// cookie's scope ignores the port. Every 3xx is refused, 304
/// included: trawld sends no validators, so a 304 is not an answer it
/// gives. Shared by every route that relays an upstream status.
///
/// # Errors
/// [`ProxyError::Upstream`] for any 3xx, which answers 502.
pub(crate) fn refuse_redirect(status: reqwest::StatusCode) -> Result<(), ProxyError> {
    if status.is_redirection() {
        return Err(ProxyError::Upstream(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        ));
    }
    Ok(())
}

/// Buffer a request body whole, within the proxy's body limit and its
/// body budget.
///
/// A `Content-Length` past the limit is refused before a byte is read,
/// and a body found over the limit while it streams in is refused at the
/// byte that crosses it: 413 `request_too_large`, with trawld's sentence.
/// Before a byte is read the body's worst case is reserved from the
/// budget (`body_budget`): its declared length, or the whole limit when
/// it declares none. A body the budget cannot cover now is refused 503
/// `request_limit_reached`, and a body larger than the whole budget,
/// which only a limit above the budget allows, is refused 413 naming
/// the budget. The buffer is allocated at the reservation and never
/// grows past it, so the reservation is the memory. One copy is made,
/// from each chunk as it arrives into that buffer, and the buffer goes
/// to reqwest as the `Bytes` it is frozen into. The body deadline is the
/// router's (`middleware::body_deadline`); a read that fails because it
/// passed is 408 `timeout` here, the answer the layer gives whatever a
/// handler makes of it. None of these refusals contacts trawld. The
/// count and the reservation the request holds are released with the
/// refusal, as with any response head.
///
/// # Errors
/// [`ProxyError::RequestTooLarge`], [`ProxyError::BodyBudgetFull`],
/// [`ProxyError::BodyTimeout`], or [`ProxyError::BadRequest`] when the
/// body cannot be read or is longer than the length it declared.
async fn collect_body(
    headers: &HeaderMap,
    body: Body,
    limits: &ProxyLimits,
    budget: &BodyBudget,
) -> Result<(Bytes, BodyReservation), ProxyError> {
    let limit = limits.request_body_bytes;
    // What one body may hold: the limit, or the budget when the limit is
    // the larger of the two, since the budget could never cover more.
    // The 413 names whichever bound was crossed first.
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
    // `Bytes` takes the vector's allocation as it is: no copy.
    Ok((Bytes::from(buffer), reservation))
}

/// Compose the upstream URL: the configured base followed by the
/// browser's own path and query, verbatim.
///
/// The proxy relays one namespace only (`/api/v1/*` to trawld's own
/// `/api/v1/*`), so there is no prefix rewriting: what the browser asked
/// for is what upstream sees.
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
    use crate::test_support::{HandUpstream, TlsUpstream};

    /// The browser origin these fixtures answer on. The CSRF tests below
    /// send it verbatim; anything else is a foreign origin by definition,
    /// which is the whole of ADR-0016's policy.
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

    /// The production router over a state pointing at the hand upstream,
    /// under a request count of `count`.
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

    /// [`counted_app`] with `adjust` applied to the resolved config
    /// first, so a test can shrink a limit or a deadline.
    fn limited_app(
        upstream: &HandUpstream,
        count: usize,
        adjust: impl FnOnce(&mut ResolvedConfig),
    ) -> Router {
        limited_app_under(upstream, RequestCount::new(count), adjust)
    }

    /// [`limited_app`] under a count the test built itself, so it can
    /// watch the count fill.
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

    /// A state over the hand upstream with `adjust` applied to its
    /// resolved config, kept by the test so it can watch the body budget.
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

    /// A bearer POST to `path` whose body starts and never finishes: one
    /// byte, then nothing, forever, with no length declared.
    fn stalled_post(path: &str) -> Request<Body> {
        let stalled = futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from("{")) })
            .chain(futures::stream::pending());
        bearer_post(path, Body::from_stream(stalled))
    }

    /// A login whose body starts and never finishes: one byte, then
    /// nothing, forever. It carries no token, so anyone can send it.
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

    /// A bearer POST of `body` to `path`.
    fn bearer_post(path: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", "Bearer flt_direct")
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    /// Assert `resp` is one of the proxy's envelope refusals: `status`,
    /// the `trawl_api` envelope with `code` and exactly `message`,
    /// `Cache-Control: no-store` and no cookie change. Returns nothing a
    /// caller needs; the body is consumed.
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

    /// A bearer GET of `path`, the simplest counted request.
    fn bearer_get(path: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(path)
            .header("authorization", "Bearer flt_direct")
            .body(Body::empty())
            .unwrap()
    }

    /// Assert `resp` is the proxy's own 503 `request_limit_reached`: the
    /// envelope with a message naming trawl-web, `Cache-Control:
    /// no-store`, no `Retry-After` and no cookie change.
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

    /// Over the count, the proxy answers its own 503 at once and sends
    /// nothing to trawld; the held requests finish, and the count is free
    /// again afterwards.
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
        // Nothing for the refused request reached trawld, then or later.
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

    /// With the count full, `/healthz` still answers: the probes are
    /// outside the count.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_healthz_is_never_counted() {
        let upstream = HandUpstream::start().await;
        upstream.hold();
        let app = counted_app(&upstream, 1);

        let held = tokio::spawn(app.clone().oneshot(bearer_get("/api/v1/schema")));
        upstream.wait_for_requests(1).await;

        // Under a deadline: a proxy without the count would forward this
        // to the held upstream and wait there instead of failing.
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

    /// The count is given back at the response head: an open server-sent
    /// event stream, whose body is still flowing, holds no count.
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
        // The stream is open: an event arrives.
        let mut body = stream.into_body().into_data_stream();
        let first =
            tokio::time::timeout(Duration::from_secs(5), futures::StreamExt::next(&mut body))
                .await
                .expect("an event before the deadline")
                .expect("the stream is open")
                .unwrap();
        assert!(first.starts_with(b"data: "), "{first:?}");

        // With the only count's request still streaming, another request
        // is admitted.
        let next = app.oneshot(bearer_get("/api/v1/schema")).await.unwrap();
        assert_eq!(
            next.status(),
            StatusCode::OK,
            "an open stream held the count past its head"
        );
        // And the stream is still flowing afterwards.
        let later =
            tokio::time::timeout(Duration::from_secs(5), futures::StreamExt::next(&mut body))
                .await
                .expect("another event before the deadline")
                .expect("the stream is still open")
                .unwrap();
        assert!(later.starts_with(b"data: "), "{later:?}");
        drop(body);
    }

    /// A body over `[server] max_request_body_bytes` is answered 413
    /// `request_too_large` with trawld's own sentence, whether its length
    /// was announced or found while it streamed in, and trawld is never
    /// called. A body exactly at the limit goes through.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_over_server_limit_gets_413_without_upstream_call() {
        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 4, |cfg| cfg.limits.request_body_bytes = 64);
        let sentence = "request body exceeds [server] max_request_body_bytes (64 bytes)";

        // Announced: refused on the header alone.
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

        // Unannounced: refused at the byte that crosses the limit.
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

        // At the limit: forwarded.
        let resp = app
            .oneshot(bearer_post("/api/v1/search", Body::from(vec![b'x'; 64])))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        upstream.wait_for_requests(1).await;
        assert_eq!(upstream.seen(), ["POST /api/v1/search"]);
    }

    /// The default limit is trawld's default, so a proxy resolved without
    /// a `[server]` section refuses what a default trawld would.
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

    /// A trawld that has not sent a response head by the deadline is
    /// answered 504 `timeout` with the envelope, and the count is free
    /// again.
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

        // The only count is free again, and a released upstream answers.
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

    /// The head deadline covers the head only: a server-sent event stream
    /// whose head arrived in time keeps flowing well past the deadline.
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

    /// A login body over 16 KiB is answered 413 `request_too_large` with
    /// the login limit named, and trawld is never asked about the key.
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

    /// A forwarded body that stops arriving is answered 408 `timeout` at
    /// the body deadline, trawld is never called, and the request's count
    /// is free again for the next request.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_collection_deadline_releases_slot() {
        let upstream = HandUpstream::start().await;
        let app = limited_app(&upstream, 1, |cfg| {
            cfg.limits.body_read_timeout = Duration::from_millis(200);
        });

        // One chunk, then nothing, forever.
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

        // The only count is free again.
        let next = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the next request was answered")
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the count was not released");
    }

    /// A login whose body stops arriving is answered 408 `timeout` at the
    /// body deadline like the forwarder's, trawld is never asked, and the
    /// count is free again. The deadline is the router's, not one
    /// handler's: login reads its body through axum's `Json`, which has
    /// no deadline of its own, and it is the route anyone can send.
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

        // The only count is free again.
        let next = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the next request was answered")
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the count was not released");
    }

    /// Sixty-four stalled login bodies, the whole production count, hold
    /// it only until the deadline. While they stall, the next request is
    /// refused; at the deadline every one of them is answered 408; and
    /// the request after them is served. No token is needed to send a
    /// login, so without the deadline this is a pre-authentication
    /// lockout of every route but `/healthz`.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_stalled_logins_release_the_whole_count_at_the_deadline() {
        let upstream = HandUpstream::start().await;
        let count = RequestCount::new(PROXY_MAX_REQUESTS);
        let app = limited_app_under(&upstream, count.clone(), |cfg| {
            cfg.limits.body_read_timeout = Duration::from_millis(500);
        });

        let stalled: Vec<_> = (0..PROXY_MAX_REQUESTS)
            .map(|_| tokio::spawn(app.clone().oneshot(stalled_login())))
            .collect();
        let started = std::time::Instant::now();
        while count.available() > 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{} of the stalled logins never took a count",
                count.available()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Full: the next request is refused at once, not queued.
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the refusal is immediate")
        .unwrap();
        assert_proxy_request_limit_refusal(refused).await;

        // The deadline answers every stalled login and frees its count.
        for login in stalled {
            let resp = tokio::time::timeout(Duration::from_secs(5), login)
                .await
                .expect("the deadline answered the stalled login")
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

        let next = tokio::time::timeout(
            Duration::from_secs(5),
            app.oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the request after the stalled logins was answered")
        .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the count was not released");
    }

    /// The bodies the proxy buffers share one budget of
    /// [`PROXY_BODY_BUDGET`] bytes, whatever the body limit. At an 8 MiB
    /// limit the budget admits sixteen bodies of unknown length, each
    /// reserved at the limit; the seventeenth is refused at once with the
    /// proxy's 503, as is a two-byte body, because nothing is left. A
    /// request with no body reserves nothing and is served while the
    /// budget is full. At the deadline the stalled bodies give the
    /// budget back whole.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_budget_bounds_buffered_bodies_at_an_8_mib_limit() {
        const LIMIT: usize = 8 * 1024 * 1024;
        const ADMITTED: usize = PROXY_BODY_BUDGET / LIMIT;
        assert_eq!(ADMITTED, 16);
        let upstream = HandUpstream::start().await;
        let count = RequestCount::new(PROXY_MAX_REQUESTS);
        let state = limited_state(&upstream, |cfg| {
            cfg.limits.request_body_bytes = LIMIT;
            cfg.limits.body_read_timeout = Duration::from_millis(500);
        });
        let budget = state.body_budget().clone();
        let app = routes::build_with_count(state, count.clone());

        let stalled: Vec<_> = (0..ADMITTED)
            .map(|_| tokio::spawn(app.clone().oneshot(stalled_post("/api/v1/search"))))
            .collect();
        let started = std::time::Instant::now();
        while budget.available() > 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{} bytes of the budget were never reserved",
                budget.available()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            count.available(),
            PROXY_MAX_REQUESTS - ADMITTED,
            "the count still has room: the budget is what is full"
        );

        // Full: one more body of unknown length is refused at once, and
        // so is a declared two-byte body.
        for request in [
            stalled_post("/api/v1/search"),
            bearer_post("/api/v1/search", Body::from("{}")),
        ] {
            let refused =
                tokio::time::timeout(Duration::from_secs(5), app.clone().oneshot(request))
                    .await
                    .expect("the refusal is immediate")
                    .unwrap();
            assert_proxy_request_limit_refusal(refused).await;
        }
        // A request without a body reserves nothing.
        let bodiless = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the bodiless request was answered")
        .unwrap();
        assert_eq!(
            bodiless.status(),
            StatusCode::OK,
            "a bodiless request was charged"
        );

        // The deadline answers every stalled body and frees its reservation.
        for post in stalled {
            let resp = tokio::time::timeout(Duration::from_secs(5), post)
                .await
                .expect("the deadline answered the stalled body")
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

        let next = app
            .oneshot(bearer_post(
                "/api/v1/search",
                Body::from(vec![b'x'; 1024 * 1024]),
            ))
            .await
            .unwrap();
        assert_eq!(next.status(), StatusCode::OK, "the budget was not released");
        assert_eq!(
            upstream.seen(),
            ["GET /api/v1/schema", "POST /api/v1/search"],
            "trawld saw a refused body"
        );
    }

    /// A declared body is charged its own length, no more, and gives it
    /// back at the response head: with 600 of a 1024-byte budget held by
    /// a body trawld has not answered, a 500-byte body is refused at once
    /// and a 400-byte one is admitted. Both reservations return when
    /// trawld answers.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_budget_charges_the_declared_length_until_the_response_head() {
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

    /// At the default limit the count fills before the budget: sixty-four
    /// stalled bodies take the whole count and an eighth of the budget,
    /// and the sixty-fifth request is the count's refusal. The defaults
    /// behave as they did before the budget.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_body_budget_never_refuses_before_the_count_at_the_default_limit() {
        let upstream = HandUpstream::start().await;
        let count = RequestCount::new(PROXY_MAX_REQUESTS);
        let state = limited_state(&upstream, |cfg| {
            cfg.limits.body_read_timeout = Duration::from_millis(500);
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
        let started = std::time::Instant::now();
        while count.available() > 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{} of the stalled bodies never took a count",
                count.available()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            budget.available(),
            PROXY_BODY_BUDGET - PROXY_MAX_REQUESTS * limit,
            "each body of unknown length reserved the limit"
        );

        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            app.clone().oneshot(bearer_get("/api/v1/schema")),
        )
        .await
        .expect("the refusal is immediate")
        .unwrap();
        assert_proxy_request_limit_refusal(refused).await;

        for post in stalled {
            let resp = tokio::time::timeout(Duration::from_secs(5), post)
                .await
                .expect("the deadline answered the stalled body")
                .unwrap()
                .unwrap();
            assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
        }
        assert_eq!(budget.available(), PROXY_BODY_BUDGET);
        assert_eq!(upstream.seen(), Vec::<String>::new(), "trawld was called");
    }

    /// A limit above the budget leaves the budget as the most one body
    /// can hold, since the budget could never cover more: a body past it
    /// is refused 413 naming the proxy body budget, announced or found
    /// while it streams in, and a body at it goes through.
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

    /// A body that arrives in several chunks reaches trawld whole and in
    /// order: the chunks are collected into the one buffer the forwarder
    /// hands reqwest.
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

        // Even a logged-in user cannot reach /ingest through the proxy.
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

        // The 404 is local: no request for the ingest path ever reaches
        // upstream. Filter by path, since the login above hits /whoami.
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
    async fn ingest_preview_forwards_with_session_bearer() {
        // The exact-path ingest block must not swallow its preview child
        // (ADR-0049): the generic forwarder relays it with the session's
        // bearer token, the body bytes and the query string intact, and
        // mirrors the upstream's no-store header and body.
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
        // trawld's 503 `request_limit_reached` (ADR-0054) reaches the
        // browser as trawld sent it: status, body bytes, `Cache-Control`
        // and request id. The proxy never retries it: trawld refused the
        // request before any handler ran, and a retry is the caller's
        // call, with backoff.
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
        // trawld is a backend API — it must not set cookies in the
        // browser context. The proxy strips all Set-Cookie headers
        // from upstream responses to prevent cookie shadowing.
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

    // -- upstream auth mapping ---------------------------------------------

    #[tokio::test]
    async fn forward_upstream_401_with_session_preserves_cookie() {
        // A proxied 401 means the upstream key is dead, but cookie clearing
        // belongs to `auth::me`, not an arbitrary proxied request.
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
        // A valid key missing either a trawl grant or this route's permission
        // may still hold grants for sibling apps. Preserve the shared cookie.
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
        // Bearer clients hold no cookie — a clear directive would be
        // meaningless noise (and confusing for API clients).
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

    // -- CSRF / origin validation on mutating proxy routes ----------------

    #[tokio::test]
    async fn forward_rejects_cookie_authed_sibling_origin_mutation() {
        // The shared `fleet_session` cookie is SameSite=Lax, so the browser
        // attaches it to same-site *sibling*-origin POSTs (a compromised
        // sibling.fleet… forging a write to trawl.fleet…). Present-only
        // Origin validation must reject it before the victim's bearer token
        // reaches trawld. No upstream mock is mounted for the route: a 403
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
        // Same-origin POST from the SPA carries a matching Origin and must
        // pass through to upstream.
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
        // Present-only semantics: a request carrying no Origin header (some
        // same-origin navigations, non-browser cookie clients) is allowed —
        // the Session extractor still validates the cookie itself.
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
        // Bearer clients (CLI/API) hold no cookie and are not CSRF targets —
        // the origin guard must not apply to them even on a "cross-origin"
        // (irrelevant, header-set) POST.
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
