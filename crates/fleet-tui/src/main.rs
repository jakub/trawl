//! Terminal UI for fleet log search.

use clap::Parser;
use color_eyre::eyre::Result;

mod app;
mod config;
mod highlight;
mod input;
mod state;
mod ui;

/// fleet TUI — terminal interface for log search
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Server URL (default: `https://localhost:5514`)
    #[arg(short, long, env = "FLEET_URL")]
    url: Option<String>,

    /// API token file (default: ~/.config/fleet/token)
    #[arg(short = 'k', long, env = "FLEET_TOKEN_FILE")]
    token_file: Option<String>,

    /// Accept self-signed TLS certificates
    #[arg(long)]
    insecure: bool,

    /// Config file path (default: ~/.config/fleet/config.toml)
    #[arg(short, long)]
    config: Option<String>,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let args = Args::parse();

    // Load config from file (or defaults).
    let mut config = config::Config::load(args.config.as_deref())?;

    // Apply environment variable overrides.
    config.apply_env_overrides();

    // Apply CLI argument overrides.
    config.apply_overrides(args.url, args.token_file, args.insecure);

    // Run the TUI.
    app::run(&config)
}
