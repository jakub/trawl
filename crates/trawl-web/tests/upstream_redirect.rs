// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy never follows an upstream redirect, and never hands one to
//! the browser.
//!
//! With `TRAWL_WEB_INSECURE_UPSTREAM` on, whatever holds the loopback port
//! could answer 3xx and send the proxy, with certificate checks off, to a
//! second host. That would defeat the loopback-only rule, so the upstream
//! client follows no redirect in any trust mode. Passing the 3xx through is
//! no better: the browser would follow it, and a `Location` naming another
//! port on the browser's host carries the host-scoped session cookie to
//! whatever listens there. So every browser-facing route answers an
//! upstream 3xx with the proxy's own 502 and no `Location`.
//!
//! Each test runs two real listeners: the upstream answers 307 to the
//! other, and the other must see no request at all. In the pinned mode
//! both listeners are TLS fronts whose certificates come from the pinned
//! CA, so the second one is a target the client could reach, and only the
//! redirect policy keeps it away.

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

/// The trust modes the redirect rule is checked under.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// `[web] upstream_ca_path`, over real TLS.
    PinnedCa,
    /// The insecure switch, over plain http: the test sets the mode on the
    /// resolved config, since the resolver refuses http for it.
    InsecureLoopback,
}

const MODES: [Mode; 2] = [Mode::PinnedCa, Mode::InsecureLoopback];

/// One listener: a TLS front over wiremock, or wiremock itself.
enum Listener {
    Tls(TlsUpstream),
    Plain(MockServer),
}

impl Listener {
    fn mock(&self) -> &MockServer {
        match self {
            Self::Tls(upstream) => upstream.mock(),
            Self::Plain(server) => server,
        }
    }

    /// The URL a client dials to reach this listener.
    fn url(&self) -> String {
        match self {
            Self::Tls(upstream) => upstream.url(),
            Self::Plain(server) => server.uri(),
        }
    }
}

/// The two listeners of one test run, and the mode that connects them.
struct Leg {
    mode: Mode,
    upstream: Listener,
    elsewhere: Listener,
}

impl Leg {
    async fn start(mode: Mode) -> Self {
        let (upstream, elsewhere) = match mode {
            Mode::PinnedCa => {
                // One CA for both, so the pin trusts the redirect target.
                let ca = TestCa::generate();
                (
                    Listener::Tls(TlsUpstream::issued_by(&ca).await),
                    Listener::Tls(TlsUpstream::issued_by(&ca).await),
                )
            }
            Mode::InsecureLoopback => (
                Listener::Plain(MockServer::start().await),
                Listener::Plain(MockServer::start().await),
            ),
        };
        Self {
            mode,
            upstream,
            elsewhere,
        }
    }

    fn name(&self) -> String {
        format!("{:?}", self.mode)
    }

    fn state(&self) -> AppState {
        let web = WebConfig {
            allow_insecure_cookies: true,
            public_origins: vec![ORIGIN.to_owned()],
            ..match &self.upstream {
                Listener::Tls(upstream) => upstream.web_config(),
                Listener::Plain(server) => WebConfig {
                    upstream_url: Some(server.uri()),
                    ..WebConfig::default()
                },
            }
        };
        let mut resolved = ResolvedConfig::from_parsed(&web, None).expect("resolve config");
        match self.mode {
            Mode::PinnedCa => assert!(
                matches!(resolved.upstream_tls, UpstreamTls::PinnedCa(_)),
                "{:?}",
                resolved.upstream_tls
            ),
            // The insecure mode is chosen from the process environment,
            // which a test may not mutate; the upstream is loopback, so it
            // is a mode the resolver would pick.
            Mode::InsecureLoopback => resolved.upstream_tls = UpstreamTls::InsecureLoopback,
        }
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
        let mode = self.name();
        assert!(
            requests_to(self.upstream.mock(), route).await > before,
            "{mode}: the upstream never saw {route}, so no redirect was exercised"
        );
        let seen = self
            .elsewhere
            .mock()
            .received_requests()
            .await
            .expect("recording on");
        assert!(
            seen.is_empty(),
            "{mode}: the proxy followed the redirect to the second listener: {:?}",
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
    for mode in MODES {
        let leg = Leg::start(mode).await;
        let mode = leg.name();
        leg.redirect("/api/v1/whoami", whoami_ok()).await;

        let before = requests_to(leg.upstream.mock(), "/api/v1/whoami").await;
        let response = login(leg.state()).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{mode}");
        assert!(
            response.headers().get(header::SET_COOKIE).is_none(),
            "{mode}: a redirected login must not issue a session"
        );
        leg.assert_untouched("/api/v1/whoami", before).await;
    }
}

/// Log in against an upstream whose `/whoami` answers once, and return
/// the session cookie pair. Mount the route's redirect after this, so the
/// redirect is what the route sees and not the login.
async fn session_cookie(state: AppState, upstream: &MockServer, mode: &str) -> String {
    Mock::given(method("GET"))
        .and(path("/api/v1/whoami"))
        .respond_with(whoami_ok())
        .up_to_n_times(1)
        .mount(upstream)
        .await;
    let response = login(state).await;
    assert_eq!(response.status(), StatusCode::OK, "{mode}");
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
async fn assert_redirect_refused(state: AppState, cookie: &str, uri: &str, mode: &str) {
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
        "{mode} {uri}: the browser was handed a {} to follow",
        response.status()
    );
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{mode} {uri}");
    assert!(
        response.headers().get(header::LOCATION).is_none(),
        "{mode} {uri}: the upstream Location reached the browser: {:?}",
        response.headers().get(header::LOCATION)
    );
}

/// The generic `/api/v1/*` forwarder.
#[tokio::test]
async fn forwarding_does_not_follow_or_pass_on_an_upstream_redirect() {
    for mode in MODES {
        let leg = Leg::start(mode).await;
        let mode = leg.name();
        let state = leg.state();
        let cookie = session_cookie(state.clone(), leg.upstream.mock(), &mode).await;
        leg.redirect(
            "/api/v1/schema",
            ResponseTemplate::new(200).set_body_json(json!({"columns": []})),
        )
        .await;

        let before = requests_to(leg.upstream.mock(), "/api/v1/schema").await;
        assert_redirect_refused(state, &cookie, "/api/v1/schema", &mode).await;
        leg.assert_untouched("/api/v1/schema", before).await;
    }
}

/// `/api/auth/me` asks the upstream `/whoami` on every call.
#[tokio::test]
async fn me_does_not_follow_or_pass_on_an_upstream_redirect() {
    for mode in MODES {
        let leg = Leg::start(mode).await;
        let mode = leg.name();
        let state = leg.state();
        let cookie = session_cookie(state.clone(), leg.upstream.mock(), &mode).await;
        leg.redirect("/api/v1/whoami", whoami_ok()).await;

        let before = requests_to(leg.upstream.mock(), "/api/v1/whoami").await;
        assert_redirect_refused(state, &cookie, "/api/auth/me", &mode).await;
        leg.assert_untouched("/api/v1/whoami", before).await;
    }
}

/// Both SSE forwarders, which stream rather than buffer.
#[tokio::test]
async fn streams_do_not_follow_or_pass_on_an_upstream_redirect() {
    for (route, uri) in [
        ("/api/v1/stream", "/api/v1/stream?query=_severity%3Derror"),
        ("/api/v1/dashboard/stream", "/api/v1/dashboard/stream"),
    ] {
        for mode in MODES {
            let leg = Leg::start(mode).await;
            let mode = leg.name();
            let state = leg.state();
            let cookie = session_cookie(state.clone(), leg.upstream.mock(), &mode).await;
            leg.redirect(
                route,
                ResponseTemplate::new(200)
                    .insert_header(header::CONTENT_TYPE.as_str(), "text/event-stream")
                    .set_body_string("event: data\ndata: {}\n\n"),
            )
            .await;

            let before = requests_to(leg.upstream.mock(), route).await;
            assert_redirect_refused(state, &cookie, uri, &mode).await;
            leg.assert_untouched(route, before).await;
        }
    }
}
