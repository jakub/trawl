// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Login / logout HTTP handler factories (ADR-0030).

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::middleware::{SessionState, classify_verify_error, error_response, no_grant_response};
use crate::origin::PublicOrigins;
use crate::session::{
    self, SessionExpiry, SessionPayload, build_clear_cookie_header, build_session_cookie_header,
    zeroizing_string,
};

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    #[serde(with = "zeroizing_string")]
    pub api_key: Zeroizing<String>,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub name: String,
}

#[allow(clippy::implicit_hasher)]
pub async fn login(
    State(state): State<SessionState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    // First statement in the handler: a rejected origin must cost nothing
    // but a 403. Verifying the key first would run the argon2id KDF for an
    // attacker's forged request, and any later placement risks a future
    // edit slipping a cookie-touching step above the guard.
    if let Some(resp) = reject_cross_origin(&headers, state.config().public_origins(), "login") {
        return resp;
    }
    let api_key = req.api_key;
    if api_key.trim().is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "api_key is required",
        );
    }

    let verified = match state.store().verify_key(api_key.as_str()).await {
        Ok(v) => v,
        Err(err) => return classify_verify_error(err, "login"),
    };

    let cfg = state.config();
    if !verified.has_any_permission(cfg.app_namespace()) {
        return no_grant_response(&verified.name, cfg.app_namespace());
    }

    let now = chrono::Utc::now().timestamp();
    let ttl = match i64::try_from(cfg.ttl_secs()) {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                ttl_secs = cfg.ttl_secs(),
                ?err,
                "login: ttl_secs out of i64 range"
            );
            return internal_error("session ttl_secs out of i64 range");
        }
    };
    let payload = SessionPayload {
        token: api_key,
        name: verified.name.clone(),
        exp: SessionExpiry::after_duration(now, ttl),
    };

    let cookie_value = match session::encrypt(state.session_key(), &payload) {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "login: session encrypt failed");
            return internal_error("session encrypt failed");
        }
    };

    let cookie_header = build_session_cookie_header(
        cfg.cookie_name(),
        cookie_value,
        cfg.ttl_secs(),
        cfg.secure(),
        cfg.same_site(),
        cfg.domain(),
    );

    let mut headers = HeaderMap::new();
    let set_cookie = match cookie_header.parse() {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "login: session cookie header value invalid");
            return internal_error("session cookie header value invalid");
        }
    };
    headers.insert(header::SET_COOKIE, set_cookie);
    let location = match cfg.post_login_redirect().parse() {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                redirect = %cfg.post_login_redirect(),
                ?err,
                "login: post_login_redirect not a valid header value"
            );
            return internal_error("post_login_redirect not a valid header value");
        }
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

pub async fn logout(State(state): State<SessionState>, headers: HeaderMap) -> Response {
    // Before the clear directive is built, for the same reason login runs
    // it first: the forged request's whole effect must be a 403.
    if let Some(resp) = reject_cross_origin(&headers, state.config().public_origins(), "logout") {
        return resp;
    }
    let cfg = state.config();
    let cookie_header = build_clear_cookie_header(
        cfg.cookie_name(),
        cfg.secure(),
        cfg.same_site(),
        cfg.domain(),
    );

    let mut headers = HeaderMap::new();
    let set_cookie = match cookie_header.parse() {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "logout: clear cookie header value invalid");
            return internal_error("clear cookie header value invalid");
        }
    };
    headers.insert(header::SET_COOKIE, set_cookie);

    (StatusCode::NO_CONTENT, headers).into_response()
}

fn reject_cross_origin(
    headers: &HeaderMap,
    allowed: &PublicOrigins,
    handler: &'static str,
) -> Option<Response> {
    session::check_origin(headers, allowed, handler)
        .err()
        .map(|_| {
            error_response(
                StatusCode::FORBIDDEN,
                "origin_mismatch",
                "cross-origin request rejected",
            )
        })
}

fn internal_error(detail: &str) -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", detail)
}
