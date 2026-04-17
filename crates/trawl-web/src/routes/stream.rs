// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SSE pass-through for `/api/v1/stream`.
//!
//! The generic `/api/v1/*` forwarder buffers request/response bodies,
//! which is wrong for Server-Sent Events — those are streams of
//! unbounded size and unpredictable duration. This handler streams the
//! upstream response bytes straight to the browser without parsing. The
//! browser's `EventSource` handles framing on its end; reconnection
//! after a drop is automatic and rides the same session cookie.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde::Deserialize;

use crate::error::ProxyError;
use crate::middleware::session_extractor::Session;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub query: String,
}

pub async fn forward(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<StreamParams>,
) -> Result<Response, ProxyError> {
    let upstream_url = format!(
        "{}/api/v1/stream",
        state.upstream_url().trim_end_matches('/')
    );

    let upstream_resp = state
        .http()
        .get(&upstream_url)
        .bearer_auth(session.token())
        .query(&[("query", &params.query)])
        .send()
        .await
        .map_err(ProxyError::Network)?;

    if !upstream_resp.status().is_success() {
        return Err(ProxyError::Upstream(
            StatusCode::from_u16(upstream_resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY),
        ));
    }

    // Pass through the upstream's chunked body as an axum streaming body.
    // No parsing, no buffering, no keep-alive injection — trawld already
    // emits SSE keep-alive pings.
    let byte_stream = upstream_resp.bytes_stream();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        // Disable proxy buffering (e.g. nginx) upstream of us. Trawld sets
        // this too but we re-set it to be resilient to misconfigured
        // intermediate proxies when trawl-web is fronted by another one.
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(byte_stream))
        .map_err(|e| ProxyError::Internal(format!("SSE response build: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;
    use trawl_server::config::WebConfig;
    use wiremock::matchers::{bearer_token, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::ResolvedConfig;
    use crate::routes;

    fn state_pointing_at(upstream: &MockServer) -> AppState {
        let web = WebConfig {
            upstream_url: Some(upstream.uri()),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        AppState::from_config(ResolvedConfig::from_parsed(&web, None).unwrap()).unwrap()
    }

    async fn login_cookie(app: axum::Router, upstream: &MockServer) -> String {
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "alice", "role": "analyst", "permissions": []
            })))
            .mount(upstream)
            .await;

        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_token"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        resp.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn stream_forwards_upstream_body_with_sse_headers() {
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        let sse_body = "event: data\ndata: {\"foo\":\"bar\"}\n\nevent: data\ndata: {\"x\":1}\n\n";
        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .and(query_param("query", "level=error"))
            .and(bearer_token("flt_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&upstream)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=level%3Derror")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert_eq!(resp.headers().get("x-accel-buffering").unwrap(), "no");

        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), sse_body.as_bytes());
    }

    #[tokio::test]
    async fn stream_rejects_missing_cookie() {
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stream_maps_upstream_401_to_401() {
        let upstream = MockServer::start().await;
        let state = state_pointing_at(&upstream);
        let app = routes::build(state);

        let cookie = login_cookie(app.clone(), &upstream).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/stream"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/stream?query=*")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
