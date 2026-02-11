//! Unified server error type with HTTP status mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fleet_auth::AuthError;
use fleet_engine::error::EngineError;

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

    /// Internal server error (task panics, unexpected failures).
    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            // Parse errors are client mistakes.
            Self::Engine(EngineError::Parse(_) | EngineError::Emit(_)) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            // Database errors are server-side — don't leak details.
            Self::Engine(EngineError::Database(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "query execution failed".to_owned(),
            ),
            // Auth errors are deliberately opaque.
            Self::Auth(_) => (StatusCode::UNAUTHORIZED, "authentication failed".to_owned()),
            Self::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            Self::Internal(_) => {
                tracing::error!(error = %self, "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_owned(),
                )
            }
        };

        let body = serde_json::json!({ "error": message });
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
}
