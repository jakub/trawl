use std::io;
use std::path::PathBuf;
use std::process;
use std::sync::Mutex;

use clap::{Parser, Subcommand};

mod cli;
mod config;
mod tui;

/// fleet — search your logs with a pipeline DSL.
///
/// Run with no subcommand to launch the interactive TUI.
#[derive(Parser)]
#[command(name = "fleet", version, about)]
struct Cli {
    /// Server URL (default: `https://localhost:5514`).
    #[arg(long, env = "FLEET_URL", global = true)]
    url: Option<String>,

    /// API token (direct value).
    #[arg(long, env = "FLEET_TOKEN", global = true)]
    token: Option<String>,

    /// Path to API token file.
    #[arg(long, short = 'k', env = "FLEET_TOKEN_FILE", global = true)]
    token_file: Option<String>,

    /// Accept self-signed TLS certificates.
    #[arg(long, env = "FLEET_INSECURE", global = true)]
    insecure: bool,

    /// Config file path (default: ~/.config/fleet/config.toml).
    #[arg(long, short = 'c', global = true)]
    config: Option<String>,

    /// Enable driver mode: listen on a unix socket for programmatic control.
    /// Uses `~/.config/fleet/driver.sock` by default, or specify a custom path.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    driver: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Execute a DSL query and print results.
    Query {
        /// The fleet DSL query.
        query: String,

        /// Parquet glob path for embedded mode (e.g. "/data/**/*.parquet").
        #[arg(long)]
        data: Option<String>,

        /// Output format (auto-detected if omitted: table for TTY, json for pipes).
        #[arg(long, short, value_enum)]
        format: Option<cli::OutputFormat>,
    },

    /// Validate DSL query syntax without executing.
    Validate {
        /// The fleet DSL query to validate.
        query: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Engine(#[from] fleet_engine::error::EngineError),
    #[error("{0}")]
    Client(#[from] fleet_client::ClientError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Config(#[from] config::ConfigError),
    #[error("{0}")]
    Usage(String),
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();

    if let Err(e) = run(args).await {
        // Broken pipe is expected (e.g. `fleet query ... | head`), exit quietly.
        if let CliError::Io(ref io_err) = e {
            if io_err.kind() == io::ErrorKind::BrokenPipe {
                process::exit(1);
            }
        }
        eprintln!("fleet: {e}");
        process::exit(1);
    }
}

async fn run(args: Cli) -> Result<(), CliError> {
    // Load config file and apply overrides.
    let mut cfg = config::Config::load(args.config.as_deref())?;
    cfg.apply_env_overrides();
    cfg.apply_overrides(args.url, args.token_file, args.insecure);

    match args.command {
        None => {
            // TUI mode — tracing goes to a log file, not stderr (which corrupts the UI).
            let log_dir = shellexpand::tilde("~/.config/fleet");
            std::fs::create_dir_all(log_dir.as_ref())?;
            let log_file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(format!("{log_dir}/tui.log"))?;

            tracing_subscriber::fmt()
                .with_writer(Mutex::new(log_file))
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_ansi(false)
                .init();

            // Resolve driver socket path.
            let driver_path: Option<PathBuf> = args.driver.map(|v| {
                if v.is_empty() {
                    tui::driver::default_socket_path()
                } else {
                    PathBuf::from(v)
                }
            });

            tui::run(&cfg, args.token.as_deref(), driver_path.as_deref()).await?;
        }
        Some(Command::Query {
            query,
            data,
            format,
        }) => {
            // For embedded mode (--data), no server connection needed.
            let conn = if data.is_some() {
                None
            } else {
                let token = cfg.load_token(args.token.as_deref())?;
                Some(cli::ConnectionParams {
                    url: cfg.server.url.clone(),
                    token,
                    insecure: cfg.server.insecure,
                })
            };

            cli::run_query(&query, data.as_deref(), format, conn, &cfg.ui.timezone).await?;
        }
        Some(Command::Validate { query }) => {
            // Validate supports both daemon and local-only mode.
            let conn = if let Ok(token) = cfg.load_token(args.token.as_deref()) {
                Some(cli::ConnectionParams {
                    url: cfg.server.url.clone(),
                    token,
                    insecure: cfg.server.insecure,
                })
            } else {
                None
            };

            cli::run_validate(&query, conn).await?;
        }
    }

    Ok(())
}
