use std::path::PathBuf;

use clap::Parser;
use fleet_server::config::Config;
use fleet_server::state::AppState;
use fleet_server::transport::http;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

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
    let cli = Cli::parse();
    let config_path = resolve_path(&cli.config);
    let config = Config::from_file(&config_path)?;

    init_tracing(&config)?;

    tracing::info!(config = %config_path.display(), "configuration loaded");

    for warn in config.warnings() {
        tracing::warn!("{warn}");
    }

    tracing::info!(
        https_addr = %config.server.http_addr,
        data_path = %config.data.path,
        max_queries = config.server.max_concurrent_queries,
        "starting fleetd"
    );

    let (state, http_config) = AppState::from_config(&config)?;

    // Spawn ingest compaction task if ingestion is enabled.
    let compaction_handle = if config.ingest.enabled {
        let wal_dir = config.wal_dir();

        // Ensure the WAL directory exists at startup.
        if let Some(writer) = &state.ingest.wal_writer {
            writer.ensure_dir().map_err(|e| {
                format!("failed to create WAL directory {}: {e}", wal_dir.display())
            })?;
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let data_dir = config.data.base_dir();
        let interval = std::time::Duration::from_secs(config.ingest.compaction_interval_secs);

        tracing::info!(
            wal_dir = %wal_dir.display(),
            data_dir = %data_dir.display(),
            interval_secs = config.ingest.compaction_interval_secs,
            "ingest pipeline enabled"
        );

        let handle = fleet_server::ingest::compaction::spawn_compaction(
            wal_dir,
            data_dir,
            interval,
            shutdown_rx,
        );
        Some((handle, shutdown_tx))
    } else {
        tracing::info!("ingest pipeline disabled");
        None
    };

    http::serve(state, &http_config, &config.server).await?;

    // Signal compaction task to shut down.
    if let Some((_handle, shutdown_tx)) = compaction_handle {
        let _ = shutdown_tx.send(true);
    }

    Ok(())
}

/// Initialize the tracing subscriber with stdout and an optional log file.
fn init_tracing(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let make_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| "fleet_server=info".into());

    let stdout_layer = fmt::layer().with_filter(make_filter());

    if let Some(log_path) = &config.server.log_file {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        // JSON format for file logs: machine-parseable and inherently
        // escapes control characters (prevents log injection).
        let file_layer = fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(file)
            .with_filter(make_filter());
        tracing_subscriber::registry()
            .with(stdout_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry().with(stdout_layer).init();
    }

    Ok(())
}

/// Resolve a path, expanding `~` to the home directory.
fn resolve_path(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).as_ref())
}
