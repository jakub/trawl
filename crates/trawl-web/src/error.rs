// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified proxy error type with HTTP status mapping.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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
    /// missing/tampered. Clears no cookie: if the browser sent none there
    /// is nothing to clear, and a distinct response to a tampered cookie
    /// would confirm to a prober that it was recognized.
    #[error("unauthorized")]
    Unauthorized,

    /// The session is dead: the cookie's `exp` is in the past, or upstream
    /// trawld rejected the session's key with 401 (revoked/expired
    /// fleet-wide). This path does clear the cookie so the browser stops
    /// sending a token it can't redeem. Carries the pre-built, validated
    /// clear header from `AppState::build_clear_cookie` so the attributes
    /// match issuance (browsers ignore mismatched clears) and the
    /// `Set-Cookie` can't silently vanish on a parse failure.
    #[error("session expired")]
    ExpiredSession { clear_cookie: HeaderValue },

    /// A browser sent a cookie-authenticated request whose `Origin` is not
    /// one of the deployment's configured `public_origins` (ADR-0016).
    /// Raised by the `Session` extractor for every cookie route and by
    /// `login`/`logout`, which have no session to extract.
    ///
    /// 403, and deliberately no `Set-Cookie`: the response to a request
    /// the browser was never allowed to make must not change the session
    /// it was riding, or a foreign page could log a user out of every
    /// fleet app by provoking a rejection.
    #[error("cross-origin request rejected")]
    OriginMismatch,

    /// Upstream trawld returned a non-2xx status when the proxy called it.
    /// A 403 maps to 403 with no cookie mutation: the key is valid but
    /// lacks a trawl grant, and clearing the shared cookie would log the
    /// user out of sibling apps where they do have access.
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

        // Severity per variant: network and upstream failures are outage
        // signal and must be visible at the default `info` level, while
        // routine client errors stay at debug.
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

        // Only the expired-session path clears the cookie; see the
        // variant docs for why the other 401s stay silent.
        if let Self::ExpiredSession { clear_cookie } = &self {
            let body = axum::Json(json!({ "error": message }));
            let mut headers = HeaderMap::new();
            headers.insert(header::SET_COOKIE, clear_cookie.clone());
            return (status, headers, body).into_response();
        }

        let body = axum::Json(json!({ "error": message }));
        (status, body).into_response()
    }
}
