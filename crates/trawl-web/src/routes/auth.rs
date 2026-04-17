// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Authentication endpoints: `/login`, `/logout`, `/me`.
//!
//! The login flow is:
//! 1. Browser POSTs `{api_key}` → proxy calls upstream `/whoami` with that
//!    bearer token to validate and fetch identity metadata.
//! 2. On 200, proxy encrypts `{token, name, role, exp}` into an AEAD cookie
//!    and sets it with `HttpOnly; Secure; SameSite=Strict; Path=/`.
//! 3. Browser is 302'd to `/search`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::ProxyError;
use crate::session::{self, SessionPayload};
use crate::state::AppState;

const SESSION_COOKIE: &str = "trawl_session";

/// Request body for `POST /login`.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub api_key: String,
}

/// Response body for `POST /login`.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub name: String,
    pub role: String,
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ProxyError> {
    if req.api_key.trim().is_empty() {
        return Err(ProxyError::BadRequest("api_key is required".into()));
    }

    let whoami = fetch_whoami(&state, &req.api_key).await?;

    let now = chrono::Utc::now().timestamp();
    let ttl = i64::try_from(state.session_ttl_secs())
        .map_err(|_| ProxyError::Internal("session_ttl_secs out of i64 range".into()))?;

    let payload = SessionPayload {
        token: Zeroizing::new(req.api_key),
        name: whoami.name.clone(),
        role: whoami.role.clone(),
        exp: now.saturating_add(ttl),
    };

    let cookie_value = session::encrypt(state.cookie_key(), &payload)?;
    let cookie_header = build_cookie_header(
        SESSION_COOKIE,
        &cookie_value,
        state.session_ttl_secs(),
        !state.allow_insecure_cookies(),
    );

    let body = LoginResponse {
        name: whoami.name,
        role: whoami.role,
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        cookie_header
            .parse()
            .map_err(|e: header::InvalidHeaderValue| ProxyError::Internal(e.to_string()))?,
    );

    Ok((StatusCode::OK, headers, Json(body)).into_response())
}

#[derive(Debug, Deserialize)]
struct WhoAmI {
    name: String,
    role: String,
}

async fn fetch_whoami(state: &AppState, token: &str) -> Result<WhoAmI, ProxyError> {
    let url = format!(
        "{}/api/v1/whoami",
        state.upstream_url().trim_end_matches('/')
    );
    let resp = state
        .http()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(ProxyError::Network)?;

    match resp.status() {
        s if s.is_success() => {
            let body: WhoAmI = resp
                .json()
                .await
                .map_err(|e| ProxyError::Internal(format!("whoami body: {e}")))?;
            Ok(body)
        }
        s if s == reqwest::StatusCode::UNAUTHORIZED => Err(ProxyError::Unauthorized),
        s => Err(ProxyError::Upstream(
            StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        )),
    }
}

fn build_cookie_header(name: &str, value: &str, max_age_secs: u64, secure: bool) -> String {
    let mut s =
        format!("{name}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age_secs}");
    if secure {
        s.push_str("; Secure");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;
    use trawl_server::config::WebConfig;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::ResolvedConfig;
    use crate::routes;

    fn test_state(upstream_url: String) -> AppState {
        let web = WebConfig {
            upstream_url: Some(upstream_url),
            allow_insecure_cookies: true,
            ..WebConfig::default()
        };
        let cfg = ResolvedConfig::from_parsed(&web).unwrap();
        AppState::from_config(cfg).unwrap()
    }

    #[tokio::test]
    async fn login_accepts_valid_api_key() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .and(bearer_token("flt_goodtoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "alice",
                "role": "analyst",
                "permissions": ["query"]
            })))
            .mount(&upstream)
            .await;

        let state = test_state(upstream.uri());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_goodtoken"}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("Set-Cookie header")
            .to_str()
            .unwrap();
        assert!(set_cookie.starts_with("trawl_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        assert!(set_cookie.contains("Path=/"));
        // allow_insecure_cookies=true in test => no `Secure`
        assert!(!set_cookie.contains("Secure"));
    }

    #[tokio::test]
    async fn login_rejects_wrong_api_key() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&upstream)
            .await;

        let state = test_state(upstream.uri());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_badtoken"}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }

    #[tokio::test]
    async fn login_rejects_empty_api_key() {
        // No MockServer needed — handler must fail input validation first.
        let state = test_state("http://unused".into());
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":""}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn login_secure_cookie_when_insecure_disabled() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/whoami"))
            .and(bearer_token("flt_prod"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "prod", "role": "admin", "permissions": []
            })))
            .mount(&upstream)
            .await;

        let web = WebConfig {
            upstream_url: Some(upstream.uri()),
            allow_insecure_cookies: false,
            ..WebConfig::default()
        };
        let state = AppState::from_config(ResolvedConfig::from_parsed(&web).unwrap()).unwrap();
        let app = routes::build(state);

        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"api_key":"flt_prod"}"#))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("Secure"));
    }
}
