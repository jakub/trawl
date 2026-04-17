// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified proxy error type with HTTP status mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::session::SessionError;

/// Errors produced anywhere in the proxy, mapped to HTTP responses via
/// [`IntoResponse`]. Error bodies are intentionally minimal — the browser
/// only needs enough to render a generic failure message. Details go to
/// the server log.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Request didn't carry a valid session cookie, or the cookie expired.
    #[error("unauthorized")]
    Unauthorized,

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
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad request"),
            Self::Upstream(s) if s.as_u16() == 401 => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Upstream(s) if s.as_u16() == 403 => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Upstream(_) | Self::Network(_) => (StatusCode::BAD_GATEWAY, "upstream error"),
            Self::Session(_) | Self::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        };

        tracing::debug!(error = %self, "proxy error");

        let body = axum::Json(json!({ "error": message }));
        (status, body).into_response()
    }
}
