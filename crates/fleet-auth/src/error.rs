// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the fleet-auth subsystem.

/// Errors from the fleet-auth subsystem.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The underlying database operation failed.
    #[cfg(feature = "keystore")]
    #[error("auth database error: {0}")]
    Database(#[from] sqlx_core::Error),

    /// A schema migration operation failed.
    #[cfg(feature = "keystore")]
    #[error("auth migration error: {0}")]
    Migration(#[from] sqlx_core::migrate::MigrateError),

    /// Argon2id hashing or verification failed.
    #[error("token hashing error: {0}")]
    Hash(String),

    /// Token generation failed to produce a unique prefix after the configured
    /// retry budget — astronomically unlikely (48-bit prefix space) but coded
    /// defensively. Distinct from `Hash` so callers can tell hashing errors
    /// from generation exhaustion.
    #[error("token generation error: {0}")]
    TokenGeneration(String),

    /// A requested key expiry duration could not be represented as a
    /// `chrono::Duration` (would overflow ~292 years from now).
    #[error("invalid expiry duration: {0}")]
    InvalidExpiry(String),

    /// The provided token is not valid for any reason (bad credentials, key not
    /// found, revoked, expired). Opaque by design — callers cannot distinguish
    /// between these cases to prevent enumeration oracles.
    #[error("invalid API key: {0}")]
    InvalidKey(String),

    /// The token doesn't parse as a `flt_*` token (missing prefix, too short).
    #[error("malformed token: {0}")]
    MalformedToken(String),

    /// No key found with the given prefix (admin-facing lookup, NOT auth path).
    #[error("key not found: {prefix}")]
    KeyNotFound {
        /// The prefix that was looked up.
        prefix: String,
    },

    /// The key exists but has been revoked (admin-facing, NOT auth path).
    #[error("key has been revoked: {prefix}")]
    KeyRevoked {
        /// The prefix of the revoked key.
        prefix: String,
    },

    /// `kind` string in the database is neither `human` nor `service`. Reaching
    /// this means data corruption or a schema mismatch.
    #[error("invalid principal kind: {0}")]
    InvalidPrincipalKind(String),

    /// App namespace failed validation.
    #[error("invalid app namespace: {0}")]
    InvalidApp(String),

    /// Role name failed validation.
    #[error("invalid role: {0}")]
    InvalidRole(String),

    /// A grant already exists for this `(key, app)` pair. Callers must revoke
    /// the existing grant first to make the swap intentional.
    #[error("grant already exists for key {prefix} in app {app}")]
    GrantExists {
        /// The key prefix.
        prefix: String,
        /// The app namespace.
        app: String,
    },

    /// No grant exists for this `(key, app)` pair.
    #[error("no grant exists for key {prefix} in app {app}")]
    GrantNotFound {
        /// The key prefix.
        prefix: String,
        /// The app namespace.
        app: String,
    },
}
