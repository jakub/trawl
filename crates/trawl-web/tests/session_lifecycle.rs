// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fleet_auth::{SessionExpiry, SessionPayload, encrypt};
use tower::ServiceExt;
use trawl_config::WebConfig;
use trawl_web::config::ResolvedConfig;
use trawl_web::routes;
use trawl_web::state::AppState;
use zeroize::Zeroizing;

fn test_state(allow_insecure_cookies: bool) -> AppState {
    let web = WebConfig {
        allow_insecure_cookies,
        public_origins: vec!["https://trawl.example.com".to_owned()],
        ..WebConfig::default()
    };
    AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap()
}

fn expired_cookie(state: &AppState) -> String {
    let past_exp = chrono::Utc::now().timestamp() - 60;
    let payload = SessionPayload {
        token: Zeroizing::new("flt_irrelevant".into()),
        name: "alice".into(),
        exp: SessionExpiry::from_unix_seconds(past_exp),
    };
    let value = encrypt(state.cookie_key(), &payload).unwrap();
    format!("fleet_session={value}")
}

#[tokio::test]
async fn expired_session_returns_401_with_clear_cookie() {
    let state = test_state(true);
    let app = routes::build(state.clone());
    let cookie = expired_cookie(&state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("expired-session path must include Set-Cookie clear directive")
        .to_str()
        .unwrap();

    assert!(set_cookie.starts_with("fleet_session=;"));
    assert!(set_cookie.contains("Max-Age=0"));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));
    assert!(!set_cookie.contains("Secure"));
}

#[tokio::test]
async fn expired_cookie_in_prod_mode_includes_secure() {
    let state = test_state(false);
    let app = routes::build(state.clone());
    let cookie = expired_cookie(&state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(set_cookie.contains("Secure"));
}

#[tokio::test]
async fn missing_cookie_does_not_send_set_cookie() {
    let app = routes::build(test_state(true));

    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !resp.headers().contains_key(header::SET_COOKIE),
        "401 on missing cookie must not include Set-Cookie"
    );
}

#[tokio::test]
async fn tampered_cookie_does_not_send_set_cookie() {
    let state = test_state(true);
    let app = routes::build(state.clone());

    let payload = SessionPayload {
        token: Zeroizing::new("flt_x".into()),
        name: "x".into(),
        exp: SessionExpiry::from_unix_seconds(chrono::Utc::now().timestamp() + 3600),
    };
    let mut value = encrypt(state.cookie_key(), &payload).unwrap();
    let mid = value.len() / 2;
    let orig = value.as_bytes()[mid];
    let replacement = if orig == b'A' { 'B' } else { 'A' };
    value.replace_range(mid..=mid, &replacement.to_string());
    let cookie = format!("fleet_session={value}");

    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !resp.headers().contains_key(header::SET_COOKIE),
        "401 on tampered cookie must not include Set-Cookie"
    );
}
