// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed integration tests for `require_session` and
//! `require_bearer` middleware (ADR-0030, issue coastwatch#34).
//!
//! Builds a real `axum::Router` with the middleware layered on, fires
//! requests via `tower::ServiceExt::oneshot`, and asserts both response
//! shape and that downstream handlers receive a populated
//! `Extension<VerifiedKey>`. Real `KeyStore` against an ephemeral
//! per-test Postgres database (see `common::PgFixture`).

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
    KeyStore, PrincipalKind, RoleAssignment, SessionConfig, SessionExpiry, SessionKey,
    SessionPayload, SessionState, VerifiedKey, encrypt, require_bearer, require_session,
};
use tower::ServiceExt as _;
use zeroize::Zeroizing;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Downstream handler used by every test — proves the middleware actually
/// populated `Extension<VerifiedKey>` before forwarding.
async fn echo_handler(Extension(key): Extension<VerifiedKey>) -> impl IntoResponse {
    format!(
        "ok name={} kind={} grants={}",
        key.name,
        key.kind,
        key.assignments_display()
    )
}

fn session_config(app_namespace: &str) -> SessionConfig {
    SessionConfig::new("fleet_session", app_namespace).expect("valid session config")
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

fn trawl_grant() -> Vec<RoleAssignment> {
    vec![RoleAssignment {
        app: "trawl".into(),
        role: "analyst".into(),
    }]
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

pg_test!(
    session_valid_cookie_sets_extension,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
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
        assert!(body.contains("grants=trawl:analyst"), "got: {body}");
    }
);

pg_test!(
    session_missing_cookie_returns_401,
    |store: KeyStore| async move {
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
);

pg_test!(
    session_expired_cookie_returns_401_keeps_cookie,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
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
);

pg_test!(
    session_tampered_cookie_returns_401,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let (state, session_key) = session_state(store, "trawl");
        let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, 3600);
        // Flip a single character somewhere in the middle of the base64 body.
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
);

pg_test!(
    session_no_grant_returns_403_html_keeps_cookie,
    |store: KeyStore| async move {
        // Key has a grant for trawl, but the app is configured as "coastwatch".
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
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
);

pg_test!(
    session_grant_revoked_during_session_returns_403,
    |store: KeyStore| async move {
        // Key remains active, but the ONLY namespace grant is revoked
        // after the cookie was issued. The next request must take the
        // no-grant 403 path, proving middleware re-reads grants from the
        // DB on each request rather than trusting cached state.
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let (state, session_key) = session_state(store.clone(), "trawl");
        let cookie_value = issue_session_cookie(&session_key, &created.plaintext_token, 3600);
        let app = session_router(state);

        store
            .revoke_assignment(&created.info.prefix, "trawl")
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
);

pg_test!(
    session_revoked_key_returns_401,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
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
);

// ---------------------------------------------------------------------------
// require_bearer
// ---------------------------------------------------------------------------

pg_test!(
    bearer_valid_token_sets_extension,
    |store: KeyStore| async move {
        let created = store
            .create_key("svc", PrincipalKind::Service, &trawl_grant(), None)
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
);

pg_test!(
    bearer_missing_header_returns_401,
    |store: KeyStore| async move {
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
);

pg_test!(
    bearer_malformed_scheme_returns_401,
    |store: KeyStore| async move {
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
);

pg_test!(
    bearer_invalid_token_returns_401,
    |store: KeyStore| async move {
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
);

pg_test!(
    bearer_does_not_enforce_namespace,
    |store: KeyStore| async move {
        // A key with NO grant in "coastwatch" still passes bearer middleware
        // configured for "coastwatch" — per ADR-0030 cross-app service
        // principals must be allowed past, and each app gates further with
        // its own role guard.
        let created = store
            .create_key("svc", PrincipalKind::Service, &trawl_grant(), None)
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
);

// ---------------------------------------------------------------------------
// build-time sanity: SessionState rejects invalid config at construction
// ---------------------------------------------------------------------------

pg_test!(
    session_state_rejects_invalid_config,
    |store: KeyStore| async move {
        let bad_config = Arc::new(SessionConfig {
            cookie_name: String::new(), // empty — must reject
            ..SessionConfig::default()
        });
        let session_key = Arc::new(SessionKey::generate());
        let err = SessionState::new(store, session_key, bad_config).unwrap_err();
        assert!(matches!(err, fleet_auth::AuthError::InvalidApp(_)));
    }
);
