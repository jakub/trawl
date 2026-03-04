//! Client error types.

use trawl_api::{ErrorCode, ErrorDetail, ErrorEnvelope};

/// Errors from the fleet client library.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// HTTP transport failure.
    #[error("network error: {0}")]
    Network(String),

    /// Server returned a non-success status with structured error info.
    #[error("server error (HTTP {status}): {}", error.message)]
    Server {
        /// HTTP status code.
        status: u16,
        /// Structured error envelope from the server.
        error: ErrorEnvelope,
    },

    /// Failed to parse the server's response.
    #[error("response parse error: {0}")]
    Parse(String),
}

impl ClientError {
    /// Get the structured error envelope (if this is a server error).
    pub fn error_envelope(&self) -> Option<&ErrorEnvelope> {
        match self {
            Self::Server { error, .. } => Some(error),
            _ => None,
        }
    }

    /// Get the error details (spans, diagnostics). Empty for non-server errors.
    pub fn error_details(&self) -> &[ErrorDetail] {
        match self {
            Self::Server { error, .. } => &error.details,
            _ => &[],
        }
    }

    /// Get the machine-readable error code (if this is a server error).
    pub fn error_code(&self) -> Option<&ErrorCode> {
        match self {
            Self::Server { error, .. } => Some(&error.code),
            _ => None,
        }
    }
}
