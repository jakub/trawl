// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

/// Controller error with secret-safe, actionable diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read {kind} file {path}: {source}")]
    ReadFile {
        kind: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse {kind} file {path}: {message}")]
    ParseFile {
        kind: &'static str,
        path: PathBuf,
        message: String,
    },

    #[error("invalid {kind} file {path}: {message}")]
    InvalidConfig {
        kind: &'static str,
        path: PathBuf,
        message: String,
    },

    #[error("{0}")]
    InvalidArgument(String),

    #[error("failed to run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },

    #[error("{program} exited with status {status}: {message}")]
    CommandFailed {
        program: String,
        status: std::process::ExitStatus,
        message: String,
    },

    #[error("{program} was terminated: {reason}")]
    CommandLimit { program: String, reason: String },

    #[error("resolver for {app} returned invalid JSON: {message}")]
    ResolverProtocol { app: String, message: String },

    #[error(transparent)]
    Auth(#[from] fleet_auth::AuthError),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
