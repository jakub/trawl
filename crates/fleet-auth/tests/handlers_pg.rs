// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed integration tests for [`fleet_auth::login`] and
//! [`fleet_auth::logout`] (ADR-0030, issue coastwatch#34).

#![cfg(feature = "axum")]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::routing::post;
use fleet_auth::{
    KeyStore, PrincipalKind, RoleAssignment, SessionConfig, SessionKey, SessionState, decrypt,
    login, logout,
};
use tower::ServiceExt as _;

#[macro_use]
mod common;

fn router_with_state(state: SessionState) -> Router {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .with_state(state)
}

fn session_state(store: KeyStore, app_namespace: &str) -> (SessionState, Arc<SessionKey>) {
    let session_key = Arc::new(SessionKey::generate());
    let cfg = SessionConfig::builder()
        .cookie_name("fleet_session")
        .app_namespace(app_namespace)
        .secure(false) // tests don't run over HTTPS
        .post_login_redirect("/dashboard")
        .build()
        .unwrap();
    let state = SessionState::new(store, Arc::clone(&session_key), Arc::new(cfg)).unwrap();
    (state, session_key)
}

fn trawl_grant() -> Vec<RoleAssignment> {
    vec![RoleAssignment {
        app: "trawl".into(),
        role: "analyst".into(),
    }]
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

pg_test!(
    login_valid_key_sets_cookie_and_redirects,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
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
);

pg_test!(
    login_includes_domain_when_configured,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let session_key = Arc::new(SessionKey::generate());
        let cfg = SessionConfig::builder()
            .cookie_name("fleet_session")
            .app_namespace("trawl")
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
);

pg_test!(login_rejects_wrong_key, |store: KeyStore| async move {
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
});

pg_test!(login_rejects_empty_key, |store: KeyStore| async move {
    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app.oneshot(login_request("")).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
});

pg_test!(
    login_no_grant_returns_403_no_cookie,
    |store: KeyStore| async move {
        // Key has grant in trawl, but app namespace is coastwatch.
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
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
);

// ---------------------------------------------------------------------------
// origin validation (default-on, ADR-0004 slice 2)
// ---------------------------------------------------------------------------

fn login_request_with_origin(api_key: &str, origin: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("content-type", "application/json")
        .header("origin", origin)
        .header("host", host)
        .body(Body::from(format!(r#"{{"api_key":"{api_key}"}}"#)))
        .unwrap()
}

fn logout_request_with_origin(origin: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header("origin", origin)
        .header("host", host)
        .body(Body::empty())
        .unwrap()
}

pg_test!(
    login_rejects_cross_origin_no_cookie,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let (state, _) = session_state(store, "trawl");
        let app = router_with_state(state);

        let response = app
            .oneshot(login_request_with_origin(
                &created.plaintext_token,
                "https://evil.example.com",
                "trawl.example.com",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "cross-origin login must not set a cookie"
        );
    }
);

pg_test!(
    logout_rejects_cross_origin_no_clear,
    |store: KeyStore| async move {
        let (state, _) = session_state(store, "trawl");
        let app = router_with_state(state);

        let response = app
            .oneshot(logout_request_with_origin(
                "https://evil.example.com",
                "trawl.example.com",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "cross-origin logout must NOT clear the shared cookie"
        );
    }
);

pg_test!(
    login_allows_same_host_origin,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let (state, _) = session_state(store, "trawl");
        let app = router_with_state(state);

        let response = app
            .oneshot(login_request_with_origin(
                &created.plaintext_token,
                "http://trawl.example.com",
                "trawl.example.com",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(response.headers().contains_key(header::SET_COOKIE));
    }
);

pg_test!(
    login_rejects_sibling_under_shared_domain,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let session_key = Arc::new(SessionKey::generate());
        let cfg = SessionConfig::builder()
            .cookie_name("fleet_session")
            .app_namespace("trawl")
            .secure(false)
            .domain("fleet.localhost")
            .build()
            .unwrap();
        let state = SessionState::new(store, session_key, Arc::new(cfg)).unwrap();
        let app = router_with_state(state);

        // Origin is a sibling app under the shared cookie domain. A
        // parent-domain cookie is NOT an origin allowlist: origin
        // validation stays strictly same-host, so a compromised sibling
        // can't forge auth requests against trawl's endpoints (ADR-0004
        // slice 2, commit a527ccbf).
        let response = app
            .oneshot(login_request_with_origin(
                &created.plaintext_token,
                "http://coastwatch.fleet.localhost",
                "trawl.fleet.localhost",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "sibling-origin login under shared domain must not set a cookie"
        );
    }
);

pg_test!(logout_allows_absent_origin, |store: KeyStore| async move {
    // Non-browser clients send no Origin — logout keeps working.
    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app.oneshot(logout_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().contains_key(header::SET_COOKIE));
});

// -- HTTP/2 :authority fallback (no Host header) ----------------------------
//
// Under HTTP/2 browsers send the `:authority` pseudo-header instead of a
// `Host` header, which hyper parks in the request URI (absolute-form URI).
// These cases exercise the fallback end-to-end through the real handlers:
// `request_host`'s unit tests prove the lookup, but only a full login/logout
// request proves the wiring — a `reject_cross_origin` refactor that drops the
// `uri.authority()` fallback would 403 a legitimate same-origin h2 login while
// every `request_host` unit test still passes. Mirrors trawl-web's
// `logout_{allows_same,rejects_cross}_origin_h2_*` coverage.

/// Absolute-form URI (authority present) with an `Origin` header but no `Host`
/// header — the shape hyper produces for an HTTP/2 request.
fn h2_login_request(api_key: &str, origin: &str) -> Request<Body> {
    let req = Request::builder()
        .method("POST")
        .uri("https://trawl.example.com/api/auth/login")
        .header("content-type", "application/json")
        .header("origin", origin)
        .body(Body::from(format!(r#"{{"api_key":"{api_key}"}}"#)))
        .unwrap();
    assert!(req.headers().get(header::HOST).is_none());
    req
}

fn h2_logout_request(origin: &str) -> Request<Body> {
    let req = Request::builder()
        .method("POST")
        .uri("https://trawl.example.com/api/auth/logout")
        .header("origin", origin)
        .body(Body::empty())
        .unwrap();
    assert!(req.headers().get(header::HOST).is_none());
    req
}

pg_test!(
    login_allows_same_origin_h2_without_host_header,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let (state, _) = session_state(store, "trawl");
        let app = router_with_state(state);

        let response = app
            .oneshot(h2_login_request(
                &created.plaintext_token,
                "https://trawl.example.com",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(
            response.headers().contains_key(header::SET_COOKIE),
            "same-origin h2 login (Host from :authority) must set a cookie"
        );
    }
);

pg_test!(
    login_rejects_cross_origin_h2_via_authority_fallback,
    |store: KeyStore| async move {
        let created = store
            .create_key("alice", PrincipalKind::Human, &trawl_grant(), None)
            .await
            .unwrap();

        let (state, _) = session_state(store, "trawl");
        let app = router_with_state(state);

        let response = app
            .oneshot(h2_login_request(
                &created.plaintext_token,
                "https://evil.example.com",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "cross-origin h2 login must not set a cookie"
        );
    }
);

pg_test!(
    logout_allows_same_origin_h2_without_host_header,
    |store: KeyStore| async move {
        let (state, _) = session_state(store, "trawl");
        let app = router_with_state(state);

        let response = app
            .oneshot(h2_logout_request("https://trawl.example.com"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            response.headers().contains_key(header::SET_COOKIE),
            "same-origin h2 logout (Host from :authority) must clear the cookie"
        );
    }
);

pg_test!(
    logout_rejects_cross_origin_h2_via_authority_fallback,
    |store: KeyStore| async move {
        let (state, _) = session_state(store, "trawl");
        let app = router_with_state(state);

        let response = app
            .oneshot(h2_logout_request("https://evil.example.com"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "cross-origin h2 logout must NOT clear the shared cookie"
        );
    }
);

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

pg_test!(
    logout_clears_cookie_with_matching_attrs,
    |store: KeyStore| async move {
        let session_key = Arc::new(SessionKey::generate());
        let cfg = SessionConfig::builder()
            .cookie_name("fleet_session")
            .app_namespace("trawl")
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
);

pg_test!(logout_requires_no_session, |store: KeyStore| async move {
    // Even without a valid cookie, logout succeeds — the browser was
    // already in a confused state, our job is to make sure the cookie is
    // gone, not to gate on whether it was valid.
    let (state, _) = session_state(store, "trawl");
    let app = router_with_state(state);

    let response = app.oneshot(logout_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().contains_key(header::SET_COOKIE));
});
