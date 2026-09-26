// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy never follows an upstream redirect.
//!
//! With `TRAWL_WEB_INSECURE_UPSTREAM` on, whatever holds the loopback port
//! could answer 3xx and send the proxy, with certificate checks off, to a
//! second host. That would defeat the loopback-only rule, so the upstream
//! client follows no redirect in any trust mode. Each test runs two real
//! listeners: the upstream answers 307 to the other, and the other must see
//! no request at all.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt;
use trawl_config::WebConfig;
use trawl_web::config::{ResolvedConfig, UpstreamTls};
use trawl_web::routes;
use trawl_web::state::AppState;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://trawl.example.com";

/// The trust modes a plain-http test upstream can exercise. A pinned CA
/// refuses plain http before any request, so it cannot reach a 3xx here.
fn modes() -> [UpstreamTls; 2] {
    [UpstreamTls::System, UpstreamTls::InsecureLoopback]
}

fn state(upstream: &MockServer, tls: UpstreamTls) -> AppState {
    let web = WebConfig {
        upstream_url: Some(upstream.uri()),
        allow_insecure_cookies: true,
        public_origins: vec![ORIGIN.to_owned()],
        ..WebConfig::default()
    };
    let mut resolved = ResolvedConfig::from_parsed(&web, None).expect("resolve config");
    // The insecure mode is chosen from the process environment, which a
    // test may not mutate; the upstream is loopback, so it is a mode the
    // resolver would pick.
    resolved.upstream_tls = tls;
    AppState::from_config(resolved).expect("build state")
}

fn whoami_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "name": "alice",
        "roles": ["operator"],
        "permissions": ["query"]
    }))
}

/// Answer `route` on `upstream` with a 307 to the same path on `elsewhere`,
/// and answer it on `elsewhere` as if the redirect were legitimate.
async fn redirect(
    upstream: &MockServer,
    elsewhere: &MockServer,
    route: &str,
    ok: ResponseTemplate,
) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(307).insert_header(
            header::LOCATION.as_str(),
            format!("{}{route}", elsewhere.uri()),
        ))
        .mount(upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ok)
        .mount(elsewhere)
        .await;
}

async fn login(state: AppState) -> axum::response::Response {
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"api_key":"flt_test_not_real"}"#))
        .expect("request");
    routes::build(state)
        .oneshot(request)
        .await
        .expect("router answers")
}

async fn assert_untouched(elsewhere: &MockServer, mode: &str) {
    let seen = elsewhere.received_requests().await.expect("recording on");
    assert!(
        seen.is_empty(),
        "{mode}: the proxy followed the redirect to the second listener: {:?}",
        seen.iter().map(|r| r.url.to_string()).collect::<Vec<_>>()
    );
}

/// Login asks the upstream `/whoami` with the key as a bearer token.
#[tokio::test]
async fn login_does_not_follow_an_upstream_redirect() {
    for tls in modes() {
        let mode = format!("{tls:?}");
        let upstream = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        redirect(&upstream, &elsewhere, "/api/v1/whoami", whoami_ok()).await;

        let response = login(state(&upstream, tls)).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{mode}");
        assert!(
            response.headers().get(header::SET_COOKIE).is_none(),
            "{mode}: a redirected login must not issue a session"
        );
        assert_untouched(&elsewhere, &mode).await;
    }
}

/// The generic `/api/v1/*` forwarder.
#[tokio::test]
async fn forwarding_does_not_follow_an_upstream_redirect() {
    for tls in modes() {
        let mode = format!("{tls:?}");
        let upstream = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(whoami_ok())
            .mount(&upstream)
            .await;
        redirect(
            &upstream,
            &elsewhere,
            "/api/v1/schema",
            ResponseTemplate::new(200).set_body_json(json!({"columns": []})),
        )
        .await;

        let state = state(&upstream, tls);
        let response = login(state.clone()).await;
        assert_eq!(response.status(), StatusCode::OK, "{mode}");
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("a session cookie")
            .to_str()
            .expect("ASCII cookie")
            .split(';')
            .next()
            .expect("cookie pair")
            .to_owned();

        let request = Request::builder()
            .uri("/api/v1/schema")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .expect("request");
        let response = routes::build(state)
            .oneshot(request)
            .await
            .expect("router answers");
        assert_ne!(
            response.status(),
            StatusCode::OK,
            "{mode}: the proxy answered with the second listener's body"
        );
        assert_untouched(&elsewhere, &mode).await;
    }
}
