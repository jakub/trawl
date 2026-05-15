// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand, ValueEnum};
use trawl_auth::assignments::{PrincipalKind, RoleAssignment};
use trawl_auth::store::KeyStore;

const DEFAULT_TLS_DIR: &str = "~/.trawl/tls";

mod commands;

/// trawl administration tool.
#[derive(Parser)]
#[command(name = "trawl-admin", version, long_version = trawl_core::version::long_version(), about)]
struct Cli {
    /// Path to the auth database file.
    #[arg(long, env = "TRAWL_AUTH_DB", default_value = "~/.trawl/auth.db")]
    db: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage API keys.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Manage TLS certificates.
    Tls {
        #[command(subcommand)]
        action: TlsAction,
    },
}

#[derive(Subcommand)]
enum TlsAction {
    /// Generate a new self-signed TLS certificate.
    Generate {
        /// Output directory for cert.pem and key.pem.
        #[arg(long, default_value = DEFAULT_TLS_DIR)]
        output_dir: String,
        /// Additional Subject Alternative Names (hostnames or IPs).
        #[arg(long)]
        san: Vec<String>,
    },
}

#[derive(Subcommand)]
enum KeysAction {
    /// Create a new API key.
    Create {
        /// Principal kind: `human` for interactive principals, `service` for daemons/bots.
        #[arg(long)]
        kind: CliKind,
        /// Repeatable `app:role` grant (e.g. `--grant trawl:admin --grant coastwatch:siem_consumer`).
        /// May be empty — a grantless key authenticates but authorizes nothing until grants are added.
        #[arg(long = "grant", value_name = "APP:ROLE")]
        grants: Vec<String>,
        /// Human-readable name for the key.
        #[arg(long)]
        name: String,
        /// Expiration duration (e.g. "90d", "24h", "52w"). Omit for no expiry.
        #[arg(long)]
        expires: Option<String>,
    },
    /// List API keys (active only by default).
    List {
        /// Show all keys including revoked.
        #[arg(long)]
        all: bool,
    },
    /// Revoke an API key by its prefix.
    Revoke {
        /// The key prefix (shown in `keys list`).
        prefix: String,
        /// Skip confirmation prompt.
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
    /// Remove an `app:role` grant from an existing key.
    RevokeGrant {
        /// The key prefix (shown in `keys list`).
        prefix: String,
        /// The `app:role` (or `app`) grant to remove.
        grant: String,
    },
    /// Change a key's principal kind.
    Retype {
        /// The key prefix (shown in `keys list`).
        prefix: String,
        /// The new kind.
        kind: CliKind,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
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

/// Parse a `--grant app:role` value into a structured [`RoleAssignment`].
fn parse_grant(s: &str) -> Result<RoleAssignment, String> {
    let (app, role) = s
        .split_once(':')
        .ok_or_else(|| format!("invalid --grant {s:?}: expected APP:ROLE"))?;
    if app.is_empty() || role.is_empty() {
        return Err(format!("invalid --grant {s:?}: empty app or role"));
    }
    Ok(RoleAssignment {
        app: app.to_owned(),
        role: role.to_owned(),
    })
}

fn main() {
    let cli = Cli::parse();

    let db_path = PathBuf::from(shellexpand::tilde(&cli.db).as_ref());

    // Ensure parent directory exists.
    if let Some(parent) = db_path.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "trawl-admin: failed to create directory {}: {e}",
            parent.display()
        );
        process::exit(1);
    }

    let mut store = match KeyStore::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("trawl-admin: failed to open auth database: {e}");
            process::exit(1);
        }
    };

    let result = match cli.command {
        Command::Keys { action } => match action {
            KeysAction::Create {
                kind,
                grants,
                name,
                expires,
            } => {
                let parsed_grants: Result<Vec<RoleAssignment>, String> =
                    grants.iter().map(|g| parse_grant(g)).collect();
                match parsed_grants {
                    Ok(g) => commands::keys::create(
                        &mut store,
                        &name,
                        kind.into(),
                        &g,
                        expires.as_deref(),
                    ),
                    Err(e) => Err(e),
                }
            }
            KeysAction::List { all } => commands::keys::list(&store, all),
            KeysAction::Revoke { prefix, yes } => commands::keys::revoke(&store, &prefix, yes),
            KeysAction::Grant { prefix, grant } => match parse_grant(&grant) {
                Ok(g) => commands::keys::grant(&store, &prefix, &g),
                Err(e) => Err(e),
            },
            KeysAction::RevokeGrant { prefix, grant } => {
                // Accept either `app:role` (role half is ignored — the app is enough
                // to identify the row) or bare `app`.
                let app = grant.split_once(':').map_or(grant.as_str(), |(a, _)| a);
                commands::keys::revoke_grant(&store, &prefix, app)
            }
            KeysAction::Retype { prefix, kind } => {
                commands::keys::retype(&store, &prefix, kind.into())
            }
        },
        Command::Tls { action } => match action {
            TlsAction::Generate { output_dir, san } => {
                let dir = PathBuf::from(shellexpand::tilde(&output_dir).as_ref());
                commands::tls::generate(&dir, &san)
            }
        },
    };

    if let Err(e) = result {
        eprintln!("trawl-admin: {e}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grant_valid() {
        let g = parse_grant("trawl:admin").unwrap();
        assert_eq!(g.app, "trawl");
        assert_eq!(g.role, "admin");
    }

    #[test]
    fn parse_grant_missing_colon() {
        assert!(parse_grant("trawladmin").is_err());
    }

    #[test]
    fn parse_grant_empty_app() {
        assert!(parse_grant(":admin").is_err());
    }

    #[test]
    fn parse_grant_empty_role() {
        assert!(parse_grant("trawl:").is_err());
    }

    #[test]
    fn parse_grant_multiple_colons_keeps_role_intact() {
        let g = parse_grant("trawl:super:admin").unwrap();
        assert_eq!(g.app, "trawl");
        assert_eq!(g.role, "super:admin");
    }
}
