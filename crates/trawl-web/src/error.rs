// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::borrow::Cow;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fleet_auth::SessionError;
use trawl_api::{ErrorCode, ErrorEnvelope, ErrorResponse};

pub const REQUEST_LIMIT_MESSAGE: &str = "trawl-web is at its HTTP request limit; the request \
                                         was not sent to trawld; retry later with backoff";

/// Every variant answers the `trawl_api` envelope with a fixed message
/// (ADR-0066). Details go to the server log, never the body.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Clears no cookie: if the browser sent none there is nothing to clear,
    /// and a distinct response to a tampered cookie would confirm to a prober
    /// that it was recognized.
    #[error("unauthorized")]
    Unauthorized,

    /// Carries the pre-built, validated clear header from
    /// `AppState::build_clear_cookie` so the attributes match issuance
    /// (browsers ignore mismatched clears).
    #[error("session expired")]
    ExpiredSession { clear_cookie: HeaderValue },

    /// Deliberately no `Set-Cookie`, or a foreign page could log a user out of
    /// every fleet app by provoking a rejection.
    #[error("cross-origin request rejected")]
    OriginMismatch,

    /// A 403 maps to 403 with no cookie mutation: the key is valid but
    /// lacks a trawl grant, and clearing the shared cookie would log the
    /// user out of sibling apps where they do have access.
    #[error("upstream returned {0}")]
    Upstream(StatusCode),

    #[error("upstream certificate not available")]
    UpstreamCertificateUnavailable,

    #[error("upstream network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("session error: {0}")]
    Session(#[from] SessionError),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("request limit reached")]
    RequestLimitReached,

    #[error("request body budget full")]
    BodyBudgetFull,

    #[error("request body exceeds {setting} ({limit} bytes)")]
    RequestTooLarge { setting: &'static str, limit: usize },

    #[error("request body not received in time")]
    BodyTimeout,

    #[error("upstream response head not received in time")]
    UpstreamHeadTimeout,
}

pub const BODY_TIMEOUT_MESSAGE: &str = "request body was not received within trawl-web's deadline";

pub const UPSTREAM_HEAD_TIMEOUT_MESSAGE: &str =
    "trawld did not answer within trawl-web's deadline; trawld may still be running the work";

pub(crate) fn envelope(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    let body = axum::Json(ErrorResponse {
        error: ErrorEnvelope::simple(code, message),
    });
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

impl ProxyError {
    fn answer(&self) -> (StatusCode, ErrorCode, Cow<'static, str>) {
        use Cow::Borrowed;
        match self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                ErrorCode::AuthError,
                Borrowed("unauthorized"),
            ),
            Self::ExpiredSession { .. } => (
                StatusCode::UNAUTHORIZED,
                ErrorCode::AuthError,
                Borrowed("session expired"),
            ),
            Self::OriginMismatch => (
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                Borrowed("cross-origin request rejected"),
            ),
            Self::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                ErrorCode::BadRequest,
                Borrowed("bad request"),
            ),
            Self::Upstream(s) if s.as_u16() == 401 => (
                StatusCode::UNAUTHORIZED,
                ErrorCode::AuthError,
                Borrowed("unauthorized"),
            ),
            Self::Upstream(s) if s.as_u16() == 403 => (
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                Borrowed("forbidden"),
            ),
            Self::Upstream(_) | Self::Network(_) => (
                StatusCode::BAD_GATEWAY,
                ErrorCode::ServiceUnavailable,
                Borrowed("upstream error"),
            ),
            Self::Session(_) | Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                Borrowed("internal error"),
            ),
            Self::ServiceUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ServiceUnavailable,
                Borrowed("service unavailable"),
            ),
            Self::UpstreamCertificateUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ServiceUnavailable,
                Borrowed("upstream certificate not available"),
            ),
            Self::RequestLimitReached | Self::BodyBudgetFull => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::RequestLimitReached,
                Borrowed(REQUEST_LIMIT_MESSAGE),
            ),
            Self::RequestTooLarge { .. } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::RequestTooLarge,
                Cow::Owned(self.to_string()),
            ),
            Self::BodyTimeout => (
                StatusCode::REQUEST_TIMEOUT,
                ErrorCode::Timeout,
                Borrowed(BODY_TIMEOUT_MESSAGE),
            ),
            Self::UpstreamHeadTimeout => (
                StatusCode::GATEWAY_TIMEOUT,
                ErrorCode::Timeout,
                Borrowed(UPSTREAM_HEAD_TIMEOUT_MESSAGE),
            ),
        }
    }

    fn log(&self) {
        match self {
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
            Self::RequestLimitReached => {
                tracing::warn!(error = %self, "proxy request limit reached");
            }
            Self::BodyBudgetFull => {
                tracing::warn!(error = %self, "proxy body budget full");
            }
            Self::RequestTooLarge { .. } => {
                tracing::debug!(error = %self, "proxy refused an oversized body");
            }
            Self::BodyTimeout => {
                tracing::debug!(error = %self, "proxy body deadline passed");
            }
            Self::UpstreamHeadTimeout => {
                tracing::warn!(error = %self, "proxy upstream head deadline passed");
            }
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        self.log();
        let (status, code, message) = self.answer();
        let mut response = envelope(status, code, &message);
        if let Self::ExpiredSession { clear_cookie } = self {
            response
                .headers_mut()
                .insert(header::SET_COOKIE, clear_cookie);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Answered {
        status: StatusCode,
        cache_control: Option<String>,
        set_cookie: bool,
        error: ErrorEnvelope,
    }

    async fn answer(error: ProxyError) -> Answered {
        let response = error.into_response();
        let status = response.status();
        let cache_control = response
            .headers()
            .get(header::CACHE_CONTROL)
            .map(|value| value.to_str().unwrap().to_owned());
        let set_cookie = response.headers().contains_key(header::SET_COOKIE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let decoded: ErrorResponse = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "not an ErrorResponse ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        Answered {
            status,
            cache_control,
            set_cookie,
            error: decoded.error,
        }
    }

    fn network_error() -> reqwest::Error {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::new().get("::bad").build().unwrap_err()
    }

    #[tokio::test]
    #[expect(clippy::too_many_lines, reason = "one row per variant")]
    async fn proxy_error_answers_the_envelope() {
        let clear = HeaderValue::from_static("trawl_session=; Max-Age=0; Path=/");
        let cases: Vec<(&str, ProxyError, u16, ErrorCode, &str)> = vec![
            (
                "unauthorized",
                ProxyError::Unauthorized,
                401,
                ErrorCode::AuthError,
                "unauthorized",
            ),
            (
                "expired session",
                ProxyError::ExpiredSession {
                    clear_cookie: clear.clone(),
                },
                401,
                ErrorCode::AuthError,
                "session expired",
            ),
            (
                "origin mismatch",
                ProxyError::OriginMismatch,
                403,
                ErrorCode::Forbidden,
                "cross-origin request rejected",
            ),
            (
                "upstream 401",
                ProxyError::Upstream(StatusCode::UNAUTHORIZED),
                401,
                ErrorCode::AuthError,
                "unauthorized",
            ),
            (
                "upstream 403",
                ProxyError::Upstream(StatusCode::FORBIDDEN),
                403,
                ErrorCode::Forbidden,
                "forbidden",
            ),
            (
                "upstream 500",
                ProxyError::Upstream(StatusCode::INTERNAL_SERVER_ERROR),
                502,
                ErrorCode::ServiceUnavailable,
                "upstream error",
            ),
            (
                "upstream 404",
                ProxyError::Upstream(StatusCode::NOT_FOUND),
                502,
                ErrorCode::ServiceUnavailable,
                "upstream error",
            ),
            (
                "upstream certificate unavailable",
                ProxyError::UpstreamCertificateUnavailable,
                503,
                ErrorCode::ServiceUnavailable,
                "upstream certificate not available",
            ),
            (
                "network",
                ProxyError::Network(network_error()),
                502,
                ErrorCode::ServiceUnavailable,
                "upstream error",
            ),
            (
                "session",
                ProxyError::Session(SessionError::Aead),
                500,
                ErrorCode::InternalError,
                "internal error",
            ),
            (
                "bad request",
                ProxyError::BadRequest("api_key is required".to_owned()),
                400,
                ErrorCode::BadRequest,
                "bad request",
            ),
            (
                "internal",
                ProxyError::Internal("pool at /var/lib/secret exhausted".to_owned()),
                500,
                ErrorCode::InternalError,
                "internal error",
            ),
            (
                "service unavailable",
                ProxyError::ServiceUnavailable("no upstream configured".to_owned()),
                503,
                ErrorCode::ServiceUnavailable,
                "service unavailable",
            ),
            (
                "request limit reached",
                ProxyError::RequestLimitReached,
                503,
                ErrorCode::RequestLimitReached,
                REQUEST_LIMIT_MESSAGE,
            ),
            (
                "body budget full",
                ProxyError::BodyBudgetFull,
                503,
                ErrorCode::RequestLimitReached,
                REQUEST_LIMIT_MESSAGE,
            ),
            (
                "request too large",
                ProxyError::RequestTooLarge {
                    setting: "[server] max_request_body_bytes",
                    limit: 1024,
                },
                413,
                ErrorCode::RequestTooLarge,
                "request body exceeds [server] max_request_body_bytes (1024 bytes)",
            ),
            (
                "body timeout",
                ProxyError::BodyTimeout,
                408,
                ErrorCode::Timeout,
                BODY_TIMEOUT_MESSAGE,
            ),
            (
                "upstream head timeout",
                ProxyError::UpstreamHeadTimeout,
                504,
                ErrorCode::Timeout,
                UPSTREAM_HEAD_TIMEOUT_MESSAGE,
            ),
        ];
        for (case, error, status, code, message) in cases {
            let expired = matches!(error, ProxyError::ExpiredSession { .. });
            let answered = answer(error).await;
            assert_eq!(answered.status.as_u16(), status, "{case}");
            assert_eq!(answered.error.code, code, "{case}");
            assert_eq!(answered.error.message, message, "{case}");
            assert!(answered.error.details.is_empty(), "{case}");
            assert!(answered.error.conflict.is_none(), "{case}");
            assert_eq!(
                answered.cache_control.as_deref(),
                Some("no-store"),
                "{case}"
            );
            assert_eq!(answered.set_cookie, expired, "{case}: set-cookie");
        }
    }

    #[tokio::test]
    async fn expired_session_clears_with_the_given_header() {
        let clear = HeaderValue::from_static("trawl_session=; Max-Age=0; Path=/");
        let response = ProxyError::ExpiredSession {
            clear_cookie: clear.clone(),
        }
        .into_response();
        let cookies: Vec<_> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .collect();
        assert_eq!(cookies, [&clear]);
    }
}
