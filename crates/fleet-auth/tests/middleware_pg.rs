// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed integration tests for `require_session` and
//! `require_bearer` middleware (ADR-0030).
//!
//! Builds a real `axum::Router` with the middleware layered on, fires
//! requests via `tower::ServiceExt::oneshot`, and asserts both response
//! shape and that downstream handlers receive a populated
//! `Extension<VerifiedKey>`. Real `KeyStore` against an ephemeral
//! per-test Postgres database (via `#[sqlx::test]`).

#![cfg(feature = "axum")]

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::get;
use fleet_auth::{
    KeyStore, PrincipalKind, PublicOrigins, RolePermission, SessionConfig, SessionExpiry,
    SessionKey, SessionPayload, SessionState, VerifiedKey, encrypt, require_bearer,
    require_session,
};
use tower::ServiceExt as _;
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Downstream handler used by every test — proves the middleware actually
/// populated `Extension<VerifiedKey>` before forwarding.
async fn echo_handler(Extension(key): Extension<VerifiedKey>) -> impl IntoResponse {
    format!(
        "ok name={} kind={} roles={}",
        key.name,
        key.kind,
        key.roles_display()
    )
}

fn session_config(app_namespace: &str) -> SessionConfig {
    // ADR-0016: `public_origins` is required, so even a fixture that never
    // sends an `Origin` states one. These tests exercise `RequireSession`,
    // which does not run the origin guard.
    SessionConfig::new(
        "fleet_session",
        app_namespace,
        PublicOrigins::parse(["https://trawl.example.com"]).expect("valid allowlist"),
    )
    .expect("valid session config")
}

fn session_state(store: KeyStore, app_namespace: &str) -> (SessionState, Arc<SessionKey>) {
    let session_key = Arc::new(SessionKey::generate());
    let config = Arc::new(session_config(app_namespace));
    let state = SessionState::new(store, Arc::clone(&session_key), config).unwrap();
    (state, session_key)
}

fn session_router(state: SessionState) -> Router {
    Router::new()
        .route("/protected", get(echo_handler))
        .route_layer(from_fn_with_state(state, require_session))
}

fn bearer_router(state: SessionState) -> Router {
    Router::new()
        .route("/api/thing", get(echo_handler))
        .route_layer(from_fn_with_state(state, require_bearer))
}

fn cookie_header(name: &str, value: &str) -> String {
    format!("{name}={value}")
}

fn issue_session_cookie(session_key: &SessionKey, token: &str, ttl_secs: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let payload = SessionPayload {
        token: Zeroizing::new(token.to_owned()),
        name: "alice".to_owned(),
        exp: SessionExpiry::after_duration(now, ttl_secs),
    };
    encrypt(session_key, &payload).expect("encrypt cookie")
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

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------------------
// require_session
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn session_valid_cookie_sets_extension(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    let created = store
        .create_key("alice", PrincipalKind::Human, &roles, None)
        .await
        .unwrap();

    let (state, session_key) = session_state(store, "trawl");
    let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, 3600);
    let app = session_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(
                    header::COOKIE,
                    cookie_header("fleet_session", &cookie_value),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "happy path must not set/clear a cookie"
    );
    let body = body_string(response).await;
    assert!(body.starts_with("ok name=alice"), "got: {body}");
    assert!(body.contains("roles=trawl-analyst"), "got: {body}");
}

#[sqlx::test]
async fn session_missing_cookie_returns_401(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = session_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
}

#[sqlx::test]
async fn session_expired_cookie_returns_401_keeps_cookie(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    let created = store
        .create_key("alice", PrincipalKind::Human, &roles, None)
        .await
        .unwrap();

    let (state, session_key) = session_state(store, "trawl");
    // ttl_secs negative → exp in the past → is_expired true
    let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, -60);
    let app = session_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(
                    header::COOKIE,
                    cookie_header("fleet_session", &cookie_value),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // ADR-0030: do NOT clear shared cookie on expiry — would log user out
    // of every sibling app.
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "expired session must NOT clear the cookie"
    );
}

#[sqlx::test]
async fn session_tampered_cookie_returns_401(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    let created = store
        .create_key("alice", PrincipalKind::Human, &roles, None)
        .await
        .unwrap();

    let (state, session_key) = session_state(store, "trawl");
    let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, 3600);
    let mut bytes = cookie_value.into_bytes();
    let mid = bytes.len() / 2;
    bytes[mid] = if bytes[mid] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(bytes).unwrap();

    let app = session_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(header::COOKIE, cookie_header("fleet_session", &tampered))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn session_no_grant_returns_403_html_keeps_cookie(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    // Key has trawl permissions only, but the app is configured as
    // "coastwatch" — zero permissions in the app namespace → 403.
    let created = store
        .create_key("alice", PrincipalKind::Human, &roles, None)
        .await
        .unwrap();

    let (state, session_key) = session_state(store, "coastwatch");
    let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, 3600);
    let app = session_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(
                    header::COOKIE,
                    cookie_header("fleet_session", &cookie_value),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "no-grant 403 must NOT clear the cookie (ADR-0030)"
    );
    let body = body_string(response).await;
    assert!(body.contains("alice"), "got: {body}");
    assert!(body.contains("coastwatch"), "got: {body}");
}

#[sqlx::test]
async fn session_grant_revoked_during_session_returns_403(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    // Key remains active, but its only role is unassigned after the
    // cookie was issued. The next request must take the no-permission
    // 403 path, proving middleware re-resolves roles from the DB on
    // each request rather than trusting cached state.
    let created = store
        .create_key("alice", PrincipalKind::Human, &roles, None)
        .await
        .unwrap();

    let (state, session_key) = session_state(store.clone(), "trawl");
    let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, 3600);
    let app = session_router(state);

    store
        .unassign_role(&created.info.prefix, "trawl-analyst")
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(
                    header::COOKIE,
                    cookie_header("fleet_session", &cookie_value),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response.headers().contains_key(header::SET_COOKIE),
        "no-grant 403 must NOT clear the cookie (ADR-0030)"
    );
}

#[sqlx::test]
async fn session_revoked_key_returns_401(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    let created = store
        .create_key("alice", PrincipalKind::Human, &roles, None)
        .await
        .unwrap();

    let (state, session_key) = session_state(store.clone(), "trawl");
    let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, 3600);

    store.revoke_key(&created.info.prefix).await.unwrap();

    let app = session_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(
                    header::COOKIE,
                    cookie_header("fleet_session", &cookie_value),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// require_bearer
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn bearer_valid_token_sets_extension(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    let created = store
        .create_key("svc", PrincipalKind::Service, &roles, None)
        .await
        .unwrap();

    let (state, _) = session_state(store, "trawl");
    let app = bearer_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/thing")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", created.plaintext_token.as_str()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("name=svc"), "got: {body}");
    assert!(body.contains("kind=service"), "got: {body}");
}

#[sqlx::test]
async fn bearer_missing_header_returns_401(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = bearer_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/thing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn bearer_malformed_scheme_returns_401(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = bearer_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/thing")
                .header(header::AUTHORIZATION, "Basic dXNlcjpwYXNz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn bearer_invalid_token_returns_401(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let (state, _) = session_state(store, "trawl");
    let app = bearer_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/thing")
                .header(
                    header::AUTHORIZATION,
                    "Bearer flt_nope_not_a_real_token_at_all",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn bearer_does_not_enforce_namespace(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let roles = trawl_role(&store).await;

    // A key with no permission in "coastwatch" still passes bearer
    // middleware configured for "coastwatch": per ADR-0030 cross-app
    // service principals must be allowed past, and each app gates further
    // with its own permission guard.
    let created = store
        .create_key("svc", PrincipalKind::Service, &roles, None)
        .await
        .unwrap();

    let (state, _) = session_state(store, "coastwatch");
    let app = bearer_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/thing")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", created.plaintext_token.as_str()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// build-time sanity: the builder rejects invalid config before construction
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn session_state_rejects_invalid_config(pool: sqlx::PgPool) {
    let _store = KeyStore::from_pool(pool);

    // External code can't construct an invalid SessionConfig:
    // `#[non_exhaustive]` and `pub(crate)` fields force every external
    // value through `SessionConfig::builder().build()`, which validates
    // first. So the reachable check is the builder's, and this asserts it
    // rejects an empty cookie name.
    let err = SessionConfig::builder()
        .cookie_name("")
        .app_namespace("trawl")
        .public_origins(PublicOrigins::parse(["https://trawl.example.com"]).unwrap())
        .build()
        .unwrap_err();
    assert!(matches!(err, fleet_auth::AuthError::InvalidApp(_)));
}
