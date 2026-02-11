use std::path::PathBuf;

use clap::Parser;
use fleet_server::config::Config;
use fleet_server::state::AppState;
use fleet_server::transport::http;

/// fleetd — the fleet daemon.
#[derive(Parser)]
#[command(name = "fleetd", version, about)]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, env = "FLEET_CONFIG", default_value = "~/.fleet/fleetd.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fleet_server=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config_path = resolve_path(&cli.config);

    tracing::info!(config = %config_path.display(), "loading configuration");
    let config = Config::from_file(&config_path)?;

    tracing::info!(
        http_addr = %config.server.http_addr,
        data_path = %config.data.path,
        max_queries = config.server.max_concurrent_queries,
        "starting fleetd"
    );

    let state = AppState::from_config(&config);
    http::serve(state, &config.server.http_addr).await?;

    Ok(())
}

/// Resolve a path, expanding `~` to the home directory.
fn resolve_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}
