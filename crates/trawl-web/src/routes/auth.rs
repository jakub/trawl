// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Authentication endpoints: `/login`, `/logout`, `/me`.
//!
//! The login flow is:
//! 1. Browser POSTs `{api_key}` → proxy calls upstream `/whoami` with that
//!    bearer token to validate and fetch identity metadata.
//! 2. On 200 (with a trawl grant), proxy encrypts the app-agnostic
//!    `{token, name, exp}` payload into the shared `fleet_session` AEAD
//!    cookie (`HttpOnly; SameSite=Lax; Path=/`, `Secure` per config,
//!    `Domain=` per `shared_domain`).
//! 3. Browser is redirected client-side by the SPA.
//!
//! `role` is deliberately NOT in the cookie (ADR-0004 slice 2): the
//! payload must be byte-identical across fleet apps for SSO, so `/me`
//! re-derives the trawl role from upstream `/whoami` on every request.
//!
//! Login and logout validate the `Origin` header (present-only semantics,
//! same helper as `fleet_auth::login`/`logout`) — with the shared cookie a
//! forged cross-site logout would sign the user out of every fleet app.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use fleet_auth::{SessionExpiry, SessionPayload, session};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::ProxyError;
use crate::middleware::session_extractor::Session;
use crate::state::AppState;

/// Request body for `POST /login`.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub api_key: String,
}

/// Response body for `POST /login`.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub name: String,
    pub role: String,
}

/// Reject cross-origin browser requests to cookie-authed, state-changing
/// endpoints.
///
/// Delegates to the shared [`fleet_auth::check_origin`] — the same
/// present-only decision, log fields, and message that the fleet-auth
/// substrate handlers use (ADR-0004 slice 2) — and maps its rejection onto
/// this proxy's [`ProxyError::OriginMismatch`].
///
/// Used by the auth endpoints (`login`/`logout`) AND by the cookie-authed
/// branch of the generic proxy forwarder (`routes::proxy`): the shared
/// `fleet_session` cookie is `SameSite=Lax` and (in SSO mode) scoped to the
/// parent domain, so the browser attaches it to same-site *sibling*-origin
/// requests — this is the only thing standing between a compromised sibling
/// app and a forged state-changing request carrying the victim's session.
///
/// The request host is derived by the shared [`fleet_auth::request_host`]
/// (`Host` header, falling back to the URI's `:authority`) — the one
/// host-derivation fleet-auth's own handlers use too, so the HTTP/2
/// `:authority` handling can't drift between the two origin guards.
pub(crate) fn check_origin(
    headers: &HeaderMap,
    uri: &Uri,
    handler: &str,
) -> Result<(), ProxyError> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = session::request_host(headers, uri);
    session::check_origin(origin, host, handler).map_err(|_| ProxyError::OriginMismatch)
}

pub async fn login(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ProxyError> {
    check_origin(&headers, &uri, "login")?;

    if req.api_key.trim().is_empty() {
        return Err(ProxyError::BadRequest("api_key is required".into()));
    }

    let whoami = fetch_whoami(&state, &req.api_key).await?;

    // Valid key but no trawl grant → 403 with NO cookie: mirrors upstream
    // trawld's no-grant semantics (post-slice-1 trawld 403s grantless keys
    // before /whoami anyway; this is the belt-and-suspenders branch). The
    // user never gets a session for an app they can't access.
    let role = whoami
        .trawl_role()
        .ok_or(ProxyError::Upstream(StatusCode::FORBIDDEN))?;

    let now = chrono::Utc::now().timestamp();
    let ttl = i64::try_from(state.session_ttl_secs())
        .map_err(|_| ProxyError::Internal("session_ttl_secs out of i64 range".into()))?;

    // App-agnostic payload — {token, name, exp}, NO role. Byte-identical
    // to what coastwatch-web mints, which is the SSO compat contract.
    let payload = SessionPayload {
        token: Zeroizing::new(req.api_key),
        name: whoami.name.clone(),
        exp: SessionExpiry::after_duration(now, ttl),
    };

    let cookie_value = session::encrypt(state.cookie_key(), &payload)?;
    let cookie_header = state.build_session_cookie(cookie_value);

    let body = LoginResponse {
        name: whoami.name,
        role,
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

/// Local view of the upstream `/whoami` payload — only the fields the proxy
/// actually uses. The proxy never gates anything on non-trawl-app grants,
/// so we extract just the trawl role here.
#[derive(Debug, Deserialize)]
struct WhoAmI {
    name: String,
    #[serde(default)]
    assignments: Vec<UpstreamAssignment>,
}

#[derive(Debug, Deserialize)]
struct UpstreamAssignment {
    app: String,
    role: String,
}

impl WhoAmI {
    /// Role assigned in the trawl-app namespace, if any.
    fn trawl_role(&self) -> Option<String> {
        self.assignments
            .iter()
            .find(|a| a.app == "trawl")
            .map(|a| a.role.clone())
    }
}

async fn fetch_whoami(state: &AppState, token: &str) -> Result<WhoAmI, ProxyError> {
    let url = format!(
        "{}/api/v1/whoami",
        state.upstream_url().trim_end_matches('/')
    );
    let resp = state
        .http()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(ProxyError::Network)?;

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

/// Response body for `GET /me`.
#[derive(Debug, Serialize, Deserialize)]
pub struct MeResponse {
    pub name: String,
    pub role: String,
    pub exp: i64,
}

/// `GET /me` — identity + role for the SPA's auth shell.
///
/// The role is fetched LIVE from upstream `/whoami` on every call (it no
/// longer lives in the cookie), so a grant change takes effect on the next
/// request rather than at cookie expiry. Upstream mapping:
/// - `/whoami` 401 (key revoked/expired fleet-wide) → 401 WITH a clear
///   cookie — the session is dead everywhere.
/// - `/whoami` 200 but no trawl grant, or upstream 403 → 403 with the
///   cookie PRESERVED — the key may still hold grants in sibling apps.
pub async fn me(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<MeResponse>, ProxyError> {
    let whoami = fetch_whoami(&state, session.token())
        .await
        .map_err(|e| match e {
            // At login time Unauthorized just means "bad key, no cookie
            // yet". Here the browser IS holding a cookie for that key, so
            // upstream 401 means the session is dead fleet-wide — clear it.
            ProxyError::Unauthorized => ProxyError::ExpiredSession {
                clear_cookie: state.build_clear_cookie(),
            },
            other => other,
        })?;

    let role = whoami
        .trawl_role()
        .ok_or(ProxyError::Upstream(StatusCode::FORBIDDEN))?;

    Ok(Json(MeResponse {
        name: whoami.name,
        role,
        exp: session.exp(),
    }))
}

pub async fn logout(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ProxyError> {
    check_origin(&headers, &uri, "logout")?;

    let header_value = state.build_clear_cookie();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        header_value
            .parse()
            .map_err(|e: header::InvalidHeaderValue| ProxyError::Internal(e.to_string()))?,
    );
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
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::ResolvedConfig;
    use crate::routes;

    fn test_state(upstream_url: String) -> AppState {
        let web = WebConfig {
            upstream_url: Some(upstream_url),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    fn whoami_body(name: &str, app: &str, role: &str) -> serde_json::Value {
        json!({
            "prefix": "abcd1234",
            "name": name,
            "kind": "human",
            "assignments": [{"app": app, "role": role}],
            "permissions": ["query"]
        })
    }

    #[tokio::test]
    async fn login_accepts_valid_api_key() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .and(bearer_token("flt_goodtoken"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("alice", "trawl", "analyst")),
            )
            .mount(&upstream)
            .await;

        let state = test_state(upstream.uri());
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
        // allow_insecure_cookies=true in test => no `Secure`
        assert!(!set_cookie.contains("Secure"));
        // no shared_domain configured => no Domain attribute
        assert!(
            !set_cookie.to_lowercase().contains("domain="),
            "got: {set_cookie}"
        );
    }

    #[tokio::test]
    async fn login_rejects_wrong_api_key() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
            .await;

        let state = test_state(upstream.uri());
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
        // No MockServer needed — handler must fail input validation first.
        let state = test_state("http://unused".into());
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
        // Valid key, but every grant is for a sibling app. The proxy must
        // NOT mint a session (403, no Set-Cookie) — and must NOT clear
        // anything either: the key works elsewhere in the fleet.
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "bob",
                "coastwatch",
                "analyst",
            )))
            .mount(&upstream)
            .await;

        let state = test_state(upstream.uri());
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
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .and(bearer_token("flt_prod"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("prod", "trawl", "admin")),
            )
            .mount(&upstream)
            .await;

        let web = WebConfig {
            upstream_url: Some(upstream.uri()),
            allow_insecure_cookies: false,
            ..WebConfig::default()
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

    // -- cookie contract (AC #2) -----------------------------------------

    /// State with `shared_domain` set — SSO mode.
    fn sso_state(upstream_url: String) -> AppState {
        let web = WebConfig {
            upstream_url: Some(upstream_url),
            allow_insecure_cookies: true,
            shared_domain: Some(".fleet.test".into()),
            session_ttl_secs: Some(3600),
            ..WebConfig::default()
        };
        let cfg = ResolvedConfig::from_parsed(&web, None).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    #[tokio::test]
    async fn login_cookie_carries_domain_iff_shared_domain_set() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("alice", "trawl", "analyst")),
            )
            .mount(&upstream)
            .await;

        let app = routes::build(sso_state(upstream.uri()));
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
        // Full attribute contract in SSO mode. The `cookie` crate
        // normalises the leading dot away per RFC 6265.
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

    /// Split a Set-Cookie header into its attribute set (everything after
    /// the `name=value` pair), normalised for order-insensitive comparison.
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
        // Browsers reject clear directives whose Domain/Path/SameSite
        // don't match the issued cookie — the clear must carry the exact
        // attribute set login used (minus lifetime).
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("alice", "trawl", "analyst")),
            )
            .mount(&upstream)
            .await;

        let app = routes::build(sso_state(upstream.uri()));

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

    // -- cross-app cookie compat (AC #3) ----------------------------------

    #[tokio::test]
    async fn login_cookie_is_fleet_auth_compatible_with_role_less_payload() {
        // A cookie minted by trawl-web must decrypt via fleet_auth with a
        // payload whose JSON shape is exactly {token, name, exp} — the
        // byte-identical cross-app contract (no versioning exists).
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("web.key");
        let key_bytes = [0x42u8; fleet_auth::KEY_LEN];
        std::fs::write(&key_path, key_bytes).unwrap();

        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("alice", "trawl", "analyst")),
            )
            .mount(&upstream)
            .await;

        let web = WebConfig {
            upstream_url: Some(upstream.uri()),
            allow_insecure_cookies: true,
            cookie_secret_path: Some(key_path),
            ..WebConfig::default()
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

        // Decrypt with an INDEPENDENTLY constructed fleet-auth key — this
        // is what a sibling app holding the shared key does.
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

    // -- origin validation (AC #6) ----------------------------------------

    #[tokio::test]
    async fn login_rejects_cross_origin() {
        let state = test_state("http://unused".into());
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
        let state = test_state("http://unused".into());
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
        // Regression (ADR-0004 slice 2): a compromised sibling under the
        // shared domain — or attacker-hosted content on one — can auto-submit
        // a plain HTML form POST to trawl's logout endpoint. Its sibling
        // `Origin` must NOT be trusted just because it lives under the same
        // parent-domain cookie; the response must be 403 with NO Set-Cookie,
        // so `fleet_session` is not cleared fleet-wide.
        let app = routes::build(sso_state("http://unused".into()));

        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            // form-POST content type: no CORS preflight, no response access
            .header("content-type", "application/x-www-form-urlencoded")
            .header("origin", "https://coastwatch.fleet.test")
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
    async fn logout_allows_same_origin_h2_without_host_header() {
        // HTTP/2 regression: browsers send the `:authority` pseudo-header
        // instead of a `Host` header, which hyper parks in the request URI.
        // A present same-origin `Origin` must still be accepted — reading only
        // the (absent) Host header would 403 legitimate logout and NOT clear
        // the cookie. Absolute-form URI = authority present, no Host header.
        let app = routes::build(test_state("http://unused".into()));

        let req = Request::builder()
            .method("POST")
            .uri("https://trawl.example.com/api/auth/logout")
            .header("origin", "https://trawl.example.com")
            .body(Body::empty())
            .unwrap();
        assert!(req.headers().get(header::HOST).is_none());
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            response.headers().contains_key(header::SET_COOKIE),
            "same-origin h2 logout must clear the cookie"
        );
    }

    #[tokio::test]
    async fn logout_rejects_cross_origin_h2_via_authority_fallback() {
        // The `:authority` fallback must not weaken the guard: a cross-origin
        // POST with no Host header is still rejected against the URI authority.
        let app = routes::build(test_state("http://unused".into()));

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
    async fn login_allows_same_host_but_rejects_sso_sibling() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("alice", "trawl", "analyst")),
            )
            .mount(&upstream)
            .await;

        // Same-host origin, standalone mode → allowed.
        let app = routes::build(test_state(upstream.uri()));
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .header("origin", "https://trawl.example.com")
            .header("host", "trawl.example.com")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Sibling app under the SAME shared domain is a DIFFERENT origin and
        // must be rejected (403, no cookie). Sharing a parent-domain cookie
        // is not an origin allowlist — see `origin_allowed`.
        let app = routes::build(sso_state(upstream.uri()));
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .header("origin", "https://coastwatch.fleet.test")
            .header("host", "trawl.fleet.test")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    /// Returns just the `name=value` pair from a Set-Cookie header, ready
    /// to send back as a Cookie request header.
    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie.split(';').next().unwrap().trim().to_string()
    }

    async fn login_and_get_cookie() -> (axum::Router, MockServer, String) {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("alice", "trawl", "analyst")),
            )
            .mount(&upstream)
            .await;

        let state = test_state(upstream.uri());
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
        assert_eq!(body.role, "analyst");
        assert!(body.exp > chrono::Utc::now().timestamp());
    }

    #[tokio::test]
    async fn me_reflects_upstream_role_change_between_calls() {
        // AC #4: role lives upstream, not in the cookie. Mutating the mock
        // /whoami role between two calls on ONE cookie must be reflected —
        // proving a live fetch, not cookie residue.
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
        assert_eq!(body.role, "analyst");

        // Retype the principal upstream — same cookie, new role.
        upstream.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(whoami_body("alice", "trawl", "admin")),
            )
            .mount(&upstream)
            .await;

        let response = app.oneshot(me_req()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: MeResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.role, "admin", "role change must be live, not cached");
    }

    #[tokio::test]
    async fn me_upstream_401_clears_cookie() {
        // Key revoked fleet-wide after login: /me must 401 AND tell the
        // browser to drop the dead cookie.
        let (app, upstream, cookie) = login_and_get_cookie().await;

        upstream.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
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
        // Trawl grant revoked but key still valid (works in sibling apps):
        // /me must 403 WITHOUT touching the shared cookie.
        let (app, upstream, cookie) = login_and_get_cookie().await;

        upstream.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&upstream)
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
        // Same as above but via the whoami-200-without-trawl-grant branch.
        let (app, upstream, cookie) = login_and_get_cookie().await;

        upstream.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(whoami_body(
                "alice",
                "coastwatch",
                "analyst",
            )))
            .mount(&upstream)
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
        let state = test_state("http://unused".into());
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
