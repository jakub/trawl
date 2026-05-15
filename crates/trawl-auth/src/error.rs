// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the authentication subsystem.

/// Errors from the authentication subsystem.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The database operation failed.
    #[error("auth database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// Filesystem I/O error (e.g. setting database file permissions).
    #[error("auth I/O error: {0}")]
    Io(#[from] std::io::Error),

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

    /// Invalid app namespace.
    #[error("invalid app namespace: {0}")]
    InvalidApp(String),

    /// Invalid role name in a grant assignment.
    #[error("invalid role: {0}")]
    InvalidRole(String),

    /// A grant already exists for this (key, app) pair.
    #[error("grant already exists for key {prefix} in app {app} (use revoke first)")]
    GrantExists {
        /// The key prefix.
        prefix: String,
        /// The app namespace.
        app: String,
    },

    /// No grant exists for this (key, app) pair.
    #[error("no grant exists for key {prefix} in app {app}")]
    GrantNotFound {
        /// The key prefix.
        prefix: String,
        /// The app namespace.
        app: String,
    },

    /// A saved query with this name already exists for this user.
    #[error("a saved query named '{name}' already exists")]
    DuplicateName {
        /// The duplicate name.
        name: String,
    },

    /// Resource not found (saved query, etc.).
    #[error("{resource} not found: id={id}")]
    NotFound {
        /// The resource ID.
        id: i64,
        /// The resource type.
        resource: String,
    },

    /// The interval string is not a valid format (e.g. "5x", empty, non-numeric).
    #[error("invalid interval format: {input:?}")]
    InvalidInterval {
        /// The raw input string.
        input: String,
    },

    /// Schedule interval is below the minimum (60 seconds).
    #[error("schedule interval {secs}s is below minimum of 60s")]
    IntervalTooShort {
        /// The requested interval in seconds.
        secs: u64,
    },

    /// A schedule already exists for this saved query.
    #[error("a schedule already exists for saved query id={saved_query_id}")]
    ScheduleExists {
        /// The saved query that already has a schedule.
        saved_query_id: i64,
    },

    /// Saved query name contains invalid characters.
    #[error("invalid saved query name '{name}': must match [a-zA-Z0-9_-]+")]
    InvalidName {
        /// The rejected name.
        name: String,
    },
}
