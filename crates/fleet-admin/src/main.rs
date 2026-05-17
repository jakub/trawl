// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fleet-admin` — operational CLI for the shared fleet keystore (ADR-0030).

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
        eprintln!("fleet-admin: {e}");
        process::exit(1);
    }
}

async fn run() -> Result<(), AdminError> {
    let cli = Cli::parse();

    // `generate-session-key` is the only subcommand that doesn't need the
    // database — and operationally must work BEFORE the DB exists, since it
    // produces the key the server is then deployed with. Short-circuit
    // before pool construction so a missing `DATABASE_URL` isn't an error
    // here.
    if matches!(cli.command, Command::GenerateSessionKey) {
        return commands::session_key::run();
    }

    let pool = connect_pool().await?;

    match cli.command {
        Command::GenerateSessionKey => unreachable!("handled above"),
        Command::Migrate => commands::migrate::run(&pool).await,
        Command::Keys { action } => dispatch_keys(KeyStore::from_pool(pool), action).await,
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
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or(AdminError::MissingDatabaseUrl)?;
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
