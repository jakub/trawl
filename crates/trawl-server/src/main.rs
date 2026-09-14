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
    /// Path to the configuration file (default: ~/.trawl/trawld.toml).
    #[arg(long, env = "TRAWL_CONFIG")]
    config: Option<String>,

    /// Validate an explicitly selected config and exit without starting services.
    #[arg(long, requires = "config")]
    check_config: bool,

    /// Path to ndjson query debug log. Overrides config `server.query_log`.
    #[arg(long, env = "TRAWL_QUERY_LOG")]
    query_log: Option<std::path::PathBuf>,

    /// Disable the live monitor dashboard (use traditional log output).
    /// Useful for systemd, tmux logging, or non-interactive environments.
    #[arg(long)]
    no_monitor: bool,
}

/// Wall-clock cap on how long process exit waits for blocking-pool work
/// that has already started — chiefly the telemetry/WAL durability
/// barriers (fsync, dir-fsync). Dropping a Tokio runtime normally waits on
/// those forever, so a frozen volume would stall restarts and rolling
/// deployments indefinitely; past this budget the runtime is abandoned and
/// the process exits with the wedged thread still parked in the kernel.
const RUNTIME_SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Inspect only arguments before choosing the seal. The monitor re-exec
    // carries no arguments, so it always reaches normal crash-dump init.
    // Both paths seal before config reads or threads. Check mode must not
    // start the monitor or create its directory, even when capture is enabled.
    if std::env::args_os().any(|arg| arg == "--check-config") {
        if trawl_crashdump::seal_for_config_check().is_err() {
            eprintln!("[trawld] configuration check refused: capability seal failed");
            std::process::exit(1);
        }
        let cli = Cli::parse();
        let path = resolve_path(cli.config.as_deref().expect("clap requires config"));
        if let Err(error) = check_config(&path) {
            eprintln!("[trawld] {error}");
            std::process::exit(1);
        }
        println!("Configuration is valid: {}", path.display());
        return Ok(());
    }
    // Install crash-dump capture before any threads are spawned or the async
    // runtime is built: the minidump monitor is launched by re-execing this
    // binary, which is only fork-safe while the process is single-threaded. In
    // monitor mode this never returns. The report owns the handler guard, so it
    // lives in this frame for the whole run; `status` is the copy of the
    // verdict `async_main` logs once a subscriber exists (ADR-0023 ruling 6).
    //
    // The seal is the one verdict decided here rather than there. A capability
    // set belongs to a thread, and a new thread starts from the set of the
    // thread that spawned it, so answering a failed seal after the runtime is
    // built means every tokio worker already carries the `CAP_SYS_PTRACE` the
    // seal was supposed to drop. Everything on the way there runs holding it
    // too, including reading a config file that turns out to be a FIFO nobody
    // writes to, which parks the process with the capability live and no bound
    // on how long. So the check runs first, before the runtime and before there
    // is a second thread to inherit anything, and it prints to stderr because
    // no subscriber exists yet. Every other verdict is advisory and reaches the
    // log in `async_main`.
    let crash_dump = trawl_crashdump::init();
    let status = crash_dump.status();
    if matches!(
        status,
        trawl_crashdump::Status::Failed(trawl_crashdump::FailureReason::Seal)
    ) {
        // Boot-fatal (ADR-0023 ruling 4). `init()` is supposed to hand back a
        // process with `CAP_SYS_PTRACE` gone from both its effective and its
        // permitted set and `no_new_privs` on. A seal that did not take leaves
        // the capability live and leaves the path back to the file capability
        // through a re-exec open. That is a privilege boundary that failed to
        // establish, not a degraded feature to serve past. The line names no
        // OS message, matching the crate's own content-free reason.
        eprintln!(
            "[trawld] crash-dump seal failed: refusing to start with an unsealed \
             capability set (ADR-0023 ruling 4)"
        );
        // Explicit, and before the exit: dropping the report uninstalls the
        // handler and lets the monitor exit.
        drop(crash_dump);
        // Exit 1 rather than returning an error. `Termination` for `Result`
        // prints the error itself, as `Error: ...`, which would put a second
        // diagnostic under the one above for the same failure.
        std::process::exit(1);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async_main(status));
    // Bounded exit: `async_main` has already run the graceful shutdown
    // sequence (each task under its own budget), so anything still running
    // here is a wedged blocking operation, not pending work worth waiting on.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_BUDGET);
    // The handler has to outlive shutdown. A fatal signal raised while the
    // runtime drains is exactly the crash worth a dump, and dropping the report
    // uninstalls the handler and lets the monitor exit. So the drop is explicit
    // and last, rather than an end-of-scope accident a later edit could move.
    drop(crash_dump);
    result
}

/// Validate local configuration only. Database URLs are required, but no
/// network connectivity, credentials, or stored data contents are inspected.
/// File-log validation reads path metadata to detect marker aliases.
fn check_config(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_file(path)?;
    validate_file_log_config(&config)?;
    config.auth.resolve_database_url()?;
    config.storage.resolve_database_url()?;
    trawl_server::ingest::producer::Derivation::resolve(&config.ingest)
        .map_err(|_| "invalid setting at ingest: check severity_from and time_from")?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // lifecycle orchestration is cohesive
async fn async_main(crash_dump: trawl_crashdump::Status) -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring crypto provider");

    let cli = Cli::parse();
    let config_path = resolve_path(cli.config.as_deref().unwrap_or("~/.trawl/trawld.toml"));
    // Pre-tracing boundary: no subscriber exists yet, so a config failure
    // here can only surface through stderr. Name the resolved path so the
    // operator can tell which file failed.
    let config = Config::from_file(&config_path).map_err(|e| {
        eprintln!(
            "[trawld] failed to load configuration from {}: {e}",
            config_path.display()
        );
        e
    })?;

    // Reject file-log marker collisions before either backend or filesystem
    // initialization can change state. Telemetry-only configurations ignore it.
    validate_file_log_config(&config)?;

    // Auto-detect TTY: monitor when interactive, log tail when piped.
    let monitor_active = std::io::IsTerminal::is_terminal(&std::io::stdout()) && !cli.no_monitor;

    // Resolve the log filter explicitly: RUST_LOG is authoritative when
    // valid; unset or invalid installs DEFAULT_LOG_FILTER, and the invalid
    // case warns after the subscriber is up (visible because `trawld=info`
    // is part of the default).
    let log_filter = telemetry::resolve_log_filter(std::env::var("RUST_LOG").ok().as_deref());

    // The derivation policy is resolved before the subscriber, because the
    // telemetry layer derives through it too (ADR-0013): trawld's own
    // `level` rides the configured `severity_from` chain like any
    // sender's. That puts it on the same pre-tracing boundary as the
    // config load: boot-fatal, and the diagnostic can only reach stderr.
    // Resolved independently of `ingest.enabled`, for the same reason.
    let derivation = Arc::new(
        trawl_server::ingest::producer::Derivation::resolve(&config.ingest).map_err(|e| {
            eprintln!("[trawld] {e} — refusing to start");
            e
        })?,
    );

    let Tracing {
        telemetry,
        file_log,
    } = init_tracing(
        &config,
        monitor_active,
        &log_filter.directives,
        Arc::clone(&derivation),
    );
    if let Some(warning) = &log_filter.warning {
        tracing::warn!(event_type = "config_warning", "{warning}");
    }

    log_crash_dump(&crash_dump);

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
    // Publish the salvage profiles' rejection matrix at zero. "Telemetry
    // is rejection-free by construction" is evidenced by an absent
    // increment on a present series; an absent series would leave a scrape
    // unable to tell "never happened" from "never wired up".
    trawl_server::ingest::producer::init_profile_reject_metrics();

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

    // Admit both databases before the epoch gate can initialize a fresh
    // root or repin recovery can rename/sweep an existing corpus. This exact
    // storage owner holds the sole-writer lock through recovery and serving.
    let (auth, storage) = AppState::connect_backends(&config).await?;

    // Sole-writer guard: if the app-state advisory lock is ever lost (its
    // session died and postgres freed the lock), a second trawld could
    // acquire it and become a concurrent writer. Terminate immediately —
    // split-brain is a correctness emergency, so a hard exit that stops all
    // writes beats a graceful drain that keeps serving. The supervisor
    // restarts us; boot re-acquires the lock or fails on the replacement.
    {
        let mut lock_lost = storage.lock_lost();
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

    let (epoch_outcome, recovered_repin) = prepare_data_root(
        &config.data.base_dir(),
        &config.wal_dir(),
        config.ingest.enabled,
    )?;
    if let Some(file_log) = file_log {
        file_log.open()?;
    }
    tracing::info!(
        event_type = "epoch_gate",
        outcome = ?epoch_outcome,
        epoch = trawl_server::epoch::CURRENT_EPOCH,
        "storage epoch verified"
    );
    // One listing of the data root feeds both warnings.
    if let Some(env_dirs) = on_disk_env_dirs(&config.data.base_dir()) {
        warn_unlisted_env_dirs(&config, &env_dirs);
        warn_retention_envs_without_dir(&config, &env_dirs);
    }

    // Finish the recovered job and catalog pin before constructing a live
    // cache or any corpus reader. Recovery's cache argument is temporary:
    // no reader exists yet, and from_parts hydrates its real cache from the
    // reconciled PostgreSQL catalog. No reference to this cache escapes.
    trawl_server::repin::recover::reconcile_store(
        &storage,
        &trawl_server::catalog::FieldCatalog::new(),
        &config.data.base_dir(),
        recovered_repin,
    )
    .await?;

    let (mut state, http_config) =
        AppState::from_parts(&config, metrics_handle, derivation, auth, storage).await?;

    // Open query debug log if configured (CLI flag overrides config).
    let query_log_path = cli.query_log.or(config.server.query_log.clone());
    if let Some(ref path) = query_log_path {
        let log =
            trawl_server::query_log::QueryLog::open(path, config.server.query_log_max_bytes as u64)
                .map_err(|e| format!("failed to open query log {}: {e}", path.display()))?;
        tracing::warn!(
            event_type = "query_log_enabled",
            path = %path.display(),
            max_bytes = config.server.query_log_max_bytes,
            "query debug log enabled — this file records raw query text, \
             SQL parameter values, and result samples; it is owner-only \
             (0600) and rolls over to a single retained .1 sibling at \
             query_log_max_bytes (0 = unbounded)"
        );
        state.query.query_log = Some(Arc::new(log));
    }

    // ADR-0009 boot conformance pass: make the write-time invariant true
    // over the standing corpus before anything reads or writes it. Only on
    // ingest-enabled nodes (a query-only node does not own the data root).
    // Fatal on failure, like the epoch gate — a data root not proven
    // conformant must not serve queries. A per-path failure is not that:
    // an unreadable or foreign parquet file, or a subdirectory the walk
    // cannot enumerate, is skipped and counted inside the pass, so one bad
    // path cannot keep the daemon down.
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
            observed = summary.observed,
            "boot conformance pass finished"
        );
    } else {
        // A query-only node skips the pass, but `/api/v1/schema` still
        // answers from this catalog's pins — which describe the archive only
        // if this catalog wrote it. Check the same dual-sided marker as a
        // gate, and refuse the boot rather than advertise a schema about
        // someone else's data. Only a marker naming another catalog does
        // that: an archive with no marker (what an incomplete conformance
        // pass leaves) warns and serves, exactly as the ingest node does for
        // the same corpus.
        let identity = trawl_server::catalog::conform::verify_archive_identity(
            &state.storage.catalog,
            &config.data.base_dir(),
        )
        .await?;
        match identity {
            trawl_server::catalog::conform::ArchiveIdentity::Unproven => tracing::warn!(
                event_type = "catalog_identity_unproven",
                "query-only node: the archive carries no conformance marker, so \
                 the pins /api/v1/schema advertises are not proven to describe \
                 it — boot once with [ingest] enabled = true to run the \
                 conformance pass, and check for skipped paths if it has"
            ),
            identity => tracing::info!(
                event_type = "catalog_identity",
                identity = ?identity,
                "query-only node: archive belongs to the connected catalog"
            ),
        }
    }

    // Recovery and conformance are complete. Telemetry can now write to
    // the WAL and hot buffer, alongside the other ingest producers.
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

    let compaction_handle = spawn_ingest_pipeline(&config, &state)?;

    // Spawn syslog listeners if enabled (requires ingest to be enabled).
    let syslog_handle = if config.syslog.enabled && config.ingest.enabled {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        // Everything the listeners need to reach the one canonicalizer.
        // The env allowlist, the relay CIDRs and the derivation policy all
        // live on `IngestState`, resolved boot-fatally there.
        let door = Arc::new(trawl_server::syslog::convert::SyslogDoor {
            envs: Arc::clone(&state.ingest.envs),
            default_env: Arc::clone(&state.ingest.default_env),
            trusted_relays: Arc::clone(&state.ingest.trusted_relays),
            derivation: Arc::clone(&state.ingest.derivation),
        });
        let handles = trawl_server::syslog::spawn_syslog(
            &config.syslog,
            door,
            Arc::clone(state.ingest.pipeline.as_ref().expect("ingest enabled")),
            state.ingest.syslog_stats.clone(),
            shutdown_rx,
        )
        .map_err(|e| {
            tracing::error!(event_type = "config_error", error = %e, "syslog config rejected — refusing to start");
            e
        })?;
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
        let (shutdown, http_shutdown) = trawl_server::shutdown::shutdown_channel();

        let http_state = state.clone();
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

/// After database admission, validate the format before recovery can mutate
/// storage. Repin swaps environment directories only; EPOCH stays in the live
/// root throughout. The caller retains the admitted sole-writer lock.
/// Finish recovery before state construction or any corpus reader starts.
fn prepare_data_root(
    data_root: &std::path::Path,
    wal_dir: &std::path::Path,
    ingest_enabled: bool,
) -> Result<
    (
        trawl_server::epoch::Outcome,
        Option<trawl_server::repin::recover::Recovered>,
    ),
    String,
> {
    let epoch = trawl_server::epoch::ensure_current_epoch(data_root, wal_dir, ingest_enabled)?;
    let recovered = trawl_server::repin::recover::recover_filesystem(data_root, ingest_enabled)?;
    Ok((epoch, recovered))
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

/// A background task's join handle paired with its shutdown signal.
type IngestHandles = (JoinHandle<()>, watch::Sender<bool>);

/// Spawn the ingest pipeline tasks (compaction).
///
/// Returns handles for graceful shutdown, or `None` if ingest is disabled.
fn spawn_ingest_pipeline(
    config: &Config,
    state: &AppState,
) -> Result<Option<IngestHandles>, Box<dyn std::error::Error>> {
    if !config.ingest.enabled {
        tracing::info!(event_type = "lifecycle", "ingest pipeline disabled");
        return Ok(None);
    }

    let wal_dir = config.wal_dir();

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
    // the catalog before writing, and conforms every batch to the pins
    // (ADR-0009).
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
        state.ingest.repin_coordinator.clone(),
        shutdown_rx,
    );

    Ok(Some((handle, shutdown_tx)))
}

/// Directory names directly under the data root, or `None` when the root
/// cannot be listed.
///
/// One `read_dir` feeds both boot warnings below. A non-directory entry is
/// left out, so a stray file named `lab` counts as "no `data/lab/`".
fn on_disk_env_dirs(data_dir: &std::path::Path) -> Option<Vec<String>> {
    let entries = std::fs::read_dir(data_dir).ok()?;
    Some(
        entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str().map(ToOwned::to_owned))
            .collect(),
    )
}

/// On-disk env directories missing from the current allowlist.
fn unlisted_env_dirs<'a>(allowed: &[String], on_disk: &'a [String]) -> Vec<&'a str> {
    on_disk
        .iter()
        .map(String::as_str)
        .filter(|name| {
            trawl_server::config::is_valid_env_name(name)
                && !trawl_server::config::RESERVED_ENV_NAMES.contains(name)
                && !allowed.iter().any(|e| e == name)
        })
        .collect()
}

/// `[retention.env.<name>]` keys with no `data/<name>/` directory.
fn retention_envs_without_dir<'a>(
    retention: &'a trawl_server::config::RetentionConfig,
    on_disk: &[String],
) -> Vec<&'a str> {
    retention
        .env
        .keys()
        .map(String::as_str)
        .filter(|name| !on_disk.iter().any(|d| d == name))
        .collect()
}

/// Warn about on-disk env directories missing from the current allowlist.
///
/// The allowlist gates writes, not reads: removing an env stops new ingest
/// for it, while its directories stay queryable and age out under retention
/// (ADR-0009).
fn warn_unlisted_env_dirs(config: &Config, on_disk: &[String]) {
    for name in unlisted_env_dirs(&config.ingest.effective_envs(), on_disk) {
        tracing::warn!(
            event_type = "env_not_in_allowlist",
            unlisted_env = %name,
            "on-disk env directory is not in ingest.envs — new ingest \
             for it rejects, existing data stays queryable and ages out \
             under retention"
        );
    }
}

/// Warn about per-env retention overrides that govern nothing on disk.
///
/// Usually a typo in the env name, and a typo here is silent: the override
/// never applies and the env it was meant for keeps the global age. Warn
/// only, on every node — an env directory that does not exist yet is a
/// legitimate way to pre-declare a policy, and query-only nodes run the
/// same retention loop.
fn warn_retention_envs_without_dir(config: &Config, on_disk: &[String]) {
    for name in retention_envs_without_dir(&config.retention, on_disk) {
        tracing::warn!(
            event_type = "retention_env_without_dir",
            retention_env = %name,
            max_age_days = config.retention.max_age_days_for(name),
            "retention.env names an env with no directory under the data \
             root — the override governs nothing until data for it lands"
        );
    }
}

const STORAGE_MARKERS: [&str; 3] = ["EPOCH", "CATALOG", "REPIN"];

fn validate_file_log_config(config: &Config) -> std::io::Result<()> {
    if !config.internal_telemetry_enabled()
        && let Some(path) = &config.server.log_file
    {
        validate_log_destination(path, &config.data.base_dir())?;
    }
    Ok(())
}

/// Resolve existing aliases and missing suffixes without creating anything.
/// Resolve symlinks before `..`, including dangling links to future markers.
fn resolve_log_destination(path: &std::path::Path) -> std::io::Result<PathBuf> {
    fn resolve(path: &std::path::Path, links: u8) -> std::io::Result<PathBuf> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_symlink() => {
                if links == 40 {
                    return Err(std::io::Error::other(
                        "too many symlinks in log or storage path",
                    ));
                }
                let target = std::fs::read_link(path)?;
                resolve(&path.parent().unwrap_or(path).join(target), links + 1)
            }
            Ok(_) => std::fs::canonicalize(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(parent) = path.parent() else {
                    return Err(error);
                };
                let mut resolved = resolve(parent, links)?;
                match path.components().next_back() {
                    Some(std::path::Component::Normal(name)) => resolved.push(name),
                    Some(std::path::Component::ParentDir) => {
                        resolved.pop();
                    }
                    Some(std::path::Component::CurDir) => {}
                    _ => return Err(error),
                }
                // Collapsing a missing `child/..` can reveal an existing
                // symlink at the resulting path. Resolve that alias too.
                if resolved == path {
                    Ok(resolved)
                } else {
                    resolve(&resolved, links)
                }
            }
            Err(error) => Err(error),
        }
    }
    resolve(&std::env::current_dir()?.join(path), 0)
}

fn marker_log_error(marker: &std::path::Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "server.log_file overlaps reserved storage marker {}; select a separate log file",
            marker.display()
        ),
    )
}

fn validate_log_destination(
    path: &std::path::Path,
    data_root: &std::path::Path,
) -> std::io::Result<PathBuf> {
    let resolved = resolve_log_destination(path)?;
    for name in STORAGE_MARKERS {
        let marker = data_root.join(name);
        if resolved == resolve_log_destination(&marker)? {
            return Err(marker_log_error(&marker));
        }
    }
    #[cfg(unix)]
    match std::fs::metadata(&resolved) {
        Ok(metadata) => validate_log_identity(&metadata, data_root)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(resolved)
}

#[cfg(unix)]
fn validate_log_identity(
    metadata: &std::fs::Metadata,
    data_root: &std::path::Path,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    for name in STORAGE_MARKERS {
        let marker = data_root.join(name);
        match std::fs::metadata(&marker) {
            Ok(other) if metadata.dev() == other.dev() && metadata.ino() == other.ino() => {
                return Err(marker_log_error(&marker));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

type JsonLogLayer = fmt::Layer<
    tracing_subscriber::Registry,
    fmt::format::JsonFields,
    fmt::format::Format<fmt::format::Json>,
    fmt::writer::BoxMakeWriter,
>;

struct FileLog {
    path: PathBuf,
    data_root: PathBuf,
    writer: tracing_subscriber::reload::Handle<JsonLogLayer, tracing_subscriber::Registry>,
}

impl FileLog {
    /// Open only after database and storage admission. Until then, this
    /// layer sends JSON events to stderr without touching the configured path.
    fn open(self) -> Result<(), Box<dyn std::error::Error>> {
        let path = validate_log_destination(&self.path, &self.data_root)?;
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // Check the opened file too, before installing a writer that can
        // append bytes. This also detects existing hard-link aliases.
        #[cfg(unix)]
        validate_log_identity(&file.metadata()?, &self.data_root)?;
        self.writer.modify(|layer| {
            *layer.writer_mut() = fmt::writer::BoxMakeWriter::new(file);
        })?;
        Ok(())
    }
}

struct Tracing {
    telemetry: Option<(WalHandle, WalLayer)>,
    file_log: Option<FileLog>,
}

/// Initialize the tracing subscriber.
///
/// When internal telemetry is enabled, registers a [`WalLayer`] in place of
/// the JSON file logger and returns both the [`WalHandle`] (for deferred
/// writer injection) and a [`WalLayer`] clone (sharing the same buffer) for
/// the flush task.
///
/// When telemetry is disabled and `log_file` is configured, falls back
/// to a JSON layer that writes to stderr until the caller opens the file
/// after storage admission.
///
/// When `monitor_active` is true, the stdout `fmt::layer()` is omitted
/// to avoid corrupting the TUI with interleaved log output.
fn init_tracing(
    config: &Config,
    monitor_active: bool,
    filter_directives: &str,
    derivation: Arc<trawl_server::ingest::producer::Derivation>,
) -> Tracing {
    // The directives were resolved (and validated when operator-supplied) by
    // `telemetry::resolve_log_filter`; each layer builds its own EnvFilter
    // from the same string. The WAL layer builds a narrower one
    // (`telemetry::wal_filter`): pre-authn auth and transport events are
    // logged but never persisted, so an unauthenticated connection or
    // request flood cannot grow the corpus.
    let make_filter = || EnvFilter::new(filter_directives);

    let use_telemetry = config.internal_telemetry_enabled();

    if use_telemetry {
        let handle = WalHandle::new();
        let wal_layer = WalLayer::new_with_buffer_cap(
            handle.clone(),
            &config.ingest.effective_envs(),
            &config.ingest.default_env,
            derivation,
            config.ingest.telemetry_buffer_max_bytes,
        );
        let flush_layer = wal_layer.clone(); // same Arc<WalLayerInner>

        if monitor_active {
            // Skip stdout layer — TUI owns the terminal.
            tracing_subscriber::registry()
                .with(wal_layer.with_filter(telemetry::wal_filter(filter_directives)))
                .init();
        } else {
            let stdout_layer = fmt::layer().with_filter(make_filter());
            tracing_subscriber::registry()
                .with(stdout_layer)
                .with(wal_layer.with_filter(telemetry::wal_filter(filter_directives)))
                .init();
        }
        Tracing {
            telemetry: Some((handle, flush_layer)),
            file_log: None,
        }
    } else if let Some(log_path) = &config.server.log_file {
        // Logging must not create occupancy in a fresh root or alter refused
        // storage. Retain startup diagnostics on stderr until admission succeeds.
        let file_layer = fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(fmt::writer::BoxMakeWriter::new(std::io::stderr));
        let (file_layer, writer) = tracing_subscriber::reload::Layer::new(file_layer);
        let stdout_layer = (!monitor_active).then(|| fmt::layer().with_filter(make_filter()));
        tracing_subscriber::registry()
            .with(file_layer.with_filter(make_filter()))
            .with(stdout_layer)
            .init();
        Tracing {
            telemetry: None,
            file_log: Some(FileLog {
                path: log_path.clone(),
                data_root: config.data.base_dir(),
                writer,
            }),
        }
    } else if monitor_active {
        // Monitor active, no telemetry, no file — still need a subscriber
        // but skip stdout to avoid TUI corruption.
        tracing_subscriber::registry().init();
        Tracing {
            telemetry: None,
            file_log: None,
        }
    } else {
        let stdout_layer = fmt::layer().with_filter(make_filter());
        tracing_subscriber::registry().with(stdout_layer).init();
        Tracing {
            telemetry: None,
            file_log: None,
        }
    }
}

/// Which of [`log_crash_dump`]'s two call sites prints the verdict.
///
/// The level is picked before the field list so that each level keeps exactly
/// one `tracing` call site, and a class is never split across two of them.
enum Emit {
    Info,
    Warn,
}

/// Log the crash-dump verdict, once, now that a subscriber exists.
///
/// The crate prints nothing itself (ADR-0023 ruling 6): `init()` has to run
/// before tracing exists, so it hands its verdict back as data and this is
/// where the verdict becomes an event. That puts it in self-telemetry with
/// everything else, so an operator can query why a crash produced an empty
/// dump. `Status::Disabled` says nothing at all: no dump directory was
/// configured, which is a choice rather than a finding.
///
/// One verdict never arrives here: a failed seal. `main` refuses the boot on
/// that one before the runtime exists, so its only diagnostic is the stderr
/// line written there (ADR-0023 ruling 4).
///
/// Both call sites below print the same field list, so a query on
/// `event_type = "crash_dump"` reads the same names whatever the verdict was.
/// A field the probe could not read is OMITTED rather than guessed: printing
/// `monitor_cap_eff_ptrace=false` for a monitor whose `/proc` status was
/// unreadable would state a fact nothing established, and `readiness` already
/// carries "no verdict".
fn log_crash_dump(status: &trawl_crashdump::Status) {
    use trawl_crashdump::{ReadinessClass, Status};

    const FAILED: &str = "crash-dump capture failed to arm";

    let armed = match status {
        Status::Disabled => return,
        Status::Armed(readiness) => Some(readiness),
        Status::Failed(_) => None,
    };
    let inputs = armed.map(|readiness| &readiness.inputs);
    let monitor = inputs.and_then(|inputs| inputs.monitor);
    let sealed = armed.and_then(|readiness| readiness.after_seal);

    // -1 is neither a yama scope nor a pid, so the sentinel cannot be misread
    // as an observation. Both stay integers instead of becoming strings
    // because an operator filters and orders on them.
    let ptrace_scope = inputs
        .and_then(|inputs| inputs.ptrace_scope)
        .map_or(-1_i64, i64::from);
    let monitor_pid = armed.map_or(-1_i64, |readiness| i64::from(readiness.monitor_pid));
    let monitor_cap_eff_ptrace = monitor.map(|monitor| monitor.has_ptrace_effective());
    let monitor_cap_prm_ptrace = monitor.map(|monitor| monitor.has_ptrace_permitted());
    let monitor_no_new_privs = monitor.map(|monitor| monitor.no_new_privs);
    let self_cap_eff_ptrace = sealed.map(|sealed| sealed.has_ptrace_effective());
    let self_cap_prm_ptrace = sealed.map(|sealed| sealed.has_ptrace_permitted());
    let self_no_new_privs = sealed.map(|sealed| sealed.no_new_privs);
    let dumpable = inputs.and_then(|inputs| inputs.dumpable);
    // True only when `prctl(PR_SET_PTRACER)` returned 0. A call that failed and
    // a call never made both leave the daemon without a declared tracer, which
    // is the one fact yama scope 1 acts on.
    let ptracer_set = inputs.map(|inputs| inputs.ptracer == Some(Ok(())));
    let dir = armed.map(|readiness| readiness.dir.display().to_string());
    let retain = armed.map(|readiness| readiness.retain);
    let missing = armed.and_then(trawl_crashdump::Readiness::missing_capability);
    let reason = match status {
        Status::Failed(reason) => Some(reason.as_str()),
        _ => None,
    };

    // One expansion per level, so each level has exactly one call site and the
    // field list cannot drift between them.
    macro_rules! emit {
        ($level:ident, $message:expr) => {
            tracing::$level!(
                event_type = "crash_dump",
                readiness = status.as_str(),
                ptrace_scope,
                monitor_pid,
                monitor_cap_eff_ptrace,
                monitor_cap_prm_ptrace,
                monitor_no_new_privs,
                self_cap_eff_ptrace,
                self_cap_prm_ptrace,
                self_no_new_privs,
                dumpable,
                ptracer_set,
                dir = dir.as_deref(),
                retain,
                missing,
                reason,
                "{}",
                $message
            )
        };
    }

    let (emit, message) = match status {
        // Returned above. The arm exists because the match must be total.
        Status::Disabled => return,
        Status::Armed(readiness) => match readiness.class {
            ReadinessClass::Ready => (
                Emit::Info,
                "crash-dump capture ready; capability and yama checked, LSM policy not probed",
            ),
            ReadinessClass::Denied => (
                Emit::Warn,
                "crash-dump capture DENIED: a crash would write a minidump with no threads",
            ),
            ReadinessClass::Indeterminate => (
                Emit::Warn,
                "crash-dump capture unverified: a probe input was unreadable, so a crash may \
                 write a minidump with no threads",
            ),
        },
        // A failed seal never reaches this match: `main` returns before the
        // subscriber is built. Every other reason (dump directory, monitor
        // spawn, socket connect, monitor identity, handler install) is reported
        // by a process that DID seal, so capture is off and the capability is
        // gone, which is a warning rather than a refused boot.
        Status::Failed(_) => (Emit::Warn, FAILED),
    };

    match emit {
        Emit::Info => emit!(info, message),
        Emit::Warn => emit!(warn, message),
    }
}

/// Resolve a path, expanding `~` to the home directory.
fn resolve_path(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn log_destination_rejects_marker_aliases_and_keeps_normal_paths() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("nested")).unwrap();
        let alias = tmp.path().join("alias");
        symlink("data/nested", &alias).unwrap();
        let log_link = tmp.path().join("server.log");
        for marker in STORAGE_MARKERS {
            symlink(format!("data/{marker}"), &log_link).unwrap();
            for path in [
                log_link.clone(),
                tmp.path().join("missing/../server.log"),
                alias.join(format!("../{marker}")),
                data.join(format!("missing/../{marker}")),
            ] {
                let error = validate_log_destination(&path, &data).unwrap_err();
                assert!(
                    error.to_string().contains("reserved storage marker"),
                    "{error}"
                );
                assert!(!data.join(marker).exists());
                assert!(!data.join("missing").exists());
            }
            std::fs::remove_file(&log_link).unwrap();
            std::fs::write(data.join(marker), b"marker bytes").unwrap();
            std::fs::hard_link(data.join(marker), &log_link).unwrap();
            assert!(validate_log_destination(&log_link, &data).is_err());
            let opened = std::fs::File::open(&log_link).unwrap();
            assert!(validate_log_identity(&opened.metadata().unwrap(), &data).is_err());
            assert_eq!(std::fs::read(&log_link).unwrap(), b"marker bytes");
            std::fs::remove_file(log_link.clone()).unwrap();
            std::fs::remove_file(data.join(marker)).unwrap();
        }
        for path in [
            data.join("server.log"),
            data.join("logs/EPOCH"),
            tmp.path().join("EPOCH"),
        ] {
            assert!(validate_log_destination(&path, &data).is_ok());
            assert!(!path.exists());
        }
        symlink("data", tmp.path().join("data-alias")).unwrap();
        assert!(
            validate_log_destination(&data.join("EPOCH"), &tmp.path().join("data-alias")).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn log_destination_resolution_errors_do_not_change_storage() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let cycle = tmp.path().join("cycle");
        symlink("cycle", &cycle).unwrap();
        assert!(validate_log_destination(&cycle, &data).is_err());
        assert!(!data.exists());
        let file = tmp.path().join("file");
        std::fs::write(&file, b"preserve").unwrap();
        assert!(validate_log_destination(&file.join("log"), &data).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"preserve");
        assert!(!data.exists());
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let traversal = std::fs::read_dir(&blocked);
        let result = validate_log_destination(&blocked.join("server.log"), &data);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Privileged runners can traverse mode 000. Where the filesystem
        // denies traversal, resolution must preserve that error.
        if traversal.is_err() {
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        assert!(!data.exists());
    }

    #[test]
    fn prepare_data_root_refuses_epoch_or_wal_before_repin_cleanup() {
        for invalid_epoch in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            let shadow = tmp.path().join("data.repin-next");
            std::fs::create_dir(&data).unwrap();
            std::fs::create_dir(&shadow).unwrap();
            std::fs::write(shadow.join("sentinel"), b"must survive").unwrap();
            std::fs::write(data.join("EPOCH"), if invalid_epoch { "2" } else { "3" }).unwrap();
            let wal = tmp.path().join("wal");
            if !invalid_epoch {
                std::fs::create_dir(&wal).unwrap();
                std::fs::write(wal.join("flat.ndjson"), b"unsupported").unwrap();
            }
            // Recovery without the gate would sweep this markerless shadow.
            assert!(prepare_data_root(&data, &wal, true).is_err());
            assert_eq!(
                std::fs::read(shadow.join("sentinel")).unwrap(),
                b"must survive"
            );
        }
    }

    #[test]
    fn prepare_data_root_recovers_current_repin_before_returning() {
        use trawl_server::repin::marker::{RepinMarker, RepinPhase, write_marker};
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let shadow = tmp.path().join("data.repin-next");
        std::fs::create_dir_all(data.join("prod")).unwrap();
        std::fs::create_dir_all(shadow.join("prod")).unwrap();
        std::fs::write(data.join("EPOCH"), "3").unwrap();
        std::fs::write(data.join("prod/a.parquet"), b"old").unwrap();
        std::fs::write(shadow.join("prod/a.parquet"), b"new").unwrap();
        write_marker(
            &data,
            &RepinMarker {
                job_id: 1,
                field: "status".into(),
                from_type: "BIGINT".into(),
                to_type: "VARCHAR".into(),
                phase: RepinPhase::Cutover,
            },
        )
        .unwrap();
        assert!(prepare_data_root(&data, &data.join("wal"), false).is_err());
        assert_eq!(std::fs::read(data.join("prod/a.parquet")).unwrap(), b"old");
        let (epoch, recovered) = prepare_data_root(&data, &data.join("wal"), true).unwrap();
        assert_eq!(epoch, trawl_server::epoch::Outcome::Current);
        assert!(recovered.is_some());
        assert_eq!(std::fs::read(data.join("prod/a.parquet")).unwrap(), b"new");
        assert_eq!(std::fs::read(data.join("EPOCH")).unwrap(), b"3");
    }

    fn config_with(retention: &str, ingest: &str) -> Config {
        Config::from_toml(&format!(
            r#"
[server]
[data]
path = "/data"
[auth]
[ingest]
{ingest}
[retention]
{retention}
"#
        ))
        .expect("test config must load")
    }

    #[test]
    fn on_disk_env_dirs_lists_directories_only() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(root.path().join("prod")).expect("mkdir prod");
        std::fs::write(root.path().join("lab"), b"not a directory").expect("write lab");

        let mut dirs = on_disk_env_dirs(root.path()).expect("the root lists");
        dirs.sort();
        assert_eq!(dirs, vec!["prod".to_string()]);

        assert!(
            on_disk_env_dirs(&root.path().join("absent")).is_none(),
            "an unreadable root warns about nothing rather than everything"
        );
    }

    #[test]
    fn unlisted_env_dirs_names_dirs_outside_the_allowlist() {
        let on_disk: Vec<String> = ["prod", "lab", "wal", "Not-An-Env"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        let allowed = vec!["prod".to_string()];

        assert_eq!(
            unlisted_env_dirs(&allowed, &on_disk),
            vec!["lab"],
            "`wal` is reserved and `Not-An-Env` is not an env name at all"
        );
    }

    #[test]
    fn retention_env_without_dir_flags_the_entry_with_no_directory() {
        // The typo case: `labb` governs nothing, and `lab` keeps the
        // global 90 days while the operator thinks it keeps 7.
        let config = config_with(
            r"
max_age_days = 90

[retention.env.prod]
max_age_days = 365

[retention.env.labb]
max_age_days = 7
",
            "",
        );
        let on_disk: Vec<String> = ["prod", "lab"].into_iter().map(ToOwned::to_owned).collect();

        assert_eq!(
            retention_envs_without_dir(&config.retention, &on_disk),
            vec!["labb"]
        );
    }

    #[test]
    fn retention_env_without_dir_is_silent_when_every_entry_has_one() {
        let config = config_with(
            r"
[retention.env.prod]
max_age_days = 365
",
            "",
        );
        let on_disk = vec!["prod".to_string()];

        assert!(retention_envs_without_dir(&config.retention, &on_disk).is_empty());
    }
}
