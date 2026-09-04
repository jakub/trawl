// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every cookie-authenticated route answers the same origin verdict
//! (ADR-0016).
//!
//! The guard used to be a call each handler made for itself, which is a
//! rule you have to remember. Both SSE routes had forgotten it: they take
//! the shared `fleet_session` cookie like every other route, and a page on
//! any origin could open an `EventSource` against `/api/v1/stream` and read
//! the victim's logs. So the shape of this file is the point. One table of
//! routes crossed with one table of `Origin` values, and a new route is one
//! line here rather than a call site somewhere else.
//!
//! What each rejection row asserts is deliberately more than the status:
//! a 403 that still emitted `Set-Cookie` would let a foreign page clear the
//! session it cannot read, and a 403 issued after the proxy already called
//! upstream would have forwarded the victim's bearer token first, which is
//! the whole thing the guard exists to prevent.

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use fleet_auth::{SessionExpiry, SessionPayload, encrypt};
use serde_json::json;
use tower::ServiceExt;
use trawl_config::WebConfig;
use trawl_web::config::ResolvedConfig;
use trawl_web::routes;
use trawl_web::state::AppState;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroize::Zeroizing;

/// The deployment's configured `public_origins`: a public HTTPS origin, a
/// loopback bind by name, and that bind's IPv6 spelling. Three entries, so
/// "allowed" cannot be an accident of a one-element list.
const ALLOWED: [&str; 3] = [
    "https://trawl.example.com",
    "http://localhost:8090",
    "http://[::1]:8090",
];

/// The token both the cookie session and the bearer client carry, so the
/// upstream mocks never need to tell them apart.
const TOKEN: &str = "flt_token";

/// Which door a route's auth comes through. It decides what a request must
/// carry, and it is exactly the axis the guard must not vary along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// `login`/`logout`: there is no session to extract, so the handler
    /// asks the guard itself before it mints or clears a cookie.
    Handler,
    /// `/api/auth/me`: the `Session` extractor. Cookies only, so a bearer
    /// client gets a 401 here and is not part of the bearer row.
    Session,
    /// The proxied routes and both SSE streams: the `Auth` extractor,
    /// cookie or bearer.
    Auth,
}

/// One browser-reachable route. Adding a route to the proxy should mean
/// adding a line here.
struct Route {
    name: &'static str,
    method: &'static str,
    uri: &'static str,
    /// Request body, empty for the GETs. A non-empty body also sends
    /// `Content-Type: application/json`, since `login`'s `Json` extractor
    /// runs before the handler and would 415 first.
    body: &'static str,
    gate: Gate,
    /// What the route answers when the guard lets the request through.
    ok: StatusCode,
}

const ROUTES: &[Route] = &[
    Route {
        name: "login",
        method: "POST",
        uri: "/api/auth/login",
        body: r#"{"api_key":"flt_token"}"#,
        gate: Gate::Handler,
        ok: StatusCode::OK,
    },
    Route {
        name: "logout",
        method: "POST",
        uri: "/api/auth/logout",
        body: "",
        gate: Gate::Handler,
        ok: StatusCode::NO_CONTENT,
    },
    Route {
        name: "me",
        method: "GET",
        uri: "/api/auth/me",
        body: "",
        gate: Gate::Session,
        ok: StatusCode::OK,
    },
    Route {
        name: "proxied write",
        method: "POST",
        uri: "/api/v1/saved",
        body: r#"{"name":"errors","query":"_severity>=error"}"#,
        gate: Gate::Auth,
        ok: StatusCode::OK,
    },
    Route {
        name: "proxied read",
        method: "GET",
        uri: "/api/v1/saved",
        body: "",
        gate: Gate::Auth,
        ok: StatusCode::OK,
    },
    Route {
        name: "stream",
        method: "GET",
        uri: "/api/v1/stream?query=*",
        body: "",
        gate: Gate::Auth,
        ok: StatusCode::OK,
    },
    Route {
        name: "dashboard stream",
        method: "GET",
        uri: "/api/v1/dashboard/stream",
        body: "",
        gate: Gate::Auth,
        ok: StatusCode::OK,
    },
];

/// `Origin` values that must be refused, and why each one matters.
const REFUSED: &[(&str, &str)] = &[
    (
        "https://coastwatch.example.com",
        "a sibling fleet app: sharing the cookie's domain is not authority",
    ),
    (
        "http://trawl.example.com",
        "cross-scheme forgery, the defect ADR-0016 closes",
    ),
    (
        "https://trawl.example.com:8443",
        "another port of the same name is another origin",
    ),
    (
        "https://trawl.example.com/",
        "a trailing slash makes it a URL, not a serialized origin",
    ),
    (
        "null",
        "the opaque origin a sandboxed iframe or data: URL sends",
    ),
    (
        "http://[::1]:8091",
        "the configured IPv6 loopback on a port nobody configured",
    ),
];

/// `Origin` values a real browser sends for a configured origin. A wrong
/// refusal here is a 403 the operator cannot explain.
const ACCEPTED: &[(&str, &str)] = &[
    (
        "https://trawl.example.com",
        "the configured origin verbatim",
    ),
    (
        "https://trawl.example.com:443",
        "the default port spelled out is the same origin",
    ),
    (
        "HTTPS://Trawl.Example.COM",
        "scheme and DNS host are case-insensitive",
    ),
    ("http://[::1]:8090", "the configured IPv6 loopback"),
];

/// Headers a reverse proxy writes and anyone who can reach the port can
/// forge. None of them may move a verdict in either direction.
const FORGED: &[(&str, &str)] = &[
    ("x-forwarded-host", "coastwatch.example.com"),
    ("x-forwarded-proto", "http"),
    ("x-forwarded-port", "8443"),
    ("forwarded", "host=coastwatch.example.com;proto=http"),
];

/// What a request carries to authenticate.
enum Credential<'a> {
    None,
    Cookie(&'a str),
    Bearer,
}

impl Route {
    /// The credential this route normally travels with.
    fn credential<'a>(&self, cookie: &'a str) -> Credential<'a> {
        match self.gate {
            Gate::Handler => Credential::None,
            Gate::Session | Gate::Auth => Credential::Cookie(cookie),
        }
    }
}

/// An upstream that answers every path the routes above reach. It exists
/// so a passing request has somewhere to land; the rejection rows assert
/// it was never called at all.
async fn upstream_server() -> MockServer {
    let upstream = MockServer::start().await;
    let whoami = json!({
        "prefix": "testtest",
        "name": "alice",
        "kind": "human",
        "roles": ["trawl-analyst"],
        "permissions": ["query", "saved_query", "stream", "server_manage"],
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/whoami"))
        .respond_with(ResponseTemplate::new(200).set_body_json(whoami))
        .mount(&upstream)
        .await;
    for stream in ["/api/v1/stream", "/api/v1/dashboard/stream"] {
        Mock::given(method("GET"))
            .and(path(stream))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: data\ndata: {}\n\n")
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&upstream)
            .await;
    }
    Mock::given(path("/api/v1/saved"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"saved": []})))
        .mount(&upstream)
        .await;
    upstream
}

fn state_for(upstream: &MockServer) -> AppState {
    let web = WebConfig {
        upstream_url: Some(upstream.uri()),
        allow_insecure_cookies: true,
        public_origins: ALLOWED.iter().map(|o| (*o).to_owned()).collect(),
        ..WebConfig::default()
    };
    AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap()
}

async fn fixture() -> (MockServer, AppState, axum::Router) {
    let upstream = upstream_server().await;
    let state = state_for(&upstream);
    let app = routes::build(state.clone());
    (upstream, state, app)
}

/// Mint a session cookie straight from the app's key rather than through
/// `POST /api/auth/login`. A login is an upstream call, and the rejection
/// rows below assert that the upstream received *zero* requests, so the
/// fixture must not spend one on setup.
fn cookie_with_expiry(state: &AppState, exp: i64) -> String {
    let payload = SessionPayload {
        token: Zeroizing::new(TOKEN.into()),
        name: "alice".into(),
        exp: SessionExpiry::from_unix_seconds(exp),
    };
    let value = encrypt(state.cookie_key(), &payload).unwrap();
    format!("fleet_session={value}")
}

fn valid_cookie(state: &AppState) -> String {
    cookie_with_expiry(state, chrono::Utc::now().timestamp() + 3600)
}

fn expired_cookie(state: &AppState) -> String {
    cookie_with_expiry(state, chrono::Utc::now().timestamp() - 60)
}

/// A valid cookie with one ciphertext byte changed, so AEAD verification
/// fails rather than the cookie merely being absent.
fn tampered_cookie(state: &AppState) -> String {
    let mut cookie = valid_cookie(state);
    let mid = cookie.len() / 2;
    let original = cookie.as_bytes()[mid];
    let replacement = if original == b'A' { "B" } else { "A" };
    cookie.replace_range(mid..=mid, replacement);
    cookie
}

fn request(route: &Route, origin: Option<&str>, credential: &Credential<'_>) -> Request<Body> {
    let mut builder = Request::builder().method(route.method).uri(route.uri);
    if !route.body.is_empty() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    builder = match credential {
        Credential::None => builder,
        Credential::Cookie(cookie) => builder.header(header::COOKIE, *cookie),
        Credential::Bearer => builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}")),
    };
    builder
        .body(Body::from(route.body))
        .expect("request builds from a static table")
}

/// How many requests the upstream has seen. Zero is the load-bearing
/// assertion on every rejection: the proxy attaches the session's bearer
/// token to what it forwards, so a call made before the verdict has
/// already handed the attacker's request the victim's credential.
async fn upstream_calls(upstream: &MockServer) -> usize {
    upstream
        .received_requests()
        .await
        .expect("wiremock records requests by default")
        .len()
}

#[tokio::test]
async fn a_refused_origin_is_403_with_no_cookie_and_no_upstream_call() {
    let (upstream, state, app) = fixture().await;
    let cookie = valid_cookie(&state);

    for route in ROUTES {
        for &(origin, why) in REFUSED {
            let response = app
                .clone()
                .oneshot(request(route, Some(origin), &route.credential(&cookie)))
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{}: {origin} must be refused ({why})",
                route.name
            );
            assert!(
                !response.headers().contains_key(header::SET_COOKIE),
                "{}: a refused {origin} must not touch the cookie",
                route.name
            );
            assert_eq!(
                upstream_calls(&upstream).await,
                0,
                "{}: {origin} was refused only after the token went upstream",
                route.name
            );
        }
    }
}

#[tokio::test]
async fn a_configured_origin_reaches_the_handler() {
    let (upstream, state, app) = fixture().await;
    let cookie = valid_cookie(&state);

    for route in ROUTES {
        for &(origin, why) in ACCEPTED {
            let response = app
                .clone()
                .oneshot(request(route, Some(origin), &route.credential(&cookie)))
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                route.ok,
                "{}: {origin} must pass ({why})",
                route.name
            );
        }
    }

    assert!(
        upstream_calls(&upstream).await > 0,
        "the accepted rows must actually have reached upstream"
    );
}

#[tokio::test]
async fn an_absent_origin_passes_every_route() {
    // Present-only semantics: this is a browser CSRF control, not client
    // authentication. curl, the CLI and every scripted client send no
    // Origin and must keep working.
    let (_upstream, state, app) = fixture().await;
    let cookie = valid_cookie(&state);

    for route in ROUTES {
        let response = app
            .clone()
            .oneshot(request(route, None, &route.credential(&cookie)))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            route.ok,
            "{}: a request with no Origin must pass",
            route.name
        );
    }
}

#[tokio::test]
async fn two_origin_headers_are_refused_even_when_both_are_configured() {
    // A browser sends at most one. Two mean something in between appended
    // one, so there is no single origin to compare, and picking either
    // copy answers about a request nobody sent.
    let (upstream, state, app) = fixture().await;
    let cookie = valid_cookie(&state);

    for route in ROUTES {
        let mut req = request(route, Some(ALLOWED[0]), &route.credential(&cookie));
        req.headers_mut().append(
            header::ORIGIN,
            HeaderValue::from_static("https://trawl.example.com"),
        );

        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{}: two byte-identical configured Origins are still two Origins",
            route.name
        );
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(upstream_calls(&upstream).await, 0);
    }
}

#[tokio::test]
async fn a_bearer_client_is_exempt_from_the_guard() {
    // A bearer client holds no cookie, so it is not a CSRF target: nothing
    // a foreign page can do makes a browser send someone else's
    // Authorization header. The CLI posting from a machine that also has a
    // browser tab open must not start 403ing.
    let (_upstream, _state, app) = fixture().await;

    for route in ROUTES.iter().filter(|r| r.gate == Gate::Auth) {
        let response = app
            .clone()
            .oneshot(request(
                route,
                Some("https://coastwatch.example.com"),
                &Credential::Bearer,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            route.ok,
            "{}: a bearer client's Origin is not a verdict",
            route.name
        );
    }
}

#[tokio::test]
async fn a_refused_origin_outranks_every_cookie_state() {
    // The guard runs before the cookie is looked at, so the verdict cannot
    // depend on what the browser happens to be holding. The expired case
    // is the one with teeth: that branch normally answers 401 *with* a
    // clear directive, and a foreign page must not be able to provoke it.
    let (upstream, state, app) = fixture().await;
    let cookies = [
        ("missing", None),
        ("valid", Some(valid_cookie(&state))),
        ("expired", Some(expired_cookie(&state))),
        ("tampered", Some(tampered_cookie(&state))),
    ];

    for route in ROUTES.iter().filter(|r| r.gate != Gate::Handler) {
        for (label, cookie) in &cookies {
            let credential = cookie
                .as_deref()
                .map_or(Credential::None, Credential::Cookie);
            let response = app
                .clone()
                .oneshot(request(
                    route,
                    Some("https://coastwatch.example.com"),
                    &credential,
                ))
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{}: a foreign origin with a {label} cookie must be 403",
                route.name
            );
            assert!(
                !response.headers().contains_key(header::SET_COOKIE),
                "{}: a foreign origin with a {label} cookie must not clear it",
                route.name
            );
            assert_eq!(upstream_calls(&upstream).await, 0);
        }
    }
}

#[tokio::test]
async fn an_allowed_origin_keeps_the_existing_cookie_answers() {
    // The other half of the ordering claim: with the guard satisfied, the
    // 401s and the expiry clear behave exactly as they did before it
    // existed. A guard that also changed these would be a second auth
    // rule wearing a CSRF hat.
    let (_upstream, state, app) = fixture().await;
    let expired = expired_cookie(&state);
    let tampered = tampered_cookie(&state);

    for route in ROUTES.iter().filter(|r| r.gate != Gate::Handler) {
        for (label, credential, expected, clears) in [
            ("missing", Credential::None, StatusCode::UNAUTHORIZED, false),
            (
                "tampered",
                Credential::Cookie(&tampered),
                StatusCode::UNAUTHORIZED,
                false,
            ),
            (
                "expired",
                Credential::Cookie(&expired),
                StatusCode::UNAUTHORIZED,
                true,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(request(route, Some(ALLOWED[0]), &credential))
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                expected,
                "{}: a {label} cookie under a configured Origin",
                route.name
            );
            assert_eq!(
                response.headers().contains_key(header::SET_COOKIE),
                clears,
                "{}: a {label} cookie's clear directive",
                route.name
            );
        }
    }
}

#[tokio::test]
async fn forged_forwarding_headers_move_no_verdict() {
    // `Host`, `Forwarded` and `X-Forwarded-*` are free text to anyone who
    // can reach the port. Under the old host-comparison rule they decided
    // the answer; now they are inert in both directions, so a deployment
    // behind a proxy that rewrites them gets the same verdict as one
    // without.
    let (upstream, state, app) = fixture().await;
    let cookie = valid_cookie(&state);

    for route in ROUTES {
        for (origin, expected) in [
            (ALLOWED[0], route.ok),
            ("https://coastwatch.example.com", StatusCode::FORBIDDEN),
        ] {
            let mut req = request(route, Some(origin), &route.credential(&cookie));
            req.headers_mut().insert(
                header::HOST,
                HeaderValue::from_static("coastwatch.example.com"),
            );
            for &(name, value) in FORGED {
                req.headers_mut().insert(
                    axum::http::HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                );
            }

            let response = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                response.status(),
                expected,
                "{}: forged forwarding headers changed the verdict for {origin}",
                route.name
            );
        }
    }

    // The refused half of that loop still must not have reached upstream,
    // whatever the forged headers claimed about who was asking.
    let calls = upstream_calls(&upstream).await;
    assert!(
        calls > 0 && calls <= ROUTES.len(),
        "only the allowed half may reach upstream, saw {calls} calls"
    );
}
