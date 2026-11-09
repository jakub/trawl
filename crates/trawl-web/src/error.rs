// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified proxy error type with HTTP status mapping.

use std::borrow::Cow;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fleet_auth::SessionError;
use trawl_api::{ErrorCode, ErrorEnvelope, ErrorResponse};

/// The 503 the proxy answers over its request count. It names the hop,
/// so a caller can tell this refusal from trawld's, and no size.
pub const REQUEST_LIMIT_MESSAGE: &str = "trawl-web is at its HTTP request limit; the request \
                                         was not sent to trawld; retry later with backoff";

/// Errors produced anywhere in the proxy, mapped to HTTP responses via
/// [`IntoResponse`]. Every one answers the `trawl_api` envelope with a
/// fixed message (ADR-0066), so the browser reads trawl-web's refusals and
/// trawld's through one decoder. Details go to the server log, never the
/// body.
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

    /// The proxy's body budget cannot cover the request's body
    /// (`body_budget`, ADR-0054 amended 2026-10-17). Answered exactly as
    /// [`Self::RequestLimitReached`]: the same 503, envelope and message,
    /// because to the browser it is the same refusal, the proxy at a
    /// fixed limit of its own; only the log line tells them apart.
    #[error("request body budget full")]
    BodyBudgetFull,

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
    /// [`crate::config::ProxyLimits::BODY_READ_TIMEOUT`], the deadline
    /// `middleware::body_deadline` holds every route's body to. Answered
    /// 408 with the envelope code `timeout`; the request's count is
    /// released with it. Nothing was sent to trawld.
    #[error("request body not received in time")]
    BodyTimeout,

    /// trawld's response head did not arrive within
    /// [`crate::config::ProxyLimits::upstream_head_timeout`]. Answered 504
    /// with the envelope code `timeout`. The proxy drops its side of the
    /// call; trawld may still be running the work it was asked for.
    #[error("upstream response head not received in time")]
    UpstreamHeadTimeout,
}

/// The 408 the proxy answers when a request body is not whole by the
/// body deadline.
pub const BODY_TIMEOUT_MESSAGE: &str = "request body was not received within trawl-web's deadline";

/// The 504 the proxy answers when trawld's head is late. It says the
/// work may still be running, because trawld only learns of the drop
/// when it next writes.
pub const UPSTREAM_HEAD_TIMEOUT_MESSAGE: &str =
    "trawld did not answer within trawl-web's deadline; trawld may still be running the work";

/// An answer of trawl-web's own: trawld's error shape, `Cache-Control:
/// no-store`, and no cookie. Every refusal the proxy writes goes through
/// it (ADR-0066); a body trawld wrote is forwarded as it came.
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
    /// The status, envelope code and public message this error answers.
    /// Every message is a fixed string, apart from the setting and limit
    /// [`Self::RequestTooLarge`] names; the text a wrapped error carries
    /// goes only to the log.
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

    /// Severity per variant: network and upstream failures are outage
    /// signal and must be visible at the default `info` level, while
    /// routine client errors stay at debug.
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
        // Only the expired-session path clears the cookie; see the
        // variant docs for why the other 401s stay silent.
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

    /// One answer as the browser reads it.
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

    /// A transport error built offline: a URL that does not parse.
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

    /// The clearing cookie is exactly the header the state built.
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
