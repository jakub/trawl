// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed integration tests for [`fleet_auth::login`] and
//! [`fleet_auth::logout`] (ADR-0030).

#![cfg(feature = "axum")]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::routing::post;
use fleet_auth::{
    KeyStore, PrincipalKind, PublicOrigins, RolePermission, SessionConfig, SessionKey,
    SessionState, decrypt, login, logout,
};
use tower::ServiceExt as _;

fn router_with_state(state: SessionState) -> Router {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .with_state(state)
}

/// The deployment's browser-visible origin, as every fixture configures it
/// (ADR-0016 makes it required, so there is no fixture without one).
fn test_origins() -> PublicOrigins {
    PublicOrigins::parse(["https://trawl.example.com"]).expect("valid allowlist")
}

fn session_state(store: KeyStore, app_namespace: &str) -> (SessionState, Arc<SessionKey>) {
    let session_key = Arc::new(SessionKey::generate());
    let cfg = SessionConfig::builder()
        .cookie_name("fleet_session")
        .app_namespace(app_namespace)
        .public_origins(test_origins())
        .secure(false) // tests don't run over HTTPS
        .post_login_redirect("/dashboard")
        .build()
        .unwrap();
    let state = SessionState::new(store, Arc::clone(&session_key), Arc::new(cfg)).unwrap();
    (state, session_key)
}

/// Seed the `trawl-analyst` role and return its name as the role list every
/// test key is created with.
async fn trawl_role(store: &KeyStore) -> Vec<String> {
    store
        .create_role(
            "trawl-analyst",
            None,
            &[RolePermission {
                app: "trawl".into(),
                permission: "query".into(),
            }],
        )
        .await
        .expect("seed trawl-analyst role");
    vec!["trawl-analyst".to_owned()]
}

fn login_request(api_key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"api_key":"{api_key}"}}"#)))
        .unwrap()
}

fn logout_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .body(Body::empty())
        .unwrap()
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn login_valid_key_sets_cookie_and_redirects(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let (state, session_key) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app
        .oneshot(login_request(&created.plaintext_token))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers().get(header::LOCATION).unwrap(),
        "/dashboard"
    );

    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("Set-Cookie")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        set_cookie.starts_with("fleet_session="),
        "got: {set_cookie}"
    );
    assert!(set_cookie.contains("HttpOnly"), "got: {set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "got: {set_cookie}");
    assert!(set_cookie.contains("Path=/"), "got: {set_cookie}");
    assert!(set_cookie.contains("Max-Age="), "got: {set_cookie}");
    // secure=false in test → no Secure attribute
    assert!(!set_cookie.contains("Secure"), "got: {set_cookie}");

    // Extract the cookie value and decrypt — proves the round-trip works.
    let pair = set_cookie.split(';').next().unwrap();
    let value = pair.split_once('=').unwrap().1;
    let payload = decrypt(&session_key, value).unwrap();
    assert_eq!(payload.name, "alice");
    assert_eq!(payload.token.as_str(), created.plaintext_token.as_str());
}

#[sqlx::test]
async fn login_includes_domain_when_configured(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let session_key = Arc::new(SessionKey::generate());
    let cfg = SessionConfig::builder()
        .cookie_name("fleet_session")
        .app_namespace("trawl")
        .public_origins(test_origins())
        .secure(false)
        .domain("fleet.localhost")
        .post_login_redirect("/")
        .build()
        .unwrap();
    let state = SessionState::new(store, session_key, Arc::new(cfg)).unwrap();
    let app = router_with_state(state);

    let response = app
        .oneshot(login_request(&created.plaintext_token))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        set_cookie.contains("Domain=fleet.localhost"),
        "got: {set_cookie}"
    );
}

#[sqlx::test]
async fn login_rejects_wrong_key(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app
        .oneshot(login_request("flt_completelybogusvalue"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "failed login must not set a cookie"
    );
}

#[sqlx::test]
async fn login_rejects_empty_key(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app.oneshot(login_request("")).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
}

#[sqlx::test]
async fn login_no_grant_returns_403_no_cookie(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    // Key has grant in trawl, but app namespace is coastwatch.
    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let (state, _) = session_state(store, "coastwatch");
    let app = router_with_state(state);

    let response = app
        .oneshot(login_request(&created.plaintext_token))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "no-grant login must not set a cookie"
    );
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    let body = body_string(response).await;
    assert!(body.contains("alice"), "got: {body}");
    assert!(body.contains("coastwatch"), "got: {body}");
}

// ---------------------------------------------------------------------------
// origin validation (default-on, ADR-0016)
// ---------------------------------------------------------------------------
//
// The guard compares the whole `Origin` against the app's configured
// `public_origins`. No request header contributes: not `Host`, not
// `:authority`, not `Forwarded` or `X-Forwarded-*`. These tests drive the
// real handlers, so they prove the wiring (guard first, no `Set-Cookie` on
// the 403) rather than the comparison, which `session.rs` owns.

fn login_request_with_origin(api_key: &str, origin: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .header("origin", origin)
        .body(Body::from(format!(r#"{{"api_key":"{api_key}"}}"#)))
        .unwrap()
}

fn logout_request_with_origin(origin: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header("origin", origin)
        .body(Body::empty())
        .unwrap()
}

#[sqlx::test]
async fn login_rejects_cross_origin_no_cookie(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app
        .oneshot(login_request_with_origin(
            &created.plaintext_token,
            "https://evil.example.com",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "cross-origin login must not set a cookie"
    );
}

#[sqlx::test]
async fn logout_rejects_cross_origin_no_clear(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app
        .oneshot(logout_request_with_origin("https://evil.example.com"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "cross-origin logout must NOT clear the shared cookie"
    );
}

#[sqlx::test]
async fn login_allows_the_configured_origin(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app
        .oneshot(login_request_with_origin(
            &created.plaintext_token,
            "https://trawl.example.com",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
    assert!(response.headers().contains_key(header::SET_COOKIE));
}

#[sqlx::test]
async fn login_rejects_the_same_name_over_http(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    // The whole point of ADR-0016: an active attacker serving
    // `http://trawl.example.com` used to pass the host-only comparison.
    let response = app
        .oneshot(login_request_with_origin(
            &created.plaintext_token,
            "http://trawl.example.com",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
}

#[sqlx::test]
async fn login_rejects_sibling_under_shared_domain(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let session_key = Arc::new(SessionKey::generate());
    let cfg = SessionConfig::builder()
        .cookie_name("fleet_session")
        .app_namespace("trawl")
        .public_origins(PublicOrigins::parse(["https://trawl.fleet.localhost"]).unwrap())
        .secure(false)
        .domain("fleet.localhost")
        .build()
        .unwrap();
    let state = SessionState::new(store, session_key, Arc::new(cfg)).unwrap();
    let app = router_with_state(state);

    // The Origin is a sibling app under the shared cookie domain. Sharing
    // `Domain=` governs where the browser sends the cookie, never who may
    // call these endpoints: each app lists only its own origins.
    let response = app
        .oneshot(login_request_with_origin(
            &created.plaintext_token,
            "https://coastwatch.fleet.localhost",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "sibling-origin login under shared domain must not set a cookie"
    );
}

#[sqlx::test]
async fn logout_allows_absent_origin(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    // Non-browser clients send no Origin — logout keeps working.
    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app.oneshot(logout_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().contains_key(header::SET_COOKIE));
}

// -- Host is not an input any more ------------------------------------------
//
// These two replace the HTTP/2 `:authority` cases. That fallback existed
// because the verdict needed a request host and HTTP/2 puts it somewhere
// else; ADR-0016 deleted the need, so the interesting claims are now that a
// request with NO host information at all still passes, and that a hostile
// one still fails.

#[sqlx::test]
async fn login_allows_the_configured_origin_with_no_host_header(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "alice",
            PrincipalKind::Human,
            &trawl_role(&store).await,
            None,
        )
        .await
        .unwrap();

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .header("origin", "https://trawl.example.com")
        .body(Body::from(format!(
            r#"{{"api_key":"{}"}}"#,
            created.plaintext_token.as_str()
        )))
        .unwrap();
    assert!(request.headers().get(header::HOST).is_none());
    assert!(request.uri().authority().is_none());

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FOUND);
    assert!(
        response.headers().contains_key(header::SET_COOKIE),
        "the configured origin passes with no host information in the request"
    );
}

#[sqlx::test]
async fn logout_rejects_a_sibling_origin_whatever_the_host_says(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    // Every header an attacker might hope moves the verdict, all agreeing
    // with the forged Origin. Under the old host-only rule this request
    // passed; now none of them is read.
    let request = Request::builder()
        .method("POST")
        .uri("https://evil.example.com/api/auth/logout")
        .header("origin", "https://evil.example.com")
        .header("host", "evil.example.com")
        .header("x-forwarded-host", "evil.example.com")
        .header("x-forwarded-proto", "https")
        .header("forwarded", "host=evil.example.com;proto=https")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "cross-origin logout must NOT clear the shared cookie"
    );
}

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn logout_clears_cookie_with_matching_attrs(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let session_key = Arc::new(SessionKey::generate());
    let cfg = SessionConfig::builder()
        .cookie_name("fleet_session")
        .app_namespace("trawl")
        .public_origins(test_origins())
        .secure(true)
        .domain("fleet.home.lan")
        .build()
        .unwrap();
    let state = SessionState::new(store, session_key, Arc::new(cfg)).unwrap();
    let app = router_with_state(state);

    let response = app.oneshot(logout_request()).await.unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("logout sets clear cookie")
        .to_str()
        .unwrap();
    assert!(
        set_cookie.starts_with("fleet_session=;"),
        "got: {set_cookie}"
    );
    assert!(set_cookie.contains("Max-Age=0"), "got: {set_cookie}");
    assert!(
        set_cookie.contains("Domain=fleet.home.lan"),
        "got: {set_cookie}"
    );
    assert!(set_cookie.contains("SameSite=Lax"), "got: {set_cookie}");
    assert!(set_cookie.contains("Path=/"), "got: {set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "got: {set_cookie}");
    assert!(set_cookie.contains("Secure"), "got: {set_cookie}");
}

#[sqlx::test]
async fn logout_requires_no_session(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    // Even without a valid cookie, logout succeeds — the browser was
    // already in a confused state, our job is to make sure the cookie is
    // gone, not to gate on whether it was valid.
    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app.oneshot(logout_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().contains_key(header::SET_COOKIE));
}
