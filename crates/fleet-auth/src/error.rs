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
    Database(#[from] sqlx::Error),

    /// A schema migration operation failed.
    #[cfg(feature = "keystore")]
    #[error("auth migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// Fresh-baseline admission or read-only schema validation failed.
    #[cfg(feature = "keystore")]
    #[error(transparent)]
    Schema(#[from] crate::migrations::SchemaError),

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

    /// Permission string failed validation.
    #[error("invalid permission: {0}")]
    InvalidPermission(String),

    /// No role with the given name exists. Explicit error rather than a
    /// silent no-op so a typo can never mint a capability-less key or
    /// assign nothing.
    #[error("role not found: {name}")]
    RoleNotFound {
        /// The role name that was looked up.
        name: String,
    },

    /// A role with this name already exists (`roles(name)` is UNIQUE).
    #[error("role already exists: {name}")]
    RoleExists {
        /// The conflicting role name.
        name: String,
    },

    /// The key already holds this role. Assigning it twice is a typo, not
    /// a success.
    #[error("role {role} is already assigned to key {prefix}")]
    RoleAlreadyAssigned {
        /// The key prefix.
        prefix: String,
        /// The role name.
        role: String,
    },

    /// The key does not hold this role — unassigning nothing is a typo,
    /// not a success.
    #[error("role {role} is not assigned to key {prefix}")]
    RoleNotAssigned {
        /// The key prefix.
        prefix: String,
        /// The role name.
        role: String,
    },

    /// The role is still assigned to keys and deletion was not forced.
    /// Deleting it would silently strip capability from live keys.
    #[error("role {name} is still assigned to {key_count} key(s); use force to delete anyway")]
    RoleInUse {
        /// The role name.
        name: String,
        /// How many keys currently hold the role.
        key_count: i64,
    },
}
