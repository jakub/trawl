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

use crate::middleware::{SessionState, classify_verify_error, error_response, no_grant_response};
use crate::session::{
    self, SessionExpiry, SessionPayload, build_clear_cookie_header, build_session_cookie_header,
    zeroizing_string,
};

/// `POST /login` request body.
///
/// `api_key` is wrapped in [`Zeroizing`] on deserialization so the secret
/// is scrubbed from memory when the request frame is dropped, even on the
/// error path.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    #[serde(with = "zeroizing_string")]
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
/// - Cross-origin request (`Origin` present and matching neither the
///   request `Host` nor the configured cookie domain) → 403, no cookie.
/// - Empty `api_key` → 400.
/// - Invalid `api_key` → 401 JSON (same shape as middleware).
/// - Valid `api_key` but no grant for `app_namespace` → 403 HTML (same
///   body as middleware no-grant; no cookie set).
/// - Success → 302 with `Set-Cookie` and `Location: post_login_redirect`.
#[allow(clippy::implicit_hasher)]
pub async fn login(
    State(state): State<SessionState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    if let Some(resp) = reject_cross_origin(&headers, "login") {
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
    if verified.role_for(cfg.app_namespace()).is_none() {
        // No-grant: same body as middleware. NO Set-Cookie — the user never
        // gets a session for an app they can't access.
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

/// `POST /logout` handler.
///
/// Returns 204 with a `Set-Cookie` clear directive — never requires a
/// valid session (logout works even with a stale cookie). Attributes
/// match what login sets so browsers accept the clear.
///
/// # Origin enforcement (default-on)
///
/// Because the `fleet_session` cookie is shared across sibling apps under
/// a parent domain, a forged cross-site POST to any app's logout endpoint
/// would clear the shared cookie and sign the user out of every sibling
/// app. Both `login` and `logout` therefore validate the `Origin` header
/// by default via [`session::check_origin`] (ADR-0004 slice 2): a present Origin
/// whose host doesn't match the request `Host` → 403 with NO `Set-Cookie`.
/// Sharing a parent-domain cookie is deliberately NOT an origin allowlist —
/// a sibling app is a different origin and is rejected. Absent Origin is
/// allowed, so curl/scripted clients are unaffected.
pub async fn logout(State(state): State<SessionState>, headers: HeaderMap) -> Response {
    if let Some(resp) = reject_cross_origin(&headers, "logout") {
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

/// Run the present-only, strictly same-host Origin check against the
/// request headers. Returns `Some(403)` when the request must be rejected,
/// `None` when the handler may proceed. Delegates the decision + rejection
/// log to the shared [`session::check_origin`] so the log fields/message
/// live in one place; the cookie's shared domain is deliberately NOT an
/// origin allowlist — see [`session::origin_allowed`].
fn reject_cross_origin(headers: &HeaderMap, handler: &str) -> Option<Response> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    session::check_origin(origin, host, handler).err().map(|_| {
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
