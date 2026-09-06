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

/// Wall-clock cap on how long process exit waits for blocking-pool work
/// that has already started — chiefly the telemetry/WAL durability
/// barriers (fsync, dir-fsync). Dropping a Tokio runtime normally waits on
/// those forever, so a frozen volume would stall restarts and rolling
/// deployments indefinitely; past this budget the runtime is abandoned and
/// the process exits with the wedged thread still parked in the kernel.
const RUNTIME_SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install crash-dump capture before any threads are spawned or the async
    // runtime is built: the minidump monitor is launched by re-execing this
    // binary, which is only fork-safe while the process is single-threaded. In
    // monitor mode this never returns. The report owns the handler guard, so it
    // lives in this frame for the whole run; `status` is the copy of the
    // verdict `async_main` logs once a subscriber exists (ADR-0023 ruling 6).
    let crash_dump = trawl_crashdump::init();
    let status = crash_dump.status();

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

#[allow(clippy::too_many_lines)] // lifecycle orchestration is cohesive
async fn async_main(crash_dump: trawl_crashdump::Status) -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring crypto provider");

    let cli = Cli::parse();
    let config_path = resolve_path(&cli.config);
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

    let telemetry = init_tracing(
        &config,
        monitor_active,
        &log_filter.directives,
        Arc::clone(&derivation),
    )?;
    if let Some(warning) = &log_filter.warning {
        tracing::warn!(event_type = "config_warning", "{warning}");
    }

    log_crash_dump(&crash_dump);
    if matches!(
        crash_dump,
        trawl_crashdump::Status::Failed(trawl_crashdump::FailureReason::Seal)
    ) {
        // Boot-fatal (ADR-0023 ruling 4). The daemon is supposed to come back
        // from `init()` with `CAP_SYS_PTRACE` gone from both its effective and
        // permitted sets and `no_new_privs` set. A seal that did not take
        // leaves the capability live and the path back to the file capability
        // through a re-exec open, which is a privilege boundary that failed to
        // establish, not a degraded feature to serve past.
        return Err("crash-dump capture could not drop CAP_SYS_PTRACE".into());
    }

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

    // Repin recovery, filesystem half: an interrupted repin must be
    // finished before the epoch gate forms an opinion of the data root,
    // because a half-swapped corpus does not error, it silently promotes
    // (ADR-0011). One stat on the marker-less fast path.
    let recovered_repin = trawl_server::repin::recover::recover_filesystem(
        &config.data.base_dir(),
        config.ingest.enabled,
    )?;

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
    // One listing of the data root feeds both warnings.
    if let Some(env_dirs) = on_disk_env_dirs(&config.data.base_dir()) {
        warn_unlisted_env_dirs(&config, &env_dirs);
        warn_retention_envs_without_dir(&config, &env_dirs);
    }

    let (mut state, http_config) =
        AppState::from_config(&config, metrics_handle, derivation).await?;

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

    // Repin recovery, postgres half: finish the recovered job row
    // (idempotent flip or failure), re-arm the conformance pass for a
    // recovered cutover, sweep the aside, reconcile orphaned running rows.
    // Runs before the conformance pass so a cleared `conformed_at`
    // re-proves the corpus in this very boot.
    trawl_server::repin::recover::reconcile_store(
        &state.storage,
        &state.query.field_catalog,
        &config.data.base_dir(),
        recovered_repin,
    )
    .await?;

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

/// Initialize the tracing subscriber.
///
/// When internal telemetry is enabled, registers a [`WalLayer`] in place of
/// the JSON file logger and returns both the [`WalHandle`] (for deferred
/// writer injection) and a [`WalLayer`] clone (sharing the same buffer) for
/// the flush task.
///
/// When telemetry is disabled and `log_file` is configured, falls back
/// to the JSON file layer.
///
/// When `monitor_active` is true, the stdout `fmt::layer()` is omitted
/// to avoid corrupting the TUI with interleaved log output.
fn init_tracing(
    config: &Config,
    monitor_active: bool,
    filter_directives: &str,
    derivation: Arc<trawl_server::ingest::producer::Derivation>,
) -> Result<Option<(WalHandle, WalLayer)>, Box<dyn std::error::Error>> {
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
        Ok(Some((handle, flush_layer)))
    } else if let Some(log_path) = &config.server.log_file {
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

/// Which of [`log_crash_dump`]'s three call sites prints the verdict.
///
/// The level is picked before the field list so that each level keeps exactly
/// one `tracing` call site, and a class is never split across two of them.
enum Emit {
    Info,
    Warn,
    Error,
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
/// All three call sites below print the same field list, so a query on
/// `event_type = "crash_dump"` reads the same names whatever the verdict was.
/// A field the probe could not read is OMITTED rather than guessed: printing
/// `monitor_cap_eff_ptrace=false` for a monitor whose `/proc` status was
/// unreadable would state a fact nothing established, and `readiness` already
/// carries "no verdict".
fn log_crash_dump(status: &trawl_crashdump::Status) {
    use trawl_crashdump::{FailureReason, ReadinessClass, Status};

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
        // The seal is the one failure the caller turns into a refused boot, so
        // it is the one that logs at error: this line is the last thing the
        // operator sees before the exit.
        Status::Failed(FailureReason::Seal) => (Emit::Error, FAILED),
        Status::Failed(_) => (Emit::Warn, FAILED),
    };

    match emit {
        Emit::Info => emit!(info, message),
        Emit::Warn => emit!(warn, message),
        Emit::Error => emit!(error, message),
    }
}

/// Resolve a path, expanding `~` to the home directory.
fn resolve_path(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(retention: &str, ingest: &str) -> Config {
        Config::from_toml(&format!(
            r#"
[server]
[data]
path = "/data/*.parquet"
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
