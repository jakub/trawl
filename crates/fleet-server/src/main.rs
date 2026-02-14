use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use clap::Parser;
use fleet_server::config::Config;
use fleet_server::state::AppState;
use fleet_server::telemetry::{self, WalHandle, WalLayer};
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

    let telemetry = init_tracing(&config)?;

    tracing::info!(event_type = "lifecycle", config = %config_path.display(), "configuration loaded");

    for warn in config.warnings() {
        tracing::warn!(event_type = "config_warning", "{warn}");
    }

    tracing::info!(
        event_type = "lifecycle",
        https_addr = %config.server.http_addr,
        data_path = %config.data.path,
        max_queries = config.server.max_concurrent_queries,
        "starting fleetd"
    );

    let (state, http_config) = AppState::from_config(&config)?;

    // Activate internal telemetry by injecting the WAL writer.
    if let Some((handle, _)) = &telemetry {
        if let Some(writer) = &state.ingest.wal_writer {
            handle.set(Arc::clone(writer));
        }
    }

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
            event_type = "lifecycle",
            wal_dir = %wal_dir.display(),
            data_dir = %data_dir.display(),
            interval_secs = config.ingest.compaction_interval_secs,
            "ingest pipeline enabled"
        );

        let handle = fleet_server::ingest::compaction::spawn_compaction(
            wal_dir,
            data_dir,
            interval,
            config.ingest.daily_rollup,
            shutdown_rx,
        );
        Some((handle, shutdown_tx))
    } else {
        tracing::info!(event_type = "lifecycle", "ingest pipeline disabled");
        None
    };

    // Spawn retention task (always-on with defaults).
    let retention_handle = {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = fleet_server::retention::spawn_retention(
            config.data.base_dir(),
            config.retention.clone(),
            shutdown_rx,
        );
        (handle, shutdown_tx)
    };

    // Spawn key audit polling task if enabled.
    let audit_handle = if config.auth.audit_interval_secs > 0 {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let interval = std::time::Duration::from_secs(config.auth.audit_interval_secs);
        let handle = fleet_server::audit::spawn_audit_task(
            Arc::clone(&state.auth.key_store),
            interval,
            shutdown_rx,
        );
        Some((handle, shutdown_tx))
    } else {
        None
    };

    // Spawn telemetry flush task (1-second interval).
    let telemetry_handle = telemetry.map(|(_, layer)| {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let join =
            telemetry::spawn_flush_task(layer, std::time::Duration::from_secs(1), shutdown_rx);
        (join, shutdown_tx)
    });

    http::serve(state, &http_config, &config.server).await?;

    // Shutdown ordering: flush telemetry first so final events reach WAL,
    // then compaction (may compact those final files), then retention, then audit.
    shutdown_task(telemetry_handle, "telemetry").await;
    shutdown_task(compaction_handle, "compaction").await;
    shutdown_task(Some(retention_handle), "retention").await;
    shutdown_task(audit_handle, "audit").await;

    Ok(())
}

/// Signal a background task to shut down and await its completion.
async fn shutdown_task(task: Option<(JoinHandle<()>, watch::Sender<bool>)>, name: &str) {
    if let Some((handle, shutdown_tx)) = task {
        let _ = shutdown_tx.send(true);
        if let Err(e) = handle.await {
            tracing::warn!(event_type = "task_panic", task = name, error = %e, "task panicked during shutdown");
        }
    }
}

/// Initialize the tracing subscriber.
///
/// When internal telemetry is enabled, registers a [`WalLayer`] that
/// replaces the JSON file logger. Returns both the [`WalHandle`] (for
/// deferred writer injection) and a [`WalLayer`] clone (sharing the same
/// buffer) for the flush task.
///
/// When telemetry is disabled and `log_file` is configured, falls back
/// to the legacy JSON file layer.
fn init_tracing(
    config: &Config,
) -> Result<Option<(WalHandle, WalLayer)>, Box<dyn std::error::Error>> {
    let make_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| "fleet_server=info".into());

    let stdout_layer = fmt::layer().with_filter(make_filter());
    let use_telemetry = config.internal_telemetry_enabled();

    if use_telemetry {
        // WAL layer replaces the JSON file logger.
        let handle = WalHandle::new();
        let wal_layer = WalLayer::new(handle.clone());
        let flush_layer = wal_layer.clone(); // same Arc<WalLayerInner>
        tracing_subscriber::registry()
            .with(stdout_layer)
            .with(wal_layer.with_filter(make_filter()))
            .init();
        Ok(Some((handle, flush_layer)))
    } else if let Some(log_path) = &config.server.log_file {
        // Legacy: JSON file logger.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let file_layer = fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(file)
            .with_filter(make_filter());
        tracing_subscriber::registry()
            .with(stdout_layer)
            .with(file_layer)
            .init();
        Ok(None)
    } else {
        tracing_subscriber::registry().with(stdout_layer).init();
        Ok(None)
    }
}

/// Resolve a path, expanding `~` to the home directory.
fn resolve_path(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).as_ref())
}
