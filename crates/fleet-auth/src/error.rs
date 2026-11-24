// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[cfg(feature = "keystore")]
    #[error("auth database error: {0}")]
    Database(#[from] sqlx::Error),

    #[cfg(feature = "keystore")]
    #[error("auth migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[cfg(feature = "keystore")]
    #[error(transparent)]
    Schema(#[from] crate::migrations::SchemaError),

    #[error("token hashing error: {0}")]
    Hash(String),

    #[error("token generation error: {0}")]
    TokenGeneration(String),

    #[error("invalid expiry duration: {0}")]
    InvalidExpiry(String),

    /// The provided token is not valid for any reason (bad credentials, key not
    /// found, revoked, expired). Opaque by design — callers cannot distinguish
    /// between these cases to prevent enumeration oracles.
    #[error("invalid API key: {0}")]
    InvalidKey(String),

    #[error("malformed token: {0}")]
    MalformedToken(String),

    /// No key found with the given prefix (admin-facing lookup, NOT auth path).
    #[error("key not found: {prefix}")]
    KeyNotFound { prefix: String },

    /// The key exists but has been revoked (admin-facing, NOT auth path).
    #[error("key has been revoked: {prefix}")]
    KeyRevoked { prefix: String },

    #[error("invalid principal kind: {0}")]
    InvalidPrincipalKind(String),

    #[error("invalid app namespace: {0}")]
    InvalidApp(String),

    #[error("invalid role: {0}")]
    InvalidRole(String),

    #[error("invalid permission: {0}")]
    InvalidPermission(String),

    #[error("role not found: {name}")]
    RoleNotFound { name: String },

    #[error("role already exists: {name}")]
    RoleExists { name: String },

    #[error("role {role} is already assigned to key {prefix}")]
    RoleAlreadyAssigned { prefix: String, role: String },

    #[error("role {role} is not assigned to key {prefix}")]
    RoleNotAssigned { prefix: String, role: String },

    #[error("role {name} is still assigned to {key_count} key(s); use force to delete anyway")]
    RoleInUse { name: String, key_count: i64 },
}
