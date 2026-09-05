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
//! `KeyStore::verify_key`, with no HTTP `/whoami` hop. It then requires at
//! least one permission in the app's namespace, sets the encrypted session
//! cookie, and 302-redirects to `SessionConfig.post_login_redirect`.
//!
//! Logout clears the session cookie with matching attributes (domain, path,
//! `same_site`, secure) so browsers accept the directive. Returns 204.
//!
//! Both handlers run the configured-origin guard as their first statement
//! (ADR-0016): a present `Origin` outside `SessionConfig::public_origins`
//! is a 403 that verifies no key, mints no cookie and clears none. The
//! guard reads the `Origin` field and nothing else — no `Host`, no
//! `Forwarded`, no URI.

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
/// - Cross-origin request (a present `Origin` that is not one of the app's
///   configured `public_origins`) → 403, no cookie. The whole origin is
///   compared, so another scheme or another port of the same name is
///   rejected too, and the shared cookie domain is deliberately NOT an
///   allowlist: a sibling app under the same parent domain is rejected
///   (ADR-0016).
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
        // No-permission: same body as middleware. NO Set-Cookie — the user
        // never gets a session for an app they can't access.
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
/// via [`session::check_origin`] (ADR-0016): a present `Origin` that is
/// not one of the app's configured `public_origins` → 403 with no
/// `Set-Cookie`. Sharing a parent-domain cookie is deliberately NOT an
/// allowlist — a sibling app is a different origin and is rejected. Absent
/// `Origin` is allowed, so curl/scripted clients are unaffected.
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

/// Run the configured-origin check against the request headers. Returns
/// `Some(403)` when the request must be rejected, `None` when the handler
/// may proceed.
///
/// The decision and its log line belong to [`session::check_origin`], so
/// this is only the mapping onto an HTTP response. The 403 body is the
/// generic error shape and carries no `Set-Cookie`: a forged logout must
/// not clear the shared cookie, and a forged login must not mint one.
///
/// Note what is NOT passed in: no URI, no `Host`. Since ADR-0016 the
/// verdict compares the whole `Origin` against configured origins, so the
/// request's own idea of which host it is addressed to is irrelevant — and
/// with it goes the HTTP/2 `:authority` fallback this function used to
/// need, along with every question about which proxy rewrote what.
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
