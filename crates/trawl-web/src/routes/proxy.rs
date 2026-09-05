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
//! - `/api/v1/stream` is handled by the SSE-specific handler in
//!   `routes::stream`, which streams bytes rather than buffering.
//! - Hop-by-hop headers are stripped (connection, upgrade, te, etc.).

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::error::ProxyError;
use crate::middleware::session_extractor::Auth;
use crate::state::AppState;

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
    let http = state.http();
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

    let body_bytes = axum::body::to_bytes(body, MAX_PROXY_BODY_BYTES)
        .await
        .map_err(|e| ProxyError::BadRequest(format!("request body: {e}")))?;
    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes.to_vec());
    }

    let upstream_resp = upstream_req.send().await.map_err(ProxyError::Network)?;

    let status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let upstream_headers = upstream_resp.headers().clone();
    let body_stream = upstream_resp.bytes_stream();

    let mut out = Response::builder().status(status);
    let out_headers = out.headers_mut().expect("fresh response has headers map");
    copy_response_headers(&upstream_headers, out_headers);

    if let Some(clear) = clear_cookie_for_proxied_response(status, &auth) {
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

const MAX_PROXY_BODY_BYTES: usize = 16 * 1024 * 1024;

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

fn copy_response_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    for (name, value) in src {
        if HOP_BY_HOP
            .iter()
            .any(|h| name.as_str().eq_ignore_ascii_case(h))
            || name.as_str().eq_ignore_ascii_case("set-cookie")
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
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;
    use trawl_config::WebConfig;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::ResolvedConfig;
    use crate::routes;

    /// The browser origin these fixtures answer on. The CSRF tests below
    /// send it verbatim; anything else is a foreign origin by definition,
    /// which is the whole of ADR-0016's policy.
    const TEST_ORIGIN: &str = "https://trawl.fleet.test";

    fn state_pointing_at(upstream: &MockServer) -> AppState {
        let web = WebConfig {
            upstream_url: Some(upstream.uri()),
            allow_insecure_cookies: true,
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..WebConfig::default()
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    fn build_app(state: AppState) -> Router {
        routes::build(state)
    }

    #[tokio::test]
    async fn retired_api_namespace_is_not_routed_or_served_by_the_spa() {
        // Bare `/api` is here on purpose: the catch-all `/api/{*path}`
        // needs at least one segment, so without its own route the exact
        // path falls through to the SPA fallback and answers 200 with
        // index.html while every path below it answers the JSON 404.
        for retired_path in ["/api/intel/v1/stories", "/api", "/api/v2/query"] {
            let upstream = MockServer::start().await;
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
                upstream.received_requests().await.unwrap().is_empty(),
                "{retired_path} must not relay to the configured upstream"
            );
        }
    }

    async fn login_and_get_cookie(app: Router, upstream: &MockServer) -> String {
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .and(bearer_token("flt_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [{"name": "timestamp", "type": "TIMESTAMP"}]
            })))
            .mount(&upstream)
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
    async fn forward_rejects_missing_cookie() {
        let upstream = MockServer::start().await;
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
        let upstream = MockServer::start().await;
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
    }

    #[tokio::test]
    async fn forward_preserves_upstream_4xx() {
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&upstream)
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
    async fn forward_strips_upstream_set_cookie_headers() {
        // trawld is a backend API — it must not set cookies in the
        // browser context. The proxy strips all Set-Cookie headers
        // from upstream responses to prevent cookie shadowing.
        let upstream = MockServer::start().await;
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
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .and(bearer_token("flt_direct"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [{"name": "host", "type": "VARCHAR"}]
            })))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/schema"))
            .and(bearer_token("flt_explicit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
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
        let upstream = MockServer::start().await;
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("POST"))
            .and(path("/api/v1/saved"))
            .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        Mock::given(method("POST"))
            .and(path("/api/v1/saved"))
            .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
            .mount(&upstream)
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
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        Mock::given(method("POST"))
            .and(path("/api/v1/saved"))
            .and(bearer_token("flt_direct"))
            .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
            .mount(&upstream)
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
