// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy never follows an upstream redirect, and never hands one to
//! the browser.
//!
//! Every request the proxy sends upstream carries the signed-in user's key
//! as a bearer token. A followed 3xx would hand that key to whatever host
//! the `Location` names, as long as the trust mode accepts its
//! certificate: any host the pinned CA issued for, or, under the platform
//! roots, any host with a public certificate. So the upstream client
//! follows no redirect in any trust mode. Passing the 3xx through is no
//! better: the browser would follow it, and a `Location` naming another
//! port on the browser's host carries the host-scoped session cookie to
//! whatever listens there. So every browser-facing route answers an
//! upstream 3xx with the proxy's own 502 and no `Location`.
//!
//! Each test runs two real listeners: the upstream answers 307 to the
//! other, and the other must see no request at all. Both are TLS fronts
//! whose certificates come from the pinned CA, so the second one is a
//! target the client could reach, and only the redirect policy keeps it
//! away.

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

#[path = "../src/test_support.rs"]
mod test_support;

use test_support::{TestCa, TlsUpstream};

const ORIGIN: &str = "https://trawl.example.com";

/// The two listeners of one test run.
struct Leg {
    upstream: TlsUpstream,
    elsewhere: TlsUpstream,
}

impl Leg {
    async fn start() -> Self {
        // One CA for both, so the pin trusts the redirect target.
        let ca = TestCa::generate();
        Self {
            upstream: TlsUpstream::issued_by(&ca).await,
            elsewhere: TlsUpstream::issued_by(&ca).await,
        }
    }

    fn state(&self) -> AppState {
        let web = WebConfig {
            allow_insecure_cookies: true,
            public_origins: vec![ORIGIN.to_owned()],
            ..self.upstream.web_config()
        };
        let resolved = ResolvedConfig::from_parsed(&web, None).expect("resolve config");
        assert!(
            matches!(
                resolved.upstream_tls,
                UpstreamTls::PinnedCa { roots: Some(_), .. }
            ),
            "{:?}",
            resolved.upstream_tls
        );
        AppState::from_config(resolved).expect("build state")
    }

    /// Answer `route` on the upstream with a 307 to the same path on the
    /// second listener, and answer it there as if the redirect were
    /// legitimate.
    async fn redirect(&self, route: &str, ok: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(307).insert_header(
                header::LOCATION.as_str(),
                format!("{}{route}", self.elsewhere.url()),
            ))
            .mount(self.upstream.mock())
            .await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ok)
            .mount(self.elsewhere.mock())
            .await;
    }

    /// The upstream answered `route` with its redirect after `before`
    /// earlier requests (the positive control: the redirect path really
    /// ran), and the proxy never contacted the second listener.
    async fn assert_untouched(&self, route: &str, before: usize) {
        assert!(
            requests_to(self.upstream.mock(), route).await > before,
            "the upstream never saw {route}, so no redirect was exercised"
        );
        let seen = self
            .elsewhere
            .mock()
            .received_requests()
            .await
            .expect("recording on");
        assert!(
            seen.is_empty(),
            "the proxy followed the redirect to the second listener: {:?}",
            seen.iter().map(|r| r.url.to_string()).collect::<Vec<_>>()
        );
    }
}

fn whoami_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "name": "alice",
        "roles": ["operator"],
        "permissions": ["query"]
    }))
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

async fn requests_to(server: &MockServer, route: &str) -> usize {
    let seen = server.received_requests().await.expect("recording on");
    seen.iter().filter(|r| r.url.path() == route).count()
}

/// Login asks the upstream `/whoami` with the key as a bearer token.
#[tokio::test]
async fn login_does_not_follow_an_upstream_redirect() {
    let leg = Leg::start().await;
    leg.redirect("/api/v1/whoami", whoami_ok()).await;

    let before = requests_to(leg.upstream.mock(), "/api/v1/whoami").await;
    let response = login(leg.state()).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        response.headers().get(header::SET_COOKIE).is_none(),
        "a redirected login must not issue a session"
    );
    leg.assert_untouched("/api/v1/whoami", before).await;
}

/// Log in against an upstream whose `/whoami` answers once, and return
/// the session cookie pair. Mount the route's redirect after this, so the
/// redirect is what the route sees and not the login.
async fn session_cookie(state: AppState, upstream: &MockServer) -> String {
    Mock::given(method("GET"))
        .and(path("/api/v1/whoami"))
        .respond_with(whoami_ok())
        .up_to_n_times(1)
        .mount(upstream)
        .await;
    let response = login(state).await;
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get(header::SET_COOKIE)
        .expect("a session cookie")
        .to_str()
        .expect("ASCII cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned()
}

/// Send `uri` with the session cookie and check the browser gets the
/// proxy's upstream error: a 502, no redirect status, no `Location`.
async fn assert_redirect_refused(state: AppState, cookie: &str, uri: &str) {
    let request = Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .expect("request");
    let response = routes::build(state)
        .oneshot(request)
        .await
        .expect("router answers");
    assert!(
        !response.status().is_redirection(),
        "{uri}: the browser was handed a {} to follow",
        response.status()
    );
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{uri}");
    assert!(
        response.headers().get(header::LOCATION).is_none(),
        "{uri}: the upstream Location reached the browser: {:?}",
        response.headers().get(header::LOCATION)
    );
}

/// The generic `/api/v1/*` forwarder.
#[tokio::test]
async fn forwarding_does_not_follow_or_pass_on_an_upstream_redirect() {
    let leg = Leg::start().await;
    let state = leg.state();
    let cookie = session_cookie(state.clone(), leg.upstream.mock()).await;
    leg.redirect(
        "/api/v1/schema",
        ResponseTemplate::new(200).set_body_json(json!({"columns": []})),
    )
    .await;

    let before = requests_to(leg.upstream.mock(), "/api/v1/schema").await;
    assert_redirect_refused(state, &cookie, "/api/v1/schema").await;
    leg.assert_untouched("/api/v1/schema", before).await;
}

/// `/api/auth/me` asks the upstream `/whoami` on every call.
#[tokio::test]
async fn me_does_not_follow_or_pass_on_an_upstream_redirect() {
    let leg = Leg::start().await;
    let state = leg.state();
    let cookie = session_cookie(state.clone(), leg.upstream.mock()).await;
    leg.redirect("/api/v1/whoami", whoami_ok()).await;

    let before = requests_to(leg.upstream.mock(), "/api/v1/whoami").await;
    assert_redirect_refused(state, &cookie, "/api/auth/me").await;
    leg.assert_untouched("/api/v1/whoami", before).await;
}

/// Both SSE forwarders, which stream rather than buffer.
#[tokio::test]
async fn streams_do_not_follow_or_pass_on_an_upstream_redirect() {
    for (route, uri) in [
        ("/api/v1/stream", "/api/v1/stream?query=_severity%3Derror"),
        ("/api/v1/dashboard/stream", "/api/v1/dashboard/stream"),
    ] {
        let leg = Leg::start().await;
        let state = leg.state();
        let cookie = session_cookie(state.clone(), leg.upstream.mock()).await;
        leg.redirect(
            route,
            ResponseTemplate::new(200)
                .insert_header(header::CONTENT_TYPE.as_str(), "text/event-stream")
                .set_body_string("event: data\ndata: {}\n\n"),
        )
        .await;

        let before = requests_to(leg.upstream.mock(), route).await;
        assert_redirect_refused(state, &cookie, uri).await;
        leg.assert_untouched(route, before).await;
    }
}
