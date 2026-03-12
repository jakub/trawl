//! Unified server error type with HTTP status mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use trawl_auth::AuthError;
use trawl_engine::error::EngineError;

/// Server errors, mapped to HTTP responses via [`IntoResponse`].
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Query engine failure (parse, emit, or `DuckDB` execution).
    #[error("{0}")]
    Engine(#[from] EngineError),

    /// Authentication failure.
    #[error("{0}")]
    Auth(#[from] AuthError),

    /// Missing or malformed Authorization header.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Bad request (invalid input, duplicate name, etc.).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Resource not found (or unauthorized access).
    #[error("not found: {0}")]
    NotFound(String),

    /// Query execution exceeded the configured timeout.
    #[error("query timed out")]
    Timeout,

    /// Ingest validation error (bad ndjson, missing fields).
    #[error("ingest error: {0}")]
    Ingest(String),

    /// Rate limit exceeded (429).
    #[error("rate limit exceeded")]
    RateLimited,

    /// Too many concurrent SSE streams (429).
    #[error("too many concurrent streams")]
    TooManyStreams,

    /// Internal server error (task panics, unexpected failures).
    #[error("internal error: {0}")]
    Internal(String),
}

impl ServerError {
    /// Return a sanitized error message safe for logs and tracker history.
    ///
    /// Database internals and internal error details are redacted to prevent
    /// information disclosure. Parse/emit errors (client mistakes) are preserved.
    pub fn safe_message(&self) -> String {
        match self {
            Self::Engine(EngineError::Database(_)) => "query execution failed".to_owned(),
            Self::Internal(_) => "internal error".to_owned(),
            Self::Ingest(_) => "ingest error".to_owned(),
            Self::BadRequest(_) => "bad request".to_owned(),
            Self::NotFound(_) => "not found".to_owned(),
            Self::RateLimited => "rate limit exceeded".to_owned(),
            Self::TooManyStreams => "too many concurrent streams".to_owned(),
            other => other.to_string(),
        }
    }
}

/// Convert a `ParseError` into a structured `ErrorDetail`.
fn parse_error_to_detail(e: &trawl_core::parser::ParseError) -> trawl_api::ErrorDetail {
    trawl_api::ErrorDetail {
        message: e.message.clone(),
        span: Some(trawl_api::ErrorSpan {
            start: e.span.start,
            end: e.span.end,
        }),
        label: e.label.clone(),
        hint: e.hint.clone(),
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        use trawl_api::{ErrorCode, ErrorEnvelope};

        let (status, envelope) = match &self {
            Self::Engine(EngineError::Parse(errors)) => {
                let details: Vec<_> = errors.iter().map(parse_error_to_detail).collect();
                let message = errors
                    .first()
                    .map_or("parse error".to_owned(), |e| e.message.clone());
                (
                    StatusCode::BAD_REQUEST,
                    ErrorEnvelope {
                        code: ErrorCode::ParseError,
                        message,
                        details,
                    },
                )
            }
            Self::Engine(EngineError::Emit(e)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::ValidationError, e.to_string()),
            ),
            Self::Engine(EngineError::ResultTooLarge(n)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(
                    ErrorCode::ResultTooLarge,
                    format!("result exceeded {n} row limit"),
                ),
            ),
            // Database errors are server-side — don't leak details.
            Self::Engine(EngineError::Database(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorEnvelope::simple(ErrorCode::ExecutionError, "query execution failed"),
            ),
            // Auth errors are deliberately opaque.
            Self::Auth(_) => (
                StatusCode::UNAUTHORIZED,
                ErrorEnvelope::simple(ErrorCode::AuthError, "authentication failed"),
            ),
            Self::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                ErrorEnvelope::simple(ErrorCode::Unauthorized, msg.clone()),
            ),
            Self::Ingest(msg) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::IngestError, msg.clone()),
            ),
            Self::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::BadRequest, msg.clone()),
            ),
            Self::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                ErrorEnvelope::simple(ErrorCode::NotFound, msg.clone()),
            ),
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                ErrorEnvelope::simple(ErrorCode::Timeout, "query timed out"),
            ),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorEnvelope::simple(ErrorCode::RateLimited, "rate limit exceeded"),
            ),
            Self::TooManyStreams => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorEnvelope::simple(ErrorCode::TooManyStreams, "too many concurrent streams"),
            ),
            Self::Internal(_) => {
                tracing::error!(event_type = "internal_error", error = %self, "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorEnvelope::simple(ErrorCode::InternalError, "internal server error"),
                )
            }
        };

        let body = trawl_api::ErrorResponse { error: envelope };
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_error_maps_to_401() {
        let err = ServerError::Auth(AuthError::InvalidKey("bad".into()));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn unauthorized_maps_to_401() {
        let err = ServerError::Unauthorized("missing token".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn internal_maps_to_500() {
        let err = ServerError::Internal("something broke".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn safe_message_redacts_database_errors() {
        let err = ServerError::Engine(EngineError::Database(duckdb::Error::InvalidColumnName(
            "secret_column".into(),
        )));
        assert_eq!(err.safe_message(), "query execution failed");
    }

    #[test]
    fn safe_message_preserves_parse_errors() {
        let err = ServerError::Engine(EngineError::Parse(vec![trawl_core::parser::ParseError {
            message: "bad syntax".into(),
            span: 0..3,
            label: None,
            hint: None,
        }]));
        assert!(err.safe_message().contains("bad syntax"));
    }

    #[test]
    fn safe_message_redacts_internal_errors() {
        let err = ServerError::Internal("db connection string leaked".into());
        assert_eq!(err.safe_message(), "internal error");
    }
}
