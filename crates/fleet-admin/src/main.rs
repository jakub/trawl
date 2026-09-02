// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fleet-admin` — operational CLI for the shared fleet keystore (ADR-0030).

use std::io::Write;
use std::process;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use fleet_auth::{KeyStore, PrincipalKind, RolePermission};
use sqlx::postgres::{PgPool, PgPoolOptions};

use fleet_admin::commands;
use fleet_admin::commands::keys::{KeyPrefix, RoleName};
use fleet_admin::error::AdminError;

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
    /// Manage data-defined roles (cross-app permission bundles, ADR-0006).
    Roles {
        #[command(subcommand)]
        action: RolesAction,
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
        /// Repeatable role name (e.g. `--role trawl-admin --role
        /// coastwatch-analyst`). Every role must already exist (`roles
        /// create`). May be empty — a role-less key authenticates but
        /// authorizes nothing until roles are assigned.
        #[arg(long = "role", value_name = "ROLE", value_parser = RoleName::parse)]
        roles: Vec<RoleName>,
        /// Expiration duration (e.g. `90d`, `24h`, `52w`). Omit for no expiry.
        #[arg(long, value_parser = commands::keys::parse_duration)]
        expires: Option<Duration>,
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
        #[arg(value_parser = KeyPrefix::parse)]
        prefix: KeyPrefix,
        /// Skip the interactive confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Assign a role to an existing key.
    AssignRole {
        /// The key prefix (shown in `keys list`).
        #[arg(value_parser = KeyPrefix::parse)]
        prefix: KeyPrefix,
        /// The role name to assign (must exist).
        #[arg(value_parser = RoleName::parse)]
        role: RoleName,
    },
    /// Remove a role from a key.
    UnassignRole {
        /// The key prefix (shown in `keys list`).
        #[arg(value_parser = KeyPrefix::parse)]
        prefix: KeyPrefix,
        /// The role name to remove.
        #[arg(value_parser = RoleName::parse)]
        role: RoleName,
        /// Skip the interactive confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Change a key's kind (human <-> service).
    Retype {
        /// The key prefix (shown in `keys list`).
        #[arg(value_parser = KeyPrefix::parse)]
        prefix: KeyPrefix,
        /// The new principal kind.
        kind: CliKind,
    },
}

#[derive(Subcommand)]
enum RolesAction {
    /// Create a new role.
    Create {
        /// Unique role name (lowercase alnum + underscore + hyphen).
        #[arg(long, value_parser = RoleName::parse)]
        name: RoleName,
        /// Repeatable `APP:PERMISSION` entry (e.g. `--perm trawl:query`).
        /// Unknown permissions warn but persist (warn-only registry).
        #[arg(long = "perm", value_name = "APP:PERMISSION", value_parser = commands::roles::parse_perm)]
        perms: Vec<RolePermission>,
        /// Per-key requests/minute ceiling for keys holding this role.
        /// Overrides the route-class config default (max across the key's
        /// roles wins). Omit to use the config defaults.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        rate_rpm: Option<u32>,
    },
    /// List all roles with their permission bundles.
    List,
    /// Show one role's details, including how many keys hold it.
    Show {
        /// The role name.
        #[arg(value_parser = RoleName::parse)]
        name: RoleName,
    },
    /// Add `APP:PERMISSION` entries to a role.
    AddPerm {
        /// The role name.
        #[arg(value_parser = RoleName::parse)]
        name: RoleName,
        /// One or more `APP:PERMISSION` entries.
        #[arg(required = true, value_name = "APP:PERMISSION", value_parser = commands::roles::parse_perm)]
        perms: Vec<RolePermission>,
    },
    /// Remove `APP:PERMISSION` entries from a role.
    RemovePerm {
        /// The role name.
        #[arg(value_parser = RoleName::parse)]
        name: RoleName,
        /// One or more `APP:PERMISSION` entries.
        #[arg(required = true, value_name = "APP:PERMISSION", value_parser = commands::roles::parse_perm)]
        perms: Vec<RolePermission>,
    },
    /// Change a role's rate ceiling in place (keys keep the role).
    SetRate {
        /// The role name.
        #[arg(value_parser = RoleName::parse)]
        name: RoleName,
        /// New per-key requests/minute ceiling for keys holding this role.
        #[arg(
            long,
            value_parser = clap::value_parser!(u32).range(1..),
            conflicts_with = "default",
            required_unless_present = "default"
        )]
        rate_rpm: Option<u32>,
        /// Clear the ceiling — keys fall back to the route-class defaults.
        #[arg(long)]
        default: bool,
    },
    /// Delete a role. Refuses while keys still hold it unless --force.
    Delete {
        /// The role name.
        #[arg(value_parser = RoleName::parse)]
        name: RoleName,
        /// Delete even if keys hold the role (they lose it immediately).
        #[arg(long)]
        force: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
}

/// CLI-side mirror of [`PrincipalKind`] — deriving `ValueEnum` on the
/// original would pull `clap` into `fleet-auth`, which does not depend on it.
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
        Command::Roles { action } => {
            let pool = connect_pool().await?;
            dispatch_roles(KeyStore::from_pool(pool), action).await
        }
    }
}

async fn dispatch_keys(store: KeyStore, action: KeysAction) -> Result<(), AdminError> {
    match action {
        KeysAction::Create {
            name,
            kind,
            roles,
            expires,
        } => {
            let role_names: Vec<String> = roles.iter().map(|r| r.as_str().to_owned()).collect();
            commands::keys::create(&store, &name, kind.into(), &role_names, expires).await
        }
        KeysAction::List { all } => commands::keys::list(&store, all).await,
        KeysAction::Revoke { prefix, yes } => commands::keys::revoke(&store, &prefix, yes).await,
        KeysAction::AssignRole { prefix, role } => {
            commands::keys::assign_role(&store, &prefix, &role).await
        }
        KeysAction::UnassignRole { prefix, role, yes } => {
            commands::keys::unassign_role(&store, &prefix, &role, yes).await
        }
        KeysAction::Retype { prefix, kind } => {
            commands::keys::retype(&store, &prefix, kind.into()).await
        }
    }
}

async fn dispatch_roles(store: KeyStore, action: RolesAction) -> Result<(), AdminError> {
    match action {
        RolesAction::Create {
            name,
            perms,
            rate_rpm,
        } => commands::roles::create(&store, &name, &perms, rate_rpm).await,
        RolesAction::List => commands::roles::list(&store).await,
        RolesAction::Show { name } => commands::roles::show(&store, &name).await,
        RolesAction::AddPerm { name, perms } => {
            commands::roles::add_perm(&store, &name, &perms).await
        }
        RolesAction::RemovePerm { name, perms } => {
            commands::roles::remove_perm(&store, &name, &perms).await
        }
        // `--default` only exists to make "clear the ceiling" explicit at
        // the CLI boundary; clap's conflict/requirement rules already
        // collapse it into `rate_rpm == None`.
        RolesAction::SetRate {
            name,
            rate_rpm,
            default: _,
        } => commands::roles::set_rate(&store, &name, rate_rpm).await,
        RolesAction::Delete { name, force, yes } => {
            commands::roles::delete(&store, &name, force, yes).await
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
    // connect_pool is the named production pool owner for the fleet-admin
    // CLI (ADR-0021 ruling 3).
    #[allow(clippy::disallowed_methods)]
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

    #[test]
    fn set_rate_takes_exactly_one_of_rate_rpm_or_default() {
        let parse = |args: &[&str]| Cli::try_parse_from(args);

        // Ambiguity is a parse error in both directions — an operator never
        // gets to guess whether a ceiling was set or cleared.
        assert!(parse(&["fleet-admin", "roles", "set-rate", "tier"]).is_err());
        assert!(
            parse(&[
                "fleet-admin",
                "roles",
                "set-rate",
                "tier",
                "--rate-rpm",
                "60",
                "--default",
            ])
            .is_err()
        );

        // `--default` is how "clear the ceiling" reaches the store as None.
        let cli = parse(&["fleet-admin", "roles", "set-rate", "tier", "--default"]).unwrap();
        let Command::Roles {
            action: RolesAction::SetRate { rate_rpm, .. },
        } = cli.command
        else {
            panic!("expected roles set-rate");
        };
        assert_eq!(rate_rpm, None);
    }
}
