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

/// Always-404 handler for `/api/v1/ingest`. The browser never ingests.
pub async fn block_ingest() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "not found"
        })),
    )
        .into_response()
}

pub async fn forward(
    State(state): State<AppState>,
    auth: Auth,
    req: Request<Body>,
) -> Result<Response, ProxyError> {
    do_forward(&state, state.upstream_url(), "", auth, req).await
}

pub async fn forward_intel(
    State(state): State<AppState>,
    auth: Auth,
    req: Request<Body>,
) -> Result<Response, ProxyError> {
    let base = state.coastwatch_url().ok_or_else(|| {
        ProxyError::ServiceUnavailable("coastwatch upstream not configured".into())
    })?;
    do_forward(&state, base, "/api/intel", auth, req).await
}

async fn do_forward(
    state: &AppState,
    base: &str,
    strip_prefix: &str,
    auth: Auth,
    req: Request<Body>,
) -> Result<Response, ProxyError> {
    let http = state.http();
    let (parts, body) = req.into_parts();

    let upstream_uri = build_upstream_uri(base, &parts.uri, strip_prefix)?;

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

    // Deliberately NO cookie clearing here (ADR-0004 slice 2). trawld
    // returns an opaque 401 for BOTH classes of failure — a dead key
    // (revoked/expired fleet-wide) AND a live key that merely lacks the
    // permission grant for this one endpoint (e.g. a non-admin whose SPA
    // hits an admin-only route). The two are indistinguishable at this
    // layer, so clearing on any proxied 401 would log valid users out of
    // the entire fleet (shared `fleet_session`) on a routine authz denial.
    // Cookie lifecycle is owned solely by `auth::me`, which decides against
    // the PERMISSION-FREE upstream `/whoami`: a 401 there is unambiguously
    // a dead key. The session extractor's local expiry check plus that
    // `/me` path are the only safe places to drop the shared cookie.

    out.body(Body::from_stream(body_stream))
        .map_err(|e| ProxyError::Internal(format!("response build: {e}")))
}

const MAX_PROXY_BODY_BYTES: usize = 16 * 1024 * 1024;

fn build_upstream_uri(base: &str, orig: &Uri, strip_prefix: &str) -> Result<String, ProxyError> {
    let path_and_query = orig
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .ok_or_else(|| ProxyError::Internal("request URI missing path".into()))?;
    let stripped = if strip_prefix.is_empty() {
        path_and_query
    } else {
        path_and_query.strip_prefix(strip_prefix).ok_or_else(|| {
            ProxyError::Internal(format!(
                "path '{path_and_query}' does not start with '{strip_prefix}'"
            ))
        })?
    };
    Ok(format!("{}{stripped}", base.trim_end_matches('/')))
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

    fn state_pointing_at(upstream: &MockServer) -> AppState {
        let web = WebConfig {
            upstream_url: Some(upstream.uri()),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    fn state_with_intel(trawld: &MockServer, coastwatch: &MockServer) -> AppState {
        let web = WebConfig {
            upstream_url: Some(trawld.uri()),
            coastwatch_url: Some(coastwatch.uri()),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    fn build_app(state: AppState) -> Router {
        // `routes::build` already wires /login, /me, /logout, /api/v1/*
        // forward, and the /api/v1/ingest blocker — exactly what we're
        // testing.
        routes::build(state)
    }

    async fn login_and_get_cookie(app: Router, upstream: &MockServer) -> String {
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "prefix": "testtest",
                "name": "alice",
                "kind": "human",
                "assignments": [{"app": "trawl", "role": "analyst"}],
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
        let built = build_upstream_uri("https://trawld:5514/", &orig, "").unwrap();
        assert_eq!(
            built,
            "https://trawld:5514/api/v1/saved?limit=50&cursor=abc"
        );
    }

    #[test]
    fn upstream_uri_strips_intel_prefix() {
        let orig: Uri = "/api/intel/v1/stories?cursor=xyz".parse().unwrap();
        let built = build_upstream_uri("https://coastwatch:7700", &orig, "/api/intel").unwrap();
        assert_eq!(built, "https://coastwatch:7700/v1/stories?cursor=xyz");
    }

    #[test]
    fn upstream_uri_strips_prefix_without_query() {
        let orig: Uri = "/api/intel/v1/stories/sto_abc123".parse().unwrap();
        let built = build_upstream_uri("https://coastwatch:7700", &orig, "/api/intel").unwrap();
        assert_eq!(built, "https://coastwatch:7700/v1/stories/sto_abc123");
    }

    // -- upstream auth mapping (AC #5) -------------------------------------

    #[tokio::test]
    async fn forward_upstream_401_with_session_preserves_cookie() {
        // trawld returns an opaque 401 for BOTH a dead key and a live key
        // that merely lacks the permission grant for one endpoint (e.g. a
        // non-admin whose SPA hits an admin-only route). The proxy can't
        // tell them apart, so it must NOT clear the shared cookie on any
        // proxied 401 — doing so would log valid users out of the entire
        // fleet on a routine authz denial. Cookie lifecycle is owned by
        // `auth::me`, which decides against the permission-free /whoami.
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
            "a proxied 401 (possibly a mere authz denial) must NOT clear the \
             shared fleet_session cookie"
        );
    }

    #[tokio::test]
    async fn forward_upstream_403_preserves_cookie() {
        // Valid key, no trawl grant: the shared fleet_session cookie may
        // still hold grants for sibling apps — clearing it would log the
        // user out of coastwatch. 403 passes through with NO Set-Cookie.
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
    async fn forward_intel_upstream_401_with_session_preserves_cookie() {
        // The coastwatch intel path is a proxied route like any other — its
        // 401 is equally ambiguous (dead key vs. authz denial), so it must
        // not clear the shared cookie either.
        let trawld = MockServer::start().await;
        let coastwatch = MockServer::start().await;
        let state = state_with_intel(&trawld, &coastwatch);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &trawld).await;

        Mock::given(method("GET"))
            .and(path("/v1/stories"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&coastwatch)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/intel/v1/stories")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !resp.headers().contains_key(header::SET_COOKIE),
            "a proxied intel 401 must NOT clear the shared fleet_session cookie"
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

    #[tokio::test]
    async fn forward_intel_strips_prefix_and_injects_bearer() {
        let trawld = MockServer::start().await;
        let coastwatch = MockServer::start().await;
        let state = state_with_intel(&trawld, &coastwatch);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &trawld).await;

        Mock::given(method("GET"))
            .and(path("/v1/stories"))
            .and(bearer_token("flt_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [], "next_cursor": null, "request_id": "req_1"
            })))
            .mount(&coastwatch)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/intel/v1/stories")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn forward_intel_returns_503_when_unconfigured() {
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &upstream).await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/intel/v1/stories")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn forward_intel_mirrors_upstream_4xx() {
        let trawld = MockServer::start().await;
        let coastwatch = MockServer::start().await;
        let state = state_with_intel(&trawld, &coastwatch);
        let app = build_app(state);

        let cookie = login_and_get_cookie(app.clone(), &trawld).await;

        Mock::given(method("GET"))
            .and(path("/v1/stories/sto_missing"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "not_found", "request_id": "req_1"})),
            )
            .mount(&coastwatch)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/intel/v1/stories/sto_missing")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
