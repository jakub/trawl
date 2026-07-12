// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified proxy error type with HTTP status mapping.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fleet_auth::SessionError;
use serde_json::json;

/// Errors produced anywhere in the proxy, mapped to HTTP responses via
/// [`IntoResponse`]. Error bodies are intentionally minimal — the browser
/// only needs enough to render a generic failure message. Details go to
/// the server log.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Request didn't carry a valid session cookie, or the cookie was
    /// missing/tampered. Does NOT clear any cookie — if the browser
    /// sent no cookie there's nothing to clear, and if the cookie was
    /// tampered we don't want to tell the attacker their attempt was
    /// noticed by sending a specific response.
    #[error("unauthorized")]
    Unauthorized,

    /// The session is dead: the cookie's `exp` is in the past, or upstream
    /// trawld rejected the session's key with 401 (revoked/expired
    /// fleet-wide). This path DOES clear the cookie so the browser stops
    /// sending a token it can't redeem. Carries the pre-built clear header
    /// from `AppState::build_clear_cookie()` so the attributes are
    /// guaranteed to match issuance — browsers reject mismatched clears.
    #[error("session expired")]
    ExpiredSession { clear_cookie: String },

    /// A browser sent a cross-origin request to a state-changing auth
    /// endpoint (login/logout). With the shared `fleet_session` cookie a
    /// forged logout would sign the user out of every fleet app, so the
    /// Origin header is validated by default (ADR-0004 slice 2).
    #[error("cross-origin request rejected")]
    OriginMismatch,

    /// Upstream trawld returned a non-2xx status when the proxy called it.
    /// NOTE: 403 deliberately maps to 403 with NO cookie mutation — the
    /// key is valid but lacks a trawl grant; clearing the shared cookie
    /// would log the user out of sibling apps where they DO have access.
    #[error("upstream returned {0}")]
    Upstream(StatusCode),

    /// Network failure reaching upstream trawld.
    #[error("upstream network error: {0}")]
    Network(#[from] reqwest::Error),

    /// Session cookie couldn't be built or parsed.
    #[error("session error: {0}")]
    Session(#[from] SessionError),

    /// Malformed request body (bad JSON, missing fields, etc.).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Catch-all for internal failures that shouldn't be surfaced to the
    /// browser in detail.
    #[error("internal error: {0}")]
    Internal(String),

    /// An optional upstream is not configured.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::ExpiredSession { .. } => (StatusCode::UNAUTHORIZED, "session expired"),
            Self::OriginMismatch => (StatusCode::FORBIDDEN, "cross-origin request rejected"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad request"),
            Self::Upstream(s) if s.as_u16() == 401 => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Upstream(s) if s.as_u16() == 403 => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Upstream(_) | Self::Network(_) => (StatusCode::BAD_GATEWAY, "upstream error"),
            Self::Session(_) | Self::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
            Self::ServiceUnavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "service unavailable"),
        };

        // Severity per variant — blanket `debug!` used to hide network
        // and upstream errors at the default `info` log level, which
        // made prod outages invisible until someone turned debug
        // logging on.
        match &self {
            Self::Unauthorized | Self::ExpiredSession { .. } | Self::BadRequest(_) => {
                tracing::debug!(error = %self, "proxy error (expected)");
            }
            Self::OriginMismatch => {
                tracing::warn!(error = %self, "proxy origin mismatch");
            }
            Self::Upstream(_) | Self::Network(_) => {
                tracing::warn!(error = %self, "proxy upstream error");
            }
            Self::Session(_) | Self::Internal(_) => {
                tracing::error!(error = %self, "proxy internal error");
            }
            Self::ServiceUnavailable(_) => {
                tracing::warn!(error = %self, "proxy service unavailable");
            }
        }

        // For the expired-session path, attach Set-Cookie: Max-Age=0 so
        // the browser stops sending the dead cookie on every subsequent
        // request. Other 401 paths (missing/tampered) skip this: there
        // may be no cookie to clear, and we don't want to confirm to a
        // probing attacker that their tampered cookie was recognized.
        if let Self::ExpiredSession { clear_cookie } = &self {
            let body = axum::Json(json!({ "error": message }));
            let mut headers = HeaderMap::new();
            if let Ok(v) = clear_cookie.parse() {
                headers.insert(header::SET_COOKIE, v);
            }
            return (status, headers, body).into_response();
        }

        let body = axum::Json(json!({ "error": message }));
        (status, body).into_response()
    }
}
