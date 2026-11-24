// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use fleet_auth::{SessionExpiry, SessionPayload, session};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::ProxyError;
use crate::middleware::body_deadline::BodyDeadlineElapsed;
use crate::middleware::session_extractor::Session;
use crate::state::AppState;

pub const LOGIN_BODY_LIMIT: usize = 16 * 1024;

pub const LOGIN_BODY_LIMIT_NAME: &str = "the login body limit";

/// The body is read here and the bytes are handed on to the `Json`
/// extractor, so the limit holds whatever the request's framing or content
/// type. Nothing else enforces it: hyper gives `Transfer-Encoding: chunked`
/// precedence over `Content-Length` and removes the latter from the headers
/// the router sees, and the extractor answers a non-JSON type 415 without
/// reading the body.
pub async fn envelope_login_body_limit(request: Request, next: Next) -> Response {
    let too_large = || {
        ProxyError::RequestTooLarge {
            setting: LOGIN_BODY_LIMIT_NAME,
            limit: LOGIN_BODY_LIMIT,
        }
        .into_response()
    };
    let declared_too_large = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|len| len > LOGIN_BODY_LIMIT as u64);
    if declared_too_large {
        return too_large();
    }

    let (parts, body) = request.into_parts();
    let mut stream = body.into_data_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                let e = e.into_inner();
                return if e.is::<BodyDeadlineElapsed>() {
                    ProxyError::BodyTimeout
                } else {
                    ProxyError::BadRequest(format!("request body: {e}"))
                }
                .into_response();
            }
        };
        if buffer.len().saturating_add(chunk.len()) > LOGIN_BODY_LIMIT {
            return too_large();
        }
        buffer.extend_from_slice(&chunk);
    }
    next.run(Request::from_parts(parts, Body::from(buffer)))
        .await
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub api_key: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub name: String,
    pub roles: Vec<String>,
    pub permissions: Vec<String>,
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ProxyError> {
    fleet_auth::check_origin(&headers, state.public_origins(), "login")
        .map_err(|_| ProxyError::OriginMismatch)?;

    if req.api_key.trim().is_empty() {
        return Err(ProxyError::BadRequest("api_key is required".into()));
    }

    let whoami = fetch_whoami(&state, &req.api_key).await?;

    if whoami.permissions.is_empty() {
        return Err(ProxyError::Upstream(StatusCode::FORBIDDEN));
    }

    let now = chrono::Utc::now().timestamp();
    let ttl = i64::try_from(state.session_ttl_secs())
        .map_err(|_| ProxyError::Internal("session_ttl_secs out of i64 range".into()))?;

    let payload = SessionPayload {
        token: Zeroizing::new(req.api_key),
        name: whoami.name.clone(),
        exp: SessionExpiry::after_duration(now, ttl),
    };

    let cookie_value = session::encrypt(state.cookie_key(), &payload)?;
    let cookie_header = state.build_session_cookie(cookie_value);

    let body = LoginResponse {
        name: whoami.name,
        roles: whoami.roles,
        permissions: whoami.permissions,
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        cookie_header
            .parse()
            .map_err(|e: header::InvalidHeaderValue| ProxyError::Internal(e.to_string()))?,
    );

    Ok((StatusCode::OK, headers, Json(body)).into_response())
}

#[derive(Debug, Deserialize)]
struct WhoAmI {
    name: String,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

async fn fetch_whoami(state: &AppState, token: &str) -> Result<WhoAmI, ProxyError> {
    let url = format!(
        "{}/api/v1/whoami",
        state.upstream_url().trim_end_matches('/')
    );
    let request = state.upstream_client().await?.get(&url).bearer_auth(token);
    let resp = state.send_to_upstream(request).await?;

    match resp.status() {
        s if s.is_success() => {
            let body: WhoAmI = resp
                .json()
                .await
                .map_err(|e| ProxyError::Internal(format!("whoami body: {e}")))?;
            Ok(body)
        }
        s if s == reqwest::StatusCode::UNAUTHORIZED => Err(ProxyError::Unauthorized),
        s => Err(ProxyError::Upstream(
            StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        )),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeResponse {
    pub name: String,
    pub roles: Vec<String>,
    pub permissions: Vec<String>,
    pub exp: i64,
}

pub async fn me(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<MeResponse>, ProxyError> {
    let whoami = match fetch_whoami(&state, session.token()).await {
        Ok(whoami) => whoami,
        // At login time Unauthorized just means "bad key, no cookie yet".
        // Here the browser IS holding a cookie for that key, so upstream
        // 401 means the session is dead fleet-wide — clear it.
        Err(ProxyError::Unauthorized) => {
            let clear_cookie = state
                .build_clear_cookie()
                .map_err(|e| ProxyError::Internal(e.to_string()))?;
            return Err(ProxyError::ExpiredSession { clear_cookie });
        }
        Err(other) => return Err(other),
    };

    if whoami.permissions.is_empty() {
        return Err(ProxyError::Upstream(StatusCode::FORBIDDEN));
    }

    Ok(Json(MeResponse {
        name: whoami.name,
        roles: whoami.roles,
        permissions: whoami.permissions,
        exp: session.exp(),
    }))
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ProxyError> {
    // Before the clear directive is built, so a foreign page cannot make
    // the browser drop a session it was never allowed to read.
    fleet_auth::check_origin(&headers, state.public_origins(), "logout")
        .map_err(|_| ProxyError::OriginMismatch)?;

    let header_value = state
        .build_clear_cookie()
        .map_err(|e| ProxyError::Internal(e.to_string()))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, header_value);
    Ok((StatusCode::NO_CONTENT, headers).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;
    use trawl_config::WebConfig;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, ResponseTemplate};

    use crate::config::ResolvedConfig;
    use crate::routes;
    use crate::test_support::TlsUpstream;

    const TEST_ORIGIN: &str = "https://trawl.example.com";

    const SSO_ORIGIN: &str = "https://trawl.fleet.test";

    fn test_state(upstream: WebConfig) -> AppState {
        let web = WebConfig {
            allow_insecure_cookies: true,
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..upstream
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    fn unreached_upstream() -> WebConfig {
        WebConfig {
            upstream_url: Some("https://unused.invalid".to_owned()),
            ..WebConfig::default()
        }
    }

    fn whoami_body(name: &str, role: &str, permissions: &[&str]) -> serde_json::Value {
        json!({
            "prefix": "abcd1234",
            "name": name,
            "kind": "human",
            "roles": [role],
            "permissions": permissions,
        })
    }

    #[tokio::test]
    async fn login_accepts_valid_api_key() {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .and(bearer_token("flt_goodtoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "trawl-analyst",
                &["query", "export"],
            )))
            .mount(upstream.mock())
            .await;

        let state = test_state(upstream.web_config());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_goodtoken"}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("Set-Cookie header")
            .to_str()
            .unwrap();
        assert!(
            set_cookie.starts_with("fleet_session="),
            "got: {set_cookie}"
        );
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"), "got: {set_cookie}");
        assert!(set_cookie.contains("Path=/"));
        assert!(!set_cookie.contains("Secure"));
        assert!(
            !set_cookie.to_lowercase().contains("domain="),
            "got: {set_cookie}"
        );
    }

    #[tokio::test]
    async fn login_rejects_wrong_api_key() {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(401))
            .mount(upstream.mock())
            .await;

        let state = test_state(upstream.web_config());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_badtoken"}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    #[tokio::test]
    async fn login_rejects_empty_api_key() {
        let state = test_state(unreached_upstream());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":""}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn login_no_trawl_grant_is_403_without_cookie() {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "bob",
                "sibling-analyst",
                &[],
            )))
            .mount(upstream.mock())
            .await;

        let state = test_state(upstream.web_config());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_nogrant"}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "no-grant login must not set (or clear) any cookie"
        );
    }

    #[tokio::test]
    async fn login_secure_cookie_when_insecure_disabled() {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .and(bearer_token("flt_prod"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "prod",
                "trawl-admin",
                &["query", "server_manage"],
            )))
            .mount(upstream.mock())
            .await;

        let web = WebConfig {
            allow_insecure_cookies: false,
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..upstream.web_config()
        };
        let state =
            AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap();
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_prod"}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("Secure"));
    }

    fn sso_state(upstream: WebConfig) -> AppState {
        let web = WebConfig {
            allow_insecure_cookies: true,
            shared_domain: Some(".fleet.test".into()),
            session_ttl_secs: Some(3600),
            public_origins: vec![SSO_ORIGIN.to_owned()],
            ..upstream
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    #[tokio::test]
    async fn login_cookie_carries_domain_iff_shared_domain_set() {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "trawl-analyst",
                &["query", "export"],
            )))
            .mount(upstream.mock())
            .await;

        let app = routes::build(sso_state(upstream.web_config()));
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        // The `cookie` crate normalises the leading dot away per RFC 6265.
        assert!(
            set_cookie.starts_with("fleet_session="),
            "got: {set_cookie}"
        );
        assert!(
            set_cookie.contains("Domain=fleet.test"),
            "got: {set_cookie}"
        );
        assert!(set_cookie.contains("SameSite=Lax"), "got: {set_cookie}");
        assert!(set_cookie.contains("HttpOnly"), "got: {set_cookie}");
        assert!(set_cookie.contains("Path=/"), "got: {set_cookie}");
        assert!(set_cookie.contains("Max-Age=3600"), "got: {set_cookie}");
    }

    fn attribute_set(set_cookie: &str) -> std::collections::BTreeSet<String> {
        set_cookie
            .split(';')
            .skip(1)
            .map(|a| a.trim().to_string())
            .filter(|a| !a.starts_with("Max-Age") && !a.starts_with("Expires"))
            .collect()
    }

    #[tokio::test]
    async fn clear_cookie_attributes_match_issuance() {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "trawl-analyst",
                &["query", "export"],
            )))
            .mount(upstream.mock())
            .await;

        let app = routes::build(sso_state(upstream.web_config()));

        let login_req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let login_resp = app.clone().oneshot(login_req).await.unwrap();
        let issued = login_resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        let logout_req = Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            .body(Body::empty())
            .unwrap();
        let logout_resp = app.oneshot(logout_req).await.unwrap();
        assert_eq!(logout_resp.status(), StatusCode::NO_CONTENT);
        let cleared = logout_resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        assert!(cleared.starts_with("fleet_session=;"), "got: {cleared}");
        assert!(cleared.contains("Max-Age=0"), "got: {cleared}");
        assert_eq!(
            attribute_set(&issued),
            attribute_set(&cleared),
            "clear-cookie attributes must match issuance\nissued:  {issued}\ncleared: {cleared}"
        );
    }

    #[tokio::test]
    async fn login_cookie_is_fleet_auth_compatible_with_role_less_payload() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("web.key");
        let key_bytes = [0x42u8; fleet_auth::KEY_LEN];
        std::fs::write(&key_path, key_bytes).unwrap();

        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "trawl-analyst",
                &["query", "export"],
            )))
            .mount(upstream.mock())
            .await;

        let web = WebConfig {
            allow_insecure_cookies: true,
            cookie_secret_path: Some(key_path),
            public_origins: vec![TEST_ORIGIN.to_owned()],
            ..upstream.web_config()
        };
        let state =
            AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap();
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        let value = set_cookie
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1;

        let sibling_key = fleet_auth::SessionKey::from_bytes(key_bytes);
        let payload = fleet_auth::decrypt(&sibling_key, value).unwrap();
        assert_eq!(payload.name, "alice");
        assert_eq!(payload.token.as_str(), "flt_token");

        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().unwrap();
        let mut keys: Vec<_> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["exp", "name", "token"],
            "payload must be exactly {{token, name, exp}} — no role"
        );
    }

    #[tokio::test]
    async fn login_rejects_cross_origin() {
        let state = test_state(unreached_upstream());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .header("origin", "https://evil.example.com")
            .header("host", "trawl.example.com")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    #[tokio::test]
    async fn logout_rejects_cross_origin_without_clearing() {
        let state = test_state(unreached_upstream());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            .header("origin", "https://evil.example.com")
            .header("host", "trawl.example.com")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "forged cross-site logout must NOT clear the shared cookie"
        );
    }

    #[tokio::test]
    async fn logout_rejects_sso_sibling_origin_without_clearing() {
        let app = routes::build(sso_state(unreached_upstream()));

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            // form-POST content type: no CORS preflight, no response access
            .header("content-type", "application/x-www-form-urlencoded")
            .header("origin", "https://sibling.fleet.test")
            .header("host", "trawl.fleet.test")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "forged sibling-origin logout must NOT clear the shared cookie"
        );
    }

    #[tokio::test]
    async fn logout_allows_the_configured_origin_with_no_host_header_at_all() {
        let app = routes::build(test_state(unreached_upstream()));

        let req = Request::builder()
            .method("POST")
            .uri("https://trawl.example.com/api/auth/logout")
            .header("origin", TEST_ORIGIN)
            .body(Body::empty())
            .unwrap();
        assert!(req.headers().get(header::HOST).is_none());
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            response.headers().contains_key(header::SET_COOKIE),
            "a configured-origin logout must clear the cookie"
        );
    }

    #[tokio::test]
    async fn logout_rejects_a_foreign_origin_whatever_the_uri_authority_says() {
        let app = routes::build(test_state(unreached_upstream()));

        let req = Request::builder()
            .method("POST")
            .uri("https://trawl.example.com/api/auth/logout")
            .header("origin", "https://evil.example.com")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    #[tokio::test]
    async fn login_allows_the_configured_origin_but_rejects_an_sso_sibling() {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "trawl-analyst",
                &["query", "export"],
            )))
            .mount(upstream.mock())
            .await;

        let app = routes::build(test_state(upstream.web_config()));
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .header("origin", TEST_ORIGIN)
            .header("host", "trawl.example.com")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let app = routes::build(sso_state(upstream.web_config()));
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .header("origin", "https://sibling.fleet.test")
            .header("host", "trawl.fleet.test")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie.split(';').next().unwrap().trim().to_string()
    }

    async fn login_and_get_cookie() -> (axum::Router, TlsUpstream, String) {
        let upstream = TlsUpstream::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "trawl-analyst",
                &["query", "export"],
            )))
            .mount(upstream.mock())
            .await;

        let state = test_state(upstream.web_config());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        (app, upstream, cookie_pair(&set_cookie))
    }

    #[tokio::test]
    async fn me_returns_identity_from_live_whoami() {
        let (app, _upstream, cookie) = login_and_get_cookie().await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/auth/me")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: MeResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.name, "alice");
        assert_eq!(body.roles, ["trawl-analyst"]);
        assert_eq!(body.permissions, ["query", "export"]);
        assert!(body.exp > chrono::Utc::now().timestamp());
    }

    #[tokio::test]
    async fn me_reflects_upstream_permission_change_between_calls() {
        let (app, upstream, cookie) = login_and_get_cookie().await;

        let me_req = || {
            Request::builder()
                .method("GET")
                .uri("/api/auth/me")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap()
        };

        let response = app.clone().oneshot(me_req()).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: MeResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.roles, ["trawl-analyst"]);
        assert!(!body.permissions.contains(&"server_manage".to_owned()));

        upstream.mock().reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "trawl-admin",
                &["query", "export", "server_manage"],
            )))
            .mount(upstream.mock())
            .await;

        let response = app.oneshot(me_req()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: MeResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body.roles,
            ["trawl-admin"],
            "role change must be live, not cached"
        );
        assert!(
            body.permissions.contains(&"server_manage".to_owned()),
            "permission change must be live, not cached"
        );
    }

    #[tokio::test]
    async fn me_upstream_401_clears_cookie() {
        let (app, upstream, cookie) = login_and_get_cookie().await;

        upstream.mock().reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(401))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/auth/me")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("upstream 401 must clear the session cookie")
            .to_str()
            .unwrap();
        assert!(
            set_cookie.starts_with("fleet_session=;"),
            "got: {set_cookie}"
        );
        assert!(set_cookie.contains("Max-Age=0"), "got: {set_cookie}");
    }

    #[tokio::test]
    async fn me_upstream_403_preserves_cookie() {
        let (app, upstream, cookie) = login_and_get_cookie().await;

        upstream.mock().reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(403))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/auth/me")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "no-grant 403 must NOT clear the shared cookie"
        );
    }

    #[tokio::test]
    async fn me_grant_removed_upstream_is_403_preserving_cookie() {
        let (app, upstream, cookie) = login_and_get_cookie().await;

        upstream.mock().reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "sibling-analyst",
                &[],
            )))
            .mount(upstream.mock())
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/auth/me")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    #[tokio::test]
    async fn me_rejects_missing_cookie() {
        let state = test_state(unreached_upstream());
        let app = routes::build(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/auth/me")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn me_rejects_tampered_cookie() {
        let (app, _upstream, cookie) = login_and_get_cookie().await;
        // Flip a byte in the cookie value (not the name).
        let (name, value) = cookie.split_once('=').unwrap();
        let mut bytes = value.as_bytes().to_vec();
        bytes[5] ^= 0x01;
        let tampered = format!("{name}={}", String::from_utf8_lossy(&bytes));

        let req = Request::builder()
            .method("GET")
            .uri("/api/auth/me")
            .header("cookie", &tampered)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_clears_cookie() {
        let (app, _upstream, _cookie) = login_and_get_cookie().await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.starts_with("fleet_session=;"),
            "got: {set_cookie}"
        );
        assert!(set_cookie.contains("Max-Age=0"));
        assert!(set_cookie.contains("SameSite=Lax"), "got: {set_cookie}");
    }
}
