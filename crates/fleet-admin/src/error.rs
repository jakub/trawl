// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error type for the `fleet-admin` binary.

use fleet_auth::AuthError;

/// Top-level error type for the CLI.
///
/// Each variant maps to a single user-visible failure mode. `Display` output
/// is what `main` prints to stderr as `fleet-admin: {e}`.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    /// `DATABASE_URL` env var missing or empty.
    #[error("DATABASE_URL environment variable is required")]
    MissingDatabaseUrl,

    /// Postgres connection failed.
    #[error("database connection failed: {0}")]
    Connect(#[source] sqlx_core::Error),

    /// `sqlx` migrator returned an error while running fleet-auth migrations.
    #[error("migrations failed: {0}")]
    Migrate(#[from] sqlx_core::migrate::MigrateError),

    /// Pass-through for any error originating in fleet-auth (keystore /
    /// session). Preserves variant identity so future error-aware UX (exit
    /// codes, retry hints) can pattern-match without parsing strings.
    #[error(transparent)]
    Auth(#[from] AuthError),

    /// Malformed argument that clap can't catch (e.g. `--grant app:role` shape).
    #[error("invalid argument: {0}")]
    Arg(String),

    /// stdin / stdout / interactive prompt I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
