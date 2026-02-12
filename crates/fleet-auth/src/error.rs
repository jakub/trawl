//! Error types for the authentication subsystem.

/// Errors from the authentication subsystem.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The database operation failed.
    #[error("auth database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// Password hashing or verification failed.
    #[error("token hashing error: {0}")]
    Hash(String),

    /// The provided token is not valid (bad format, not found, expired, revoked).
    #[error("invalid API key: {0}")]
    InvalidKey(String),

    /// No key found with the given prefix.
    #[error("key not found: {prefix}")]
    KeyNotFound {
        /// The prefix that was looked up.
        prefix: String,
    },

    /// The key exists but has been revoked.
    #[error("key has been revoked: {prefix}")]
    KeyRevoked {
        /// The prefix of the revoked key.
        prefix: String,
    },

    /// The token format is invalid (missing prefix, wrong length, etc.).
    #[error("malformed token: {0}")]
    MalformedToken(String),

    /// Invalid role string.
    #[error("unknown role: {0}")]
    UnknownRole(String),
}
