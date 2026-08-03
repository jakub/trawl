// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};
use trawl_server::config::Config;
use trawl_server::state::AppState;
use trawl_server::telemetry::{self, WalHandle, WalLayer};
use trawl_server::transport::http;

/// trawld — the trawl daemon.
#[derive(Parser)]
#[command(name = "trawld", version, long_version = trawl_core::version::long_version(), about)]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, env = "TRAWL_CONFIG", default_value = "~/.trawl/trawld.toml")]
    config: String,

    /// Path to ndjson query debug log. Overrides config `server.query_log`.
    #[arg(long, env = "TRAWL_QUERY_LOG")]
    query_log: Option<std::path::PathBuf>,

    /// Disable the live monitor dashboard (use traditional log output).
    /// Useful for systemd, tmux logging, or non-interactive environments.
    #[arg(long)]
    no_monitor: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install crash-dump capture before any threads are spawned or the async
    // runtime is built: the minidump monitor is launched by re-execing this
    // binary, which is only fork-safe while the process is single-threaded. In
    // monitor mode this never returns. Held for the whole process lifetime.
    let _crashdump = trawl_crashdump::init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

#[allow(clippy::too_many_lines)] // lifecycle orchestration is cohesive
async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring crypto provider");

    let cli = Cli::parse();
    let config_path = resolve_path(&cli.config);
    let config = Config::from_file(&config_path)?;

    // Auto-detect TTY: monitor when interactive, log tail when piped.
    let monitor_active = std::io::IsTerminal::is_terminal(&std::io::stdout()) && !cli.no_monitor;

    let telemetry = init_tracing(&config, monitor_active)?;

    tracing::info!(event_type = "lifecycle", config = %config_path.display(), "configuration loaded");

    for warn in config.warnings() {
        tracing::warn!(event_type = "config_warning", "{warn}");
    }

    tracing::info!(
        event_type = "lifecycle",
        https_addr = %config.server.http_addr,
        data_path = %config.data.path,
        max_queries = config.server.max_concurrent_queries,
        version = trawl_core::version::PKG_VERSION,
        git_sha = trawl_core::version::GIT_SHA,
        git_date = trawl_core::version::GIT_DATE,
        rustc = trawl_core::version::RUSTC_VERSION,
        target = trawl_core::version::TARGET_TRIPLE,
        monitor = monitor_active,
        "starting trawld"
    );

    // Install the prometheus metrics recorder before building state.
    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install prometheus recorder");
    #[cfg(target_os = "linux")]
    metrics_process::Collector::default().describe();
    trawl_server::metrics::describe_metrics();

    // Spawn upkeep task to prevent histogram bucket memory bloat.
    let prom_handle = metrics_handle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            prom_handle.run_upkeep();
        }
    });

    // ADR-0009 storage-epoch gate: runs before any component touches the
    // data root. Refuses to start on the ambiguous branch.
    let epoch_outcome = trawl_server::epoch::ensure_current_epoch(
        &config.data.base_dir(),
        &config.wal_dir(),
        config.ingest.enabled,
    )?;
    tracing::info!(
        event_type = "epoch_gate",
        outcome = ?epoch_outcome,
        epoch = trawl_server::epoch::CURRENT_EPOCH,
        "storage epoch verified"
    );
    warn_unlisted_env_dirs(&config);

    let (mut state, http_config) = AppState::from_config(&config, metrics_handle).await?;

    // Open query debug log if configured (CLI flag overrides config).
    let query_log_path = cli.query_log.or(config.server.query_log.clone());
    if let Some(ref path) = query_log_path {
        let log = trawl_server::query_log::QueryLog::open(path)
            .map_err(|e| format!("failed to open query log {}: {e}", path.display()))?;
        tracing::info!(
            event_type = "lifecycle",
            path = %path.display(),
            "query debug log enabled"
        );
        state.query.query_log = Some(Arc::new(log));
    }

    // Activate internal telemetry by injecting the WAL writer.
    if let Some((handle, layer)) = &telemetry {
        if let Some(writer) = &state.ingest.wal_writer {
            handle.set(Arc::clone(writer), &config.ingest.default_env);
        }
        // Activate event bus for real-time telemetry fanout (SSE streaming).
        if let Some(bus) = &state.ingest.event_bus {
            layer.set_bus(Arc::clone(bus));
        }
        // Activate hot buffer for synchronous telemetry event insertion.
        if let Some(buf) = &state.query.hot_buffer {
            layer.set_hot_buffer(Arc::clone(buf));
        }
    }

    // Sole-writer guard: if the app-state advisory lock is ever lost (its
    // session died and postgres freed the lock), a second trawld could
    // acquire it and become a concurrent writer. Terminate immediately —
    // split-brain is a correctness emergency, so a hard exit that stops all
    // writes beats a graceful drain that keeps serving. The supervisor
    // restarts us; boot re-acquires the lock or fails on the replacement.
    {
        let mut lock_lost = state.storage.lock_lost();
        tokio::spawn(async move {
            if lock_lost.wait_for(|lost| *lost).await.is_ok() {
                tracing::error!(
                    event_type = "lifecycle",
                    "app-state sole-writer lock lost; terminating trawld to prevent a \
                     split-brain second writer"
                );
                std::process::exit(1);
            }
        });
    }

    // ADR-0009 boot conformance pass: make the write-time invariant true
    // over the standing corpus before anything reads or writes it. Only on
    // ingest-enabled nodes (a query-only node does not own the data root).
    // Fatal on failure, like the epoch gate — a data root not proven
    // conformant must not serve queries. Per-file failures are NOT that:
    // an unreadable or foreign parquet file is skipped and counted inside
    // the pass, so one bad file cannot keep the daemon down.
    if config.ingest.enabled {
        let summary = trawl_server::catalog::conform::ensure_conformance(
            &state.storage.catalog,
            &state.query.field_catalog,
            &config.data.base_dir(),
            &config.ingest.compaction_memory_limit,
        )
        .await?;
        tracing::info!(
            event_type = "catalog_conform",
            ran = summary.ran,
            scanned = summary.scanned,
            rewritten = summary.rewritten,
            skipped = summary.skipped,
            "boot conformance pass finished"
        );
    }

    let compaction_handle = spawn_ingest_pipeline(&config, &state)?;

    // Spawn syslog listeners if enabled (requires ingest to be enabled).
    let syslog_handle = if config.syslog.enabled && config.ingest.enabled {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handles = trawl_server::syslog::spawn_syslog(
            &config.syslog,
            config.ingest.default_env.as_str().into(),
            Arc::clone(state.ingest.pipeline.as_ref().expect("ingest enabled")),
            state.ingest.syslog_stats.clone(),
            shutdown_rx,
        );
        tracing::info!(
            event_type = "lifecycle",
            udp = config.syslog.udp_enabled,
            tcp = config.syslog.tcp_enabled,
            udp_addr = %config.syslog.udp_addr,
            tcp_addr = %config.syslog.tcp_addr,
            sources = config.syslog.source_service_map.len(),
            "syslog listener enabled"
        );
        Some((handles, shutdown_tx))
    } else {
        None
    };

    // Spawn retention task (always-on with defaults).
    let retention_handle = {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = trawl_server::retention::spawn_retention(
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
        let handle = trawl_server::audit::spawn_audit_task(
            state.auth.key_store.clone(),
            interval,
            shutdown_rx,
        );
        Some((handle, shutdown_tx))
    } else {
        None
    };

    // Spawn telemetry flush task.
    let flush_interval =
        std::time::Duration::from_secs(config.ingest.telemetry_flush_interval_secs);
    let telemetry_handle = telemetry.map(|(_, layer)| {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let join = telemetry::spawn_flush_task(layer, flush_interval, shutdown_rx);
        (join, shutdown_tx)
    });

    // Spawn periodic server stats emitter.
    let stats_interval = std::time::Duration::from_secs(config.ingest.stats_interval_secs);
    let stats_handle = {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = trawl_server::stats::spawn_stats_emitter(&state, stats_interval, shutdown_rx);
        (handle, shutdown_tx)
    };

    // Spawn scheduled query executor.
    let scheduler_handle = if config.scheduler.enabled {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = trawl_server::scheduler::spawn_scheduler(
            state.storage.schedule.clone(),
            state.auth.key_store.clone(),
            state.query.pool.clone(),
            config.scheduler.clone(),
            config.server.timeout_secs,
            shutdown_rx,
        );
        Some((handle, shutdown_tx))
    } else {
        None
    };

    let state_dir = config.state_dir();

    // Pre-compute per-service schema metadata in the background.
    let _schema_refresh = trawl_server::schema_refresh::spawn_schema_refresh(state.clone());

    // Always collect dashboard snapshots (1s interval) so the
    // /api/v1/dashboard endpoint works even under systemd / --no-monitor.
    let _snapshot_collector = trawl_server::monitor::spawn_snapshot_collector(
        state.clone(),
        config.server.http_addr.clone(),
        config.server.max_sse_connections,
        config.scheduler.enabled,
        config.syslog.enabled && config.ingest.enabled,
    );

    if monitor_active {
        // Monitor mode: spawn HTTP server in background, run TUI on main.
        let shutdown = Arc::new(tokio::sync::Notify::new());

        let http_state = state.clone();
        let http_shutdown = Arc::clone(&shutdown);
        let server_config = config.server.clone();
        let sd = state_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = http::serve(
                http_state,
                &http_config,
                &server_config,
                &sd,
                Some(http_shutdown),
            )
            .await
            {
                tracing::error!(event_type = "lifecycle", error = %e, "HTTP server error");
            }
        });

        // Run monitor on the main task — blocks until ctrl-c.
        if let Err(e) =
            trawl_server::monitor::run(state, config.server.monitor_refresh_ms, shutdown).await
        {
            // Terminal restore happens via TerminalGuard drop, so just log.
            tracing::error!(event_type = "lifecycle", error = %e, "monitor error");
        }
    } else {
        // Traditional mode: HTTP server runs on main, handles its own shutdown.
        http::serve(state, &http_config, &config.server, &state_dir, None).await?;
    }

    // Shutdown ordering: flush telemetry first so final events reach WAL,
    // then stats emitter, then scheduler (stop issuing new queries),
    // then syslog (flush final events to WAL before compaction),
    // then compaction (may compact final files and drain hot buffer),
    // then retention, then audit.
    shutdown_task(telemetry_handle, "telemetry").await;
    shutdown_task(Some(stats_handle), "stats_emitter").await;
    shutdown_task(scheduler_handle, "scheduler").await;
    if let Some((handles, shutdown_tx)) = syslog_handle {
        let _ = shutdown_tx.send(true);
        for handle in handles {
            if let Err(e) = handle.await {
                tracing::warn!(event_type = "task_panic", task = "syslog", error = %e, "syslog task panicked during shutdown");
            }
        }
    }
    if let Some((compaction_jh, compaction_tx)) = compaction_handle {
        shutdown_task(Some((compaction_jh, compaction_tx)), "compaction").await;
    }
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

/// Spawn the ingest pipeline tasks (compaction).
///
/// Returns handles for graceful shutdown, or `None` if ingest is disabled.
type IngestHandles = (JoinHandle<()>, watch::Sender<bool>);

fn spawn_ingest_pipeline(
    config: &Config,
    state: &AppState,
) -> Result<Option<IngestHandles>, Box<dyn std::error::Error>> {
    if !config.ingest.enabled {
        tracing::info!(event_type = "lifecycle", "ingest pipeline disabled");
        return Ok(None);
    }

    let wal_dir = config.wal_dir();

    // Ensure the WAL directory exists at startup.
    if let Some(writer) = &state.ingest.wal_writer {
        writer
            .ensure_dir()
            .map_err(|e| format!("failed to create WAL directory {}: {e}", wal_dir.display()))?;
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

    // Compaction is the only parquet writer: it pins every field's type in
    // the catalog BEFORE writing, and conforms every batch to the pins
    // (ADR-0009 slice 2).
    let catalog = trawl_server::catalog::CatalogContext {
        store: state.storage.catalog.clone(),
        cache: state.query.field_catalog.clone(),
    };

    let handle = trawl_server::ingest::compaction::spawn_compaction(
        wal_dir,
        data_dir,
        interval,
        config.ingest.daily_rollup,
        config.ingest.compaction_chunk_size,
        config.ingest.compaction_memory_limit.clone(),
        state.query.hot_buffer.clone(),
        state.ingest.compaction_stats.clone(),
        Some(catalog),
        shutdown_rx,
    );

    Ok(Some((handle, shutdown_tx)))
}

/// Initialize the tracing subscriber.
///
/// Warn for on-disk env directories not in the current allowlist: the
/// allowlist gates writes, not reads — removing an env stops new ingest
/// but its directories stay queryable and age out under retention
/// normally (ADR-0009).
fn warn_unlisted_env_dirs(config: &Config) {
    let allowed = config.ingest.effective_envs();
    let Ok(entries) = std::fs::read_dir(config.data.base_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if trawl_server::config::is_valid_env_name(name)
            && !trawl_server::config::RESERVED_ENV_NAMES.contains(&name)
            && !allowed.iter().any(|e| e == name)
        {
            tracing::warn!(
                event_type = "env_not_in_allowlist",
                env = %name,
                "on-disk env directory is not in ingest.envs — new ingest \
                 for it rejects, existing data stays queryable and ages out \
                 under retention"
            );
        }
    }
}

/// When internal telemetry is enabled, registers a [`WalLayer`] that
/// replaces the JSON file logger. Returns both the [`WalHandle`] (for
/// deferred writer injection) and a [`WalLayer`] clone (sharing the same
/// buffer) for the flush task.
///
/// When telemetry is disabled and `log_file` is configured, falls back
/// to the legacy JSON file layer.
///
/// When `monitor_active` is true, the stdout `fmt::layer()` is omitted
/// to avoid corrupting the TUI with interleaved log output.
fn init_tracing(
    config: &Config,
    monitor_active: bool,
) -> Result<Option<(WalHandle, WalLayer)>, Box<dyn std::error::Error>> {
    let make_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| "trawl_server=info".into());

    let use_telemetry = config.internal_telemetry_enabled();

    if use_telemetry {
        // WAL layer replaces the JSON file logger.
        let handle = WalHandle::new();
        let wal_layer = WalLayer::new(handle.clone(), &config.ingest.default_env);
        let flush_layer = wal_layer.clone(); // same Arc<WalLayerInner>

        if monitor_active {
            // Skip stdout layer — TUI owns the terminal.
            tracing_subscriber::registry()
                .with(wal_layer.with_filter(make_filter()))
                .init();
        } else {
            let stdout_layer = fmt::layer().with_filter(make_filter());
            tracing_subscriber::registry()
                .with(stdout_layer)
                .with(wal_layer.with_filter(make_filter()))
                .init();
        }
        Ok(Some((handle, flush_layer)))
    } else if let Some(log_path) = &config.server.log_file {
        // Legacy: JSON file logger.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let open_file = || -> Result<std::fs::File, Box<dyn std::error::Error>> {
            Ok(std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)?)
        };

        if monitor_active {
            let file_layer = fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(open_file()?)
                .with_filter(make_filter());
            tracing_subscriber::registry().with(file_layer).init();
        } else {
            let stdout_layer = fmt::layer().with_filter(make_filter());
            let file_layer = fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(open_file()?)
                .with_filter(make_filter());
            tracing_subscriber::registry()
                .with(stdout_layer)
                .with(file_layer)
                .init();
        }
        Ok(None)
    } else if monitor_active {
        // Monitor active, no telemetry, no file — still need a subscriber
        // but skip stdout to avoid TUI corruption.
        tracing_subscriber::registry().init();
        Ok(None)
    } else {
        let stdout_layer = fmt::layer().with_filter(make_filter());
        tracing_subscriber::registry().with(stdout_layer).init();
        Ok(None)
    }
}

/// Resolve a path, expanding `~` to the home directory.
fn resolve_path(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).as_ref())
}
