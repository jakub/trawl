// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-admin — local operational tooling for trawl.
//!
//! Key management moved to `fleet-admin` with the fleet-auth postgres
//! keystore cutover (ADR-0004 slice 1); this tool keeps the purely local
//! concerns (TLS certificate generation).

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

const DEFAULT_TLS_DIR: &str = "~/.trawl/tls";

mod commands;

/// trawl administration tool.
#[derive(Debug, Parser)]
#[command(name = "trawl-admin", version, long_version = trawl_core::version::long_version(), about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage TLS certificates. (API key management lives in `fleet-admin`.)
    Tls {
        #[command(subcommand)]
        action: TlsAction,
    },
}

#[derive(Debug, Subcommand)]
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

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
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
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn cli_parses() {
        Cli::command().debug_assert();
    }

    #[test]
    fn keys_subcommand_is_gone() {
        // Key management moved to fleet-admin (ADR-0004). A `keys`
        // subcommand reappearing here would resurrect the sqlite keystore
        // path.
        let err = Cli::try_parse_from(["trawl-admin", "keys", "list"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn tls_generate_still_parses() {
        let cli = Cli::try_parse_from(["trawl-admin", "tls", "generate", "--san", "example.com"])
            .unwrap();
        let Command::Tls {
            action: TlsAction::Generate { san, .. },
        } = cli.command;
        assert_eq!(san, vec!["example.com".to_owned()]);
    }

    #[test]
    fn db_flag_is_gone() {
        // --db / TRAWL_AUTH_DB pointed at the retired sqlite keystore.
        let err = Cli::try_parse_from(["trawl-admin", "--db", "/tmp/x.db", "tls", "generate"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
