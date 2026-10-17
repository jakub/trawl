// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified proxy error type with HTTP status mapping.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fleet_auth::SessionError;
use serde_json::json;
use trawl_api::{ErrorCode, ErrorEnvelope, ErrorResponse};

/// The 503 the proxy answers over its request count. It names the hop,
/// so a caller can tell this refusal from trawld's, and no size.
pub const REQUEST_LIMIT_MESSAGE: &str = "trawl-web is at its HTTP request limit; the request \
                                         was not sent to trawld; retry later with backoff";

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

    /// `[web] upstream_ca_path` has never held a usable certificate, so
    /// no client can verify trawld yet (ADR-0048). trawld writes its
    /// generated certificate on its first start, which may come after
    /// trawl-web's. Also the answer when no read of the file finished
    /// within [`crate::upstream::CA_READ_WAIT`], as on a stalled volume.
    ///
    /// 503, distinct from [`Self::ServiceUnavailable`] so an operator can
    /// tell the cause from the body alone, and no cookie mutation: the
    /// session is fine, only the hop to trawld is not ready. Logged per
    /// request at debug only; the pin's own log lines say when the file
    /// is missing, refused or loaded.
    #[error("upstream certificate not available")]
    UpstreamCertificateUnavailable,

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

    /// The proxy's request count is full
    /// (`middleware::request_count`, ADR-0054 amended 2026-10-17). The
    /// request was not sent to trawld. Answered with the `trawl_api`
    /// envelope, as trawld answers its own, so the browser handles both
    /// the same way; no cookie mutation, since the session is fine.
    #[error("request limit reached")]
    RequestLimitReached,

    /// A request body over the limit the proxy buffers to. The message is
    /// trawld's own sentence for its 413, naming the setting and the exact
    /// value, so a caller resizes the request the same way whichever hop
    /// refused it. Nothing was sent to trawld.
    #[error("request body exceeds {setting} ({limit} bytes)")]
    RequestTooLarge {
        /// The setting's name as an operator writes it, or the fixed
        /// login limit's name.
        setting: &'static str,
        /// The limit in bytes.
        limit: usize,
    },

    /// A request body that did not arrive whole within
    /// [`crate::config::ProxyLimits::BODY_READ_TIMEOUT`]. Answered 408
    /// with the envelope code `timeout`; the request's count is released
    /// with it. Nothing was sent to trawld.
    #[error("request body not received in time")]
    BodyTimeout,
}

/// The envelope answers: trawld's error shape, `Cache-Control:
/// no-store`, and no cookie.
///
/// The legacy `{"error": "..."}` body stays on the older variants; only
/// the answers that mirror a trawld refusal use the envelope, so the UI
/// reads them through the same typed contract.
fn envelope(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    let body = axum::Json(ErrorResponse {
        error: ErrorEnvelope::simple(code, message),
    });
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        match &self {
            Self::RequestLimitReached => {
                tracing::warn!(error = %self, "proxy request limit reached");
                return envelope(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::RequestLimitReached,
                    REQUEST_LIMIT_MESSAGE,
                );
            }
            Self::RequestTooLarge { .. } => {
                tracing::debug!(error = %self, "proxy refused an oversized body");
                return envelope(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    ErrorCode::RequestTooLarge,
                    &self.to_string(),
                );
            }
            Self::BodyTimeout => {
                tracing::debug!(error = %self, "proxy body deadline passed");
                return envelope(
                    StatusCode::REQUEST_TIMEOUT,
                    ErrorCode::Timeout,
                    "request body was not received within trawl-web's deadline",
                );
            }
            _ => {}
        }
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
            Self::UpstreamCertificateUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream certificate not available",
            ),
            Self::RequestLimitReached | Self::RequestTooLarge { .. } | Self::BodyTimeout => {
                unreachable!("answered above")
            }
        };

        // Severity per variant: network and upstream failures are outage
        // signal and must be visible at the default `info` level, while
        // routine client errors stay at debug.
        match &self {
            Self::Unauthorized
            | Self::ExpiredSession { .. }
            | Self::BadRequest(_)
            | Self::UpstreamCertificateUnavailable => {
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
            Self::RequestLimitReached | Self::RequestTooLarge { .. } | Self::BodyTimeout => {
                unreachable!("answered above")
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
