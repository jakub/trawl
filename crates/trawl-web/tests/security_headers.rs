// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests that verify baseline security headers are present
//! on every response, including both API and unauthenticated paths.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use trawl_config::WebConfig;
use trawl_web::config::ResolvedConfig;
use trawl_web::routes;
use trawl_web::state::AppState;

fn test_state() -> AppState {
    let web = WebConfig {
        allow_insecure_cookies: true,
        ..WebConfig::default()
    };
    AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap()
}

#[tokio::test]
async fn every_response_has_csp_and_friends() {
    let app = routes::build(test_state());

    // /healthz is unauthenticated and trivially passes through all layers.
    let req = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let headers = resp.headers();
    let csp = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("CSP header")
        .to_str()
        .unwrap();
    // wasm-unsafe-eval is non-negotiable for our SPA to boot.
    assert!(
        csp.contains("wasm-unsafe-eval"),
        "CSP must include wasm-unsafe-eval; got: {csp}"
    );
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));

    assert!(
        headers
            .get(header::STRICT_TRANSPORT_SECURITY)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("max-age="),
        "HSTS must be present and have max-age",
    );

    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(headers.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
}

#[tokio::test]
async fn unauthorized_responses_still_carry_security_headers() {
    let app = routes::build(test_state());

    // /me without cookie → 401. Security headers must still be applied
    // so error responses don't become a weak link.
    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().contains_key(header::CONTENT_SECURITY_POLICY));
    assert_eq!(
        resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
}
