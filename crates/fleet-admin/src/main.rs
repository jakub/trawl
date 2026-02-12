use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand, ValueEnum};
use fleet_auth::store::KeyStore;

const DEFAULT_TLS_DIR: &str = "~/.fleet/tls";

mod commands;

/// fleet administration tool.
#[derive(Parser)]
#[command(name = "fleet-admin", version, about)]
struct Cli {
    /// Path to the auth database file.
    #[arg(long, env = "FLEET_AUTH_DB", default_value = "~/.fleet/auth.db")]
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
        /// Role for the key (admin, analyst, reader, ingest).
        #[arg(long)]
        role: CliRole,
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
    },
}

/// Wrapper for clap `ValueEnum` derive (fleet-auth's `Role` uses `FromStr`).
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum CliRole {
    Admin,
    Analyst,
    Reader,
    Ingest,
}

impl From<CliRole> for fleet_auth::Role {
    fn from(r: CliRole) -> Self {
        match r {
            CliRole::Admin => Self::Admin,
            CliRole::Analyst => Self::Analyst,
            CliRole::Reader => Self::Reader,
            CliRole::Ingest => Self::Ingest,
        }
    }
}

fn main() {
    let cli = Cli::parse();

    let db_path = resolve_db_path(&cli.db);

    // Ensure parent directory exists.
    if let Some(parent) = db_path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "fleet-admin: failed to create directory {}: {e}",
                    parent.display()
                );
                process::exit(1);
            }
        }
    }

    let store = match KeyStore::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fleet-admin: failed to open auth database: {e}");
            process::exit(1);
        }
    };

    let result = match cli.command {
        Command::Keys { action } => match action {
            KeysAction::Create {
                role,
                name,
                expires,
            } => commands::keys::create(&store, &name, role.into(), expires.as_deref()),
            KeysAction::List { all } => commands::keys::list(&store, all),
            KeysAction::Revoke { prefix } => commands::keys::revoke(&store, &prefix),
        },
        Command::Tls { action } => match action {
            TlsAction::Generate { output_dir, san } => {
                let dir = resolve_db_path(&output_dir);
                commands::tls::generate(&dir, &san)
            }
        },
    };

    if let Err(e) = result {
        eprintln!("fleet-admin: {e}");
        process::exit(1);
    }
}

/// Resolve the database path, expanding `~` to the home directory.
fn resolve_db_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Get the user's home directory.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
