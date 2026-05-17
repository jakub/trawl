// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Login / logout HTTP handler factories (ADR-0030).
//!
//! Both handlers consume `axum::State<SessionState>` — apps mount them on
//! whatever paths they prefer:
//!
//! ```ignore
//! Router::new()
//!     .route("/api/auth/login", post(fleet_auth::login))
//!     .route("/api/auth/logout", post(fleet_auth::logout))
//!     .with_state(SessionState::new(store, key, config)?);
//! ```
//!
//! Login validates the submitted API key directly via
//! `KeyStore::verify_key` (no HTTP `/whoami` hop — that lived in trawl-web's
//! old proxy and is replaced per ADR-0030's "What happens to
//! coastwatch-trawl-client" section). It then filters grants to the app's
//! namespace, sets the encrypted session cookie, and 302-redirects to
//! `SessionConfig.post_login_redirect`.
//!
//! Logout clears the session cookie with matching attributes (domain, path,
//! `same_site`, secure) so browsers accept the directive. Returns 204.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::middleware::{SessionState, no_grant_response, unauthorized_json};
use crate::session::{
    self, SessionPayload, build_clear_cookie_header, build_session_cookie_header,
};

/// `POST /login` request body.
///
/// `api_key` is wrapped in [`Zeroizing`] on deserialization so the secret
/// is scrubbed from memory when the request frame is dropped, even on the
/// error path.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    #[serde(deserialize_with = "deserialize_zeroizing_string")]
    pub api_key: Zeroizing<String>,
}

/// Body returned on `POST /login` success (also encoded in the
/// 302 response). Apps that prefer 200+JSON over a redirect can use this
/// shape directly.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// Identity name from the verified key — convenient for the UI's user
    /// menu without a follow-up `/me` round-trip.
    pub name: String,
}

/// `POST /login` handler.
///
/// - Empty `api_key` → 400.
/// - Invalid `api_key` → 401 JSON (same shape as middleware).
/// - Valid `api_key` but no grant for `app_namespace` → 403 HTML (same
///   body as middleware no-grant; no cookie set).
/// - Success → 302 with `Set-Cookie` and `Location: post_login_redirect`.
#[allow(clippy::implicit_hasher)]
pub async fn login(State(state): State<SessionState>, Json(req): Json<LoginRequest>) -> Response {
    let api_key = req.api_key;
    if api_key.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"bad_request","detail":"api_key is required"}"#,
        )
            .into_response();
    }

    let Ok(verified) = state.store().verify_key(api_key.as_str()).await else {
        return unauthorized_json("invalid api key");
    };

    let cfg = state.config();
    if verified.role_for(&cfg.app_namespace).is_none() {
        // No-grant: same body as middleware. NO Set-Cookie — the user never
        // gets a session for an app they can't access.
        return no_grant_response(&verified.name, &cfg.app_namespace);
    }

    let now = chrono::Utc::now().timestamp();
    let Ok(ttl) = i64::try_from(cfg.ttl_secs) else {
        return internal_error("session ttl_secs out of i64 range");
    };
    let payload = SessionPayload {
        token: api_key,
        name: verified.name.clone(),
        exp: now.saturating_add(ttl),
    };

    let Ok(cookie_value) = session::encrypt(state.session_key(), &payload) else {
        return internal_error("session encrypt failed");
    };

    let cookie_header = build_session_cookie_header(
        &cfg.cookie_name,
        cookie_value,
        cfg.ttl_secs,
        cfg.secure,
        cfg.same_site,
        cfg.domain.as_deref(),
    );

    let mut headers = HeaderMap::new();
    let Ok(set_cookie) = cookie_header.parse() else {
        return internal_error("session cookie header value invalid");
    };
    headers.insert(header::SET_COOKIE, set_cookie);
    let Ok(location) = cfg.post_login_redirect.parse() else {
        return internal_error("post_login_redirect not a valid header value");
    };
    headers.insert(header::LOCATION, location);

    (
        StatusCode::FOUND,
        headers,
        Json(LoginResponse {
            name: verified.name,
        }),
    )
        .into_response()
}

/// `POST /logout` handler.
///
/// Always returns 204 with a `Set-Cookie` clear directive — never errors,
/// never requires a valid session (logout works even with a stale cookie).
/// Attributes match what login sets so browsers accept the clear.
pub async fn logout(State(state): State<SessionState>) -> Response {
    let cfg = state.config();
    let cookie_header = build_clear_cookie_header(
        &cfg.cookie_name,
        cfg.secure,
        cfg.same_site,
        cfg.domain.as_deref(),
    );

    let mut headers = HeaderMap::new();
    let Ok(set_cookie) = cookie_header.parse() else {
        return internal_error("clear cookie header value invalid");
    };
    headers.insert(header::SET_COOKIE, set_cookie);

    (StatusCode::NO_CONTENT, headers).into_response()
}

fn internal_error(detail: &str) -> Response {
    let body = format!(r#"{{"error":"internal","detail":"{detail}"}}"#);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn deserialize_zeroizing_string<'de, D>(d: D) -> Result<Zeroizing<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    Ok(Zeroizing::new(s))
}
