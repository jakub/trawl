// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fleet-admin` — operational CLI for the shared fleet keystore (ADR-0030).

use std::io::Write;
use std::process;

use clap::{Parser, Subcommand, ValueEnum};
use fleet_auth::{KeyStore, PrincipalKind, RoleAssignment};
use sqlx_postgres::{PgPool, PgPoolOptions};

mod commands;
mod error;

use error::AdminError;

/// Fleet-wide operational CLI.
#[derive(Parser)]
#[command(name = "fleet-admin", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply embedded fleet-auth schema migrations to the fleet database.
    Migrate,
    /// Generate a new XChaCha20-Poly1305 session key (base64url, stdout).
    GenerateSessionKey,
    /// Manage API keys in the fleet keystore.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
}

#[derive(Subcommand)]
enum KeysAction {
    /// Create a new API key.
    Create {
        /// Human-readable name for the key.
        #[arg(long)]
        name: String,
        /// Principal kind: `human` for interactive principals, `service` for
        /// non-interactive ones.
        #[arg(long)]
        kind: CliKind,
        /// Repeatable `app:role` grant (e.g. `--grant trawl:admin
        /// --grant coastwatch:siem_consumer`). May be empty — a grantless
        /// key authenticates but authorizes nothing until grants are added.
        #[arg(long = "grant", value_name = "APP:ROLE")]
        grants: Vec<String>,
        /// Expiration duration (e.g. `90d`, `24h`, `52w`). Omit for no expiry.
        #[arg(long)]
        expires: Option<String>,
    },
    /// List API keys (active only by default).
    List {
        /// Show all keys, including revoked.
        #[arg(long)]
        all: bool,
    },
    /// Revoke an API key by its prefix.
    Revoke {
        /// The key prefix (shown in `keys list`).
        prefix: String,
        /// Skip the interactive confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Add an `app:role` grant to an existing key.
    Grant {
        /// The key prefix (shown in `keys list`).
        prefix: String,
        /// The `app:role` grant to add.
        grant: String,
    },
}

/// CLI-side mirror of [`PrincipalKind`] — `clap` requires the type to live
/// in the binary crate to derive `ValueEnum`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum CliKind {
    Human,
    Service,
}

impl From<CliKind> for PrincipalKind {
    fn from(k: CliKind) -> Self {
        match k {
            CliKind::Human => Self::Human,
            CliKind::Service => Self::Service,
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        // process::exit skips stdlib destructors, so flush stdout
        // explicitly — any partially-buffered token must reach the fd.
        let _ = std::io::stdout().flush();
        eprintln!("fleet-admin: {e}");
        process::exit(1);
    }
}

async fn run() -> Result<(), AdminError> {
    match Cli::parse().command {
        // Skip pool construction — `generate-session-key` must work before
        // the database exists, since it produces the key the server is
        // deployed with.
        Command::GenerateSessionKey => commands::session_key::run(),
        Command::Migrate => {
            let pool = connect_pool().await?;
            commands::migrate::run(&pool).await
        }
        Command::Keys { action } => {
            let pool = connect_pool().await?;
            dispatch_keys(KeyStore::from_pool(pool), action).await
        }
    }
}

async fn dispatch_keys(store: KeyStore, action: KeysAction) -> Result<(), AdminError> {
    match action {
        KeysAction::Create {
            name,
            kind,
            grants,
            expires,
        } => {
            let parsed: Vec<RoleAssignment> = grants
                .iter()
                .map(|g| commands::keys::parse_grant(g))
                .collect::<Result<_, _>>()?;
            commands::keys::create(&store, &name, kind.into(), &parsed, expires.as_deref()).await
        }
        KeysAction::List { all } => commands::keys::list(&store, all).await,
        KeysAction::Revoke { prefix, yes } => commands::keys::revoke(&store, &prefix, yes).await,
        KeysAction::Grant { prefix, grant } => {
            let assignment = commands::keys::parse_grant(&grant)?;
            commands::keys::grant(&store, &prefix, &assignment).await
        }
    }
}

async fn connect_pool() -> Result<PgPool, AdminError> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(v) if v.is_empty() => return Err(AdminError::EmptyDatabaseUrl),
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => return Err(AdminError::MissingDatabaseUrl),
        Err(std::env::VarError::NotUnicode(_)) => return Err(AdminError::EmptyDatabaseUrl),
    };
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .map_err(AdminError::Connect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
