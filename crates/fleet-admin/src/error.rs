// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use fleet_auth::AuthError;

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("DATABASE_URL environment variable is required")]
    MissingDatabaseUrl,

    #[error("DATABASE_URL is set but empty")]
    EmptyDatabaseUrl,

    #[error("database connection failed: {0}")]
    Connect(#[source] sqlx::Error),

    #[error("migrations failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// Pass-through for any error originating in fleet-auth (keystore /
    /// session). Preserves variant identity so future error-aware UX (exit
    /// codes, retry hints) can pattern-match without parsing strings.
    #[error(transparent)]
    Auth(#[from] AuthError),

    #[error("invalid APP:PERMISSION {input:?}: {reason}")]
    InvalidPermSpec { input: String, reason: &'static str },

    #[error("invalid --expires {input:?}: {reason}")]
    InvalidDuration { input: String, reason: &'static str },

    #[error("invalid key prefix {input:?}: {reason}")]
    InvalidKeyPrefix { input: String, reason: &'static str },

    #[error("key {prefix} ({name}) is already revoked")]
    AlreadyRevoked { prefix: String, name: String },

    #[error("refusing to revoke without --yes when stdin is not a TTY")]
    NonInteractive,

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
