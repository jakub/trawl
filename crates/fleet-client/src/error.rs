//! Client error types.

/// Errors from the fleet client library.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// HTTP transport failure.
    #[error("network error: {0}")]
    Network(String),

    /// Server returned a non-success status.
    #[error("server error (HTTP {status}): {message}")]
    Server {
        /// HTTP status code.
        status: u16,
        /// Error message from the server.
        message: String,
    },

    /// Failed to parse the server's response.
    #[error("response parse error: {0}")]
    Parse(String),
}
