// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified proxy error type with HTTP status mapping.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::session::{SESSION_COOKIE, SessionError, build_clear_cookie_header};

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

    /// Session cookie decrypted correctly but `exp` is in the past.
    /// This path DOES clear the cookie so the browser stops sending a
    /// token it can't redeem. `secure_cookie` mirrors the original
    /// cookie's `Secure` attribute (which depends on whether dev's
    /// `allow_insecure_cookies` is set) so the clear directive matches.
    #[error("session expired")]
    ExpiredSession { secure_cookie: bool },

    /// Upstream trawld returned a non-2xx status when the proxy called it.
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
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::ExpiredSession { .. } => (StatusCode::UNAUTHORIZED, "session expired"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad request"),
            Self::Upstream(s) if s.as_u16() == 401 => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Upstream(s) if s.as_u16() == 403 => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Upstream(_) | Self::Network(_) => (StatusCode::BAD_GATEWAY, "upstream error"),
            Self::Session(_) | Self::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        };

        // Severity per variant — blanket `debug!` used to hide network
        // and upstream errors at the default `info` log level, which
        // made prod outages invisible until someone turned debug
        // logging on.
        match &self {
            Self::Unauthorized | Self::ExpiredSession { .. } | Self::BadRequest(_) => {
                tracing::debug!(error = %self, "proxy error (expected)");
            }
            Self::Upstream(_) | Self::Network(_) => {
                tracing::warn!(error = %self, "proxy upstream error");
            }
            Self::Session(_) | Self::Internal(_) => {
                tracing::error!(error = %self, "proxy internal error");
            }
        }

        // For the expired-session path, attach Set-Cookie: Max-Age=0 so
        // the browser stops sending the dead cookie on every subsequent
        // request. Other 401 paths (missing/tampered) skip this: there
        // may be no cookie to clear, and we don't want to confirm to a
        // probing attacker that their tampered cookie was recognized.
        if let Self::ExpiredSession { secure_cookie } = &self {
            let body = axum::Json(json!({ "error": message }));
            let header_val = build_clear_cookie_header(SESSION_COOKIE, *secure_cookie);
            let mut headers = HeaderMap::new();
            if let Ok(v) = header_val.parse() {
                headers.insert(header::SET_COOKIE, v);
            }
            return (status, headers, body).into_response();
        }

        let body = axum::Json(json!({ "error": message }));
        (status, body).into_response()
    }
}
