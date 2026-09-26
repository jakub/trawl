// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background WAL → parquet compaction task.
//!
//! Periodically scans the WAL directory for `.ndjson` files, groups
//! them by service, and uses `DuckDB` to convert each batch to parquet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::watch;
use trawl_core::conform::RepinTarget;
use trawl_core::schema::{CanonicalType, LADDER, TypeResolution, normalize_duckdb_type};
use trawl_core::severity::Dialect;

use crate::catalog::CatalogContext;
use crate::env_dirs::{list_env_dirs, try_list_env_dirs_observed};
use crate::hot_buffer::{AdmissionState, HotBuffer};
use crate::ingest::publication_marker::{
    self, PublicationClaims, RecoveryOutcomeKind, ValidatedMarker,
};
use crate::metrics::{BookkeepingWrite, CompactionOperation, QuarantineKind};
use crate::publication::PublicationGate;
use crate::repin::RepinCoordinator;
use crate::state::CompactionStats;
use crate::store::{FieldConflict, MAX_CONFLICT_SAMPLE_BYTES, MAX_CONFLICT_SAMPLES, PinProposal};

/// Chunk size for tests that call a compaction entry point directly, with
/// no config to thread through.
#[cfg(test)]
const DEFAULT_CHUNK_SIZE: usize = 500;

/// Why a compaction pass runs (ADR-0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassKind {
    /// The interval deadline fired. Runs the daily rollup.
    Normal,
    /// The hot buffer is not `Open`, or its pressure generation advanced.
    /// Compacts every WAL file whatever its age, and skips the rollup so the
    /// pass spends its time on draining.
    Pressure,
}

/// What one pass does: which WAL files it may take, and whether it rolls up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PassPlan {
    wal_min_age: Duration,
    daily_rollup: bool,
}

impl PassPlan {
    /// A normal pass under pressure still rolls up, but takes young WAL too:
    /// the buffer needs every drain it can get.
    fn new(kind: PassKind, admission: AdmissionState, interval: Duration, rollup: bool) -> Self {
        match (kind, admission) {
            (PassKind::Normal, AdmissionState::Open) => Self {
                wal_min_age: interval,
                daily_rollup: rollup,
            },
            (PassKind::Normal, _) => Self {
                wal_min_age: Duration::ZERO,
                daily_rollup: rollup,
            },
            (PassKind::Pressure, _) => Self {
                wal_min_age: Duration::ZERO,
                daily_rollup: false,
            },
        }
    }
}

/// When the compaction loop runs its next pass.
///
/// The normal deadline is independent of pressure: only a normal pass moves
/// it, so continuous pressure can never postpone the rollup. Pressure passes
/// run back to back while they make progress (drain at least one batch) and
/// the buffer is still not `Open`. A pass that drains nothing while the
/// buffer is not `Open` starts a cooldown of one interval, during which
/// pressure is ignored. Under continuous refusals and a stalled drain, the
/// loop therefore runs at most about one pass per interval, not one per
/// refusal.
///
/// An insert after the pass started ends the cooldown early: its WAL file
/// is work the pass could not see, such as the WAL of a reservation that
/// was still in flight. Inserts need admitted space and a stalled drain
/// frees none, so a stall can end at most one cooldown per batch that fits
/// the buffer. A `Full` refusal inserts nothing and never ends one.
#[derive(Debug)]
struct Cadence {
    interval: Duration,
    next_normal: Instant,
    cooldown: Option<Cooldown>,
}

/// A cooldown after a pass that drained nothing under pressure.
#[derive(Debug, Clone, Copy)]
struct Cooldown {
    until: Instant,
    /// [`HotBuffer::inserted_batches`] when the pass started.
    inserted: u64,
}

impl Cadence {
    fn new(start: Instant, interval: Duration) -> Self {
        Self {
            interval,
            next_normal: start + interval,
            cooldown: None,
        }
    }

    /// `inserted` is the current [`HotBuffer::inserted_batches`].
    fn cooling(&self, now: Instant, inserted: u64) -> bool {
        self.cooldown
            .is_some_and(|cooldown| now < cooldown.until && inserted <= cooldown.inserted)
    }

    /// The pass due at `now` without waiting, if any. The normal deadline
    /// comes first, so back-to-back pressure passes cannot starve it.
    fn due(&self, now: Instant, admission: AdmissionState, inserted: u64) -> Option<PassKind> {
        if now >= self.next_normal {
            Some(PassKind::Normal)
        } else if admission != AdmissionState::Open && !self.cooling(now, inserted) {
            Some(PassKind::Pressure)
        } else {
            None
        }
    }

    /// Whether a pressure wake may start a pass at `now`.
    fn accepts_pressure(&self, now: Instant, inserted: u64) -> bool {
        !self.cooling(now, inserted)
    }

    /// Record a finished pass. `drained` is whether it removed at least one
    /// hot batch; `admission` is the state after it; `inserted` is
    /// [`HotBuffer::inserted_batches`] read before the pass scanned the WAL.
    fn finished(
        &mut self,
        kind: PassKind,
        now: Instant,
        drained: bool,
        admission: AdmissionState,
        inserted: u64,
    ) {
        if kind == PassKind::Normal {
            self.next_normal = now + self.interval;
        }
        self.cooldown = (!drained && admission != AdmissionState::Open).then_some(Cooldown {
            until: now + self.interval,
            inserted,
        });
    }
}

/// Wait for the next generation of a hot-buffer signal, or forever when
/// there is no hot buffer to watch.
async fn signal_changed(
    rx: &mut Option<watch::Receiver<u64>>,
) -> Result<(), watch::error::RecvError> {
    match rx {
        Some(rx) => rx.changed().await,
        None => std::future::pending().await,
    }
}

/// Spawn the compaction background loop.
///
/// Runs a normal pass every `interval`, compacting `wal_dir` `.ndjson`
/// files whose mtime is older than `interval`, and then the daily rollup.
/// Groups by service and writes parquet to
/// `data_dir/{env}/{date}/{HH}/{service}.parquet`.
///
/// Between normal passes, hot-buffer pressure (ADR-0043) starts extra
/// passes: when the buffer is not `Open` or its pressure generation
/// advances, a pass compacts every WAL file whatever its age and skips the
/// rollup. A normal pass that fires while the buffer is not `Open` also
/// takes young WAL. See [`Cadence`] for how pressure passes stay bounded.
///
/// Stops when `shutdown_rx` receives a signal.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // internal API, config struct is overkill here
pub fn spawn_compaction(
    wal_dir: PathBuf,
    data_dir: PathBuf,
    interval: Duration,
    daily_rollup: bool,
    chunk_size: usize,
    memory_limit: String,
    hot_buffer: Option<Arc<HotBuffer>>,
    compaction_stats: Option<Arc<CompactionStats>>,
    catalog: Option<CatalogContext>,
    repin: Option<Arc<RepinCoordinator>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            event_type = "lifecycle",
            action = "compaction_start",
            wal_dir = %wal_dir.display(),
            data_dir = %data_dir.display(),
            interval_secs = interval.as_secs(),
            daily_rollup,
            chunk_size,
            memory_limit = %memory_limit,
            "compaction task started"
        );

        let admission = || {
            hot_buffer
                .as_ref()
                .map_or(AdmissionState::Open, |buf| buf.admission_state())
        };
        let drained = || hot_buffer.as_ref().map_or(0, |buf| buf.drained_batches());
        let inserted = || hot_buffer.as_ref().map_or(0, |buf| buf.inserted_batches());
        let mut pressure = hot_buffer.as_ref().map(|buf| buf.subscribe_pressure());
        let mut inserts = hot_buffer.as_ref().map(|buf| buf.subscribe_inserted());
        let mut cadence = Cadence::new(Instant::now(), interval);

        loop {
            // A pass that is due now skips the wait below, so check for
            // shutdown here as well.
            if shutdown_rx.has_changed().unwrap_or(true) {
                tracing::info!(
                    event_type = "lifecycle",
                    action = "compaction_stop",
                    "compaction task shutting down"
                );
                break;
            }
            let kind = if let Some(kind) = cadence.due(Instant::now(), admission(), inserted()) {
                kind
            } else {
                let next_normal = cadence.next_normal;
                let accepts_pressure = cadence.accepts_pressure(Instant::now(), inserted());
                #[cfg(test)]
                if let Some(stats) = &compaction_stats {
                    stats.waits.fetch_add(1, Ordering::Release);
                }
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => {
                        tracing::info!(event_type = "lifecycle", action = "compaction_stop", "compaction task shutting down");
                        break;
                    }
                    () = tokio::time::sleep_until(next_normal.into()) => PassKind::Normal,
                    changed = signal_changed(&mut pressure), if accepts_pressure => {
                        if changed.is_err() {
                            // The buffer is gone; only the interval remains.
                            pressure = None;
                            continue;
                        }
                        PassKind::Pressure
                    }
                    // Cooling: refusals stay muted, but an insert may end
                    // the cooldown. The top of the loop decides.
                    changed = signal_changed(&mut inserts), if !accepts_pressure => {
                        if changed.is_err() {
                            inserts = None;
                        }
                        continue;
                    }
                }
            };

            // Generations up to here are answered by this pass; one that
            // advances during it wakes the loop again. The insert count is
            // read before the scan, so an insert the scan may miss ends a
            // cooldown this pass starts.
            if let Some(rx) = pressure.as_mut() {
                rx.borrow_and_update();
            }
            if let Some(rx) = inserts.as_mut() {
                rx.borrow_and_update();
            }
            let inserted_before = inserted();
            let plan = PassPlan::new(kind, admission(), interval, daily_rollup);
            if kind == PassKind::Pressure {
                tracing::debug!(
                    event_type = "compaction_pressure_pass",
                    "hot buffer under pressure; compacting all WAL now"
                );
            }
            let drained_before = drained();
            match compact_once_coordinated(
                &wal_dir,
                &data_dir,
                interval,
                plan.wal_min_age,
                plan.daily_rollup,
                hot_buffer.as_ref(),
                chunk_size,
                &memory_limit,
                catalog.as_ref(),
                repin.as_ref(),
            )
            .await
            {
                Ok(data_loss) => {
                    if let Some(ref stats) = compaction_stats {
                        stats.total_runs.fetch_add(1, Ordering::Relaxed);
                        // compact_once returns the combined data-loss
                        // tally: best-effort daily-rollup failures,
                        // WAL files quarantined this cycle and
                        // publication markers left blocking. Surface it
                        // on the dashboard counter, not just in logs.
                        if data_loss > 0 {
                            stats.total_errors.fetch_add(data_loss, Ordering::Relaxed);
                        }
                        let epoch_secs = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs());
                        stats
                            .last_run_epoch_secs
                            .store(epoch_secs, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    if let Some(ref stats) = compaction_stats {
                        stats.total_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    tracing::error!(event_type = "compaction_error", error = %e, "compaction tick failed");
                }
            }
            // Progress is batches drained, not occupancy: producers refill
            // the buffer while a pass runs.
            let progressed = drained() != drained_before;
            cadence.finished(
                kind,
                Instant::now(),
                progressed,
                admission(),
                inserted_before,
            );
        }
    })
}

/// Run one compaction cycle.
///
/// Returns the number of failures this cycle (0 on a clean run): per-service
/// daily rollups that failed, WAL files quarantined, env WAL directories
/// that could not be scanned, and publication markers recovery left
/// blocking. All of these are best-effort and reported via the count so the
/// caller can track them without failing the whole cycle.
///
/// Public for integration tests only — not part of the external API.
/// Called internally by [`spawn_compaction`].
///
/// `catalog` carries the field-catalog store + pin cache (ADR-0009).
/// `None` (tests, embedded-style callers) conforms each batch against
/// the envelope seed plus its own local pins, without persistence.
#[allow(clippy::too_many_arguments)] // internal API, config struct is overkill here
pub async fn compact_once(
    wal_dir: &Path,
    data_dir: &Path,
    min_age: Duration,
    daily_rollup: bool,
    hot_buffer: Option<&Arc<HotBuffer>>,
    chunk_size: usize,
    memory_limit: &str,
    catalog: Option<&CatalogContext>,
) -> Result<u64, String> {
    compact_once_coordinated(
        wal_dir,
        data_dir,
        min_age,
        min_age,
        daily_rollup,
        hot_buffer,
        chunk_size,
        memory_limit,
        catalog,
        None,
    )
    .await
}

/// [`compact_once`] with the repin interlocks attached (ADR-0011): each
/// service batch's pin-snapshot → conform → publish phase runs under the
/// coordinator's corpus-gate read guard, so no batch can straddle a repin
/// cutover, and the file-relocating daily rollup is suppressed for as long
/// as the coordinator holds a rollup pause, so the shadow build's catch-up
/// diff stays additive. The pause stops a pass that has not begun; the
/// corpus gate, which every relocating unit takes, stops a pass already in
/// flight from continuing past the cutover. WAL draining waits for cutover.
/// It also waits when pending rollup recovery needs to relocate files during
/// a repin pause, because those hourly inputs must remain unchanged.
///
/// Each tick first recovers interrupted compaction publishes from their
/// publication markers (ADR-0041) under the corpus gate, and skips every
/// `(env, service)` a marker still claims. Each chunk then publishes under
/// its own marker: see [`publish_output`].
///
/// `min_age` is the compaction interval: orphaned `.parquet.tmp` files
/// older than twice it are removed. `wal_min_age` selects the WAL files the
/// pass compacts: those older than it, or every file, a future mtime
/// included, when it is zero (a pressure pass, ADR-0043).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // internal API, config struct is overkill here
pub async fn compact_once_coordinated(
    wal_dir: &Path,
    data_dir: &Path,
    min_age: Duration,
    wal_min_age: Duration,
    daily_rollup: bool,
    hot_buffer: Option<&Arc<HotBuffer>>,
    chunk_size: usize,
    memory_limit: &str,
    catalog: Option<&CatalogContext>,
    repin: Option<&Arc<RepinCoordinator>>,
) -> Result<u64, String> {
    // Recover before new WAL can merge into an hourly path named by an
    // interrupted rollup. Retiring that path after a new merge would delete
    // fresh rows that the daily file never contained.
    let publication =
        hot_buffer.map_or_else(|| Arc::new(PublicationGate::new()), |buf| buf.publication());
    publication.initialize(data_dir);
    recover_pending_rollups(&publication, repin).await?;

    // Tally of corrupt WAL files quarantined this cycle. Folded into the
    // return value so it lands on `CompactionStats.total_errors` as a
    // data-loss signal, mirroring the rollup quarantine count.
    let mut wal_quarantined: u64 = 0;

    // Tally of env WAL directories that could not be scanned this cycle.
    let mut scan_failures: u64 = 0;

    // Env is the outermost storage dimension (ADR-0009): WAL lives in
    // `wal_dir/{env}/` and parquet in `data_dir/{env}/{date}/{HH}/`.
    //
    // The root listing is fallible on purpose: an unreadable WAL root is not
    // an empty WAL root. Swallowing it would iterate nothing and report a
    // clean cycle while the WAL never drains and the hot buffer fills until
    // admission refuses ingest (ADR-0043). A *missing* root is a cold start and yields an
    // empty list silently. Unlike a single unreadable env (isolated and
    // counted), a bad root leaves nothing to carry on with.
    let mut skipped_scan_error = false;
    let env_wal_dirs = try_list_env_dirs_observed(wal_dir, || skipped_scan_error = true);
    if skipped_scan_error || env_wal_dirs.is_err() {
        CompactionOperation::WalRootScan.record_failure();
    }
    let env_wal_dirs = env_wal_dirs.map_err(|e| {
        format!(
            "failed to list WAL env directories in {}: {e}",
            wal_dir.display()
        )
    })?;

    // Finish or roll back every interrupted publish (ADR-0041) before this
    // tick can merge a WAL file a marker names or clean up a claimed tmp.
    // Markers recovery could not resolve count as errors and keep their
    // service out of this tick.
    let (recovery_blocked, claims) =
        recover_publications(wal_dir, data_dir, hot_buffer, repin).await?;

    // Remove orphaned .parquet.tmp files from interrupted compaction runs,
    // except any a pending marker still claims: recovery needs that tmp to
    // roll its publish back.
    for (env, env_data_dir) in list_env_dirs(data_dir) {
        cleanup_stale_tmp_files(&env, &env_data_dir, min_age * 2, &claims);
    }

    for (env, env_wal_dir) in env_wal_dirs {
        let env_data_dir = data_dir.join(&env);
        let Some(files) = scan_env_wal_files(&env, &env_wal_dir, wal_min_age) else {
            scan_failures += 1;
            continue;
        };

        if files.is_empty() {
            continue;
        }
        // Group WAL files by service prefix.
        let groups = group_by_service(files);

        for (service, wal_files) in &groups {
            if claims.blocks_service(&env, service) {
                tracing::debug!(
                    event_type = "compaction_blocked",
                    env = %env,
                    compact_service = %service,
                    "a pending publication marker blocks this service until recovery resolves it"
                );
                continue;
            }
            tracing::debug!(
                event_type = "compaction_start",
                compact_service = %service,
                wal_files = wal_files.len(),
                "compacting service batch"
            );

            // Process WAL files in chunks to avoid OOM on large backlogs.
            // Each chunk independently compacts to parquet (merging with
            // the canonical file if it exists), drains the hot buffer, and
            // retires its consumed WAL files, all inside one marker-bracketed
            // publish. If a chunk fails, remaining chunks are skipped and
            // retried on the next tick: a failed publish may have left its
            // marker, and the next chunk must not overwrite it.
            let safe_chunk_size = chunk_size.max(1);
            let chunks: Vec<&[PathBuf]> = wal_files.chunks(safe_chunk_size).collect();
            let total_chunks = chunks.len();

            for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
                if total_chunks > 1 {
                    tracing::debug!(
                        event_type = "compaction_chunk",
                        compact_service = %service,
                        chunk = chunk_idx + 1,
                        total_chunks,
                        chunk_files = chunk.len(),
                        "processing compaction chunk"
                    );
                }

                // Publish cold rows and drain these hot batches under one
                // publication guard so readers cannot see both copies.
                let batch_ids: Vec<String> =
                    chunk.iter().filter_map(|f| wal_batch_id(&env, f)).collect();

                // The corpus-gate read guard covers the whole batch —
                // pin snapshot, conform, publish — so the cutover's write
                // guard means "no batch in flight, none can start".
                let corpus_guard = match repin {
                    Some(c) => Some(c.compaction_guard().await),
                    None => None,
                };
                let outcome = compact_service_batch(
                    chunk,
                    &env_data_dir,
                    wal_dir,
                    &env,
                    service,
                    memory_limit,
                    catalog,
                    hot_buffer.cloned(),
                    batch_ids,
                )
                .await;
                drop(corpus_guard);

                // Folded unconditionally: quarantining renames the corrupt
                // file to `.corrupt`, so a retry can't re-see (and re-count)
                // it — dropping the tally on an `Err` result would keep those
                // data-loss quarantines off the dashboard counter. Mirrors the
                // rollup's `RollupOutcome` handling.
                wal_quarantined += outcome.quarantined;

                match outcome.result {
                    Ok(None) => {}
                    Ok(Some(incomplete)) => {
                        // The output is published and its rows drained; the
                        // marker still names the consumed WAL, so no later
                        // pass can merge it again. Recovery at the start of
                        // a later tick finishes the publish.
                        incomplete.operation.record_failure();
                        tracing::error!(
                            event_type = "compaction_error",
                            compact_service = %service,
                            chunk = chunk_idx + 1,
                            total_chunks,
                            error = %incomplete.error,
                            "published, but finishing the publish failed; its \
                             publication marker blocks this service until recovery \
                             completes it"
                        );
                        break;
                    }
                    Err(e) => {
                        // Owns ordinary failures and handled blocking-task
                        // failures returned in this chunk's outcome.
                        CompactionOperation::Chunk.record_failure();
                        // Leave remaining WAL files for retry on next tick.
                        tracing::error!(
                            event_type = "compaction_error",
                            compact_service = %service,
                            chunk = chunk_idx + 1,
                            total_chunks,
                            error = %e,
                            "compaction chunk failed, will retry next tick"
                        );
                        break;
                    }
                }
            }
        }
    }

    // After WAL compaction, consolidate older days' hourly files into
    // per-service daily files. This dramatically reduces file count for
    // long lookback queries. Rollup is best-effort: a failure for one
    // day/service is counted and retried next tick, not propagated.
    let rollup_suppressed = repin.is_some_and(|c| c.rollup_paused());
    if daily_rollup && rollup_suppressed {
        tracing::info!(
            event_type = "rollup_suppressed",
            "daily rollup suppressed for the duration of the running repin \
             job (the catch-up diff must stay additive); hourly files \
             consolidate on the first tick after the job ends"
        );
    }
    let rollup_failures = if daily_rollup && !rollup_suppressed {
        // Scan again: a publish that failed during this tick left a marker
        // the scan before compaction could not see, and rollup must not
        // move the hourly file it names.
        match scan_claims_blocking(wal_dir).await {
            Ok(claims) => {
                match rollup_once(
                    data_dir,
                    &claims,
                    memory_limit,
                    repin,
                    hot_buffer.map(|buf| buf.publication()),
                )
                .await
                {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::error!(event_type = "rollup_error", error = %e, "daily rollup failed");
                        1
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    event_type = "rollup_error",
                    error = %e,
                    "cannot read publication markers; daily rollup skipped this tick"
                );
                1
            }
        }
    } else {
        0
    };

    Ok(rollup_failures + wal_quarantined + scan_failures + recovery_blocked)
}

/// Finish or roll back every interrupted compaction publish (ADR-0041), then
/// report what the markers left behind still claim.
///
/// Runs on a blocking thread under the repin corpus read guard, which the
/// task owns so cancelling the tick cannot release it mid-recovery. A
/// published marker drains its batches from the hot buffer under the
/// publication write guard, taken inside the corpus guard (ADR-0026 lock
/// order: corpus, then publication).
///
/// Returns how many markers stay blocking (contradictions and failed
/// recoveries, each also counted on `trawl_publication_recovery_total`) and
/// the claims compaction must skip. An unreadable WAL root is an error.
async fn recover_publications(
    wal_dir: &Path,
    data_dir: &Path,
    hot_buffer: Option<&Arc<HotBuffer>>,
    repin: Option<&Arc<RepinCoordinator>>,
) -> Result<(u64, PublicationClaims), String> {
    let corpus_guard = match repin {
        Some(c) => Some(c.compaction_guard_owned().await),
        None => None,
    };
    let wal_dir = wal_dir.to_path_buf();
    let data_dir = data_dir.to_path_buf();
    let hot_buffer = hot_buffer.cloned();
    tokio::task::spawn_blocking(move || {
        let _corpus_guard = corpus_guard;
        let report = publication_marker::recover(&wal_dir, &data_dir, |marker| {
            if let Some(buf) = &hot_buffer {
                let gate = buf.publication();
                let _publication_guard = gate.blocking_write();
                let ids = marker.batch_ids();
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                buf.drain(&ids);
            }
            Ok(())
        });
        let blocked = match report {
            Ok(report) => report.record(),
            Err(error) => {
                RecoveryOutcomeKind::Failed.record();
                tracing::error!(
                    event_type = "publication_recovery_failed",
                    error = %error,
                    "cannot list publication markers; no WAL is compacted this tick"
                );
                return Err(error);
            }
        };
        let claims = publication_marker::scan_claims(&wal_dir)?;
        Ok((blocked, claims))
    })
    .await
    .map_err(|e| {
        RecoveryOutcomeKind::Failed.record();
        crate::error::join_failure_text("publication recovery", e)
    })?
}

/// [`publication_marker::scan_claims`] on a blocking thread.
async fn scan_claims_blocking(wal_dir: &Path) -> Result<PublicationClaims, String> {
    let wal_dir = wal_dir.to_path_buf();
    tokio::task::spawn_blocking(move || publication_marker::scan_claims(&wal_dir))
        .await
        .map_err(|e| crate::error::join_failure_text("publication claims scan", e))?
}

async fn recover_pending_rollups(
    publication: &Arc<PublicationGate>,
    repin: Option<&Arc<RepinCoordinator>>,
) -> Result<(), String> {
    let markers = publication.pending_rollup_markers();
    if !markers.is_empty() {
        let corpus_guard = match rollup_unit(repin).await {
            RollupUnit::Proceed(guard) => guard,
            RollupUnit::StandDown => {
                return Err("pending rollup recovery is paused by a repin job".to_owned());
            }
        };
        let publication_guard = publication.write().await;
        let publication = Arc::clone(publication);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            // Own both guards here so cancellation of the async caller cannot
            // release either guard while recovery still relocates files.
            let _corpus_guard = corpus_guard;
            let _publication_guard = publication_guard;
            // A previous cleanup or retention pass may already have removed a
            // marker. Clear only confirmed missing paths before selecting days.
            for marker in &markers {
                publication.finish_rollup(marker);
            }
            let days: std::collections::BTreeSet<PathBuf> = publication
                .pending_rollup_markers()
                .iter()
                .filter_map(|marker| marker.parent().map(Path::to_path_buf))
                .collect();
            for day in days {
                recover_rollup_markers_coordinated(&day, Some(&publication))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| {
            CompactionOperation::PendingRollupRecovery.record_failure();
            crate::error::join_failure_text("rollup recovery", e)
        })??;
    }
    // An incomplete bootstrap scan must also stop WAL publication, even if
    // the failed scan did not discover a marker before encountering an error.
    let _reader = publication
        .read()
        .await
        .map_err(|_| "pending rollup recovery is incomplete".to_owned())?;
    Ok(())
}

/// Consolidate hourly per-service parquet files into daily files.
///
/// For each date-directory older than today, collects all
/// `{hour}/{service}.parquet` files, merges them (sorted by timestamp)
/// into `{date}/{service}.parquet`, then removes the hourly sources.
///
/// Every unit of that relocation runs under the repin corpus gate (see
/// [`rollup_unit`]), so a pass already running when a repin job starts
/// stands down instead of moving files across a cutover.
async fn rollup_once(
    data_dir: &Path,
    claims: &PublicationClaims,
    memory_limit: &str,
    repin: Option<&Arc<RepinCoordinator>>,
    publication: Option<Arc<PublicationGate>>,
) -> Result<u64, String> {
    let mut total: u64 = 0;
    // Preserve the existing best-effort empty result and warning. Observe
    // this rollup scan here rather than changing other env-list consumers.
    let mut skipped_scan_error = false;
    let env_dirs = try_list_env_dirs_observed(data_dir, || skipped_scan_error = true);
    if skipped_scan_error || env_dirs.is_err() {
        CompactionOperation::DailyRollupScan.record_failure();
    }
    let env_dirs = env_dirs.unwrap_or_else(|e| {
        tracing::warn!(dir = %data_dir.display(), error = %e,
            "failed to list env directories, treating as empty");
        Vec::new()
    });
    for (env, env_data_dir) in env_dirs {
        if repin.is_some_and(|c| c.rollup_paused()) {
            break;
        }
        total += rollup_env_once(
            &env,
            &env_data_dir,
            claims,
            memory_limit,
            repin,
            publication.clone(),
        )
        .await?;
    }
    Ok(total)
}

/// Whether one file-relocating rollup unit may proceed, and the
/// corpus-gate read guard that keeps a repin cutover out of it while it
/// does. Without a coordinator (tests, embedded-style callers) there is no
/// repin engine to exclude and every unit proceeds ungated.
enum RollupUnit {
    Proceed(Option<tokio::sync::OwnedRwLockReadGuard<()>>),
    StandDown,
}

/// Claim the corpus for one relocating unit. See
/// [`RepinCoordinator::rollup_unit_guard`] for why the pause is read under
/// the guard rather than before it.
async fn rollup_unit(repin: Option<&Arc<RepinCoordinator>>) -> RollupUnit {
    match repin {
        None => RollupUnit::Proceed(None),
        Some(c) => match c.rollup_unit_guard_owned().await {
            Some(guard) => RollupUnit::Proceed(Some(guard)),
            None => RollupUnit::StandDown,
        },
    }
}

/// Log the mid-pass stand-down once, on the unit that saw the claim.
fn log_rollup_stand_down() {
    tracing::info!(
        event_type = "rollup_suppressed",
        "daily rollup stood down mid-pass: a repin job claimed the corpus \
         while this pass was running; hourly files consolidate on the first \
         tick after the job ends"
    );
}

/// Log a `(date, service)` the rollup skips because a publication marker
/// claims one of its hourly files.
fn log_rollup_blocked(env: &str, date: &str, service: &str) {
    tracing::info!(
        event_type = "rollup_blocked",
        env = %env,
        date = %date,
        compact_service = %service,
        "a pending publication marker claims an hourly file of this service; \
         its daily rollup waits until recovery resolves it"
    );
}

/// Read an env's entries, observing scan errors while retaining the existing
/// root-error and per-entry skip behavior.
fn read_rollup_env_entries(
    data_dir: &Path,
) -> Result<impl Iterator<Item = std::fs::DirEntry>, String> {
    let date_dirs = std::fs::read_dir(data_dir).map_err(|e| {
        // Retention can remove a directory after this pass discovered it.
        // Keep the existing Err, but absence is not failed-operation evidence.
        if e.kind() != std::io::ErrorKind::NotFound {
            CompactionOperation::DailyRollupScan.record_failure();
        }
        format!("failed to read data_dir: {e}")
    })?;

    Ok(date_dirs
        .inspect(|entry| {
            if entry
                .as_ref()
                .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
            {
                CompactionOperation::DailyRollupScan.record_failure();
            }
        })
        .flatten())
}

/// Select historical date directories. Today stays hourly for fast writes;
/// non-date entries such as `wal` do not participate in daily rollup.
fn historical_rollup_date(path: &Path, today: &str) -> Option<String> {
    // Follow symlinks and preserve the previous skip policy, but observe
    // metadata failures that Path::is_dir would otherwise hide.
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                CompactionOperation::DailyRollupScan.record_failure();
            }
            return None;
        }
    };
    if !metadata.is_dir() {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    (name != today && looks_like_date(name)).then(|| name.to_owned())
}

/// Roll up one env root (`data_dir/{env}`): consolidate each historical
/// date's hourly files into per-service daily files. Never crosses envs.
///
/// A `(date, service)` with any hourly output a publication marker claims
/// is skipped: rolling it up would delete the canonical file whose identity
/// recovery checks, and the marker would then read as a contradiction.
async fn rollup_env_once(
    env: &str,
    data_dir: &Path,
    claims: &PublicationClaims,
    memory_limit: &str,
    repin: Option<&Arc<RepinCoordinator>>,
    publication: Option<Arc<PublicationGate>>,
) -> Result<u64, String> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut failures: u64 = 0;
    let mut quarantined_total: u64 = 0;

    for entry in read_rollup_env_entries(data_dir)? {
        let path = entry.path();
        let Some(dir_name) = historical_rollup_date(&path, &today) else {
            continue;
        };

        // Recover any interrupted rollups from previous runs before
        // starting new ones. This ensures crash-orphaned hourly files
        // are cleaned up without re-merging already-consolidated data.
        // Recovery relocates files too (it deletes the hourly sources of a
        // merge that already completed), so it is a gated unit like the
        // merges below.
        let corpus_guard = match rollup_unit(repin).await {
            RollupUnit::Proceed(guard) => guard,
            RollupUnit::StandDown => {
                log_rollup_stand_down();
                return Ok(failures + quarantined_total);
            }
        };
        let publication_guard = match &publication {
            Some(gate) => Some(gate.write().await),
            None => None,
        };
        let day = path.clone();
        let recovery_publication = publication.clone();
        let recovery = tokio::task::spawn_blocking(move || {
            // Lock acquisition stays async so readers can use the blocking
            // pool to finish. Cancellation cannot release these moved guards.
            let _corpus_guard = corpus_guard;
            let _publication_guard = publication_guard;
            recover_rollup_markers_coordinated(&day, recovery_publication.as_deref())
        })
        .await
        .map_err(|e| {
            CompactionOperation::PendingRollupRecovery.record_failure();
            crate::error::join_failure_text("rollup recovery", e)
        })?;
        if let Err(e) = recovery {
            // A wedged recovery is data-loss-adjacent (an interrupted rollup
            // left orphaned hourlies/tmp that couldn't be cleaned up), so count
            // it on the error tally like the quarantine path does. Do not
            // merge surviving inputs again until recovery has retired them.
            failures += 1;
            tracing::error!(
                event_type = "rollup_error",
                date = %dir_name,
                error = %e,
                "rollup recovery failed"
            );
            continue;
        }
        // Collect hourly subdirs. If none exist, this day is already consolidated.
        let hour_dirs = collect_hour_dirs(&path);
        if hour_dirs.is_empty() {
            continue;
        }

        // Group all parquet files across hour-dirs by service name.
        let service_files = collect_service_files(&hour_dirs);
        if service_files.is_empty() {
            continue;
        }

        let date = chrono::NaiveDate::parse_from_str(&dir_name, "%Y-%m-%d").ok();
        for (service, files) in &service_files {
            if date.is_some_and(|date| claims.claims_service_day(env, date, service)) {
                log_rollup_blocked(env, &dir_name, service);
                continue;
            }
            // One merge = one gated unit: it reads the day's hourlies,
            // renames the merged daily into place and deletes the sources,
            // so a cutover swapping the shadow in between those steps would
            // republish this file's pre-repin types into the new
            // generation. Held for the merge, re-taken per service, so the
            // cutover waits out at most one merge and the rest of the pass
            // stands down.
            let unit_guard = match rollup_unit(repin).await {
                RollupUnit::Proceed(guard) => guard,
                RollupUnit::StandDown => {
                    log_rollup_stand_down();
                    return Ok(failures + quarantined_total);
                }
            };

            let day_dir = path.clone();
            let svc = service.clone();
            let files = files.clone();

            let mem_limit = memory_limit.to_owned();
            let publication = publication.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                rollup_day_coordinated(&day_dir, &svc, &files, &mem_limit, publication.as_deref())
            })
            .await
            .map_err(|e| {
                CompactionOperation::DailyRollupUnit.record_failure();
                crate::error::join_failure_text("rollup", e)
            })?;
            drop(unit_guard);

            // Quarantined inputs are data-loss whether or not the merge then
            // succeeded — fold the count in unconditionally so it lands on the
            // dashboard counter even when the merge errored and dropped its
            // valid output (the quarantined files are already renamed aside, so
            // a retry can't re-count them).
            quarantined_total += outcome.quarantined;
            if let Err(e) = outcome.result {
                CompactionOperation::DailyRollupUnit.record_failure();
                failures += 1;
                tracing::error!(
                    event_type = "rollup_error",
                    compact_service = %service,
                    error = %e,
                    "rollup failed for service, will retry next tick"
                );
            }
        }

        // Remove empty hour-directories after all services are rolled up.
        // Deliberately ungated: `remove_dir` fails on a non-empty
        // directory, so this can never take a parquet file — of either
        // generation — with it.
        for hour_dir in &hour_dirs {
            if is_dir_empty(hour_dir) {
                let _ = std::fs::remove_dir(hour_dir);
            }
        }
    }

    Ok(failures + quarantined_total)
}

/// Check if a directory name looks like a date (YYYY-MM-DD).
fn looks_like_date(name: &str) -> bool {
    name.len() == 10
        && name.as_bytes().get(4) == Some(&b'-')
        && name.as_bytes().get(7) == Some(&b'-')
        && name[..4].bytes().all(|b| b.is_ascii_digit())
}

/// Collect subdirectories that look like hour directories (00-23).
fn collect_hour_dirs(day_dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(day_dir) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                CompactionOperation::DailyRollupScan.record_failure();
            }
            return Vec::new();
        }
    };
    entries
        .inspect(|entry| {
            if entry
                .as_ref()
                .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
            {
                CompactionOperation::DailyRollupScan.record_failure();
            }
        })
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            // Path metadata follows symlinks, matching the previous is_dir.
            let metadata = match p.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        CompactionOperation::DailyRollupScan.record_failure();
                    }
                    return None;
                }
            };
            if metadata.is_dir() {
                let name = p.file_name()?.to_str()?;
                if name.len() == 2 && name.bytes().all(|b| b.is_ascii_digit()) {
                    return Some(p);
                }
            }
            None
        })
        .collect()
}

/// Collect `{service}.parquet` files across all hour-dirs, grouped by service.
fn collect_service_files(hour_dirs: &[PathBuf]) -> HashMap<String, Vec<PathBuf>> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for hour_dir in hour_dirs {
        let entries = match std::fs::read_dir(hour_dir) {
            Ok(entries) => entries,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    CompactionOperation::DailyRollupScan.record_failure();
                }
                continue;
            }
        };
        for entry in entries
            .inspect(|entry| {
                if entry
                    .as_ref()
                    .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
                {
                    CompactionOperation::DailyRollupScan.record_failure();
                }
            })
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "parquet")
                && let Some(service) = path.file_stem().and_then(|s| s.to_str())
            {
                groups.entry(service.to_owned()).or_default().push(path);
            }
        }
    }

    groups
}

/// Check if a directory is empty.
fn is_dir_empty(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// Path for the rollup marker file that tracks in-progress merges.
///
/// The marker lists hourly file paths being merged, enabling crash
/// recovery without re-reading already-consolidated data.
fn rollup_marker_path(day_dir: &Path, service: &str) -> PathBuf {
    day_dir.join(format!(".rollup-{service}"))
}

/// Write a rollup marker listing the hourly files being merged.
fn write_rollup_marker(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
) -> Result<(), String> {
    let content = hourly_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    crate::epoch::publish_marker_staged(day_dir, &format!(".rollup-{service}"), &content)
}

/// Delete the rollup marker after successful cleanup.
fn delete_rollup_marker(day_dir: &Path, service: &str) {
    let marker = rollup_marker_path(day_dir, service);
    let _ = std::fs::remove_file(marker);
}

/// Recover from interrupted rollup operations in a date directory.
///
/// Checks for `.rollup-{service}` marker files and completes the
/// interrupted operation:
/// - If a complete `.parquet.tmp` exists, promote it before retiring hourlies.
///   The existing canonical may belong to an earlier merge.
/// - If the tmp is corrupt, quarantine it and retain hourly inputs.
/// - If only the canonical exists, retire the marker's hourly inputs.
/// - If neither exists: stale marker, just remove it.
#[cfg(test)]
fn recover_rollup_markers(day_dir: &Path) -> Result<(), String> {
    recover_rollup_markers_coordinated(day_dir, None)
}

/// The caller holds the publication write guard through recovery.
fn recover_rollup_markers_coordinated(
    day_dir: &Path,
    publication: Option<&PublicationGate>,
) -> Result<(), String> {
    // Classify the actual read error before converting it to the existing
    // string result. A stat-before-read check would race retention again.
    let entries = std::fs::read_dir(day_dir).map_err(|e| {
        if e.kind() != std::io::ErrorKind::NotFound {
            CompactionOperation::PendingRollupRecovery.record_failure();
        }
        format!("failed to list rollup markers: {e}")
    })?;
    // Returned errors are owned here, including errors from nested file
    // operations. Async callers separately own only their JoinError branch.
    recover_rollup_markers_inner(day_dir, publication, entries).inspect_err(|_| {
        CompactionOperation::PendingRollupRecovery.record_failure();
    })
}

fn recover_rollup_markers_inner(
    day_dir: &Path,
    publication: Option<&PublicationGate>,
    entries: std::fs::ReadDir,
) -> Result<(), String> {
    for entry in entries {
        let path = entry
            .map_err(|e| format!("failed to read rollup entry: {e}"))?
            .path();
        let Some(service) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|name| name.strip_prefix(".rollup-"))
        else {
            continue;
        };
        if let Some(gate) = publication {
            gate.mark_rollup(&path);
            #[cfg(any(test, feature = "test-support"))]
            gate.hold_after_publish_for_test();
        }
        let canonical = day_dir.join(format!("{service}.parquet"));
        let tmp = day_dir.join(format!("{service}.parquet.tmp"));
        let marker_content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read rollup marker: {e}"))?;
        let hourly_files: Vec<PathBuf> = marker_content
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();

        // An existing daily file may precede this merge. A complete tmp
        // contains both that daily file and the new hourly inputs.
        let tmp_exists = match std::fs::symlink_metadata(&tmp) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(format!("failed to inspect rollup tmp: {e}")),
        };
        if tmp_exists {
            if is_valid_parquet(&tmp) {
                tracing::info!(
                    event_type = "rollup_recovery",
                    compact_service = %service,
                    "recovering rollup: renaming tmp to canonical"
                );
                std::fs::rename(&tmp, &canonical)
                    .map_err(|e| format!("rollup recovery rename failed: {e}"))?;
                for file in &hourly_files {
                    retire_merged_input(file)?;
                }
            } else {
                // Keep the old daily and hourly sources for a fresh merge.
                // Remove the marker durably before removing the invalid tmp.
                // Otherwise a crash after quarantine would make recovery
                // mistake the old daily for this merge's published output.
                tracing::warn!(
                    event_type = "rollup_recovery",
                    compact_service = %service,
                    "discarding truncated rollup tmp; retaining hourly files for retry"
                );
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("failed to remove rollup marker: {e}")),
                }
                std::fs::File::open(day_dir)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|e| format!("failed to sync rollup marker removal: {e}"))?;
                if let Some(gate) = publication {
                    gate.finish_rollup(&path);
                }
                quarantine_file(
                    &tmp,
                    service,
                    "rollup_quarantine",
                    QuarantineKind::RollupTemporary,
                )?;
                continue;
            }
        } else {
            let canonical_exists = match std::fs::symlink_metadata(&canonical) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(format!("failed to inspect rollup canonical: {e}")),
            };
            if canonical_exists {
                tracing::info!(
                    event_type = "rollup_recovery",
                    compact_service = %service,
                    "recovering rollup: canonical exists, deleting hourly files"
                );
                for file in &hourly_files {
                    retire_merged_input(file)?;
                }
            } else {
                tracing::warn!(
                    event_type = "rollup_recovery",
                    compact_service = %service,
                    "removing stale rollup marker (no tmp or canonical file)"
                );
            }
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("failed to remove rollup marker: {e}")),
        }
        if let Some(gate) = publication {
            gate.finish_rollup(&path);
        }
    }
    Ok(())
}

/// Parquet magic bytes, written at both the start and end of every valid
/// parquet file.
const PARQUET_MAGIC: &[u8; 4] = b"PAR1";

/// Cheap structural validity check for a parquet file.
///
/// A valid parquet file is at least large enough to hold its header and
/// footer magic and starts and ends with the `PAR1` magic bytes. This
/// catches zero-byte and truncated files (e.g. a crash mid-`COPY`) before
/// they reach `read_parquet`, where they otherwise surface as
/// "File ... too small to be a Parquet file" and wedge the rollup forever
/// — no amount of retrying repairs a truncated file.
pub(crate) fn is_valid_parquet(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    // Header magic (4) + a minimal footer + footer length (4) + trailer
    // magic (4). Anything below this cannot be a parquet file.
    if meta.len() < 12 {
        return false;
    }
    let mut head = [0u8; 4];
    if file.read_exact(&mut head).is_err() || &head != PARQUET_MAGIC {
        return false;
    }
    let mut tail = [0u8; 4];
    if file.seek(SeekFrom::End(-4)).is_err() || file.read_exact(&mut tail).is_err() {
        return false;
    }
    &tail == PARQUET_MAGIC
}

/// Cheap content sniff for a WAL ndjson file, catching the torn-write
/// corruption signature before it reaches `read_json` — where it otherwise
/// fails with "Malformed JSON ... unexpected character" and wedges the whole
/// batch. Like a truncated parquet, no amount of retrying repairs a file of
/// zeros, so the only escape is to set it aside.
///
/// A WAL file is text: newline-delimited JSON objects. We reject it as
/// corrupt if it is empty (`read_json(records=true)` errors on empty input),
/// contains a NUL byte (illegal in JSON text and the dead giveaway of a torn
/// write whose data blocks never flushed), or is not valid UTF-8. An
/// unreadable file is likewise rejected — `read_json` can't read it either,
/// so quarantining beats wedging.
///
/// This deliberately does not fully JSON-parse each line: that would
/// re-implement `read_json` and reject merely-malformed-but-textual data, a
/// different (and rarer) class than the crash debris this guards against.
fn is_valid_ndjson(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.is_empty() || bytes.contains(&0u8) {
        return false;
    }
    std::str::from_utf8(&bytes).is_ok()
}

/// How many `.corrupt` candidates one quarantine will try before giving up.
///
/// A path that corrupts a thousand times is a standing fault, not debris:
/// exhaustion is a hard error so it surfaces rather than silently rotating.
const MAX_QUARANTINE_ATTEMPTS: u32 = 1000;

/// Pick, and reserve, a free quarantine name for `path`.
///
/// The first candidate is the bare `<path>.corrupt`; subsequent ones append a
/// counter (`<path>.corrupt.1`, `.corrupt.2`, …). A candidate is claimed by
/// creating it with `O_EXCL` (`create_new`), so the returned path is an empty
/// file this caller owns and the subsequent `rename` overwrites nothing that
/// was already there.
///
/// Scope, stated honestly: the reservation makes no-clobber hold between
/// cooperating in-process quarantiners — every quarantine in trawl funnels
/// through `quarantine_file`, so that is the whole population. External
/// mutation of the trawl-owned data root (another process planting or renaming
/// files under it) is outside the threat model; the workspace forbids `unsafe`,
/// so the truly atomic `renameat2(RENAME_NOREPLACE)` is not reachable.
///
/// A crash between reservation and rename strands a zero-byte reservation
/// under its `.corrupt`/`.corrupt.<n>` name: inert to every scan glob,
/// skipped by later quarantines, and distinguishable from a real artifact
/// only by its size. Accepted — closing it needs
/// `renameat2(RENAME_NOREPLACE)`, unavailable under the workspace unsafe
/// forbid.
fn quarantine_target(path: &Path) -> Result<PathBuf, String> {
    for n in 0..MAX_QUARANTINE_ATTEMPTS {
        let mut candidate = path.as_os_str().to_owned();
        if n == 0 {
            candidate.push(".corrupt");
        } else {
            candidate.push(format!(".corrupt.{n}"));
        }
        let candidate = PathBuf::from(candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(format!(
                    "failed to reserve quarantine name {}: {e}",
                    candidate.display()
                ));
            }
        }
    }
    Err(format!(
        "no free quarantine name for {} after {MAX_QUARANTINE_ATTEMPTS} attempts",
        path.display()
    ))
}

/// Move a corrupt/unreadable file aside so it stops wedging compaction,
/// preserving the bytes for forensics. Appends `.corrupt` to the filename,
/// which makes it inert: it no longer matches the `*.parquet`/`*.tmp` globs
/// the rollup scans, nor the `*.ndjson` glob WAL compaction scans.
///
/// The destination is reserved first (`quarantine_target`) rather than renamed
/// onto blind: `std::fs::rename` silently replaces an existing destination, so
/// a second corruption of a recurring path (`{env}/{date}/{HH}/{service}` is
/// stable across ticks) would destroy the first forensic artifact. A taken name
/// yields `<path>.corrupt.1`, `.corrupt.2`, … — every artifact is kept, and
/// every one of those names is as inert to the scan globs as the bare form.
///
/// `event_type` tags the structured log so operators can distinguish a
/// rollup quarantine (`rollup_quarantine`) from a WAL-compaction one
/// (`compaction_quarantine`).
///
/// A failed rename is a hard error: the bad file still matches the scan glob
/// and would re-wedge on every tick forever, so callers must surface (and
/// count) the failure rather than swallow it. The reservation is removed on
/// that path so a failure leaves no empty `.corrupt` debris behind.
///
/// Returns the path the file now lives at.
fn quarantine_file(
    path: &Path,
    service: &str,
    event_type: &str,
    kind: QuarantineKind,
) -> Result<PathBuf, String> {
    let quarantined = quarantine_target(path).inspect_err(|e| {
        tracing::error!(
            event_type,
            compact_service = %service,
            file = %path.display(),
            error = %e,
            "failed to quarantine corrupt file"
        );
    })?;
    match std::fs::rename(path, &quarantined) {
        Ok(()) => {
            // Observe the rename now: later work can fail or panic, and a
            // retry cannot rediscover the original path to count this fact.
            metrics::counter!(crate::metrics::FILES_QUARANTINED_TOTAL, "kind" => kind.label())
                .increment(1);
            tracing::warn!(
                event_type,
                compact_service = %service,
                from = %path.display(),
                to = %quarantined.display(),
                "quarantined corrupt file"
            );
            Ok(quarantined)
        }
        Err(e) => {
            // The reservation is ours and empty; drop it so a retry re-uses
            // the same name instead of accumulating zero-byte placeholders.
            if let Err(cleanup) = std::fs::remove_file(&quarantined) {
                tracing::warn!(
                    event_type = "quarantine_reservation_stranded",
                    path = %quarantined.display(),
                    error = %cleanup,
                    "failed to remove the quarantine reservation after a failed rename; \
                     a zero-byte artifact remains under this name"
                );
            }
            tracing::error!(
                event_type,
                compact_service = %service,
                file = %path.display(),
                error = %e,
                "failed to quarantine corrupt file"
            );
            Err(format!("failed to quarantine {}: {e}", path.display()))
        }
    }
}

/// Retire a merge input whose rows now live in a published output: a
/// merged hourly parquet file after a daily rollup, or a consumed WAL file
/// after a compaction publish. Deletes it, or makes it inert when it can't
/// be deleted.
///
/// Renames it aside with a `.merged` suffix so it no longer matches the
/// `*.parquet`/`*.tmp` rollup globs or the `*.ndjson` WAL scan, and can
/// never be merged again (which would duplicate its rows — the merge path
/// has no dedup by design, since two identical log lines are distinct
/// events).
///
/// A missing source is success: the goal — "this file is no longer present as
/// a re-mergeable input" — is already satisfied if it's gone. This keeps the
/// op idempotent so a recovery pass that replays a marker after a partial
/// retirement loop (some inputs already deleted) doesn't wedge on a phantom.
/// A failed rename whose source vanished from under us (lost a delete race) is
/// likewise fine; only a genuine non-`NotFound` rename failure is a hard error,
/// because then the file still matches its glob and would be merged again.
pub(crate) fn retire_merged_input(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    let mut aside = path.as_os_str().to_owned();
    aside.push(".merged");
    match std::fs::rename(path, PathBuf::from(&aside)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "failed to retire merged input {}: {e}",
            path.display()
        )),
    }
}

/// Outcome of one per-service daily rollup.
///
/// Carries `quarantined` alongside `result` so the data-loss count survives a
/// hard merge error: quarantining renames the bad input to `.corrupt`, so a
/// later retry can't re-see (and re-count) it — if a `result: Err` dropped the
/// tally, those quarantines would never reach the dashboard counter.
#[derive(Debug, Clone)]
struct RollupOutcome {
    /// Inputs quarantined this call (corrupt/truncated → `.corrupt`).
    /// Counts as data-loss against the compaction error counter, whether or not
    /// the merge itself then succeeded.
    quarantined: u64,
    /// Whether the merge wrote a daily file (`Ok`) or failed and must retry
    /// next tick (`Err`).
    result: Result<(), String>,
}

/// Blocking: merge hourly parquet files for one service into a daily file.
///
/// Opens an in-memory `DuckDB` connection, reads all hourly files via
/// `read_parquet([list], union_by_name=true)`, sorts by timestamp for
/// optimal row group statistics, writes to `.tmp`, then atomic-renames.
///
/// Uses a `.rollup-{service}` marker file for crash safety: the marker
/// lists which hourly files are being merged, enabling recovery without
/// re-reading already-consolidated data.
///
/// Thin wrapper over [`rollup_day_inner`] that pairs the accumulated quarantine
/// count with the merge result, so the count is reported even when the merge
/// then errors — every `?` bail-out in the inner body would otherwise drop it.
#[cfg(test)]
fn rollup_day_blocking(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
    memory_limit: &str,
) -> RollupOutcome {
    rollup_day_coordinated(day_dir, service, hourly_files, memory_limit, None)
}

fn rollup_day_coordinated(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
    memory_limit: &str,
    publication: Option<&PublicationGate>,
) -> RollupOutcome {
    let mut quarantined: u64 = 0;
    let result = rollup_day_inner(
        day_dir,
        service,
        hourly_files,
        memory_limit,
        &mut quarantined,
        publication,
    );
    RollupOutcome {
        quarantined,
        result,
    }
}

/// The fallible body of one per-service rollup. Increments `*quarantined` as
/// corrupt inputs are renamed aside; [`rollup_day_coordinated`] pairs that running
/// count with this `Result` so a mid-merge `Err` can't lose it.
#[allow(clippy::too_many_lines)] // keep the publication and retirement order together
fn rollup_day_inner(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
    memory_limit: &str,
    quarantined: &mut u64,
    publication: Option<&PublicationGate>,
) -> Result<(), String> {
    let rollup_start = std::time::Instant::now();
    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;

    // Point DuckDB temp directory at the PVC so spill-to-disk works on
    // read-only container overlay filesystems.
    conn.execute_batch(&format!(
        "SET temp_directory='{}'",
        day_dir.to_string_lossy().replace('\'', "''")
    ))
    .map_err(|e| format!("SET temp_directory failed: {e}"))?;

    // Cap memory and threads — same rationale as compact_service_blocking.
    conn.execute_batch(&format!(
        "SET memory_limit='{}'; SET threads=2",
        memory_limit.replace('\'', "''")
    ))
    .map_err(|e| format!("SET memory_limit/threads failed: {e}"))?;

    // Every conform in this connection reads the session zone.
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .map_err(|e| format!("SET TimeZone failed: {e}"))?;

    // Build the merge input list: all hourly files, plus the existing
    // day-level file (late-arriving data merges into it after a prior
    // rollup). Validate each input and quarantine truncated/corrupt files
    // so one bad file (e.g. a crash mid-COPY) doesn't wedge the rollup
    // forever. `merged_hourly` tracks the valid hourlies we will delete on
    // success — quarantined files are already renamed away.
    let canonical_path = day_dir.join(format!("{service}.parquet"));
    let mut all_files: Vec<PathBuf> = Vec::with_capacity(hourly_files.len() + 1);
    let mut merged_hourly: Vec<PathBuf> = Vec::with_capacity(hourly_files.len());
    for f in hourly_files {
        if is_valid_parquet(f) {
            all_files.push(f.clone());
            merged_hourly.push(f.clone());
        } else {
            // A failed quarantine is a hard error — the bad file still
            // matches `*.parquet` and would wedge the rollup forever.
            let _publication_guard = publication.map(PublicationGate::blocking_write);
            quarantine_file(f, service, "rollup_quarantine", QuarantineKind::Parquet)?;
            *quarantined += 1;
        }
    }
    if canonical_path.exists() {
        if is_valid_parquet(&canonical_path) {
            all_files.push(canonical_path.clone());
        } else {
            let _publication_guard = publication.map(PublicationGate::blocking_write);
            quarantine_file(
                &canonical_path,
                service,
                "rollup_quarantine",
                QuarantineKind::Parquet,
            )?;
            *quarantined += 1;
        }
    }

    // Every input was corrupt and has been quarantined — nothing readable
    // to merge, and nothing left to retry. This is data loss: a whole
    // service/day produced no daily file. Surface it distinctly at error
    // level (the partial-corrupt path only logs a cheerful rollup_complete)
    // and report the quarantine count so it lands on the dashboard counter.
    if all_files.is_empty() {
        if *quarantined > 0 {
            tracing::error!(
                event_type = "rollup_data_loss",
                compact_service = %service,
                quarantined = *quarantined,
                "all rollup inputs corrupt; quarantined, no daily file produced — DATA LOSS"
            );
        }
        return Ok(());
    }

    // Path provenance: ingest validates every service name against
    // [A-Za-z0-9._-] (no quote, no space, no leading dot) and data_dir is
    // operator-trusted, so direct interpolation cannot inject.
    let file_list_sql = all_files
        .iter()
        .map(|f| format!("'{}'", f.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
    let tmp_path = day_dir.join(format!("{service}.parquet.tmp"));

    // Read, merge, sort by timestamp, and write to tmp file. Every hourly
    // file is write-time conformant to the field catalog (ADR-0009), so
    // `union_by_name` cannot hit a type conflict on trawl-written files — a
    // conversion-class failure here means foreign parquet in the tree, and
    // errors loudly (inputs retained, retried next tick) rather than being
    // silently rewritten by a VARCHAR cast.
    let fast = conn.execute_batch(&format!(
        "COPY (\
             SELECT * FROM read_parquet([{file_list_sql}], union_by_name=true) \
             ORDER BY \"_time\"\
         ) TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY, \
             BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
        tmp_path.to_string_lossy(),
    ));
    if let Err(e) = fast {
        // A file that passed `is_valid_parquet`'s magic-byte sniff but
        // fails `read_parquet` — corrupt middle, or a schema the catalog
        // invariant says trawl cannot have written. No amount of retrying
        // repairs either — surface it distinctly at error level so the rare
        // forever-retry is visible on the dashboard rather than buried in
        // the count.
        tracing::error!(
            event_type = "rollup_unreadable",
            compact_service = %service,
            error = %e,
            "rollup inputs unreadable or nonconformant (foreign parquet?); will retry indefinitely"
        );
        return Err(format!("rollup COPY failed: {e}"));
    }

    // Keep marker publication, daily publication, and hourly retirement
    // invisible to readers until the complete generation is ready.
    let publication_guard = publication.map(PublicationGate::blocking_write);
    let marker = rollup_marker_path(day_dir, service);
    if let Some(gate) = publication {
        gate.mark_rollup(&marker);
    }
    write_rollup_marker(day_dir, service, &merged_hourly)?;

    // Atomic rename.
    std::fs::rename(&tmp_path, &canonical_path)
        .map_err(|e| format!("rollup rename failed: {e}"))?;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(gate) = publication {
        gate.hold_after_publish_for_test();
    }

    // Delete the hourly source files that were merged. If a delete fails,
    // rename the file aside (`.merged`) so it can never be re-merged into a
    // later rollup — the merge path has no dedup, so a surviving hourly would
    // silently duplicate every one of its rows on the next tick. A failed
    // rename-aside is a hard error: the file still matches `*.parquet`.
    for f in &merged_hourly {
        retire_merged_input(f)?;
    }

    // Remove marker — rollup fully complete.
    delete_rollup_marker(day_dir, service);
    if let Some(gate) = publication {
        gate.finish_rollup(&marker);
    }
    drop(publication_guard);

    let output_bytes = std::fs::metadata(&canonical_path).map_or(0, |m| m.len());

    let duration_ms = rollup_start.elapsed().as_millis();
    tracing::info!(
        event_type = "rollup_complete",
        compact_service = %service,
        output = %canonical_path.display(),
        hourly_files = merged_hourly.len(),
        output_bytes,
        duration_ms,
        "daily rollup complete"
    );

    Ok(())
}

/// Outcome of one per-service WAL compaction batch.
///
/// Mirrors [`RollupOutcome`]: carries `quarantined` alongside `result` so the
/// data-loss count survives a hard error after files were already set aside.
/// Quarantining renames the corrupt file to `.corrupt`, so a retry can't
/// re-see (and re-count) it — if a `result: Err` dropped the tally, those
/// quarantines would never reach the dashboard counter.
struct CompactOutcome {
    /// WAL files quarantined this batch (corrupt → `.corrupt`), counted as
    /// data-loss whether or not the rest of the batch then compacted.
    quarantined: u64,
    /// Whether the batch compacted to parquet (`Ok`) or failed and must retry
    /// next tick (`Err`). `Ok(Some(_))` is a published batch whose marker
    /// could not be removed: see [`PublishIncomplete`].
    result: Result<Option<PublishIncomplete>, String>,
}

/// A publish whose output is renamed into place and whose hot batches are
/// drained, but whose publication marker stays: a later step (output
/// directory fsync, WAL retirement, WAL directory fsync or marker removal)
/// failed. The marker blocks the service until recovery completes the
/// publish, so the consumed WAL is never merged again.
#[derive(Debug)]
struct PublishIncomplete {
    /// The failed attempt's owner on the operation-failure counter.
    operation: CompactionOperation,
    error: String,
}

/// Compact a batch of WAL files for a single service into parquet,
/// enforcing the write-time invariant (ADR-0009): a parquet file is never
/// written before its columns' pins are durable in postgres.
///
/// Four phases:
/// 1. blocking — read the WAL into `wal_batch`, `DESCRIBE` it, and derive
///    pin proposals for unpinned columns (candidate ladder over values).
/// 2. async — `pin_missing` the proposals; the authoritative pins come
///    back and are folded into the in-process cache (skipped entirely when
///    the batch proposes nothing — the common case). A store failure is `Err`:
///    the WAL is retained and retried next tick — never an unconformant
///    parquet write.
/// 3. blocking — conform `wal_batch` to the pins (`TRY_CAST` on mismatch,
///    drop deferred all-null unpinned columns), then merge/sort/COPY, and
///    publish under a publication marker: rename, drain, retire the consumed
///    WAL (see [`publish_output`]).
/// 4. async, best-effort — record `field_conflicts`, touch
///    `field_services`, bump metrics. A failure here warns and moves on
///    (the data is already durable and conformant).
///
/// With `catalog: None` (tests, embedded-style callers) the pins are the
/// envelope seed plus this batch's own proposals — same conform algebra,
/// no persistence and no invariant across processes.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // internal API, config struct is overkill here
async fn compact_service_batch(
    wal_files: &[PathBuf],
    data_dir: &Path,
    wal_dir: &Path,
    env: &str,
    service: &str,
    memory_limit: &str,
    catalog: Option<&CatalogContext>,
    hot_buffer: Option<Arc<HotBuffer>>,
    batch_ids: Vec<String>,
) -> CompactOutcome {
    let wal_files = wal_files.to_vec();
    let data_dir_owned = data_dir.to_path_buf();
    let service_owned = service.to_owned();
    let memory_limit = memory_limit.to_owned();
    let known = catalog.map_or_else(HashMap::new, |c| c.cache.snapshot());

    // Phase 1: read + infer + propose (blocking). `known` comes back out of
    // the closure rather than being cloned into it: phase 2 folds this
    // batch's new pins into it instead of re-reading the whole catalog.
    // The quarantine list is shared, not returned, so a panic after a
    // quarantine still leaves the list for the drain.
    let quarantined = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let phase1 = tokio::task::spawn_blocking({
        let quarantined = Arc::clone(&quarantined);
        move || {
            // parking_lot does not poison: an unwinding panic releases the
            // lock and keeps what was pushed.
            let result = prepare_service_batch(
                &wal_files,
                &data_dir_owned,
                &service_owned,
                &memory_limit,
                &mut quarantined.lock(),
                &known,
            );
            (result, known)
        }
    })
    .await;
    let quarantine_drain = QuarantineDrain {
        hot_buffer: hot_buffer.clone(),
        env,
        quarantined: std::mem::take(&mut *quarantined.lock()),
    };
    let quarantined = quarantine_drain.count();
    let (prep_result, known) = match phase1 {
        Ok(v) => v,
        Err(e) => {
            quarantine_drain.run().await;
            return CompactOutcome {
                quarantined,
                result: Err(crate::error::join_failure_text("compaction", e)),
            };
        }
    };
    let prep = match prep_result {
        Ok(Some(p)) => p,
        // All inputs corrupt — data loss surfaced via the quarantine count.
        Ok(None) => {
            if let Some(buf) = &hot_buffer {
                let gate = buf.publication();
                let _guard = gate.write().await;
                let ids: Vec<&str> = batch_ids.iter().map(String::as_str).collect();
                buf.drain(&ids);
            }
            return CompactOutcome {
                quarantined,
                result: Ok(None),
            };
        }
        Err(e) => {
            quarantine_drain.run().await;
            return CompactOutcome {
                quarantined,
                result: Err(e),
            };
        }
    };

    // Phase 2: pins become durable BEFORE any parquet write.
    let pins = match catalog {
        Some(cat) => match resolve_pins_durable(cat, known, &prep.proposals).await {
            Ok(pins) => pins,
            Err(e) => {
                quarantine_drain.run().await;
                return CompactOutcome {
                    quarantined,
                    result: Err(format!(
                        "field catalog unavailable, batch retained for retry: {e}"
                    )),
                };
            }
        },
        None => local_pins(&prep.proposals),
    };

    // Phase 3: conform + write + publish (blocking).
    let data_dir_owned = data_dir.to_path_buf();
    let wal_dir_owned = wal_dir.to_path_buf();
    let env_owned = env.to_owned();
    let service_owned = service.to_owned();
    let phase3 = tokio::task::spawn_blocking(move || {
        conform_and_publish(
            prep,
            &pins,
            &PublishTarget {
                data_dir: &data_dir_owned,
                wal_dir: &wal_dir_owned,
                env: &env_owned,
            },
            &service_owned,
            hot_buffer.as_deref(),
            &batch_ids,
        )
    })
    .await;
    let (report, incomplete) = match phase3 {
        Ok(Ok(published)) => published,
        Ok(Err(e)) => {
            quarantine_drain.run().await;
            return CompactOutcome {
                quarantined,
                result: Err(e),
            };
        }
        Err(e) => {
            quarantine_drain.run().await;
            return CompactOutcome {
                quarantined,
                result: Err(crate::error::join_failure_text("compaction", e)),
            };
        }
    };

    // Phase 4: bookkeeping (best-effort — the parquet is already durable).
    // Runs for an incomplete publish too: its rows are published, and
    // recovery finishing it later does not repeat the bookkeeping.
    record_conflict_metrics(service, &report.conflicts);
    if let Some(cat) = catalog {
        record_batch_bookkeeping(cat, service, &report).await;
    }

    CompactOutcome {
        quarantined,
        result: Ok(incomplete),
    }
}

/// The hot batch id of a WAL file: `{env}/{file stem}`, as ingest names it.
fn wal_batch_id(env: &str, wal_file: &Path) -> Option<String> {
    Some(format!("{env}/{}", wal_file.file_stem()?.to_str()?))
}

/// The hot batches of the WAL files one chunk quarantined in phase 1.
///
/// A quarantined file is renamed to `.corrupt`, out of every later scan, so
/// no later chunk can name its batch again. A publish drains it with the
/// rest of the chunk, since the chunk's batch ids name every input; the
/// all-corrupt branch drains the whole chunk. A chunk that fails (or
/// panics) after a quarantine drains nothing, so [`Self::run`] drains
/// these; otherwise
/// their charge would stay until restart. The quarantined rows never reach
/// cold storage, which is outside ADR-0041's publication guarantee.
struct QuarantineDrain<'a> {
    hot_buffer: Option<Arc<HotBuffer>>,
    env: &'a str,
    quarantined: Vec<PathBuf>,
}

impl QuarantineDrain<'_> {
    fn count(&self) -> u64 {
        self.quarantined.len() as u64
    }

    /// Drain under the publication write guard, as every drain does. The
    /// caller holds the repin corpus guard (ADR-0026 lock order: corpus,
    /// then publication).
    async fn run(&self) {
        let Some(buf) = &self.hot_buffer else {
            return;
        };
        if self.quarantined.is_empty() {
            return;
        }
        let ids: Vec<String> = self
            .quarantined
            .iter()
            .filter_map(|f| wal_batch_id(self.env, f))
            .collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let gate = buf.publication();
        let _guard = gate.write().await;
        buf.drain(&ids);
    }
}

/// Make the batch's pin proposals durable and return the authoritative pins
/// for this batch's columns, folding the new ones into the in-process cache
/// on the way.
///
/// `known` is the cache snapshot phase 1 proposed against, and the two
/// together cover every column of the batch by construction: `propose_pins`
/// emits a proposal for exactly the columns `known` did not already pin, and
/// `pin_missing` returns the authoritative pin for each proposal (a racing
/// batch's pin, not ours, when it got there first). Only a repin cutover
/// retypes a standing pin, and it holds the corpus gate against this whole
/// batch, so a `known` entry cannot go stale meanwhile.
///
/// Deliberately not a `load_pins` + `replace` per batch. A pin row exists
/// for every distinct JSON key ever ingested, so reloading it per chunk per
/// service per tick is O(catalog) work whose size a noisy or hostile sender
/// picks — up to [`crate::store::MAX_PINNED_FIELDS`], which is what keeps
/// that "up to" a number at all. The common case (nothing new to pin) costs
/// no postgres round trip at all.
async fn resolve_pins_durable(
    cat: &CatalogContext,
    mut known: HashMap<String, CanonicalType>,
    proposals: &[PinProposal],
) -> Result<HashMap<String, CanonicalType>, crate::store::StoreError> {
    if proposals.is_empty() {
        return Ok(known);
    }
    let pinned = cat.store.pin_missing(proposals).await?;
    if !pinned.is_empty() {
        cat.cache.merge(pinned.iter().map(|(f, t)| (f.clone(), *t)));
        known.extend(pinned);
    }
    Ok(known)
}

/// The no-catalog pin map: the declared envelope plus this batch's own
/// proposals. Same conform algebra as production, no persistence.
fn local_pins(proposals: &[PinProposal]) -> HashMap<String, CanonicalType> {
    let mut pins: HashMap<String, CanonicalType> = trawl_core::schema::ENVELOPE_TYPES
        .iter()
        .map(|(f, t)| ((*f).to_owned(), *t))
        .collect();
    for p in proposals {
        pins.entry(p.field.clone()).or_insert(p.ty);
    }
    pins
}

/// Bump the conflict counters (bounded by the shared service-label cap;
/// never a field-name label).
pub(crate) fn record_conflict_metrics(service: &str, conflicts: &[FieldConflict]) {
    if conflicts.is_empty() {
        return;
    }
    let label = crate::metrics::repair_service_label(service);
    let nulled: u64 = conflicts.iter().map(|c| c.rows_nulled).sum();
    metrics::counter!(
        crate::metrics::CATALOG_CONFLICTS_TOTAL,
        "service" => label.clone()
    )
    .increment(conflicts.len() as u64);
    if nulled > 0 {
        metrics::counter!(
            crate::metrics::CATALOG_ROWS_NULLED_TOTAL,
            "service" => label
        )
        .increment(nulled);
    }
}

/// Attempts a bookkeeping write gets before it is given up on, and the base
/// of its exponential backoff.
///
/// The parquet is already durable when these run, so a failure cannot fail
/// the batch — but it can leave a permanent hole: `field_services` is the
/// authority behind `?service=` and the `last_seen` window, and a lost
/// observation is re-made only when that service next sends that field,
/// which for a field it has stopped sending is never. A momentary postgres
/// blip (failover, restart, a full pool) is therefore worth riding out
/// in-tick — inside the wall-clock budget below, which is what keeps a
/// sustained outage from stalling anything.
const BOOKKEEPING_ATTEMPTS: u32 = 3;
/// Base delay between bookkeeping attempts; doubles per attempt.
const BOOKKEEPING_BACKOFF: Duration = Duration::from_millis(100);

/// Wall-clock ceiling on one batch's bookkeeping, retries and all.
///
/// The attempt count alone bounds nothing: a postgres outage does not fail
/// fast, it blocks each write for the pool's whole acquire timeout (10s,
/// [`crate::store`]), so retries would cost attempts × 10s per batch. And
/// bookkeeping is awaited inline in phase 4 of a chunk while `compact_once`
/// walks envs → services → chunks sequentially with no per-tick deadline —
/// so that cost multiplies by the number of pending batches, against a
/// 10s compaction interval. A stalled compactor is the one failure this
/// file will not take: WAL stops draining, the hot buffer fills, and
/// admission refuses new events (ADR-0043).
///
/// So the retry gets a budget instead of a promise. The blips it exists
/// for (a full pool, a failover mid-query) fail in milliseconds and still
/// get every attempt; an outage costs one budget per batch no matter what
/// the pool's acquire timeout is.
const BOOKKEEPING_BUDGET: Duration = Duration::from_secs(2);

/// Run one bookkeeping write, retrying a transient failure.
///
/// Both writes are safe to repeat after a failure: `touch_services` is an
/// upsert and `record_conflicts` commits atomically. The one case a retry
/// can double is a lost ack (postgres committed, the answer never arrived),
/// which over-counts a `row_count` that is already an approximation or
/// re-appends conflict evidence the per-field trim bounds anyway — both
/// strictly better than the gap the retry exists to prevent.
async fn retry_bookkeeping<F, Fut>(write: BookkeepingWrite, service: &str, mut attempt: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), crate::store::StoreError>>,
{
    for n in 1..=BOOKKEEPING_ATTEMPTS {
        let Err(e) = attempt().await else {
            return;
        };
        if n == BOOKKEEPING_ATTEMPTS {
            tracing::warn!(
                event_type = "catalog_bookkeeping_error",
                compact_service = %service,
                write = write.table(),
                attempts = n,
                error = %e,
                "catalog bookkeeping write failed and was given up on"
            );
            return;
        }
        tracing::debug!(
            event_type = "catalog_bookkeeping_retry",
            compact_service = %service,
            write = write.table(),
            attempt = n,
            error = %e,
            "catalog bookkeeping write failed; retrying"
        );
        tokio::time::sleep(BOOKKEEPING_BACKOFF * 2u32.pow(n - 1)).await;
    }
}

/// Catalog bookkeeping after a successful conformant write: conflict rows
/// and per-service field observations. Retried under a shared budget, then
/// warned — the parquet is already durable and conformant, so retrying the
/// batch for a bookkeeping error would duplicate data.
async fn record_batch_bookkeeping(cat: &CatalogContext, service: &str, report: &WriteReport) {
    let store = &cat.store;
    let conflicts = &report.conflicts;
    let observed = &report.observed_fields;
    let rows = report.batch_rows;
    let in_flight = InFlight::new();
    let writes = async {
        in_flight.set(BookkeepingWrite::Conflicts);
        retry_bookkeeping(BookkeepingWrite::Conflicts, service, move || {
            store.record_conflicts(conflicts)
        })
        .await;
        in_flight.set(BookkeepingWrite::Observations);
        retry_bookkeeping(BookkeepingWrite::Observations, service, move || {
            store.touch_services(service, observed, rows)
        })
        .await;
    };
    budgeted_bookkeeping(service, &in_flight, writes).await;
}

/// Which of the two bookkeeping writes is running right now, so the budget's
/// timeout can name the one it abandoned.
///
/// The two writes share ONE budget, so the timeout fires on the composed
/// future and has no idea by itself which half was still going. An atomic
/// rather than a `Cell` because that composed future is held across awaits
/// by `tokio::time::timeout` inside a compaction task, and it must stay
/// `Send`.
struct InFlight(std::sync::atomic::AtomicU8);

impl InFlight {
    fn new() -> Self {
        Self(std::sync::atomic::AtomicU8::new(0))
    }

    fn set(&self, write: BookkeepingWrite) {
        let code = match write {
            BookkeepingWrite::Conflicts => 0,
            BookkeepingWrite::Observations => 1,
        };
        self.0.store(code, Ordering::Relaxed);
    }

    fn get(&self) -> BookkeepingWrite {
        match self.0.load(Ordering::Relaxed) {
            1 => BookkeepingWrite::Observations,
            _ => BookkeepingWrite::Conflicts,
        }
    }
}

/// Run a batch's bookkeeping writes under [`BOOKKEEPING_BUDGET`], dropping
/// them when it runs out so compaction can get on with the next chunk.
///
/// Cancelling mid-write is safe for the same reason the retry is: an
/// abandoned write is at worst a lost ack on an idempotent upsert or an
/// atomic, per-field-trimmed conflict insert.
///
/// The counter fires ONLY here, on the elapsed budget. A write that fails
/// fast and exhausts its three attempts is a different failure with a
/// different remedy, and it keeps the `catalog_bookkeeping_error` warning it
/// has always had. One consequence worth stating: because the two writes
/// share one budget, a first write that hangs forever means the second never
/// starts, so a full outage costs one increment on `conflicts` per batch and
/// none at all on `observations`.
async fn budgeted_bookkeeping<Fut: std::future::Future<Output = ()>>(
    service: &str,
    in_flight: &InFlight,
    writes: Fut,
) {
    if tokio::time::timeout(BOOKKEEPING_BUDGET, writes)
        .await
        .is_err()
    {
        let write = in_flight.get();
        metrics::counter!(
            crate::metrics::CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL,
            "write" => write.label(),
        )
        .increment(1);
        tracing::warn!(
            event_type = "catalog_bookkeeping_timeout",
            compact_service = %service,
            write = write.table(),
            budget_ms = BOOKKEEPING_BUDGET.as_millis(),
            "catalog bookkeeping exceeded its budget and was abandoned so \
             compaction keeps draining the WAL"
        );
    }
}

/// Count rows in a `DuckDB` table. Used to capture row counts before
/// dropping temporary tables.
fn count_rows(conn: &duckdb::Connection, table: &str) -> Result<u64, String> {
    conn.query_row(
        &format!("SELECT count(*)::BIGINT FROM \"{table}\""),
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| u64::try_from(n).unwrap_or(0))
    .map_err(|e| format!("count_rows failed: {e}"))
}

/// Read WAL ndjson files into a `DuckDB` temp table called `wal_batch`,
/// returning the number of files that actually contributed rows.
///
/// Fast path: one multi-file `read_json` over the whole batch. If that fails
/// for a non-recoverable reason (a malformed-but-textual file that slipped
/// past [`is_valid_ndjson`]'s NUL/UTF-8 sniff — e.g. a clean truncation
/// mid-token), it isolates the offender: probe each file alone, quarantine
/// the ones that throw, and rebuild from the survivors. This makes a single
/// poison-pill file unable to wedge the whole batch (the residual the byte
/// sniff alone could not close). Each file set aside is pushed onto
/// `quarantined`. Returns the files that contributed rows, empty when every
/// file turned out corrupt (the caller treats that as data-loss, not error).
///
/// Complex-typed columns cannot arise here: ingest canonicalization
/// stringifies top-level object/array values before the WAL is written, and
/// the conform phase pins every column to a canonical scalar type before
/// the parquet write (ADR-0009).
fn read_wal_to_table(
    conn: &duckdb::Connection,
    wal_files: &[PathBuf],
    service: &str,
    quarantined: &mut Vec<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    // Fast path: read the whole batch in one scan. The common case.
    match build_wal_batch(conn, wal_files, service) {
        Ok(()) => return Ok(wal_files.to_vec()),
        Err(e) => {
            // A non-"Duplicate name" read error means at least one file is
            // malformed-but-textual. Without isolation that one file fails
            // the whole batch and wedges the service forever. Isolate it.
            tracing::warn!(
                event_type = "compaction_isolation",
                compact_service = %service,
                error = %e,
                "multi-file WAL read failed; isolating corrupt files"
            );
        }
    }

    // Clear any partial table left by the failed multi-file attempt.
    let _ = conn.execute_batch("DROP TABLE IF EXISTS wal_batch");

    let mut survivors: Vec<PathBuf> = Vec::with_capacity(wal_files.len());
    for f in wal_files {
        if probe_ndjson(conn, f).is_ok() {
            survivors.push(f.clone());
        } else {
            quarantine_file(f, service, "compaction_quarantine", QuarantineKind::Wal)?;
            quarantined.push(f.clone());
        }
    }

    if survivors.is_empty() {
        return Ok(survivors);
    }

    // Rebuild from the survivors. Each parsed cleanly alone, so a residual
    // failure here is a genuine cross-file issue (e.g. schema), not a single
    // poison pill — surface it as Err to retry next tick.
    build_wal_batch(conn, &survivors, service)
        .map_err(|e| format!("{e} (after isolating corrupt files)"))?;
    Ok(survivors)
}

/// Synthetic column carrying each row's source WAL file path
/// (`read_json(..., filename='_trawl_wal_file')`). Named — not the literal
/// `filename=true` form — so a user event legitimately carrying a `filename`
/// field cannot trip the "Duplicate name" fallback, and excluded from
/// `wal_batch` so it never reaches parquet.
///
/// The name is reserved, not unreachable: ingest's reserved-prefix strip
/// (`envelope::canonicalize`) stops a client planting it, and a WAL file
/// that carries it anyway drains losslessly through a renamed provenance
/// column (see [`build_wal_batch`]) instead of wedging forever.
pub(super) const WAL_FILE_COL: &str = "_trawl_wal_file";

/// SQL expression producing a never-NULL TIMESTAMP `column` for a WAL row
/// (ADR-0008: the partition key is never hard-CAST).
///
/// Three arms: `TRY_CAST` the raw value (always succeeds on the values
/// ingest canonicalized); else recover the ingest instant from the row's
/// own WAL filename (`{service}_{unix_millis}_{4_hex}`), which drains a
/// hand-written or damaged file with no operator step; else the compaction
/// instant — a NULL partition key would sort first and fall outside every
/// `last=Xh` filter, a silent failure of its own.
fn repair_expr(prov_col: &str, column: &str) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f");
    format!(
        "COALESCE(\
             TRY_CAST(\"{column}\" AS TIMESTAMP), \
             epoch_ms(TRY_CAST(regexp_extract({prov_col}, \
                 '_([0-9]+)_[0-9a-f]{{4}}\\.ndjson$', 1) AS BIGINT)), \
             TIMESTAMP '{now}'\
         ) AS \"{column}\""
    )
}

/// Body of the `REPLACE (...)` clause repairing every envelope TIMESTAMP
/// column (`trawl_core::schema::TIMESTAMP_COLUMNS`) with the same ladder,
/// so neither can wedge a batch or land as VARCHAR in parquet. Ingest always
/// stamps `_ingested`, so its fallback arms fire only on hand-written or
/// damaged WAL.
fn timestamp_repair_list(prov_col: &str) -> String {
    trawl_core::schema::TIMESTAMP_COLUMNS
        .iter()
        .map(|column| repair_expr(prov_col, column))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The nested-key collision shape: keys inside a nested object that
/// collide (case-insensitively) when `DuckDB` builds the auto-detected
/// STRUCT at `maximum_depth=2`. Verified by execution: only reachable via
/// nested values, which ingest stringifies (ADR-0009) — so this fires only
/// on hand-written WAL, and the depth-1 retry ([`build_wal_batch`]) drains
/// it losslessly.
fn is_duplicate_name(e: &duckdb::Error) -> bool {
    e.to_string().contains("Duplicate name")
}

/// The collision shape specific to the `filename=` option: the data itself
/// carries a column named like the synthetic provenance column
/// ([`WAL_FILE_COL`]). Ingest strips that key, so only a hand-written WAL
/// file reaches this — and it would fail the read on every tick forever,
/// which is why the retry under a renamed provenance column exists.
fn is_filename_collision(e: &duckdb::Error) -> bool {
    e.to_string().contains("adds column")
}

/// Build the `wal_batch` table from a multi-file `read_json`.
///
/// A lossless ladder — no rung drops a column. The simpler recovery, falling
/// back to a fixed envelope-column list, is wrong: it loses every custom
/// column for the whole batch.
///
/// 1. Auto-detection (`maximum_depth=2`) with [`WAL_FILE_COL`] as the
///    provenance column.
/// 2. On a `filename=` collision ([`is_filename_collision`] — a WAL file
///    carrying the reserved key itself), the same auto-detect read under
///    a randomized provenance name. Lossless: every user field survives, and
///    the row's literal `_trawl_wal_file` value lands in parquet as ordinary
///    data — it never feeds [`repair_expr`].
/// 3. On a nested-key collision ([`is_duplicate_name`] — only reachable
///    from WAL whose nested values ingest never stringified), the same
///    read at `maximum_depth=1`: every top-level field arrives as an opaque
///    JSON column and the catalog conform step types it via the lattice. No
///    column is ever dropped.
///
/// Any other read error is returned so the caller can isolate the offending
/// file. A malformed `timestamp` is never fatal here: see
/// [`repair_expr`].
///
/// `sample_size=-1` (schema detection over every row, not `DuckDB`'s default
/// ~20480-row prefix) is what keeps a sparse column alive: `_repairs` rides
/// only on repaired events, so on a WAL file bigger than the sample it would
/// fall outside the inferred schema and `union_by_name` would drop it with no
/// error at all, losing the evidence ADR-0008 promises to preserve. Any
/// sparse user field is equally exposed.
fn build_wal_batch(
    conn: &duckdb::Connection,
    wal_files: &[PathBuf],
    service: &str,
) -> Result<(), String> {
    let file_list_sql = wal_files
        .iter()
        .map(|p| format!("'{}'", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
    let auto_read = |prov_col: &str, depth: i32| {
        let repair = timestamp_repair_list(prov_col);
        conn.execute_batch(&format!(
            "CREATE TABLE wal_batch AS \
             SELECT * EXCLUDE ({prov_col}) REPLACE ({repair}) \
             FROM read_json([{file_list_sql}], format='newline_delimited', \
             records=true, auto_detect=true, union_by_name=true, \
             field_appearance_threshold=0, maximum_depth={depth}, sample_size=-1, \
             filename='{prov_col}')"
        ))
    };
    // Depth 2, then — on a nested-key collision — depth 1, where nothing
    // can collide (nested values stay opaque JSON; the conform step types
    // them via the lattice, so batch-mates keep every column).
    let read_with_depth_ladder = |prov_col: &str| match auto_read(prov_col, 2) {
        Ok(()) => Ok(()),
        Err(e) if is_duplicate_name(&e) => {
            tracing::warn!(
                event_type = "compaction_depth_fallback",
                compact_service = %service,
                error = %e,
                "nested keys collide at depth 2 (legacy pre-stringification \
                 WAL); retrying at maximum_depth=1 — no columns dropped"
            );
            let _ = conn.execute_batch("DROP TABLE IF EXISTS wal_batch");
            auto_read(prov_col, 1)
        }
        Err(e) => Err(e),
    };

    match read_with_depth_ladder(WAL_FILE_COL) {
        Ok(()) => Ok(()),
        Err(e) if is_filename_collision(&e) => {
            // Randomized so the data itself cannot collide with it too.
            let alt = format!("{WAL_FILE_COL}_{:08x}", rand::random::<u32>());
            tracing::warn!(
                event_type = "compaction_provenance_rename",
                compact_service = %service,
                error = %e,
                "WAL data carries the provenance column name; retrying under a renamed column"
            );
            let _ = conn.execute_batch("DROP TABLE IF EXISTS wal_batch");
            read_with_depth_ladder(&alt)
                .map_err(|e2| format!("read_json (renamed provenance) failed: {e2}"))
        }
        Err(e) => Err(format!("read_json failed: {e}")),
    }
}

/// Probe a single WAL file by fully scanning it through `read_json`.
///
/// Returns `Err` only if the file is genuinely unparseable — the
/// malformed-but-textual corruption the byte sniff can't catch. A full
/// `count(*)` scan forces every record to parse, so a malformed line anywhere
/// in the file surfaces. A "Duplicate name" collision is not corruption —
/// [`build_wal_batch`]'s depth-1 rung drains it — so a colliding file is
/// re-probed at `maximum_depth=1` and only quarantined when both depths
/// fail.
fn probe_ndjson(conn: &duckdb::Connection, file: &Path) -> Result<(), String> {
    let path = file.to_string_lossy();
    let probe_at = |depth: i32| {
        conn.query_row(
            &format!(
                "SELECT count(*) FROM read_json(['{path}'], format='newline_delimited', \
                 records=true, auto_detect=true, union_by_name=true, \
                 field_appearance_threshold=0, maximum_depth={depth})"
            ),
            [],
            |_| Ok(()),
        )
    };
    match probe_at(2) {
        Ok(()) => Ok(()),
        Err(e) if is_duplicate_name(&e) => {
            probe_at(1).map_err(|e| format!("probe read_json (depth 1) failed: {e}"))
        }
        Err(e) => Err(format!("probe read_json failed: {e}")),
    }
}

/// Column name and type from `DuckDB` `DESCRIBE`. Shared with the boot
/// conformance pass (`crate::catalog::conform`).
pub(crate) struct ColInfo {
    pub(crate) name: String,
    pub(crate) dtype: String,
}

/// Run `DESCRIBE <query>` and return the column names and types.
pub(crate) fn describe_source(
    conn: &duckdb::Connection,
    query: &str,
) -> Result<Vec<ColInfo>, String> {
    let mut stmt = conn
        .prepare(&format!("DESCRIBE {query}"))
        .map_err(|e| format!("DESCRIBE failed: {e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(ColInfo {
                name: row.get::<_, String>(0)?,
                dtype: row.get::<_, String>(1)?,
            })
        })
        .map_err(|e| format!("DESCRIBE query failed: {e}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("DESCRIBE row read failed: {e}"))
}

/// Escape a `DuckDB` identifier: wrap in double-quotes, doubling any
/// embedded double-quotes. Column names come from ingested JSON keys
/// (user-controlled), so every generated statement naming one must go
/// through this.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Merge `wal_batch` with an existing parquet file via `UNION ALL BY NAME`.
///
/// The batch is conformed to the catalog pins before this runs, and every
/// trawl-written parquet file is write-time conformant too (ADR-0009), so
/// the direct union cannot conflict on trawl's own files. A failure here
/// therefore means the existing file violates the invariant — foreign
/// parquet dropped into the tree, or a restore against a stale catalog —
/// and is surfaced as `catalog_invariant_violation`: the batch errors, the
/// WAL files are retained, and the tick retries. Do not add a cast-and-retry
/// fallback: rewriting both sides silently hides exactly the nonconformant
/// corpus this error exists to report.
fn merge_with_existing(
    conn: &duckdb::Connection,
    canonical_path: &Path,
    service: &str,
) -> Result<(), String> {
    let pq_path = canonical_path.display();

    conn.execute_batch(&format!(
        "CREATE TABLE merged AS \
         SELECT * FROM read_parquet('{pq_path}') \
         UNION ALL BY NAME \
         SELECT * FROM wal_batch",
    ))
    .map_err(|e| {
        tracing::error!(
            event_type = "catalog_invariant_violation",
            compact_service = %service,
            path = %canonical_path.display(),
            error = %e,
            "merge with existing parquet failed — write-time conformance makes this \
             impossible for trawl-written files (foreign parquet at this path?); \
             WAL retained, batch retried next tick"
        );
        format!("merge read_parquet failed: {e}")
    })
}

/// A batch staged in `DuckDB` between the read/infer phase and the
/// conform/write phase. The `Connection` moves across the two
/// `spawn_blocking` calls so the async pin phase can sit between them.
struct PreparedBatch {
    conn: duckdb::Connection,
    /// `DESCRIBE` of `wal_batch` as read.
    schema: Vec<ColInfo>,
    /// Pin proposals for columns absent from the known-pin set.
    proposals: Vec<PinProposal>,
    /// WAL files that contributed rows, in input order: the inputs minus
    /// any quarantined as corrupt. The publication marker lists exactly
    /// these, and the publish retires exactly these.
    survivors: Vec<PathBuf>,
    /// Start instant for the completion log.
    compact_start: std::time::Instant,
}

/// Outcome of a conformant write, carried to the bookkeeping phase.
#[derive(Debug)]
struct WriteReport {
    /// Rows this batch contributed, deliberately not the merged file's
    /// total: `field_services.row_count` accumulates this value, so a
    /// whole-file count would re-add every earlier batch on every tick.
    batch_rows: u64,
    /// Conform casts that constitute recordable conflicts.
    conflicts: Vec<FieldConflict>,
    /// Fields this batch actually wrote (for `field_services`) — the
    /// post-conform column set, so deferred-pin columns dropped by
    /// [`conform_wal_batch`] are not claimed as observed.
    observed_fields: Vec<String>,
}

/// Unit-test fault: panic in phase 1 right after it quarantines a file.
///
/// Phase 1 runs on a blocking-pool thread, so a thread-local switch like
/// [`publication_marker::interrupt`] cannot reach it. The switch is keyed by
/// the data directory instead, which each test owns.
#[cfg(test)]
mod phase1_panic {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static ARMED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Panic after a quarantine at or under `data_dir` until the guard
    /// drops. Phase 1 sees the env's data directory.
    pub(super) fn arm(data_dir: &Path) -> Guard {
        ARMED.lock().unwrap().push(data_dir.to_path_buf());
        Guard(data_dir.to_path_buf())
    }

    pub(super) fn after_quarantine(data_dir: &Path, quarantined: &[PathBuf]) {
        let armed = ARMED
            .lock()
            .unwrap()
            .iter()
            .any(|dir| data_dir.starts_with(dir));
        assert!(
            !armed || quarantined.is_empty(),
            "test panic in phase 1 after a quarantine"
        );
    }

    pub(super) struct Guard(PathBuf);

    impl Drop for Guard {
        fn drop(&mut self) {
            ARMED.lock().unwrap().retain(|dir| *dir != self.0);
        }
    }
}

/// Blocking phase 1: validate + read WAL files into a `wal_batch` table,
/// `DESCRIBE` it, and derive pin proposals for unpinned columns.
///
/// Returns `Ok(None)` when every input was corrupt (data loss surfaced via
/// the quarantine count — nothing to retry).
///
/// `quarantined` accumulates the corrupt WAL files set aside during this
/// batch, by their original paths (both the byte sniff and read isolation).
/// It is threaded by reference so the count, and the drain of those files'
/// hot batches ([`QuarantineDrain`]), survive an `Err` from any later step —
/// see [`CompactOutcome`]
/// (mirrors the rollup [`RollupOutcome`] pattern). Every quarantine is
/// permanent (`.corrupt` rename), so a retry can't re-count it.
fn prepare_service_batch(
    wal_files: &[PathBuf],
    data_dir: &Path,
    service: &str,
    memory_limit: &str,
    quarantined: &mut Vec<PathBuf>,
    known_pins: &HashMap<String, CanonicalType>,
) -> Result<Option<PreparedBatch>, String> {
    let compact_start = std::time::Instant::now();

    // Validate WAL inputs before they reach `read_json`. A single corrupt
    // file (e.g. a pure-NUL torn write from a hard kill) fails the whole
    // multi-file `read_json` call and, with no quarantine, head-of-line
    // blocks this service's compaction forever — re-scanned and re-failed
    // every tick. Sniff each file and move the corrupt ones aside (bytes
    // preserved), mirroring the rollup parquet-quarantine path.
    let mut valid_files: Vec<PathBuf> = Vec::with_capacity(wal_files.len());
    for f in wal_files {
        if is_valid_ndjson(f) {
            valid_files.push(f.clone());
        } else {
            quarantine_file(f, service, "compaction_quarantine", QuarantineKind::Wal)?;
            quarantined.push(f.clone());
        }
    }
    #[cfg(test)]
    phase1_panic::after_quarantine(data_dir, quarantined);

    if valid_files.is_empty() {
        // Every file in the batch was corrupt — nothing readable to compact.
        // This is data loss (torn writes are unrecoverable), not an error:
        // there is nothing to retry, so surface it via the quarantine count
        // rather than wedging. Mirrors the rollup all-corrupt branch.
        tracing::error!(
            event_type = "compaction_data_loss",
            compact_service = %service,
            quarantined = quarantined.len(),
            "all WAL files in batch were corrupt — no parquet produced, DATA LOSS"
        );
        return Ok(None);
    }

    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;

    // Point DuckDB temp directory at the PVC so spill-to-disk works on
    // read-only container overlay filesystems.
    conn.execute_batch(&format!(
        "SET temp_directory='{}'",
        data_dir.to_string_lossy().replace('\'', "''")
    ))
    .map_err(|e| format!("SET temp_directory failed: {e}"))?;

    // Cap memory usage so DuckDB spills to disk earlier rather than
    // consuming 80% of container RAM. Limit threads to reduce peak
    // memory — compaction is a background task where latency is fine.
    conn.execute_batch(&format!(
        "SET memory_limit='{}'; SET threads=2",
        memory_limit.replace('\'', "''")
    ))
    .map_err(|e| format!("SET memory_limit/threads failed: {e}"))?;

    // The pin ladder and the conform both read the session zone: a
    // TIMESTAMP candidate is scored, and written, in UTC or not at all
    // ([`trawl_core::conform::SESSION_TIME_ZONE_SQL`]).
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .map_err(|e| format!("SET TimeZone failed: {e}"))?;

    let survivors = read_wal_to_table(&conn, &valid_files, service, quarantined)?;
    if survivors.is_empty() {
        // Every sniff-passing file turned out malformed and was quarantined
        // during read isolation — data loss, not error (nothing to retry).
        tracing::error!(
            event_type = "compaction_data_loss",
            compact_service = %service,
            quarantined = quarantined.len(),
            "all WAL files corrupt after read isolation — no parquet produced, DATA LOSS"
        );
        return Ok(None);
    }

    let schema = describe_source(&conn, "SELECT * FROM wal_batch")?;
    let proposals = propose_pins(&conn, &schema, service, known_pins)?;

    Ok(Some(PreparedBatch {
        conn,
        schema,
        proposals,
        survivors,
        compact_start,
    }))
}

/// Derive pin proposals for every column not already pinned.
///
/// This is *pin on first complete batch*, not first value: mixed and
/// out-of-range inferences run the candidate [`LADDER`] over the batch's
/// actual values, and an all-null unpinned column proposes nothing (the
/// pin defers and the column is dropped from this batch's output).
/// `_time`/`_ingested` are excluded — the ADR-0008 repair ladder owns
/// them and they are already TIMESTAMP by the time this runs.
fn propose_pins(
    conn: &duckdb::Connection,
    schema: &[ColInfo],
    service: &str,
    known_pins: &HashMap<String, CanonicalType>,
) -> Result<Vec<PinProposal>, String> {
    // One pass to classify, in schema order: a type that maps straight onto
    // the canonical vocabulary pins from the DESCRIBE alone, everything else
    // needs the values and is resolved in batch below (never per column —
    // the batch is attacker-wide, see `run_pin_ladders`).
    //
    // Names are ASCII-case-folded for both the lookup and the proposal: the
    // catalog holds folded names only (ingest folds at canonicalization),
    // and a mixed-case WAL column must pin — and be conformed — under the
    // same key its data will be stored as.
    let mut candidates: Vec<(String, Option<CanonicalType>)> = Vec::new();
    let mut ladder_cols: Vec<&ColInfo> = Vec::new();
    for col in schema {
        let folded = col.name.to_ascii_lowercase();
        if trawl_core::schema::TIMESTAMP_COLUMNS.contains(&folded.as_str())
            || known_pins.contains_key(&folded)
        {
            continue;
        }
        match normalize_duckdb_type(&col.dtype) {
            TypeResolution::Pin(t) => candidates.push((folded, Some(t))),
            TypeResolution::Ladder | TypeResolution::Json => {
                candidates.push((folded, None));
                ladder_cols.push(col);
            }
        }
    }

    let mut laddered = run_pin_ladders(conn, &ladder_cols)?.into_iter();
    let mut proposals = Vec::with_capacity(candidates.len());
    for (field, direct) in candidates {
        let proposed = match direct {
            Some(t) => Some(t),
            // Parallel to `ladder_cols` by construction.
            None => laddered
                .next()
                .ok_or_else(|| "pin ladder returned fewer results than columns".to_owned())?,
        };
        if let Some(ty) = proposed {
            proposals.push(PinProposal {
                field,
                ty,
                pinned_from: service.to_owned(),
            });
        }
    }
    Ok(proposals)
}

/// How many columns one per-column aggregate query may cover.
///
/// The bound every wide-batch aggregate pass funnels through: the pin ladder
/// (`LADDER.len() + 1` aggregates per column), [`ConformPlan::tally_conflicts`]
/// (2 per cast column) and the boot scan's non-null vote weighting (1 per
/// voting column). Column count is client-chosen — `MAX_PINNED_FIELDS` bounds
/// the catalog at 10k fields — so none of them may fan out over the full
/// width.
///
/// Both extremes are pathological on a wide batch (measured by execution
/// against the bundled `DuckDB` 1.5.5, 10k columns × 50 rows): one query per
/// column is superlinear because every query re-binds the whole table (27s,
/// and ~n^1.4 in the column count), while a single query over every column
/// blows aggregate memory (a 40k-column batch OOMs outright) and is roughly
/// quadratic well before that (the 2-aggregate tally shape: 1k cols 0.68s,
/// 5k 10.6s, 10k 38.5s). Chunking is flat in both: 10k ladder columns take
/// ~1.4s and 40k ~7.9s at this width, and the peak is bounded by the chunk,
/// not the batch. 64…1024 all measure within 25% of each other, so this sits
/// in the middle.
pub(crate) const AGG_CHUNK_COLS: usize = 256;

/// How many conflicted columns one batch captures misfit samples for
/// ([`ConformPlan::capture_samples`]).
///
/// One chunk's worth, for the reason above — but the bound bites far
/// earlier here, because a sampling aggregate is not a counting one:
/// `list(DISTINCT …)` holds every distinct misfit of every sampled column
/// in memory before the slice throws all but five away, and the values are
/// client text. A batch conflicting on more columns than this is a corpus
/// with a modelling problem the first 256 fields already evidence.
const MAX_SAMPLED_CONFLICT_COLUMNS: usize = AGG_CHUNK_COLS;

/// How many distinct raw values the capture pulls per column before Rust
/// sanitises them ([`sanitize_sample`]) and re-deduplicates.
///
/// `DISTINCT` runs in `DuckDB`, on the raw text, but the values that are
/// stored are the sanitised, byte-capped ones — and both transforms can
/// collapse two distinct misfits into one (`a\x01b` and `a\x02b` both
/// become `a\u{FFFD}b`; two long values can share their first 256 bytes).
/// Pulling a small multiple of the budget keeps the slots fillable when
/// that happens. Residual, accepted: a column whose misfits differ only in
/// collapsing positions still yields fewer than [`MAX_CONFLICT_SAMPLES`] —
/// the samples are evidence of shape, and re-querying for more would cost a
/// second pass over the batch to distinguish values an operator cannot see
/// apart anyway.
const SAMPLE_CANDIDATES: usize = MAX_CONFLICT_SAMPLES * 2;

/// Run the candidate ladder over the columns' actual values, resolving up to
/// [`AGG_CHUNK_COLS`] columns per aggregate pass: per column the first
/// candidate whose `TRY_CAST` success rate over non-null values reaches
/// [`LADDER_SUCCESS_THRESHOLD`] pins; none qualifying pins `VARCHAR`.
///
/// Returns one entry per input column, in order; `None` for an all-null
/// column — that pin defers.
#[allow(clippy::cast_precision_loss)] // ratios over row counts
fn run_pin_ladders(
    conn: &duckdb::Connection,
    columns: &[&ColInfo],
) -> Result<Vec<Option<CanonicalType>>, String> {
    // Non-null count followed by one TRY_CAST count per ladder candidate.
    let stride = LADDER.len() + 1;
    let mut out = Vec::with_capacity(columns.len());
    for chunk in columns.chunks(AGG_CHUNK_COLS) {
        let sql = format!(
            "SELECT {} FROM wal_batch",
            chunk
                .iter()
                .map(|col| {
                    let q = quote_ident(&col.name);
                    let mut aggs = Vec::with_capacity(stride);
                    aggs.push(format!("count({q})::BIGINT"));
                    aggs.extend(LADDER.iter().map(|t| {
                        let expr = conform_expr(&q, &col.dtype, *t).unwrap_or_else(|| q.clone());
                        format!("count({expr})::BIGINT")
                    }));
                    aggs.join(", ")
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let counts: Vec<i64> = conn
            .query_row(&sql, [], |row| {
                let mut counts = Vec::with_capacity(chunk.len() * stride);
                for i in 0..chunk.len() * stride {
                    counts.push(row.get::<_, i64>(i)?);
                }
                Ok(counts)
            })
            .map_err(|e| format!("pin ladder query failed: {e}"))?;

        for slot in counts.chunks(stride) {
            let non_null = slot[0];
            if non_null == 0 {
                out.push(None);
                continue;
            }
            let pin = LADDER
                .iter()
                .zip(&slot[1..])
                .find(|(_, ok)| {
                    (**ok as f64) / (non_null as f64)
                        >= trawl_core::schema::LADDER_SUCCESS_THRESHOLD
                })
                .map_or(CanonicalType::Varchar, |(candidate, _)| *candidate);
            out.push(Some(pin));
        }
    }
    Ok(out)
}

/// The SQL expression conforming one column to its pin, or `None` when the
/// observed type already matches AND the conform over it is the identity
/// (pass-through).
///
/// Literally the emitter's hot-branch expression: the same text form
/// ([`trawl_core::conform::untyped_text`]) under the same guard
/// ([`trawl_core::conform::guarded_cast`]), because a value that reads one
/// way while it is hot and another once it compacts is a query whose answer
/// changes with a background timer. The `DESCRIBE`d type decides only
/// whether the column already is its pin — never how it is read, which is
/// what makes the reading independent of what `read_json` inferred for the
/// batch (see [`trawl_core::conform`] for the two rules and their
/// execution evidence).
///
/// Never choose the text form per observed type, which compaction could do
/// and the emitter cannot: under a VARCHAR pin the guard is the identity, so
/// the text form is the stored value, and `TRY_CAST(col AS VARCHAR)` spells a
/// DOUBLE `1e20` as `1e+20` where `to_json` spells it
/// `100000000000000000000.0`. Same value, two corpora, and a `note=/^1e/`
/// query that flips when the compactor runs.
pub(crate) fn conform_expr(quoted: &str, dtype: &str, pin: CanonicalType) -> Option<String> {
    // The pass-through is valid only where the conform is the identity
    // over values already of the physical type. SEVERITY is the one pin
    // where it is not: its domain is the 1-24 ladder, not "any BIGINT",
    // so a numeral outside it must still be nulled out here or the corpus
    // could hold a value with no token rendering — which is exactly what
    // the rung exists to prevent (ADR-0013).
    if dtype == pin.as_duckdb() && pin != CanonicalType::Severity {
        return None;
    }
    Some(trawl_core::conform::guarded_cast(
        &trawl_core::conform::untyped_text(quoted),
        pin,
    ))
}

/// Everything a repin needs to read one column: the target the rewrite
/// writes with, and the two wire-dialect readings whose disagreement is
/// ambiguity (ADR-0013).
///
/// Built once per job, by the engine, because the engine is the only place
/// that knows the old pin — and the old pin is what decides whether the
/// stored column holds wire text at all. A stored `SEVERITY` column holds
/// canonical `OTel` ladder positions, so its arm is dialect-free in every
/// variant; a stored VARCHAR holds whatever the sender wrote, so its arm
/// flips with the assertion exactly as the `_raw` arm does. Deriving that
/// downstream would mean guessing it from the dialects themselves, which
/// cannot tell `otel-because-canonical` from `otel-because-asserted`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RepinReading {
    /// What the rewrite writes and the plan counts with.
    pub(crate) written: RepinTarget,
    /// Whether the stored column's own text is sender text (the old pin was
    /// not `SEVERITY`), and therefore a wire position the dialect reaches.
    stored_is_wire: bool,
}

impl RepinReading {
    /// Build the reading rule from the repin's own facts.
    pub(crate) fn new(from: CanonicalType, to: CanonicalType, dialect: Dialect) -> Self {
        let stored_is_wire = from != CanonicalType::Severity;
        Self {
            written: RepinTarget {
                pin: to,
                stored: if stored_is_wire {
                    dialect
                } else {
                    Dialect::Otel
                },
                raw: dialect,
            },
            stored_is_wire,
        }
    }

    /// The `OTel`-and-syslog readings of the same columns, or `None` where
    /// the two are textually identical — every target but `SEVERITY`, whose
    /// numeric arm is the one dialect-sensitive rung. `None` is what makes
    /// the ambiguity count a constant `0` rather than a pair of full
    /// expressions `DuckDB` would evaluate to prove they agree.
    fn variants(self) -> Option<(RepinTarget, RepinTarget)> {
        if self.written.pin != CanonicalType::Severity {
            return None;
        }
        let variant = |dialect: Dialect| RepinTarget {
            pin: self.written.pin,
            stored: if self.stored_is_wire {
                dialect
            } else {
                Dialect::Otel
            },
            raw: dialect,
        };
        Some((variant(Dialect::Otel), variant(Dialect::Syslog)))
    }
}

/// The repin target column's expression, in the one place both consumers
/// read it from: [`ConformPlan::build`] under [`ConformPolicy::Repin`]
/// (the rewrite) and the repin scan's dry-run counting
/// (`crate::repin::plan`) — the plan predicts with the same SQL the
/// rewrite writes, by the same doctrine that makes the hot branch and
/// compaction one builder.
pub(crate) fn repin_target_expr(
    quoted: &str,
    has_raw: bool,
    folded: &str,
    target: RepinTarget,
) -> String {
    if has_raw {
        trawl_core::conform::resurrection_expr(
            quoted,
            &quote_ident(trawl_core::schema::RAW),
            folded,
            target,
        )
    } else {
        // No `_raw` to resurrect from, so the stored arm is the whole
        // reading, under the stored arm's own dialect.
        trawl_core::conform::guarded_cast_in(
            &trawl_core::conform::untyped_text(quoted),
            target.pin,
            target.stored,
        )
    }
}

/// The repin scan/rewrite counting expressions, built over
/// [`repin_target_expr`] so the dry-run numbers and the rewrite outcome
/// are the same computation.
#[derive(Debug, Clone)]
pub(crate) struct RepinCountExprs {
    /// Stored values the column carries.
    pub(crate) carrying: String,
    /// Stored values the new pin keeps (resurrection arm included).
    pub(crate) kept: String,
    /// Shelved (`NULL`-stored) values `_raw` gives back.
    pub(crate) resurrectable: String,
    /// Rows whose reading differs between the two dialects — the values
    /// only provenance can settle.
    pub(crate) ambiguous: String,
}

/// Build the counting expressions for one column under `reading`.
pub(crate) fn repin_count_exprs(
    quoted: &str,
    has_raw: bool,
    folded: &str,
    reading: RepinReading,
) -> RepinCountExprs {
    let target = repin_target_expr(quoted, has_raw, folded, reading.written);
    let carrying = format!("count({quoted})");
    let kept = format!("count(CASE WHEN {quoted} IS NOT NULL THEN {target} END)");
    let resurrectable = if has_raw {
        let raw_read = trawl_core::conform::guarded_cast_in(
            &trawl_core::conform::raw_extract(&quote_ident(trawl_core::schema::RAW), folded),
            reading.written.pin,
            reading.written.raw,
        );
        format!("count(CASE WHEN {quoted} IS NULL THEN {raw_read} END)")
    } else {
        "0".to_owned()
    };
    // Ambiguity is derived from the whole target expression rather than
    // spelled out over the syslog domain a second time: a row is ambiguous
    // when both wire dialects have a reading for it and they disagree. Over
    // the full expression this covers the resurrection arm for free (a raw
    // numeral recovering into a shelved column is just as ambiguous), and it
    // keeps one-dialect values out — those are visible loss the
    // `projected_nulls` count already reports, not silent mistranslation.
    let ambiguous = match reading.variants() {
        None => "0".to_owned(),
        Some((otel, syslog)) => {
            let a = repin_target_expr(quoted, has_raw, folded, otel);
            let b = repin_target_expr(quoted, has_raw, folded, syslog);
            format!(
                "count(CASE WHEN {a} IS NOT NULL AND {b} IS NOT NULL \
                 AND {a} <> {b} THEN 1 END)"
            )
        }
    };
    RepinCountExprs {
        carrying,
        kept,
        resurrectable,
        ambiguous,
    }
}

/// Wrap a TIMESTAMP-pinned envelope column's conform in a never-NULL last
/// arm, so conforming a standing file can never manufacture a NULL partition
/// key (the boot-pass counterpart of [`repair_expr`]'s third arm, ADR-0008).
///
/// Only fires for [`ConformPolicy::StandingFile`] on a
/// [`trawl_core::schema::TIMESTAMP_COLUMNS`] member actually pinned
/// TIMESTAMP: a `_time` pinned VARCHAR is plain text and needs no partition
/// guard (and could not take a TIMESTAMP literal anyway).
fn guard_partition_key(
    policy: &ConformPolicy,
    is_time_col: bool,
    pin: CanonicalType,
    expr: &str,
) -> String {
    match policy {
        ConformPolicy::StandingFile { time_fallback }
        | ConformPolicy::Repin { time_fallback, .. }
            if is_time_col && pin == CanonicalType::Timestamp =>
        {
            format!(
                "COALESCE({expr}, TIMESTAMP '{}')",
                time_fallback.format("%Y-%m-%d %H:%M:%S%.6f")
            )
        }
        _ => expr.to_owned(),
    }
}

/// Which corpus a [`ConformPlan`] is being built over. The two conform
/// sites agree on every rule except these, so the difference is named
/// rather than duplicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConformPolicy {
    /// A freshly-read WAL batch (compaction). `_time`/`_ingested` pass
    /// through untouched — the ADR-0008 repair ladder already made them
    /// TIMESTAMP and they must never be re-cast — and a column with no pin
    /// is dropped: `union_by_name` reads an absent column as NULL, and
    /// writing typed NULLs would let a silent field pre-empt its own real
    /// type. Unpinned means either a deferred pin (all-null column) or a
    /// denied one (unstorable name, or the catalog at
    /// [`crate::store::MAX_PINNED_FIELDS`]); both keep their values in
    /// `_raw`.
    WalBatch,
    /// A standing parquet file (boot conformance pass). Every column is
    /// conformed, `_time` included — a file whose timestamp column is
    /// mistyped is exactly what that pass exists to fix — and a column
    /// with no pin is kept verbatim (pins cover every scanned field, so
    /// this is unreachable in practice).
    ///
    /// `time_fallback` is the last arm of the TIMESTAMP-pinned envelope
    /// columns' conform, exactly as [`repair_expr`] is for WAL: a value the
    /// `TRY_CAST` cannot read must not be written as NULL, because a NULL
    /// partition key sorts first and falls outside every `last=Xh` filter
    /// — the row would survive the rewrite yet become permanently
    /// unqueryable by time (ADR-0008). The boot pass has no WAL filename to
    /// recover an instant from, so the caller supplies the file's own
    /// partition instant (its `{date}/{HH}` directory), or the conform
    /// instant when the path carries none.
    StandingFile {
        time_fallback: chrono::DateTime<chrono::Utc>,
    },
    /// A repin rewrite over an affected standing file (ADR-0011):
    /// [`ConformPolicy::StandingFile`] in every rule but one — the column
    /// whose folded name is `resurrect_field` routes through
    /// [`trawl_core::conform::resurrection_expr`] unconditionally, even
    /// when its physical type already matches the pin (the forced
    /// resurrection-only pass, `to == current`, exists precisely to
    /// rewrite a column the ordinary conform would call a noop). The pin
    /// map handed in is the live one with the target entry flipped to the
    /// new type, so every other column takes the pass-through arm — a
    /// conformant corpus casts nothing else.
    ///
    /// Defensive residual: a file with no `_raw` column (impossible for
    /// canonicalized events, reachable for a hand-planted file at a valid
    /// layout path) falls back to the plain guarded conform — no
    /// resurrection arm, rather than a rewrite-failing reference to a
    /// missing column.
    Repin {
        /// The repinned field (catalog key, folded).
        resurrect_field: String,
        /// Same role as [`ConformPolicy::StandingFile::time_fallback`].
        time_fallback: chrono::DateTime<chrono::Utc>,
        /// The per-arm reading rule for the target column (ADR-0013):
        /// which dialect each arm reads numerals in. Structurally scoped —
        /// only the `repin_target` arm of [`ConformPlan::build`] can reach
        /// it, so ordinary compaction and the boot pass stay on the `OTel`
        /// reading no matter what a job asserted.
        target: RepinTarget,
    },
}

/// One column the plan will `TRY_CAST`.
struct CastEntry {
    /// The folded (catalog-key) name — conflict evidence is attributed to
    /// the field as the catalog knows it, not to the stored spelling.
    name: String,
    dtype: String,
    pin: CanonicalType,
    expr: String,
    /// The column is already of its pin's physical type, so the cast can
    /// only null values outside the pin's domain — it can never change a
    /// conforming one. Today that is exactly the SEVERITY ladder guard
    /// (see [`conform_expr`]); every other pin passes such a column
    /// through. A plan whose only work is guard-only casts is the identity
    /// over a file that already satisfies the domain, which is what
    /// [`ConformPlan::is_guard_only`] lets the rewrite lanes prove before
    /// paying for a rewrite.
    guard_only: bool,
}

/// The per-column select list that conforms a source to the pins, plus the
/// bookkeeping needed to tally what the casts nulled.
///
/// Shared by compaction (conforming `wal_batch` in place) and the boot pass
/// (rewriting a standing parquet file): both build the same select list from
/// [`conform_expr`] and record the same [`FieldConflict`] evidence, and only
/// differ in where the rows come from ([`ConformPlan::tally_conflicts`]'s
/// `source`) and how the result is applied.
///
/// Column names are ASCII-case-folded on the output side: pins are keyed by
/// the folded name (ingest folds every field name at canonicalization), so
/// the lookup folds too, and a column whose stored spelling carries uppercase
/// is renamed to the folded form as part of the conform — a rename with no
/// cast still counts as a rewrite ([`Self::is_noop`]).
pub(crate) struct ConformPlan {
    /// Per-column SELECT expressions, in schema order.
    pub(crate) select_list: Vec<String>,
    /// The retained column names (folded) — the post-conform column set,
    /// so bookkeeping can never claim a service carried a field no parquet
    /// file holds.
    pub(crate) retained: Vec<String>,
    /// Columns omitted from the output because their pin deferred.
    pub(crate) dropped: Vec<String>,
    casts: Vec<CastEntry>,
    /// Columns whose only change is the case-fold rename.
    renamed: usize,
}

impl ConformPlan {
    /// Plan the conform of `schema` against `pins` under `policy`.
    pub(crate) fn build(
        schema: &[ColInfo],
        pins: &HashMap<String, CanonicalType>,
        policy: &ConformPolicy,
    ) -> Self {
        let mut plan = Self {
            select_list: Vec::with_capacity(schema.len()),
            retained: Vec::with_capacity(schema.len()),
            dropped: Vec::new(),
            casts: Vec::new(),
            renamed: 0,
        };
        // The resurrection arm reads `_raw`, so it exists only where the
        // file carries the column (see [`ConformPolicy::Repin`]).
        let has_raw = schema
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(trawl_core::schema::RAW));
        for col in schema {
            let quoted = quote_ident(&col.name);
            let folded = col.name.to_ascii_lowercase();
            let is_time_col = trawl_core::schema::TIMESTAMP_COLUMNS.contains(&folded.as_str());
            if matches!(policy, ConformPolicy::WalBatch) && is_time_col {
                plan.keep(col, &folded, quoted);
                continue;
            }
            // The one arm that may read a job's asserted dialect, and it
            // carries that reading with it — nothing else in this loop can
            // reach the target.
            let repin_target = match policy {
                ConformPolicy::Repin {
                    resurrect_field,
                    target,
                    ..
                } if *resurrect_field == folded => Some(*target),
                _ => None,
            };
            match pins.get(&folded).copied() {
                None if matches!(policy, ConformPolicy::WalBatch) => {
                    plan.dropped.push(col.name.clone());
                }
                None => plan.keep(col, &folded, quoted),
                Some(pin) if repin_target.is_some() => {
                    // Unconditional — never the `conform_expr` noop check:
                    // the resurrection-only pass rewrites a column whose
                    // physical type already IS the pin.
                    let target = repin_target.expect("matched Some above");
                    debug_assert_eq!(
                        pin, target.pin,
                        "the engine flips the pin map to the job's target"
                    );
                    let expr = repin_target_expr(&quoted, has_raw, &folded, target);
                    let written = guard_partition_key(policy, is_time_col, pin, &expr);
                    plan.select_list
                        .push(format!("{written} AS {}", quote_ident(&folded)));
                    plan.retained.push(folded.clone());
                    plan.casts.push(CastEntry {
                        name: folded,
                        dtype: col.dtype.clone(),
                        pin,
                        expr,
                        // A resurrection rewrites the column whatever its
                        // physical type — never a domain-only guard.
                        guard_only: false,
                    });
                }
                Some(pin) => match conform_expr(&quoted, &col.dtype, pin) {
                    None => plan.keep(col, &folded, quoted),
                    Some(expr) => {
                        // The written expression may carry a never-NULL last
                        // arm; the tallied one never does. `tally_conflicts`
                        // counts what the CAST could not read, and a fallback
                        // that substitutes an instant has still destroyed the
                        // original value — evidence worth recording.
                        let written = guard_partition_key(policy, is_time_col, pin, &expr);
                        plan.select_list
                            .push(format!("{written} AS {}", quote_ident(&folded)));
                        plan.retained.push(folded.clone());
                        plan.casts.push(CastEntry {
                            guard_only: col.dtype == pin.as_duckdb(),
                            name: folded,
                            dtype: col.dtype.clone(),
                            pin,
                            expr,
                        });
                    }
                },
            }
        }
        plan
    }

    /// Pass one column through — renamed to its folded spelling when the
    /// stored one differs, untouched otherwise.
    fn keep(&mut self, col: &ColInfo, folded: &str, quoted: String) {
        if folded == col.name {
            self.select_list.push(quoted);
        } else {
            self.select_list
                .push(format!("{quoted} AS {}", quote_ident(folded)));
            self.renamed += 1;
        }
        self.retained.push(folded.to_owned());
    }

    /// Whether the plan rewrites anything at all (a cast, a deferred-pin
    /// drop, or a case-fold rename).
    pub(crate) fn is_noop(&self) -> bool {
        self.casts.is_empty() && self.dropped.is_empty() && self.renamed == 0
    }

    /// Whether the plan's only work is domain guards over columns already
    /// of their pin's physical type ([`CastEntry::guard_only`]).
    ///
    /// Such a plan rewrites nothing unless the source actually holds a
    /// value outside the domain, and [`Self::tally_conflicts`] answers
    /// exactly that question with two aggregates per column. So a caller
    /// that has tallied and found nothing may skip the rewrite entirely —
    /// which is the difference between "conform the nonconformant files"
    /// and "COPY the whole archive": every trawl-written parquet carries
    /// the SEVERITY-pinned `_severity`, so without this the boot pass
    /// would rewrite every file in the corpus on every re-arm (ADR-0013).
    pub(crate) fn is_guard_only(&self) -> bool {
        !self.casts.is_empty()
            && self.dropped.is_empty()
            && self.renamed == 0
            && self.casts.iter().all(|c| c.guard_only)
    }

    /// How many columns the plan casts (a no-cast plan can still drop or
    /// rename columns, so this is not the inverse of [`Self::is_noop`]).
    pub(crate) fn cast_count(&self) -> usize {
        self.casts.len()
    }

    /// Tally what each cast would null over `source` (a FROM-clause source
    /// expression) — necessarily before the plan is applied, since applying
    /// it destroys the pre-cast values. Runs in aggregate passes of at most
    /// [`AGG_CHUNK_COLS`] cast columns for the same reason the pin ladder
    /// does: the width is client-chosen, and 2 aggregates × 10k columns in
    /// one statement is quadratic (38.5s at 10k, vs ~1.4s chunked).
    ///
    /// A conflict is one row per cast column the conform actually nulled.
    /// A cast that nulls nothing lost no data, whatever the two type names
    /// say: an all-null pinned column (a batch with no `_repairs`), a JSON
    /// source that converts fully, and a VARCHAR-pinned field whose batch
    /// happened to carry only numbers are all honest convergence. Recording
    /// those would append a row per (field, service) to the append-only
    /// `field_conflicts` on every compaction tick, forever, for a sender
    /// that is losing nothing — noise that outgrows the evidence.
    ///
    /// Every conflict carries a bounded sample of the values it is about to
    /// destroy ([`Self::capture_samples`]) — this phase is the only place
    /// they exist as values rather than as `_raw` text to be re-parsed.
    pub(crate) fn tally_conflicts(
        &self,
        conn: &duckdb::Connection,
        source: &str,
        service: &str,
    ) -> Result<Vec<FieldConflict>, String> {
        if self.casts.is_empty() {
            return Ok(Vec::new());
        }
        let mut stats: Vec<(i64, i64)> = Vec::with_capacity(self.casts.len());
        for chunk in self.casts.chunks(AGG_CHUNK_COLS) {
            let stats_sql = format!(
                "SELECT {} FROM {source}",
                chunk
                    .iter()
                    .map(|c| {
                        let q = quote_ident(&c.name);
                        format!("count({q})::BIGINT, count({})::BIGINT", c.expr)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let chunk_stats: Vec<(i64, i64)> = conn
                .query_row(&stats_sql, [], |row| {
                    let mut out = Vec::with_capacity(chunk.len());
                    for i in 0..chunk.len() {
                        out.push((row.get::<_, i64>(2 * i)?, row.get::<_, i64>(2 * i + 1)?));
                    }
                    Ok(out)
                })
                .map_err(|e| format!("conform stats query failed: {e}"))?;
            stats.extend(chunk_stats);
        }

        let lossy: Vec<usize> = stats
            .iter()
            .enumerate()
            .filter(|(_, (non_null, ok))| non_null > ok)
            .map(|(i, _)| i)
            .collect();
        // Best-effort, exactly like the postgres-side evidence writes it
        // feeds. `list(DISTINCT …)` accumulates every distinct misfit before
        // the slice caps it and DuckDB does not spill it, so a column with
        // pathological misfit cardinality can OOM the capture on a batch
        // whose tally (two scalar aggregates) just succeeded. Propagating
        // that would fail the whole conform: the WAL is retained, the next
        // tick retries a larger batch, and ingestion tightens itself to a
        // stop — while the boot lane would skip the file and withhold the
        // marker forever. Evidence is worth less than the data it describes.
        let mut samples = match self.capture_samples(conn, source, &lossy) {
            Ok(samples) => samples,
            Err(e) => {
                metrics::counter!(crate::metrics::CATALOG_SAMPLE_CAPTURE_FAILURES_TOTAL)
                    .increment(1);
                // The error text can name a column, like every other DuckDB
                // error this module logs; it can never carry a value — the
                // capture expression embeds no literals, the misfits are
                // data.
                tracing::warn!(
                    event_type = "catalog_sample_capture_failed",
                    columns = lossy.len(),
                    error = %e,
                    "misfit sample capture failed; conflicts are recorded without samples"
                );
                HashMap::new()
            }
        };

        let mut conflicts = Vec::new();
        for (i, (cast, (non_null, ok))) in self.casts.iter().zip(&stats).enumerate() {
            let rows_nulled = u64::try_from(non_null - ok).unwrap_or(0);
            if rows_nulled > 0 {
                conflicts.push(FieldConflict {
                    field: cast.name.clone(),
                    service: service.to_owned(),
                    observed_type: cast.dtype.clone(),
                    expected_type: cast.pin,
                    rows_nulled,
                    samples: samples.remove(&i).unwrap_or_default(),
                });
            }
        }
        Ok(conflicts)
    }

    /// Capture up to [`MAX_CONFLICT_SAMPLES`] DISTINCT misfit values for
    /// each cast column named by `lossy` (indices into [`Self::casts`]) —
    /// the values whose guarded cast reads NULL while the stored value does
    /// not, which is exactly what the conform is about to shelve.
    ///
    /// Only the columns that actually conflicted are sampled, and never more
    /// than [`MAX_SAMPLED_CONFLICT_COLUMNS`] of them in one pass:
    /// `list(DISTINCT …)` accumulates every distinct misfit before the slice
    /// caps it, so the cost is paid per sampled column, and a healthy install
    /// pays nothing at all (no conflicts, no query). Which columns win the
    /// cap is decided by name rather than by schema position, so a wide
    /// conflicting batch samples the same fields whatever order the source
    /// happens to `DESCRIBE` in.
    ///
    /// Values are sanitised and byte-capped in Rust ([`sanitize_sample`]):
    /// `left()` inside the aggregate counts characters (probed in
    /// `trawl-engine/tests/duckdb_probe.rs`), so it bounds what `DuckDB`
    /// accumulates, never what the store is promised. Samples are attacker
    /// text end to end — they are never logged, at any level.
    fn capture_samples(
        &self,
        conn: &duckdb::Connection,
        source: &str,
        lossy: &[usize],
    ) -> Result<HashMap<usize, Vec<String>>, String> {
        if lossy.is_empty() {
            return Ok(HashMap::new());
        }
        let mut sampled: Vec<usize> = lossy.to_vec();
        sampled.sort_by(|a, b| self.casts[*a].name.cmp(&self.casts[*b].name));
        sampled.truncate(MAX_SAMPLED_CONFLICT_COLUMNS);

        let sql = format!(
            "SELECT {} FROM {source}",
            sampled
                .iter()
                .map(|i| sample_expr(&self.casts[*i]))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let rendered: Vec<Option<String>> = conn
            .query_row(&sql, [], |row| {
                let mut out = Vec::with_capacity(sampled.len());
                for i in 0..sampled.len() {
                    out.push(row.get::<_, Option<String>>(i)?);
                }
                Ok(out)
            })
            .map_err(|e| format!("conform sample query failed: {e}"))?;

        let mut out = HashMap::with_capacity(sampled.len());
        for (i, json) in sampled.into_iter().zip(rendered) {
            // Decoded through the shared reader, so the conform's evidence
            // and the repin's report cap, sanitise and de-duplicate the same
            // way (a NULL cell — zero matching rows — is not an error).
            let kept = decode_misfit_samples(json.as_deref())?;
            out.insert(i, kept);
        }
        Ok(out)
    }
}

/// The misfit-sample capture for one cast column: up to
/// [`SAMPLE_CANDIDATES`] distinct values the cast nulls, rendered as one
/// JSON array of strings, from which the caller keeps at most
/// [`MAX_CONFLICT_SAMPLES`] once they are sanitised.
///
/// The predicate is the definition of a shelved value — a stored value the
/// guarded cast cannot read — written against the same `cast.expr` the
/// tally counts with, so the samples can only ever be values the conflict
/// row is counting.
fn sample_expr(cast: &CastEntry) -> String {
    let quoted = quote_ident(&cast.name);
    let text = trawl_core::conform::untyped_text(&quoted);
    distinct_misfit_samples_sql(&quoted, &text, &cast.expr)
}

/// The conform's own misfit-sampling aggregate (ADR-0011): up to
/// [`SAMPLE_CANDIDATES`] DISTINCT values the cast nulls, as one JSON array of
/// strings, from which [`decode_misfit_samples`] keeps at most
/// [`MAX_CONFLICT_SAMPLES`].
///
/// `list(DISTINCT …)` accumulates every distinct misfit before the slice
/// caps it, which is safe here and only here: the source is one WAL batch,
/// already bounded by the compactor's own batch limits. The repin scan runs
/// the same question over a whole corpus, where the distinct count is
/// unbounded, so it takes [`bounded_misfit_samples_sql`] instead — a
/// deliberate second spelling, not a drift.
///
/// `left()` counts characters (probed), so it bounds what `DuckDB`
/// accumulates and never what the store is promised — the byte cap is
/// [`sanitize_sample`]'s, in Rust, after the control-character
/// substitution. `array_slice` takes twice [`MAX_CONFLICT_SAMPLES`]
/// candidates because sanitising can collapse two distinct raw values into
/// one sample.
fn distinct_misfit_samples_sql(quoted: &str, text: &str, target_expr: &str) -> String {
    format!(
        "to_json(array_slice(list(DISTINCT left({text}, {MAX_CONFLICT_SAMPLE_BYTES})) \
         FILTER (WHERE {quoted} IS NOT NULL AND ({target_expr}) IS NULL), \
         1, {SAMPLE_CANDIDATES}))::VARCHAR"
    )
}

/// The repin scan's misfit-sampling aggregate: at most
/// [`MAX_CONFLICT_SAMPLES`] of the values a column carries whose reading
/// through `target_expr` is NULL, as one JSON array of strings — bounded by
/// construction over a corpus of any size.
///
/// `approx_top_k` is a fixed-size sketch: its memory is a function of `k`,
/// never of the distinct cardinality. That is the whole reason it is here
/// rather than [`distinct_misfit_samples_sql`], whose `list(DISTINCT …)`
/// holds every distinct misfit before the slice caps it — measured to
/// exhaust the scan's 2GB memory limit at a few million distinct misfits,
/// on exactly the corpus a repin exists to repair (a field a sender has
/// been writing free text into). A report that cannot be produced for the
/// worst corpus is a report for the cases that did not need it.
///
/// What changes with the sketch is which samples: the most frequent
/// misfits rather than the first distinct ones, approximately ordered. For
/// an advisory "here is what this pin cannot read" that is at least as
/// useful, and every value in the list is an exact value the corpus holds
/// — the approximation is in the ranking, never in the strings.
///
/// It rides the same statement as the counts (`count_repin_effect`), so
/// the samples and the numbers describe one read of one file: two
/// statements could straddle a compaction that replaced the file underneath
/// them and report evidence from a corpus that never existed.
pub(crate) fn bounded_misfit_samples_sql(quoted: &str, text: &str, target_expr: &str) -> String {
    format!(
        "to_json(approx_top_k(left({text}, {MAX_CONFLICT_SAMPLE_BYTES}), \
         {MAX_CONFLICT_SAMPLES}) \
         FILTER (WHERE {quoted} IS NOT NULL AND ({target_expr}) IS NULL))::VARCHAR"
    )
}

/// Decode one misfit-sample cell — from either sampling aggregate — into at
/// most [`MAX_CONFLICT_SAMPLES`] sanitised, distinct samples in first-seen
/// order.
///
/// Distinct after both transforms: SQL's `DISTINCT` ran over the raw text,
/// and what is promised — and stored — is distinct samples. A capture over
/// zero matching rows is SQL NULL rather than `[]` (probed), which is not
/// an error.
pub(crate) fn decode_misfit_samples(rendered: Option<&str>) -> Result<Vec<String>, String> {
    let Some(json) = rendered else {
        return Ok(Vec::new());
    };
    let values: Vec<String> =
        serde_json::from_str(json).map_err(|e| format!("conform sample decode failed: {e}"))?;
    let mut kept: Vec<String> = Vec::with_capacity(MAX_CONFLICT_SAMPLES);
    for value in values.iter().map(|v| sanitize_sample(v)) {
        if kept.len() == MAX_CONFLICT_SAMPLES {
            break;
        }
        if !kept.contains(&value) {
            kept.push(value);
        }
    }
    Ok(kept)
}

/// One captured misfit made safe to store, return and render: control and
/// format characters become U+FFFD ([`trawl_core::sanitize`]) and the result
/// is cut to [`MAX_CONFLICT_SAMPLE_BYTES`] on a `char` boundary.
///
/// Sanitising at capture rather than at each read is the point: the value is
/// whatever bytes a client sent, and it goes on to a postgres row, a JSON
/// body, an operator's terminal and a browser — one door is auditable, four
/// are not. Order matters, too: U+FFFD is three bytes where most of what it
/// replaces is one, so the byte cap is applied after the substitution or it
/// is not a cap.
pub(crate) fn sanitize_sample(value: &str) -> String {
    let mut cleaned = trawl_core::sanitize::sanitize_display_text(value);
    if cleaned.len() > MAX_CONFLICT_SAMPLE_BYTES {
        let mut end = MAX_CONFLICT_SAMPLE_BYTES;
        while !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned.truncate(end);
    }
    cleaned
}

/// Where a batch publishes: its env's data directory, and the WAL root and
/// env its inputs came from. The publication marker names paths relative to
/// these, so recovery rebuilds exactly the paths the publish used.
struct PublishTarget<'a> {
    /// `data_root/{env}`.
    data_dir: &'a Path,
    /// The WAL root. The batch's inputs live in `wal_dir/{env}`.
    wal_dir: &'a Path,
    env: &'a str,
}

/// A conformed batch written to `{service}.parquet.tmp` and not yet
/// published.
struct StagedOutput {
    output_dir: PathBuf,
    tmp_path: PathBuf,
    canonical_path: PathBuf,
    date: chrono::NaiveDate,
    hour: u8,
    /// Whether the output merged an existing canonical file.
    merged: bool,
    /// Rows in the output file.
    rows: u64,
    survivors: Vec<PathBuf>,
    compact_start: std::time::Instant,
    report: WriteReport,
}

/// Blocking phase 3, test-only form: [`write_output`] and a bare rename,
/// with no publication marker and no WAL retirement. Unit tests of conform
/// and merge semantics use it with WAL files outside any `wal_dir/{env}`;
/// the bracketed publish is exercised through [`compact_once`].
#[cfg(test)]
fn conform_and_write(
    prep: PreparedBatch,
    pins: &HashMap<String, CanonicalType>,
    data_dir: &Path,
    service: &str,
) -> Result<WriteReport, String> {
    let staged = write_output(prep, pins, data_dir, service)?;
    std::fs::rename(&staged.tmp_path, &staged.canonical_path)
        .map_err(|e| format!("atomic rename failed: {e}"))?;
    log_compaction_complete(&staged, service);
    Ok(staged.report)
}

/// Blocking phase 3: conform `wal_batch` to the authoritative pins, write
/// the canonical `{service}.parquet` for the current hour (merged with the
/// existing file when present) to a `.tmp`, and publish it through
/// [`publish_output`].
///
/// `Err` means the output was not published; the WAL stays for the next
/// tick, and a marker may stay for recovery. `Ok` carries the write report
/// and, when the output is published but its marker could not be removed,
/// the [`PublishIncomplete`] failure.
fn conform_and_publish(
    prep: PreparedBatch,
    pins: &HashMap<String, CanonicalType>,
    target: &PublishTarget<'_>,
    service: &str,
    hot_buffer: Option<&HotBuffer>,
    batch_ids: &[String],
) -> Result<(WriteReport, Option<PublishIncomplete>), String> {
    let staged = write_output(prep, pins, target.data_dir, service)?;
    let incomplete = publish_output(&staged, target, service, hot_buffer, batch_ids)?;
    log_compaction_complete(&staged, service);
    Ok((staged.report, incomplete))
}

/// Conform `wal_batch` and COPY it, merged with any existing canonical
/// file, to `{service}.parquet.tmp` in the current hour's directory.
fn write_output(
    prep: PreparedBatch,
    pins: &HashMap<String, CanonicalType>,
    data_dir: &Path,
    service: &str,
) -> Result<StagedOutput, String> {
    use chrono::Timelike as _;

    let PreparedBatch {
        conn,
        schema,
        survivors,
        compact_start,
        ..
    } = prep;

    let (conflicts, retained) = conform_wal_batch(&conn, &schema, pins, service)?;
    let observed_fields: Vec<String> = retained
        .into_iter()
        .filter(|n| !n.starts_with(WAL_FILE_COL))
        .collect();
    // Counted before the merge: `merged` holds the whole hour file, and
    // accumulating that into `field_services.row_count` would re-add every
    // previously-compacted row on every tick.
    let batch_rows = count_rows(&conn, "wal_batch")?;

    // Determine output directory from current time.
    let now = chrono::Utc::now();
    let date = now.date_naive();
    let hour = u8::try_from(now.hour()).map_err(|e| format!("hour out of range: {e}"))?;
    let day_dir = data_dir.join(date.format("%Y-%m-%d").to_string());
    let output_dir = day_dir.join(format!("{hour:02}"));

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("failed to create output dir: {e}"))?;

    // Canonical filename: one parquet file per service per hour-directory.
    // Merge with existing data if present, then atomic-rename into place.
    let canonical_path = output_dir.join(format!("{service}.parquet"));
    let tmp_path = output_dir.join(format!("{service}.parquet.tmp"));

    let merged = canonical_path.exists();

    let rows: u64 = if merged {
        // Merge: union existing parquet rows with new WAL batch.
        // BY NAME handles heterogeneous schemas (different events have
        // different fields) — missing columns become NULL in parquet.
        // Both sides are catalog-conformant, so a type conflict here means
        // a foreign file and errors loudly (WAL retained, retried).
        merge_with_existing(&conn, &canonical_path, service)?;

        // ORDER BY timestamp so row-group min/max stats enable range
        // pruning for `last=Xh` queries — the dominant query shape.
        // BLOOM_FILTER_FALSE_POSITIVE_RATIO pins the bloom filter FP
        // target (DuckDB auto-writes bloom filters on any column it
        // dictionary-encodes; this locks in a known FP rate).
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM merged ORDER BY \"_time\") TO '{}' \
             (FORMAT PARQUET, COMPRESSION SNAPPY, \
              BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
            tmp_path.display(),
        ))
        .map_err(|e| format!("COPY TO parquet failed: {e}"))?;

        let count = count_rows(&conn, "merged")?;
        conn.execute_batch("DROP TABLE IF EXISTS merged")
            .map_err(|e| format!("DROP TABLE failed: {e}"))?;
        count
    } else {
        // Fresh write: no existing file to merge with.
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM wal_batch ORDER BY \"_time\") TO '{}' \
             (FORMAT PARQUET, COMPRESSION SNAPPY, \
              BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
            tmp_path.display(),
        ))
        .map_err(|e| format!("COPY TO parquet failed: {e}"))?;

        batch_rows
    };

    conn.execute_batch("DROP TABLE IF EXISTS wal_batch")
        .map_err(|e| format!("DROP TABLE failed: {e}"))?;

    Ok(StagedOutput {
        output_dir,
        tmp_path,
        canonical_path,
        date,
        hour,
        merged,
        rows,
        survivors,
        compact_start,
        report: WriteReport {
            batch_rows,
            conflicts,
            observed_fields,
        },
    })
}

/// Publish a staged output exactly once (ADR-0041): bracket the rename with
/// a publication marker that names the output's identity and the consumed
/// WAL, so a crash or failure at any step leaves a record recovery can
/// finish or roll back without merging the WAL twice.
///
/// 1. fsync the tmp and every directory entry leading to it, then hash it
///    ([`durable_marker_for`]).
/// 2. Write the marker durably.
/// 3. Under the publication write guard: confirm every consumed WAL file is
///    still there, rename, drain the hot batches, fsync the output
///    directory.
/// 4. Retire the consumed WAL files and fsync their directory.
/// 5. Remove the marker durably.
///
/// Runs inside the caller's repin corpus read guard. `Err` means the output
/// was not published: a failure before the marker leaves only an orphan tmp
/// that the next COPY overwrites; a failure after it leaves the marker for
/// recovery, which rolls the publish back. Once the rename has happened,
/// every failure is `Ok(Some(_))`: the output is published and the marker
/// stays until recovery completes the publish.
fn publish_output(
    staged: &StagedOutput,
    target: &PublishTarget<'_>,
    service: &str,
    hot_buffer: Option<&HotBuffer>,
    batch_ids: &[String],
) -> Result<Option<PublishIncomplete>, String> {
    let incomplete = |operation, error| -> Result<Option<PublishIncomplete>, String> {
        Ok(Some(PublishIncomplete { operation, error }))
    };
    let marker = durable_marker_for(staged, target, service)?;
    let marker_path = marker.marker_path(target.wal_dir);
    let wal_env_dir = marker.wal_env_dir(target.wal_dir);

    // 2. From here on a failure before the rename leaves the marker, and
    //    recovery rolls back: the canonical lacks the identity and the tmp
    //    is present.
    publication_marker::write_marker(target.wal_dir, &marker)?;
    publication_marker::crash_point("publish:after_marker").map_err(|e| e.to_string())?;

    // 3. A reader sees either the old cold file plus these hot batches, or
    //    the replacement cold file with those batches drained.
    {
        let publication = hot_buffer.map(HotBuffer::publication);
        let publication_guard = publication.as_ref().map(|gate| gate.blocking_write());
        // A WAL writer holds the ingest side of this gate from its rename
        // until it has fsynced or withdrawn the file, so under the write
        // guard a consumed file that is gone was withdrawn: its write was
        // rejected and its sender may retry. Publishing its rows would
        // count them twice. Roll back; the next tick reads what remains.
        if let Some(withdrawn) = staged.survivors.iter().find(|path| {
            !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
        }) {
            drop(publication_guard);
            publication_marker::roll_back_unpublished(&marker_path, &staged.tmp_path)?;
            return Err(format!(
                "consumed WAL file {} disappeared before publication (a rejected write \
                 was withdrawn); nothing published, the batch is read again next tick",
                withdrawn.display()
            ));
        }
        std::fs::rename(&staged.tmp_path, &staged.canonical_path)
            .map_err(|e| format!("atomic rename failed: {e}"))?;
        #[cfg(any(test, feature = "test-support"))]
        if let Some(gate) = &publication {
            gate.hold_after_publish_for_test();
        }
        if let Some(buf) = hot_buffer {
            let ids: Vec<&str> = batch_ids.iter().map(String::as_str).collect();
            buf.drain(&ids);
        }
        if let Err(e) = crate::epoch::fsync_dir(&staged.output_dir) {
            return incomplete(
                CompactionOperation::Chunk,
                format!(
                    "failed to fsync directory {}: {e}",
                    staged.output_dir.display()
                ),
            );
        }
        drop(publication_guard);
    }
    if let Err(e) = publication_marker::crash_point("publish:after_rename") {
        return incomplete(CompactionOperation::Chunk, e.to_string());
    }

    // 4. Delete each consumed WAL file, or rename it aside out of the scan.
    //    A failure keeps the marker, which keeps the file from being merged
    //    again while it stays.
    for path in marker.wal_paths(target.wal_dir) {
        if let Err(e) = retire_merged_input(&path) {
            return incomplete(CompactionOperation::ConsumedWalRemoval, e);
        }
    }
    if let Err(e) = crate::epoch::fsync_dir(&wal_env_dir) {
        return incomplete(
            CompactionOperation::Chunk,
            format!("failed to fsync directory {}: {e}", wal_env_dir.display()),
        );
    }
    if let Err(e) = publication_marker::crash_point("publish:after_retire") {
        return incomplete(CompactionOperation::Chunk, e.to_string());
    }

    // 5. Durable before the next publish of this service writes its own
    //    marker: a resurrected marker would read as a contradiction.
    if let Err(e) = publication_marker::remove_marker_durably(&marker_path) {
        return incomplete(CompactionOperation::Chunk, e);
    }
    Ok(None)
}

/// Step 1 of [`publish_output`]: make the tmp's bytes and every directory
/// entry leading to it durable, hash it, and build the marker naming it and
/// the consumed WAL. Touches nothing a marker claims.
fn durable_marker_for(
    staged: &StagedOutput,
    target: &PublishTarget<'_>,
    service: &str,
) -> Result<ValidatedMarker, String> {
    let wal_env_dir = target.wal_dir.join(target.env);
    let mut wal_names = Vec::with_capacity(staged.survivors.len());
    for path in &staged.survivors {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| wal_env_dir.join(name) == *path)
            .ok_or_else(|| {
                format!(
                    "WAL input {} is not in {}",
                    path.display(),
                    wal_env_dir.display()
                )
            })?;
        wal_names.push(name.to_owned());
    }
    std::fs::File::open(&staged.tmp_path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("failed to fsync {}: {e}", staged.tmp_path.display()))?;
    let identity = publication_marker::identity_of(&staged.tmp_path)
        .map_err(|e| format!("failed to hash {}: {e}", staged.tmp_path.display()))?;
    // Sync the whole chain on every publish: the hour directory for the
    // tmp's entry, then the date, env and data root directories for each
    // directory's entry in its parent. Syncing only the directories this
    // attempt created would skip one an earlier, failed attempt created, so
    // a retry could retire WAL under an output path that is not durable.
    // Directory fsyncs cost little next to the COPY and the tmp's own
    // fsync, and a per-process "already durable" cache would go stale when
    // retention deletes a date directory and a publish recreates it.
    let entry_dirs = [
        Some(staged.output_dir.as_path()),
        staged.output_dir.parent(),
        Some(target.data_dir),
        target.data_dir.parent(),
    ];
    for dir in entry_dirs.into_iter().flatten() {
        crate::epoch::fsync_dir(dir)
            .map_err(|e| format!("failed to fsync directory {}: {e}", dir.display()))?;
    }
    let marker = ValidatedMarker::new(
        target.env,
        service,
        staged.date,
        staged.hour,
        wal_names,
        identity,
    )?;
    // Recovery rebuilds every path from the marker; refuse a publish whose
    // paths it would not rebuild.
    let data_root = target.data_dir.parent().unwrap_or(target.data_dir);
    if marker.canonical(data_root) != staged.canonical_path {
        return Err(format!(
            "output {} is not where a publication marker for env {:?} points",
            staged.canonical_path.display(),
            target.env
        ));
    }
    Ok(marker)
}

fn log_compaction_complete(staged: &StagedOutput, service: &str) {
    let output_bytes = std::fs::metadata(&staged.canonical_path).map_or(0, |m| m.len());
    let duration_ms = staged.compact_start.elapsed().as_millis();
    tracing::info!(
        event_type = "compaction_complete",
        compact_service = %service,
        output = %staged.canonical_path.display(),
        wal_files = staged.survivors.len(),
        merged = staged.merged,
        rows = staged.rows,
        conflicts = staged.report.conflicts.len(),
        output_bytes,
        duration_ms,
        "compaction complete"
    );
}

/// Conform `wal_batch` to the pins in place, per [`ConformPolicy::WalBatch`].
///
/// Returns the recordable conflicts (see [`ConformPlan::tally_conflicts`])
/// and the retained column names.
fn conform_wal_batch(
    conn: &duckdb::Connection,
    schema: &[ColInfo],
    pins: &HashMap<String, CanonicalType>,
    service: &str,
) -> Result<(Vec<FieldConflict>, Vec<String>), String> {
    let plan = ConformPlan::build(schema, pins, &ConformPolicy::WalBatch);
    if plan.select_list.is_empty() {
        return Err("conform produced an empty column list".to_owned());
    }

    // Tallied before the table is replaced.
    let conflicts = plan.tally_conflicts(conn, "wal_batch", service)?;

    if !plan.dropped.is_empty() {
        tracing::info!(
            event_type = "catalog_pin_deferred",
            compact_service = %service,
            columns = ?plan.dropped,
            "unpinned columns absent from this file (pin deferred as all-null, \
             or denied — see catalog_pin_cap_reached / \
             catalog_field_name_unstorable); values remain in _raw"
        );
    }

    // Same skip the boot pass takes: a guard-only plan that nulled nothing
    // would copy the batch table to reproduce it byte for byte.
    let identity = plan.is_noop() || (plan.is_guard_only() && conflicts.is_empty());
    if !identity {
        conn.execute_batch(&format!(
            "CREATE TABLE wal_conformed AS SELECT {} FROM wal_batch; \
             DROP TABLE wal_batch; \
             ALTER TABLE wal_conformed RENAME TO wal_batch",
            plan.select_list.join(", ")
        ))
        .map_err(|e| format!("catalog conform failed: {e}"))?;
    }

    Ok((conflicts, plan.retained))
}

/// Test-only convenience wrapper: prepare + local pins + conform/write,
/// folding the quarantine count into the `Ok` value. Production goes
/// through [`compact_service_batch`], which persists pins first and
/// preserves the count on `Err` too.
#[cfg(test)]
fn compact_service_blocking(
    wal_files: &[PathBuf],
    data_dir: &Path,
    service: &str,
    memory_limit: &str,
) -> Result<u64, String> {
    let mut quarantined = Vec::new();
    let prep = prepare_service_batch(
        wal_files,
        data_dir,
        service,
        memory_limit,
        &mut quarantined,
        &HashMap::new(),
    )?;
    if let Some(prep) = prep {
        let pins = local_pins(&prep.proposals);
        conform_and_write(prep, &pins, data_dir, service)?;
    }
    Ok(quarantined.len() as u64)
}

/// Remove stale `.parquet.tmp` files left by interrupted compaction or
/// rollup runs.
///
/// `data_dir` is one env root (`data/{env}`): checks both `{date}/{HH}/`
/// (hourly compaction) and `{date}/` (daily rollup) for `.tmp` files older
/// than `max_age`. These are inert (don't match `*.parquet` globs) but
/// should be cleaned up to avoid disk waste.
///
/// An hourly tmp that a publication marker claims is kept however old it
/// is: recovery decides "unpublished" from its presence, and without it
/// the marker would read as a contradiction and block the service.
fn cleanup_stale_tmp_files(
    env: &str,
    data_dir: &Path,
    max_age: Duration,
    claims: &PublicationClaims,
) {
    let Ok(days) = std::fs::read_dir(data_dir) else {
        return;
    };
    for day_entry in days.flatten() {
        let day_path = day_entry.path();
        if !day_path.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&day_path) else {
            continue;
        };
        let date = day_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d").ok());
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Hour subdirectory — check its contents.
                let hour = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| name.len() == 2)
                    .and_then(|name| name.parse::<u8>().ok());
                let claimed = |service: &str| match (date, hour) {
                    (Some(date), Some(hour)) => claims.claims_output(env, date, hour, service),
                    // A marker only ever names a valid date and hour.
                    _ => false,
                };
                cleanup_tmp_in_dir(&path, max_age, claimed);
            } else {
                // Day-level file (from rollup).
                remove_stale_tmp(&path, max_age);
            }
        }
    }
}

/// Remove `.tmp` files in a single hour directory that are older than
/// `max_age`, except those `claimed` names by service.
fn cleanup_tmp_in_dir(dir: &Path, max_age: Duration, claimed: impl Fn(&str) -> bool) {
    let Ok(files) = std::fs::read_dir(dir) else {
        return;
    };
    for file in files.flatten() {
        let path = file.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".parquet.tmp"))
            .is_some_and(&claimed)
        {
            continue;
        }
        remove_stale_tmp(&path, max_age);
    }
}

/// Remove a single `.tmp` file if older than `max_age`.
fn remove_stale_tmp(path: &Path, max_age: Duration) {
    // Recovery must see the prepared generation before it considers an
    // existing canonical complete. Keep any tmp claimed by a rollup marker.
    if let Some(service) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".parquet.tmp"))
        && let Some(parent) = path.parent()
        && !matches!(std::fs::symlink_metadata(rollup_marker_path(parent, service)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        return;
    }
    if path.extension().is_some_and(|ext| ext == "tmp")
        && let Ok(meta) = std::fs::metadata(path)
        && let Ok(mtime) = meta.modified()
        && SystemTime::now().duration_since(mtime).unwrap_or_default() > max_age
    {
        let _ = std::fs::remove_file(path);
        tracing::debug!(event_type = "tmp_cleanup", path = %path.display(), "removed stale tmp file");
    }
}

/// Scan one env's WAL directory, isolating a read failure to that env.
///
/// Returns `None` (after logging) when the directory cannot be read, so the
/// compaction cycle can skip that env and carry on. Propagating instead
/// would abort the whole cycle — every other env's WAL drain plus the daily
/// rollup — for as long as the one bad directory stays unreadable, growing
/// the WAL without bound and leaving the hot buffer full, so admission
/// refuses ingest (ADR-0043). A missing directory is not a failure: [`scan_wal_files`] already
/// reports `NotFound` as an empty scan.
fn scan_env_wal_files(env: &str, env_wal_dir: &Path, min_age: Duration) -> Option<Vec<PathBuf>> {
    match scan_wal_files(env_wal_dir, min_age) {
        Ok(files) => Some(files),
        Err(e) => {
            CompactionOperation::WalEnvironmentScan.record_failure();
            tracing::error!(
                event_type = "compaction_error",
                compact_env = %env,
                dir = %env_wal_dir.display(),
                error = %e,
                "failed to scan env WAL directory, skipping env this tick"
            );
            None
        }
    }
}

/// Scan the WAL directory for `.ndjson` files older than `min_age`.
///
/// A zero `min_age` takes every file without reading its mtime, so a file
/// whose mtime is in the future (clock skew, a restored backup) is taken
/// too. A pressure pass relies on that: its whole point is to drain what
/// is resident now.
pub(crate) fn scan_wal_files(wal_dir: &Path, min_age: Duration) -> std::io::Result<Vec<PathBuf>> {
    let now = SystemTime::now();
    let mut files = Vec::new();

    let entries = match std::fs::read_dir(wal_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_none_or(|ext| ext != "ndjson") {
            continue;
        }
        if min_age.is_zero() {
            files.push(path);
        } else if let Ok(mtime) = entry.metadata()?.modified()
            && now.duration_since(mtime).unwrap_or_default() > min_age
        {
            files.push(path);
        }
    }

    Ok(files)
}

/// Group WAL file paths by service name (extracted from filename prefix).
fn group_by_service(files: Vec<PathBuf>) -> HashMap<String, Vec<PathBuf>> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for path in files {
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        // Filename format: {service}_{unix_millis}_{hex_random}
        // Find the service by taking everything before the last two `_` segments.
        let service = extract_service_from_filename(filename);
        groups.entry(service).or_default().push(path);
    }

    groups
}

/// Extract the service name from a WAL filename.
///
/// Filename format: `{service}_{unix_millis}_{hex_random}`
/// The millis and hex segments are always the last two `_`-delimited parts.
pub(crate) fn extract_service_from_filename(filename: &str) -> String {
    let parts: Vec<&str> = filename.rsplitn(3, '_').collect();
    if parts.len() == 3 {
        parts[2].to_owned()
    } else {
        filename.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Async operation tests need the same recorder on their blocking workers.
    // nextest runs each test in its own process, as for the server's other
    // global-recorder tests. Read deltas so initialization never resets data.
    fn operational_metrics() -> metrics_exporter_prometheus::PrometheusHandle {
        static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
            std::sync::OnceLock::new();
        let handle = HANDLE
            .get_or_init(|| {
                crate::metrics::prometheus_builder()
                    .install_recorder()
                    .unwrap()
            })
            .clone();
        crate::metrics::init_operational_alert_metrics();
        crate::metrics::init_publication_recovery_metrics();
        handle
    }

    fn operation_count(
        handle: &metrics_exporter_prometheus::PrometheusHandle,
        operation: CompactionOperation,
    ) -> u64 {
        crate::metrics::test_support::sample(
            handle,
            &format!(
                "trawl_compaction_operation_failures_total{{operation=\"{}\"}}",
                operation.label()
            ),
        )
    }

    fn quarantine_count(
        handle: &metrics_exporter_prometheus::PrometheusHandle,
        kind: QuarantineKind,
    ) -> u64 {
        crate::metrics::test_support::sample(
            handle,
            &format!("trawl_files_quarantined_total{{kind=\"{}\"}}", kind.label()),
        )
    }

    /// [`rollup_once`] with no publication claims and no repin or gate.
    async fn rollup_unclaimed(data: &Path) -> Result<u64, String> {
        rollup_once(data, &PublicationClaims::default(), "2GB", None, None).await
    }

    /// [`rollup_env_once`] for `prod` with no publication claims and no
    /// repin or gate.
    async fn rollup_env_unclaimed(env_data: &Path) -> Result<u64, String> {
        rollup_env_once(
            "prod",
            env_data,
            &PublicationClaims::default(),
            "2GB",
            None,
            None,
        )
        .await
    }

    const OPERATIONAL_ROW: &str = r#"{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"svc","message":"retained"}"#;

    #[tokio::test]
    async fn operational_scan_failures_and_idle_controls() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // These direct scans and marker-free cycles do not spawn blocking
        // workers. A thread-local recorder observes every attempted operation.
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("missing-wal");
        let data = tmp.path().join("missing-data");
        for rollup in [false, true] {
            assert_eq!(
                compact_once(&wal, &data, Duration::ZERO, rollup, None, 1, "2GB", None)
                    .await
                    .unwrap(),
                0
            );
        }
        for operation in CompactionOperation::ALL {
            assert_eq!(operation_count(&handle, operation), 0);
        }

        std::fs::write(&wal, b"not a WAL directory").unwrap();
        assert!(
            compact_once(&wal, &data, Duration::ZERO, false, None, 1, "2GB", None)
                .await
                .is_err()
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::WalRootScan),
            1
        );
        assert!(scan_env_wal_files("prod", &wal, Duration::ZERO).is_none());
        assert_eq!(
            operation_count(&handle, CompactionOperation::WalEnvironmentScan),
            1
        );
        assert_eq!(
            scan_env_wal_files("prod", &tmp.path().join("absent"), Duration::ZERO),
            Some(Vec::new())
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::WalEnvironmentScan),
            1
        );

        // Exercise each existing daily-rollup scan owner with a regular file
        // in place of its directory, preserving its return/skip behavior.
        assert_eq!(rollup_unclaimed(&wal).await.unwrap(), 0);
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            1
        );
        assert!(rollup_env_unclaimed(&wal).await.is_err());
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            2
        );
        assert!(collect_hour_dirs(&wal).is_empty());
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            3
        );
        assert!(collect_service_files(std::slice::from_ref(&wal)).is_empty());
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            4
        );
        for operation in [
            CompactionOperation::Chunk,
            CompactionOperation::DailyRollupUnit,
            CompactionOperation::PendingRollupRecovery,
        ] {
            assert_eq!(operation_count(&handle, operation), 0);
        }

        let broken_data = tmp.path().join("broken-data");
        std::fs::write(&broken_data, b"not a data directory").unwrap();
        let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 100_000,
        }));
        for _ in 0..2 {
            assert!(
                compact_once(
                    &tmp.path().join("no-wal"),
                    &broken_data,
                    Duration::ZERO,
                    false,
                    Some(&hot),
                    500,
                    "2GB",
                    None
                )
                .await
                .is_err()
            );
        }
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupScan),
            1
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery),
            0
        );
    }

    #[tokio::test]
    async fn operational_deleted_rollup_directories_are_not_failure_evidence() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // These directory-boundary calls return before any worker is spawned.
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let tmp = tempfile::tempdir().unwrap();
        // Retention can remove a discovered env/day/hour before its scan or
        // recovery starts. Preserve the original control flow, including the
        // recovery Err, without manufacturing failed-operation evidence.
        let removed = tmp.path().join("removed-by-retention");
        std::fs::create_dir(&removed).unwrap();
        std::fs::remove_dir(&removed).unwrap();
        assert!(rollup_env_unclaimed(&removed).await.is_err());
        assert!(collect_hour_dirs(&removed).is_empty());
        assert!(collect_service_files(std::slice::from_ref(&removed)).is_empty());
        let recovery = recover_rollup_markers_coordinated(&removed, None);
        assert!(
            recovery
                .unwrap_err()
                .starts_with("failed to list rollup markers:")
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            0
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery),
            0
        );

        // A file in place of those directories must still emit real failures.
        std::fs::write(&removed, b"not a directory").unwrap();
        assert!(rollup_env_unclaimed(&removed).await.is_err());
        assert!(collect_hour_dirs(&removed).is_empty());
        assert!(collect_service_files(std::slice::from_ref(&removed)).is_empty());
        assert!(recover_rollup_markers_coordinated(&removed, None).is_err());
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            3
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn operational_wal_root_skipped_metadata_failure_counts_once_per_scan() {
        use std::os::unix::fs::PermissionsExt;

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // The skipped environments never reach blocking compaction workers.
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let envs = [wal.join("prod"), wal.join("dev")];
        for env in &envs {
            std::fs::create_dir_all(env).unwrap();
        }
        let files = envs
            .each_ref()
            .map(|env| write_wal_file(env, "svc", &[OPERATIONAL_ROW]));
        let original = files.each_ref().map(|file| std::fs::read(file).unwrap());
        let permissions = std::fs::metadata(&wal).unwrap().permissions();

        // Read permission permits enumeration; missing search permission
        // makes metadata lookup fail for both otherwise valid environments.
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o600)).unwrap();
        let entries = std::fs::read_dir(&wal).and_then(Iterator::collect::<Result<Vec<_>, _>>);
        let metadata = envs.each_ref().map(std::fs::metadata);
        std::fs::set_permissions(&wal, permissions.clone()).unwrap();
        assert_eq!(entries.unwrap().len(), 2, "root must remain enumerable");
        if metadata.iter().all(Result::is_ok) {
            eprintln!(
                "skipped: privileges bypass directory search permissions; metadata failure was not exercised"
            );
            return;
        }
        assert!(
            metadata.iter().all(|result| result
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)),
            "both environment metadata lookups must fail with PermissionDenied"
        );

        let root_before = operation_count(&handle, CompactionOperation::WalRootScan);
        let env_before = operation_count(&handle, CompactionOperation::WalEnvironmentScan);
        for expected_delta in 1..=2 {
            std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o600)).unwrap();
            let result =
                compact_once(&wal, &data, Duration::ZERO, false, None, 1, "2GB", None).await;
            // Restore before every assertion and before temporary-directory cleanup.
            std::fs::set_permissions(&wal, permissions.clone()).unwrap();
            assert_eq!(
                result.unwrap(),
                0,
                "preserve the existing empty-success result"
            );
            assert_eq!(
                operation_count(&handle, CompactionOperation::WalRootScan) - root_before,
                expected_delta
            );
            assert_eq!(
                operation_count(&handle, CompactionOperation::WalEnvironmentScan) - env_before,
                0
            );
            for (file, bytes) in files.iter().zip(&original) {
                assert_eq!(std::fs::read(file).unwrap(), *bytes);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn operational_rollup_root_skipped_metadata_failure_counts_once() {
        use std::os::unix::fs::PermissionsExt;

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // No environment reaches a blocking rollup worker in this case.
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let envs = [data.join("prod"), data.join("dev")];
        for env in &envs {
            std::fs::create_dir_all(env).unwrap();
        }
        let permissions = std::fs::metadata(&data).unwrap().permissions();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
        let entries = std::fs::read_dir(&data).and_then(Iterator::collect::<Result<Vec<_>, _>>);
        let metadata = envs.each_ref().map(std::fs::metadata);
        std::fs::set_permissions(&data, permissions.clone()).unwrap();
        assert_eq!(entries.unwrap().len(), 2, "root must remain enumerable");
        if metadata.iter().all(Result::is_ok) {
            eprintln!(
                "skipped: privileges bypass directory search permissions; metadata failure was not exercised"
            );
            return;
        }
        assert!(
            metadata.iter().all(|result| result
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)),
            "both environment metadata lookups must fail with PermissionDenied"
        );

        let before = operation_count(&handle, CompactionOperation::DailyRollupScan);
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = rollup_unclaimed(&data).await;
        std::fs::set_permissions(&data, permissions).unwrap();
        assert_eq!(
            result.unwrap(),
            0,
            "preserve the existing empty-success result"
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan) - before,
            1
        );
        assert!(envs.iter().all(|env| env.is_dir()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn operational_historical_date_metadata_failure_counts_with_initialized_hot_gate() {
        use std::os::unix::fs::PermissionsExt;

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // The denied date lookup prevents dispatch to a blocking rollup worker.
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let env = data.join("prod");
        let historical_path = env.join("2026-01-15");
        let input = write_hourly_parquet(&env, "2026-01-15", "00", "svc", &[OPERATIONAL_ROW]);
        let original = std::fs::read(&input).unwrap();
        let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 100_000,
        }));
        // Initialize while readable, as production does before later permission
        // changes. The next cycle reuses this gate instead of rescanning markers.
        hot.publication().initialize(&data);
        let permissions = std::fs::metadata(&env).unwrap().permissions();
        std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600)).unwrap();
        let entries = std::fs::read_dir(&env).and_then(Iterator::collect::<Result<Vec<_>, _>>);
        let metadata = historical_path.metadata();
        std::fs::set_permissions(&env, permissions.clone()).unwrap();
        assert_eq!(
            entries.unwrap().len(),
            1,
            "environment must remain enumerable"
        );
        if metadata.is_ok() {
            eprintln!(
                "skipped: privileges bypass directory search permissions; historical-date metadata failure was not exercised"
            );
            return;
        }
        assert_eq!(
            metadata.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );

        let before = operation_count(&handle, CompactionOperation::DailyRollupScan);
        std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = compact_once(
            &tmp.path().join("no-wal"),
            &data,
            Duration::ZERO,
            true,
            Some(&hot),
            500,
            "2GB",
            None,
        )
        .await;
        // Restore permissions before assertions and temporary-directory cleanup.
        std::fs::set_permissions(&env, permissions).unwrap();
        assert_eq!(
            result.unwrap(),
            0,
            "preserve the existing skipped-date result"
        );
        assert_eq!(std::fs::read(&input).unwrap(), original);
        for operation in CompactionOperation::ALL {
            if operation != CompactionOperation::DailyRollupScan {
                assert_eq!(operation_count(&handle, operation), 0);
            }
        }
        assert_eq!(quarantine_count(&handle, QuarantineKind::Parquet), 0);
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan) - before,
            1
        );
    }

    #[tokio::test]
    async fn operational_successful_quarantine_and_healthy_work_are_not_operation_failures() {
        let handle = operational_metrics();
        let before = quarantine_count(&handle, QuarantineKind::Wal);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        let corrupt = wal.join("prod/svc_0_bad.ndjson");
        std::fs::write(&corrupt, [0; 32]).unwrap();
        assert_eq!(
            compact_once(&wal, &data, Duration::ZERO, false, None, 500, "2GB", None)
                .await
                .unwrap(),
            1
        );
        assert_eq!(quarantine_count(&handle, QuarantineKind::Wal) - before, 1);
        assert_eq!(
            std::fs::read(corrupt.with_extension("ndjson.corrupt")).unwrap(),
            [0; 32]
        );
        let healthy = write_wal_file(&wal.join("prod"), "svc", &[OPERATIONAL_ROW]);
        assert_eq!(
            compact_once(&wal, &data, Duration::ZERO, true, None, 500, "2GB", None)
                .await
                .unwrap(),
            0
        );
        assert!(!healthy.exists());
        let output = find_files_by_ext(&data, "parquet");
        assert_eq!(output.len(), 1);
        assert_eq!(read_strings(&output[0], "message"), ["retained"]);
        for operation in CompactionOperation::ALL {
            assert_eq!(operation_count(&handle, operation), 0);
        }
        assert_eq!(quarantine_count(&handle, QuarantineKind::Wal) - before, 1);
    }

    #[tokio::test]
    async fn operational_best_effort_failures_preserve_prior_quarantines() {
        let handle = operational_metrics();
        let chunk_before = operation_count(&handle, CompactionOperation::Chunk);
        let wal_before = quarantine_count(&handle, QuarantineKind::Wal);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("prod"), b"output obstruction").unwrap();
        let good = write_wal_file(&wal.join("prod"), "svc", &[OPERATIONAL_ROW]);
        let bad = wal.join("prod/svc_0_bad.ndjson");
        std::fs::write(&bad, [0; 32]).unwrap();
        let result = compact_once(&wal, &data, Duration::ZERO, false, None, 500, "2GB", None).await;
        assert!(result.is_ok(), "chunk errors remain best-effort");
        assert_eq!(
            operation_count(&handle, CompactionOperation::Chunk) - chunk_before,
            1
        );
        assert_eq!(
            quarantine_count(&handle, QuarantineKind::Wal) - wal_before,
            1
        );
        assert!(good.exists());
        assert_eq!(
            std::fs::read(bad.with_extension("ndjson.corrupt")).unwrap(),
            [0; 32]
        );
        assert!(!bad.exists());
        assert!(find_files_by_ext(&data, "parquet").is_empty());

        let rollup_before = operation_count(&handle, CompactionOperation::DailyRollupUnit);
        let parquet_before = quarantine_count(&handle, QuarantineKind::Parquet);
        let rollup_root = tmp.path().join("rollup-data");
        let day = rollup_root.join("prod/2026-01-15");
        std::fs::create_dir_all(day.join("00")).unwrap();
        std::fs::create_dir_all(day.join("01")).unwrap();
        let corrupt = day.join("00/svc.parquet");
        let unreadable = day.join("01/svc.parquet");
        std::fs::write(&corrupt, b"").unwrap();
        std::fs::write(&unreadable, b"PAR1\xff\xff\xff\xffPAR1").unwrap();
        assert!(
            compact_once(
                &tmp.path().join("no-wal"),
                &rollup_root,
                Duration::ZERO,
                true,
                None,
                500,
                "2GB",
                None
            )
            .await
            .is_ok()
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupUnit) - rollup_before,
            1
        );
        assert_eq!(
            quarantine_count(&handle, QuarantineKind::Parquet) - parquet_before,
            1
        );
        assert!(!corrupt.exists());
        assert!(corrupt.with_extension("parquet.corrupt").exists());
        assert_eq!(
            std::fs::read(&unreadable).unwrap(),
            b"PAR1\xff\xff\xff\xffPAR1"
        );
        assert!(!day.join("svc.parquet").exists());
    }

    #[tokio::test]
    async fn operational_recovery_failure_and_temporary_quarantine_are_separate_facts() {
        let handle = operational_metrics();
        let recovery_before = operation_count(&handle, CompactionOperation::PendingRollupRecovery);
        let quarantine_before = quarantine_count(&handle, QuarantineKind::RollupTemporary);
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let env = data.join("prod");
        let hourly = write_hourly_parquet(&env, "2026-01-15", "00", "svc", &[OPERATIONAL_ROW]);
        let hourly_bytes = std::fs::read(&hourly).unwrap();
        let day = env.join("2026-01-15");
        let temporary = day.join("svc.parquet.tmp");
        std::fs::write(&temporary, b"truncated output").unwrap();
        write_rollup_marker(&day, "svc", std::slice::from_ref(&hourly)).unwrap();
        assert!(
            compact_once(
                &tmp.path().join("no-wal"),
                &data,
                Duration::ZERO,
                false,
                None,
                500,
                "2GB",
                None
            )
            .await
            .is_ok()
        );
        assert_eq!(
            quarantine_count(&handle, QuarantineKind::RollupTemporary) - quarantine_before,
            1
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery) - recovery_before,
            0
        );
        assert_eq!(std::fs::read(&hourly).unwrap(), hourly_bytes);
        assert_eq!(
            std::fs::read(day.join("svc.parquet.tmp.corrupt")).unwrap(),
            b"truncated output"
        );
        assert!(!day.join("svc.parquet").exists());

        // A directory cannot replace quarantine_target's reserved file. The
        // failed rename retains the source and is one failed recovery attempt.
        std::fs::create_dir(&temporary).unwrap();
        write_rollup_marker(&day, "svc", std::slice::from_ref(&hourly)).unwrap();
        assert!(
            compact_once(
                &tmp.path().join("no-wal"),
                &data,
                Duration::ZERO,
                false,
                None,
                500,
                "2GB",
                None
            )
            .await
            .is_err()
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery) - recovery_before,
            1
        );
        assert_eq!(
            quarantine_count(&handle, QuarantineKind::RollupTemporary) - quarantine_before,
            1
        );
        assert_eq!(std::fs::read(&hourly).unwrap(), hourly_bytes);
        assert!(temporary.is_dir());
        assert!(
            !day.join("svc.parquet.tmp.corrupt.1").exists(),
            "failed rename removes its reservation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn operational_failed_quarantine_reservation_is_not_a_quarantined_file() {
        let handle = operational_metrics();
        let chunk_before = operation_count(&handle, CompactionOperation::Chunk);
        let wal_before = quarantine_count(&handle, QuarantineKind::Wal);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        // The source component fits the filesystem's 255-byte limit, while
        // its .corrupt reservation does not. No permission-bit assumptions.
        let source = wal
            .join("prod")
            .join(format!("{}_0_0.ndjson", "x".repeat(239)));
        std::fs::write(&source, [0; 32]).unwrap();
        assert!(
            compact_once(
                &wal,
                &tmp.path().join("data"),
                Duration::ZERO,
                false,
                None,
                500,
                "2GB",
                None
            )
            .await
            .is_ok()
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::Chunk) - chunk_before,
            1
        );
        assert_eq!(
            quarantine_count(&handle, QuarantineKind::Wal) - wal_before,
            0
        );
        assert_eq!(std::fs::read(&source).unwrap(), [0; 32]);
        assert_eq!(std::fs::read_dir(wal.join("prod")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn operational_repin_suppression_and_pending_wait_are_not_failures() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // Both paths stand down before spawning any blocking work, so this
        // thread-local recorder observes all operation accounting in the test.
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let day = data.join("prod/2026-01-15");
        std::fs::create_dir_all(day.join("00")).unwrap();
        let input = day.join("00/svc.parquet");
        std::fs::write(&input, b"would be quarantined if rollup ran").unwrap();
        let coordinator = Arc::new(RepinCoordinator::new());
        let _pause = coordinator.pause_rollup();
        let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 100_000,
        }));
        assert_eq!(
            compact_once_coordinated(
                &tmp.path().join("no-wal"),
                &data,
                Duration::ZERO,
                Duration::ZERO,
                true,
                Some(&hot),
                500,
                "2GB",
                None,
                Some(&coordinator)
            )
            .await
            .unwrap(),
            0
        );
        let marker = day.join(".rollup-svc");
        std::fs::write(&marker, input.to_string_lossy().as_bytes()).unwrap();
        hot.publication().mark_rollup(&marker);
        assert!(
            compact_once_coordinated(
                &tmp.path().join("no-wal"),
                &data,
                Duration::ZERO,
                Duration::ZERO,
                true,
                Some(&hot),
                500,
                "2GB",
                None,
                Some(&coordinator)
            )
            .await
            .is_err()
        );
        for operation in CompactionOperation::ALL {
            assert_eq!(operation_count(&handle, operation), 0);
        }
        for kind in QuarantineKind::ALL {
            assert_eq!(quarantine_count(&handle, kind), 0);
        }
        assert_eq!(
            std::fs::read(&input).unwrap(),
            b"would be quarantined if rollup ran"
        );
        assert!(marker.exists());
    }

    /// A panicked conform-and-publish task fails the batch with fixed text.
    /// The panic payload can quote event values, and this error is logged
    /// as `compaction_error` on a persisted target, so it never carries it.
    #[tokio::test]
    async fn a_compaction_task_panic_never_carries_its_payload() {
        // The payload of the publication seam's injected panic.
        const PAYLOAD: &str = "injected publication task panic";
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        let input = write_wal_file(&wal.join("prod"), "svc", &[OPERATIONAL_ROW]);
        let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 100_000,
        }));
        hot.publication().panic_next_publication_for_test();
        let outcome = compact_service_batch(
            &[input],
            &data.join("prod"),
            &wal,
            "prod",
            "svc",
            "2GB",
            None,
            Some(hot),
            Vec::new(),
        )
        .await;
        let error = outcome.result.expect_err("the task panicked");
        assert_eq!(error, "compaction task panicked");
        assert!(!error.contains(PAYLOAD), "{error}");
    }

    #[tokio::test]
    async fn operational_chunk_task_panic_keeps_published_output_and_counts_once() {
        let handle = operational_metrics();
        let before = operation_count(&handle, CompactionOperation::Chunk);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        let input = write_wal_file(&wal.join("prod"), "svc", &[OPERATIONAL_ROW]);
        let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 100_000,
        }));
        hot.publication().panic_next_publication_for_test();
        assert!(
            compact_once(
                &wal,
                &data,
                Duration::ZERO,
                false,
                Some(&hot),
                500,
                "2GB",
                None
            )
            .await
            .is_ok()
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::Chunk) - before,
            1
        );
        assert!(input.exists(), "a failed chunk does not remove its WAL");
        let output = find_files_by_ext(&data, "parquet");
        assert_eq!(
            output.len(),
            1,
            "the handled task failure follows publication"
        );
        assert_eq!(read_strings(&output[0], "message"), ["retained"]);
        assert_eq!(
            operation_count(&handle, CompactionOperation::ConsumedWalRemoval),
            0
        );
        assert_eq!(quarantine_count(&handle, QuarantineKind::Wal), 0);
    }

    #[tokio::test]
    async fn operational_rollup_task_panic_counts_once_without_erasing_output() {
        let handle = operational_metrics();
        let before = operation_count(&handle, CompactionOperation::DailyRollupUnit);
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let env = data.join("prod");
        let input = write_hourly_parquet(&env, "2026-01-15", "00", "svc", &[OPERATIONAL_ROW]);
        // The outer cycle handles the propagated rollup task error. Only
        // the per-unit JoinError owner emits the operation increment.
        let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 100_000,
        }));
        let gate = hot.publication();
        gate.initialize(&data);
        gate.panic_next_publication_for_test();
        assert!(
            compact_once(
                &tmp.path().join("no-wal"),
                &data,
                Duration::ZERO,
                true,
                Some(&hot),
                500,
                "2GB",
                None
            )
            .await
            .is_ok()
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupUnit) - before,
            1
        );
        let output = env.join("2026-01-15/svc.parquet");
        assert_eq!(read_strings(&output, "message"), ["retained"]);
        assert!(input.exists(), "the task panicked before source retirement");
        assert!(env.join("2026-01-15/.rollup-svc").exists());
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery),
            0
        );
    }

    #[tokio::test]
    async fn operational_recovery_task_panics_count_once_at_each_async_owner() {
        let handle = operational_metrics();
        let before = operation_count(&handle, CompactionOperation::PendingRollupRecovery);
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let env = data.join("prod");
        let input = write_hourly_parquet(&env, "2026-01-15", "00", "svc", &[OPERATIONAL_ROW]);
        let day = env.join("2026-01-15");
        let original = std::fs::read(&input).unwrap();
        write_rollup_marker(&day, "svc", std::slice::from_ref(&input)).unwrap();
        let gate = Arc::new(PublicationGate::new());
        gate.initialize(&data);
        gate.panic_next_publication_for_test();
        assert!(recover_pending_rollups(&gate, None).await.is_err());
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery) - before,
            1
        );
        gate.panic_next_publication_for_test();
        assert!(
            rollup_env_once(
                "prod",
                &env,
                &PublicationClaims::default(),
                "2GB",
                None,
                Some(Arc::clone(&gate))
            )
            .await
            .is_err()
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::PendingRollupRecovery) - before,
            2
        );
        assert_eq!(std::fs::read(&input).unwrap(), original);
        assert!(day.join(".rollup-svc").exists());
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupUnit),
            0
        );
        assert_eq!(
            quarantine_count(&handle, QuarantineKind::RollupTemporary),
            0
        );
    }

    // ---- publication markers (ADR-0041, #252) -----------------------------

    fn recovery_count(
        handle: &metrics_exporter_prometheus::PrometheusHandle,
        outcome: RecoveryOutcomeKind,
    ) -> u64 {
        crate::metrics::test_support::sample(
            handle,
            &format!(
                "trawl_publication_recovery_total{{outcome=\"{}\"}}",
                outcome.label()
            ),
        )
    }

    /// Everything logged in this test process. Compaction logs from blocking
    /// threads, so a thread-local subscriber would miss it; nextest runs each
    /// test in its own process, as for [`operational_metrics`].
    fn captured_logs() -> Arc<std::sync::Mutex<Vec<u8>>> {
        #[derive(Clone)]
        struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        static LOGS: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> =
            std::sync::OnceLock::new();
        Arc::clone(LOGS.get_or_init(|| {
            let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Sink(Arc::clone(&logs));
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || sink.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("one global subscriber per test process");
            logs
        }))
    }

    fn logged(logs: &std::sync::Mutex<Vec<u8>>) -> String {
        String::from_utf8_lossy(&logs.lock().unwrap()).into_owned()
    }

    /// One tagged event per message, in a WAL file with a unique name.
    fn tagged_wal(env_wal: &Path, service: &str, tags: &[&str]) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = env_wal.join(format!(
            "{service}_{}_{seq:04x}.ndjson",
            1_730_000_000_000u64 + seq
        ));
        let rows: Vec<String> = tags
            .iter()
            .map(|tag| {
                format!(
                    r#"{{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"{service}","message":"{tag}"}}"#
                )
            })
            .collect();
        std::fs::write(&path, rows.join("\n")).unwrap();
        path
    }

    /// Put a WAL file's events in the hot buffer under its batch id, as
    /// ingest does after the WAL write.
    fn insert_hot(hot: &HotBuffer, env: &str, wal: &Path, service: &str) {
        let body = std::fs::read_to_string(wal).unwrap();
        let events: Vec<serde_json::Map<String, serde_json::Value>> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        hot.insert_for_test(Arc::new(crate::bus::IngestBatch {
            batch_id: format!("{env}/{}", wal.file_stem().unwrap().to_str().unwrap()).into(),
            service: service.into(),
            events,
            byte_size: body.len(),
        }));
    }

    fn hot_buffer() -> Arc<HotBuffer> {
        Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 1000,
            max_bytes: 1_000_000,
        }))
    }

    /// Every `message` in every published parquet file, sorted.
    fn published_messages(data: &Path) -> Vec<String> {
        let mut rows: Vec<String> = find_files_by_ext(data, "parquet")
            .iter()
            .flat_map(|file| read_strings(file, "message"))
            .collect();
        rows.sort();
        rows
    }

    fn sorted(tags: &[&str]) -> Vec<String> {
        let mut tags: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
        tags.sort();
        tags
    }

    fn marker_file(wal: &Path, env: &str, service: &str) -> PathBuf {
        wal.join(env).join(format!(".publish-{service}.json"))
    }

    async fn tick(wal: &Path, data: &Path, hot: &Arc<HotBuffer>) -> Result<u64, String> {
        compact_once(
            wal,
            data,
            Duration::ZERO,
            false,
            Some(hot),
            DEFAULT_CHUNK_SIZE,
            "2GB",
            None,
        )
        .await
    }

    /// Run phase 1 and phase 3 of one `prod`/`svc` batch on a blocking
    /// thread, as compaction does. `stop_at` interrupts the publish at that
    /// crash point ([`publication_marker::interrupt`] is per thread), and
    /// `between` runs after the WAL read and before the publish.
    async fn publish_batch(
        wal: &Path,
        data: &Path,
        files: &[PathBuf],
        hot: Option<&Arc<HotBuffer>>,
        stop_at: Option<&'static str>,
        between: impl FnOnce() + Send + 'static,
    ) -> Result<(WriteReport, Option<PublishIncomplete>), String> {
        let wal = wal.to_path_buf();
        let env_data = data.join("prod");
        let files = files.to_vec();
        let hot = hot.cloned();
        tokio::task::spawn_blocking(move || {
            let _stop = stop_at.map(publication_marker::interrupt::at);
            let mut quarantined = Vec::new();
            let prep = prepare_service_batch(
                &files,
                &env_data,
                "svc",
                "2GB",
                &mut quarantined,
                &HashMap::new(),
            )?
            .expect("a readable batch");
            between();
            let pins = local_pins(&prep.proposals);
            let batch_ids: Vec<String> = files
                .iter()
                .map(|f| format!("prod/{}", f.file_stem().unwrap().to_str().unwrap()))
                .collect();
            conform_and_publish(
                prep,
                &pins,
                &PublishTarget {
                    data_dir: &env_data,
                    wal_dir: &wal,
                    env: "prod",
                },
                "svc",
                hot.as_deref(),
                &batch_ids,
            )
        })
        .await
        .unwrap()
    }

    /// Restores a directory's mode on drop, so a failed assertion cannot
    /// leave the temporary directory undeletable.
    #[cfg(unix)]
    struct ModeGuard(PathBuf);

    #[cfg(unix)]
    impl ModeGuard {
        fn read_only(dir: &Path) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            Self(dir.to_path_buf())
        }
    }

    #[cfg(unix)]
    impl Drop for ModeGuard {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    #[cfg(unix)]
    fn running_as_root(dir: &Path) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(dir).unwrap().uid() == 0
    }

    /// A consumed WAL file that can be neither deleted nor renamed aside
    /// after the publish: the failure is counted once as the removal
    /// operation, the published output and its drained batches stand, and
    /// the marker keeps the WAL from being merged again. Before ADR-0041
    /// this failure was a warning after the publish and the next tick merged
    /// the WAL a second time.
    #[cfg(unix)]
    #[tokio::test]
    async fn operational_consumed_wal_removal_failure_preserves_published_output() {
        let handle = operational_metrics();
        let removal_before = operation_count(&handle, CompactionOperation::ConsumedWalRemoval);
        let chunk_before = operation_count(&handle, CompactionOperation::Chunk);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        if running_as_root(&env_wal) {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let input = write_wal_file(&env_wal, "svc", &[OPERATIONAL_ROW]);
        let original = std::fs::read(&input).unwrap();
        let hot = hot_buffer();
        let (entered, release) = hot.publication().pause_next_publication_for_test();
        let obstruct_dir = env_wal.clone();
        let obstruction = std::thread::spawn(move || {
            entered.recv_timeout(Duration::from_secs(10)).unwrap();
            // The output is renamed into place and the marker written. Make
            // both the delete and the rename-aside of the input fail.
            let guard = ModeGuard::read_only(&obstruct_dir);
            release.send(()).unwrap();
            guard
        });
        let result = tick(&wal, &data, &hot).await;
        let guard = obstruction.join().unwrap();
        assert!(
            result.is_ok(),
            "an incomplete publish is a chunk-level failure"
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::ConsumedWalRemoval) - removal_before,
            1
        );
        assert_eq!(
            operation_count(&handle, CompactionOperation::Chunk) - chunk_before,
            0
        );
        assert_eq!(std::fs::read(&input).unwrap(), original);
        assert!(marker_file(&wal, "prod", "svc").is_file());
        let output = find_files_by_ext(&data, "parquet");
        assert_eq!(output.len(), 1);
        assert_eq!(read_strings(&output[0], "message"), ["retained"]);

        drop(guard);
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert!(!input.exists(), "recovery retires the WAL");
        assert!(!marker_file(&wal, "prod", "svc").exists());
        assert_eq!(published_messages(&data), ["retained"]);
    }

    /// AC4: a consumed WAL that cannot be deleted never duplicates. Several
    /// ticks leave exactly one copy of every event, the marker blocks the
    /// re-merge, and each failure is logged and counted. Once the WAL can be
    /// retired again, one tick finishes the publish.
    #[cfg(unix)]
    #[tokio::test]
    async fn compaction_undeletable_wal_never_duplicates() {
        let handle = operational_metrics();
        let logs = captured_logs();
        let removal_before = operation_count(&handle, CompactionOperation::ConsumedWalRemoval);
        let failed_before = recovery_count(&handle, RecoveryOutcomeKind::Failed);
        let published_before = recovery_count(&handle, RecoveryOutcomeKind::Published);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        if running_as_root(&env_wal) {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let hot = hot_buffer();
        let tags = ["ac4-a0", "ac4-a1", "ac4-b0", "ac4-b1", "ac4-b2"];
        let first = tagged_wal(&env_wal, "svc", &tags[..2]);
        let second = tagged_wal(&env_wal, "svc", &tags[2..]);
        insert_hot(&hot, "prod", &first, "svc");
        insert_hot(&hot, "prod", &second, "svc");

        let (entered, release) = hot.publication().pause_next_publication_for_test();
        let obstruct_dir = env_wal.clone();
        let obstruction = std::thread::spawn(move || {
            entered.recv_timeout(Duration::from_secs(10)).unwrap();
            let guard = ModeGuard::read_only(&obstruct_dir);
            release.send(()).unwrap();
            guard
        });
        tick(&wal, &data, &hot).await.unwrap();
        let guard = obstruction.join().unwrap();
        assert_eq!(
            operation_count(&handle, CompactionOperation::ConsumedWalRemoval) - removal_before,
            1
        );
        assert_eq!(hot.event_count(), 0, "published batches are drained");

        // The marker keeps the service out of compaction: no chunk even
        // starts, so none can fail on the read-only directory either.
        let chunk_before = operation_count(&handle, CompactionOperation::Chunk);
        for round in 1..=3u64 {
            let errors = tick(&wal, &data, &hot).await.unwrap();
            assert_eq!(errors, 1, "round {round}: the stuck marker is a tick error");
            assert_eq!(published_messages(&data), sorted(&tags), "round {round}");
            assert!(marker_file(&wal, "prod", "svc").is_file(), "round {round}");
            assert!(first.is_file() && second.is_file(), "round {round}");
            assert_eq!(
                recovery_count(&handle, RecoveryOutcomeKind::Failed) - failed_before,
                round
            );
            assert_eq!(
                operation_count(&handle, CompactionOperation::Chunk),
                chunk_before
            );
        }
        let log = logged(&logs);
        assert!(log.contains("finishing the publish failed"), "{log}");
        assert!(log.contains("publication_recovery_failed"), "{log}");
        assert!(log.contains("failed to retire merged input"), "{log}");

        drop(guard);
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(published_messages(&data), sorted(&tags));
        assert!(!marker_file(&wal, "prod", "svc").exists());
        assert!(!first.exists() && !second.exists());
        assert_eq!(
            recovery_count(&handle, RecoveryOutcomeKind::Published) - published_before,
            1
        );
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(published_messages(&data), sorted(&tags));
    }

    /// AC5 at tick level: a marker whose canonical output carries another
    /// identity and whose tmp is gone is a named, counted failure. Nothing
    /// under the WAL or data root changes, and the service stays blocked on
    /// every later tick although its WAL is ready to compact.
    #[tokio::test]
    async fn publication_recovery_contradiction_touches_nothing() {
        fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
            let mut out = std::collections::BTreeMap::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    if path.is_dir() {
                        out.insert(rel, b"<dir>".to_vec());
                        stack.push(path);
                    } else {
                        out.insert(rel, std::fs::read(&path).unwrap());
                    }
                }
            }
            out
        }

        let handle = operational_metrics();
        let logs = captured_logs();
        let before = recovery_count(&handle, RecoveryOutcomeKind::Contradictory);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        let canonical = write_hourly_parquet(
            &data.join("prod"),
            "2026-01-15",
            "07",
            "svc",
            &[OPERATIONAL_ROW],
        );
        let pending = tagged_wal(&env_wal, "svc", &["ac5-pending"]);
        let marker = ValidatedMarker::new(
            "prod",
            "svc",
            chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            7,
            vec![pending.file_name().unwrap().to_str().unwrap().to_owned()],
            publication_marker::OutputIdentity {
                size: 4,
                hash: blake3::hash(b"PAR1"),
            },
        )
        .unwrap();
        publication_marker::write_marker(&wal, &marker).unwrap();
        assert_eq!(marker.canonical(&data), canonical);
        assert!(!marker.tmp(&data).exists());
        let hot = hot_buffer();
        insert_hot(&hot, "prod", &pending, "svc");
        let before_bytes = snapshot(tmp.path());

        for round in 1..=3u64 {
            assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 1, "round {round}");
            assert_eq!(
                recovery_count(&handle, RecoveryOutcomeKind::Contradictory) - before,
                round
            );
            assert_eq!(snapshot(tmp.path()), before_bytes, "round {round}");
            assert_eq!(hot.event_count(), 1, "round {round}: nothing drained");
        }
        let log = logged(&logs);
        assert!(log.contains("publication_recovery_failed"), "{log}");
        assert!(log.contains("reason=\"output_mismatch\""), "{log}");
    }

    /// A crash after the marker and before the rename: the next tick rolls
    /// the publish back (marker and tmp removed, WAL and hot batches kept)
    /// and then publishes the batch exactly once, merged with the canonical
    /// that was already there.
    #[tokio::test]
    async fn publish_interrupted_after_marker_rolls_back_and_publishes_once() {
        let handle = operational_metrics();
        let unpublished_before = recovery_count(&handle, RecoveryOutcomeKind::Unpublished);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        let hot = hot_buffer();

        // An earlier batch is already published in this hour.
        let earlier = tagged_wal(&env_wal, "svc", &["cam-earlier"]);
        insert_hot(&hot, "prod", &earlier, "svc");
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        let canonical = find_files_by_ext(&data, "parquet");
        assert_eq!(canonical.len(), 1);
        let canonical = canonical[0].clone();
        let canonical_before = std::fs::read(&canonical).unwrap();

        let files = vec![
            tagged_wal(&env_wal, "svc", &["cam-a0", "cam-a1"]),
            tagged_wal(&env_wal, "svc", &["cam-b0"]),
        ];
        for file in &files {
            insert_hot(&hot, "prod", file, "svc");
        }
        let err = publish_batch(
            &wal,
            &data,
            &files,
            Some(&hot),
            Some("publish:after_marker"),
            || {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("publish:after_marker"), "{err}");
        let marker = publication_marker::read_marker(&marker_file(&wal, "prod", "svc")).unwrap();
        assert!(marker.tmp(&data).is_file());
        assert_eq!(std::fs::read(&canonical).unwrap(), canonical_before);
        assert_eq!(hot.event_count(), 3, "nothing drained before the rename");

        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(
            recovery_count(&handle, RecoveryOutcomeKind::Unpublished) - unpublished_before,
            1
        );
        let expected = sorted(&["cam-earlier", "cam-a0", "cam-a1", "cam-b0"]);
        assert_eq!(published_messages(&data), expected);
        assert!(!marker_file(&wal, "prod", "svc").exists());
        assert!(!marker.tmp(&data).exists());
        assert!(files.iter().all(|f| !f.exists()));
        assert_eq!(hot.event_count(), 0);
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(published_messages(&data), expected);
    }

    /// A crash after the rename and before retirement: the output and the
    /// drain stand, and the next tick retires the WAL instead of merging it
    /// again.
    #[tokio::test]
    async fn publish_interrupted_after_rename_retires_without_duplicating() {
        let handle = operational_metrics();
        let published_before = recovery_count(&handle, RecoveryOutcomeKind::Published);
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        let hot = hot_buffer();
        let files = vec![
            tagged_wal(&env_wal, "svc", &["car-a0", "car-a1"]),
            tagged_wal(&env_wal, "svc", &["car-b0"]),
        ];
        for file in &files {
            insert_hot(&hot, "prod", file, "svc");
        }
        let (_report, incomplete) = publish_batch(
            &wal,
            &data,
            &files,
            Some(&hot),
            Some("publish:after_rename"),
            || {},
        )
        .await
        .unwrap();
        let incomplete = incomplete.expect("the publish stopped after the rename");
        assert!(
            incomplete.error.contains("publish:after_rename"),
            "{incomplete:?}"
        );
        let expected = sorted(&["car-a0", "car-a1", "car-b0"]);
        assert_eq!(published_messages(&data), expected);
        assert_eq!(hot.event_count(), 0, "drained with the rename");
        assert!(files.iter().all(|f| f.is_file()));
        assert!(marker_file(&wal, "prod", "svc").is_file());

        for _ in 0..2 {
            assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
            assert_eq!(published_messages(&data), expected);
            assert!(files.iter().all(|f| !f.exists()));
            assert!(!marker_file(&wal, "prod", "svc").exists());
        }
        assert_eq!(
            recovery_count(&handle, RecoveryOutcomeKind::Published) - published_before,
            1
        );
    }

    /// AC7: stale-tmp cleanup keeps a tmp that a pending marker claims. A
    /// publish stops after its marker, and the read-only WAL env directory
    /// keeps recovery from removing that marker, so its claim stands while
    /// the tmp ages past the cleanup threshold. Deleting the tmp there would
    /// leave a marker with neither output, a contradiction that blocks the
    /// service for good. Once recovery can run, the batch publishes once.
    #[cfg(unix)]
    #[tokio::test]
    async fn stale_tmp_cleanup_keeps_a_tmp_a_marker_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        if running_as_root(&env_wal) {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let hot = hot_buffer();
        let tags = ["claimed-tmp-a0", "claimed-tmp-a1"];
        let files = vec![tagged_wal(&env_wal, "svc", &tags)];
        insert_hot(&hot, "prod", &files[0], "svc");
        let err = publish_batch(
            &wal,
            &data,
            &files,
            Some(&hot),
            Some("publish:after_marker"),
            || {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("publish:after_marker"), "{err}");
        let marker = publication_marker::read_marker(&marker_file(&wal, "prod", "svc")).unwrap();
        let staged = marker.tmp(&data);
        assert!(staged.is_file());

        let guard = ModeGuard::read_only(&env_wal);
        // `tick` compacts with a zero minimum age, so cleanup takes any tmp
        // older than zero.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            tick(&wal, &data, &hot).await.unwrap(),
            1,
            "the marker recovery could not remove is a tick error"
        );
        assert!(staged.is_file(), "cleanup keeps the claimed tmp");
        assert!(marker_file(&wal, "prod", "svc").is_file());
        assert!(published_messages(&data).is_empty());

        drop(guard);
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(published_messages(&data), sorted(&tags));
        assert!(!marker_file(&wal, "prod", "svc").exists());
        assert!(!staged.exists());
        assert!(!files[0].exists());
        assert_eq!(hot.event_count(), 0);
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(published_messages(&data), sorted(&tags));
    }

    /// AC7: the daily rollup leaves alone every hourly file of a
    /// `(date, service)` a pending marker claims. A published marker whose
    /// WAL cannot be retired stays after recovery, and rolling its hourly
    /// file into the daily one would delete the canonical output whose
    /// identity recovery checks. Another service on the same day rolls up
    /// as usual. Once retirement works, recovery finishes and the next
    /// rollup takes the file.
    #[cfg(unix)]
    #[tokio::test]
    async fn rollup_leaves_a_claimed_hourly_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        if running_as_root(&env_wal) {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1)).date_naive();
        let day = yesterday.format("%Y-%m-%d").to_string();
        let row = |service: &str, message: &str| {
            format!(
                r#"{{"_time":"{day}T07:00:00Z","_ingested":"{day}T07:00:00Z","service":"{service}","message":"{message}"}}"#
            )
        };
        let env_data = data.join("prod");
        let claimed = write_hourly_parquet(
            &env_data,
            &day,
            "07",
            "svc",
            &[&row("svc", "rollup-claimed")],
        );
        let unclaimed = write_hourly_parquet(
            &env_data,
            &day,
            "07",
            "other",
            &[&row("other", "rollup-unclaimed")],
        );
        let consumed = tagged_wal(&env_wal, "svc", &["rollup-claimed"]);
        let marker = ValidatedMarker::new(
            "prod",
            "svc",
            yesterday,
            7,
            vec![consumed.file_name().unwrap().to_str().unwrap().to_owned()],
            publication_marker::identity_of(&claimed).unwrap(),
        )
        .unwrap();
        publication_marker::write_marker(&wal, &marker).unwrap();
        assert_eq!(marker.canonical(&data), claimed);
        let claimed_bytes = std::fs::read(&claimed).unwrap();
        let hot = hot_buffer();
        let rollup_tick = || {
            compact_once(
                &wal,
                &data,
                Duration::ZERO,
                true,
                Some(&hot),
                DEFAULT_CHUNK_SIZE,
                "2GB",
                None,
            )
        };

        let guard = ModeGuard::read_only(&env_wal);
        assert_eq!(
            rollup_tick().await.unwrap(),
            1,
            "the unretired marker is a tick error"
        );
        assert_eq!(std::fs::read(&claimed).unwrap(), claimed_bytes);
        assert!(!env_data.join(&day).join("svc.parquet").exists());
        assert!(!unclaimed.exists(), "an unclaimed service rolls up");
        assert_eq!(
            read_strings(&env_data.join(&day).join("other.parquet"), "message"),
            ["rollup-unclaimed"]
        );
        assert!(marker_file(&wal, "prod", "svc").is_file());

        drop(guard);
        assert_eq!(rollup_tick().await.unwrap(), 0);
        assert!(!marker_file(&wal, "prod", "svc").exists());
        assert!(!consumed.exists(), "recovery retires the WAL");
        assert!(!claimed.exists(), "the next rollup takes the hourly file");
        assert_eq!(
            published_messages(&data),
            sorted(&["rollup-claimed", "rollup-unclaimed"])
        );
    }

    /// Every publish syncs the whole output directory chain before its
    /// marker, not only the directories that attempt created. The first
    /// attempt creates `data/prod/{date}/{HH}` and fails on the sync of
    /// `data/prod`. A retry finds every directory in place, but it must
    /// still sync `data/prod`, so while that sync fails it may neither
    /// publish nor retire the WAL.
    #[test]
    fn a_retried_publish_syncs_directories_an_earlier_attempt_created() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        let env_data = data.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let tags = ["chain-a0", "chain-a1"];
        let files = vec![tagged_wal(&env_wal, "svc", &tags)];
        let attempt = || {
            let mut quarantined = Vec::new();
            let prep = prepare_service_batch(
                &files,
                &env_data,
                "svc",
                "2GB",
                &mut quarantined,
                &HashMap::new(),
            )?
            .expect("a readable batch");
            let pins = local_pins(&prep.proposals);
            let batch_ids = vec![format!(
                "prod/{}",
                files[0].file_stem().unwrap().to_str().unwrap()
            )];
            conform_and_publish(
                prep,
                &pins,
                &PublishTarget {
                    data_dir: &env_data,
                    wal_dir: &wal,
                    env: "prod",
                },
                "svc",
                None,
                &batch_ids,
            )
        };

        let failing = crate::epoch::fail_dir_fsync::set(&env_data);
        for round in 1..=2 {
            let err = attempt().unwrap_err();
            assert!(
                err.contains("failed to fsync directory"),
                "round {round}: {err}"
            );
            assert!(files[0].is_file(), "round {round}: the WAL is kept");
            assert!(!marker_file(&wal, "prod", "svc").exists(), "round {round}");
            assert!(published_messages(&data).is_empty(), "round {round}");
        }

        drop(failing);
        let (_report, incomplete) = attempt().unwrap();
        assert!(incomplete.is_none(), "{incomplete:?}");
        assert_eq!(published_messages(&data), sorted(&tags));
        assert!(!files[0].exists());
        assert!(!marker_file(&wal, "prod", "svc").exists());
    }

    /// A consumed WAL file withdrawn after compaction read it (its writer's
    /// directory fsync failed and it rejected the write) must not publish:
    /// the sender retries that batch. The publish rolls back, keeps the
    /// other inputs, and the next tick publishes them exactly once.
    #[tokio::test]
    async fn a_withdrawn_wal_file_rolls_the_publish_back() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        let hot = hot_buffer();
        let kept = tagged_wal(&env_wal, "svc", &["wd-kept"]);
        let withdrawn = tagged_wal(&env_wal, "svc", &["wd-withdrawn"]);
        insert_hot(&hot, "prod", &kept, "svc");
        let withdraw = withdrawn.clone();
        let err = publish_batch(
            &wal,
            &data,
            &[kept.clone(), withdrawn.clone()],
            Some(&hot),
            None,
            // The writer withdraws its file after compaction read it.
            move || std::fs::remove_file(withdraw).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("disappeared before publication"), "{err}");
        assert!(published_messages(&data).is_empty());
        assert!(find_files_by_ext(&data, "tmp").is_empty(), "tmp removed");
        assert!(!marker_file(&wal, "prod", "svc").exists());
        assert!(kept.is_file());
        assert_eq!(hot.event_count(), 1, "nothing drained");

        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(published_messages(&data), ["wd-kept"]);
        assert!(!kept.exists());
        assert_eq!(hot.event_count(), 0);
    }

    /// The marker lists only the inputs that contributed rows. A corrupt
    /// input is quarantined and never named, and a chunk whose every input
    /// is corrupt writes no marker at all.
    #[tokio::test]
    async fn the_marker_names_only_surviving_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        let env_wal = wal.join("prod");
        std::fs::create_dir_all(&env_wal).unwrap();
        let good = tagged_wal(&env_wal, "svc", &["surv-good"]);
        let corrupt = env_wal.join("svc_1729999999999_dead.ndjson");
        std::fs::write(&corrupt, [0; 32]).unwrap();
        let err = publish_batch(
            &wal,
            &data,
            &[corrupt.clone(), good.clone()],
            None,
            Some("publish:after_marker"),
            || {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("publish:after_marker"), "{err}");
        let marker = publication_marker::read_marker(&marker_file(&wal, "prod", "svc")).unwrap();
        assert_eq!(
            marker.wal_names(),
            [good.file_name().unwrap().to_str().unwrap()]
        );
        assert!(corrupt.with_extension("ndjson.corrupt").is_file());

        let hot = hot_buffer();
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0);
        assert_eq!(published_messages(&data), ["surv-good"]);

        let all_corrupt = env_wal.join("svc_1729999999998_beef.ndjson");
        std::fs::write(&all_corrupt, [0; 32]).unwrap();
        assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 1, "one quarantine");
        assert!(!marker_file(&wal, "prod", "svc").exists());
        assert_eq!(published_messages(&data), ["surv-good"]);
    }

    /// How [`quarantined_batches_drain_whether_or_not_their_chunk_publishes`]
    /// ends its first compaction pass.
    #[derive(Debug, Clone, Copy)]
    enum ChunkEnd {
        Publishes,
        /// Phase 2: the catalog is unreachable, so the pins never become
        /// durable.
        CatalogUnreachable,
        /// Phase 3: the env data path is a file, so the write fails.
        WriteFails,
        /// Phase 1 panics right after the quarantine.
        PrepPanics,
    }

    /// A quarantined WAL file is renamed out of every later scan, so only
    /// the chunk that quarantined it can drain its hot batch. A publish
    /// drains it with the chunk; a chunk that fails after the quarantine
    /// must drain it too, or its charge stays until restart. That includes a
    /// chunk whose preparation panics after the quarantine. The chunk's
    /// other batches stay resident until a retry publishes them.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one fixture, every way the chunk can end
    async fn quarantined_batches_drain_whether_or_not_their_chunk_publishes() {
        for end in [
            ChunkEnd::Publishes,
            ChunkEnd::CatalogUnreachable,
            ChunkEnd::WriteFails,
            ChunkEnd::PrepPanics,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let wal = tmp.path().join("wal");
            let data = tmp.path().join("data");
            let env_wal = wal.join("prod");
            std::fs::create_dir_all(&env_wal).unwrap();
            let hot = hot_buffer();

            // The corrupt batch goes in first, so it is the oldest resident.
            let corrupt = env_wal.join("svc_1729999999999_dead.ndjson");
            std::fs::write(&corrupt, [0; 32]).unwrap();
            hot.insert_for_test(Arc::new(crate::bus::IngestBatch {
                batch_id: "prod/svc_1729999999999_dead".into(),
                service: "svc".into(),
                events: vec![serde_json::Map::new(); 3],
                byte_size: 32,
            }));
            let corrupt_charge = hot.charged();
            assert!(!corrupt_charge.is_zero());

            // A field no pin covers, so phase 2 needs the catalog.
            let good: Vec<PathBuf> = ["q-good-0", "q-good-1"]
                .iter()
                .enumerate()
                .map(|(n, tag)| {
                    let path = env_wal.join(format!("svc_173000000000{n}_{n:04x}.ndjson"));
                    std::fs::write(
                        &path,
                        format!(
                            r#"{{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"svc","message":"{tag}","quarantine_probe":{n}}}"#
                        ),
                    )
                    .unwrap();
                    insert_hot(&hot, "prod", &path, "svc");
                    path
                })
                .collect();
            let all_charged = hot.charged();
            assert_eq!(hot.batch_count(), 3);

            // Deliberately unreachable: this pool simulates a dead catalog
            // store, not a fixture database (ADR-0021 ruling 3).
            #[allow(clippy::disallowed_methods)]
            let dead_catalog = CatalogContext {
                store: crate::store::CatalogStore::new(
                    sqlx::postgres::PgPoolOptions::new()
                        .acquire_timeout(Duration::from_millis(200))
                        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
                        .unwrap(),
                ),
                cache: Arc::new(crate::catalog::FieldCatalog::new()),
            };
            let catalog = match end {
                ChunkEnd::CatalogUnreachable => Some(&dead_catalog),
                ChunkEnd::Publishes | ChunkEnd::WriteFails | ChunkEnd::PrepPanics => None,
            };
            let panic = matches!(end, ChunkEnd::PrepPanics).then(|| phase1_panic::arm(&data));
            let env_data = data.join("prod");
            if matches!(end, ChunkEnd::WriteFails) {
                std::fs::create_dir_all(&data).unwrap();
                std::fs::write(&env_data, b"not a directory").unwrap();
            }
            let quarantines = compact_once(
                &wal,
                &data,
                Duration::ZERO,
                false,
                Some(&hot),
                DEFAULT_CHUNK_SIZE,
                "2GB",
                catalog,
            )
            .await
            .unwrap();
            assert_eq!(quarantines, 1, "{end:?}");
            assert!(
                corrupt.with_extension("ndjson.corrupt").is_file(),
                "{end:?}"
            );

            if matches!(end, ChunkEnd::Publishes) {
                assert_eq!(hot.charged(), Charge::ZERO, "{end:?}");
                assert_eq!(hot.oldest_batch_age(), None, "{end:?}");
                assert_eq!(hot.drained_batches(), 3, "{end:?}");
                assert_eq!(published_messages(&data), ["q-good-0", "q-good-1"]);
                continue;
            }

            assert!(good.iter().all(|path| path.is_file()), "{end:?}: WAL kept");
            assert_eq!(
                hot.charged(),
                all_charged.checked_sub(corrupt_charge).unwrap(),
                "{end:?}: exactly the quarantined batch's charge is released"
            );
            assert_eq!(hot.drained_batches(), 1, "{end:?}");
            assert_eq!(hot.batch_count(), 2, "{end:?}: the failed chunk stays");
            assert!(published_messages(&data).is_empty(), "{end:?}");

            // Clear the fault: the retry publishes the rest, and nothing
            // stays behind to age.
            if matches!(end, ChunkEnd::WriteFails) {
                std::fs::remove_file(&env_data).unwrap();
            }
            drop(panic);
            assert_eq!(tick(&wal, &data, &hot).await.unwrap(), 0, "{end:?}");
            assert_eq!(hot.charged(), Charge::ZERO, "{end:?}");
            assert_eq!(hot.oldest_batch_age(), None, "{end:?}");
            assert_eq!(published_messages(&data), ["q-good-0", "q-good-1"]);
        }
    }

    #[test]
    fn recovery_keeps_runtime_responsive_and_guards_after_cancellation() {
        for pending in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("data");
            let env = root.join("prod");
            let row = r#"{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"nginx","msg":"one"}"#;
            let hourly = write_hourly_parquet(&env, "2026-01-15", "00", "nginx", &[row]);
            let day = env.join("2026-01-15");
            let daily = day.join("nginx.parquet");
            std::fs::copy(&hourly, &daily).unwrap();
            write_rollup_marker(&day, "nginx", std::slice::from_ref(&hourly)).unwrap();
            let marker = rollup_marker_path(&day, "nginx");
            let gate = Arc::new(PublicationGate::new());
            gate.initialize(&root);
            let coordinator = Arc::new(RepinCoordinator::new());
            let (entered, release) = gate.pause_next_publication_for_test();
            let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
            let (checked_tx, checked_rx) = std::sync::mpsc::channel();
            let runtime_gate = Arc::clone(&gate);
            let runtime_coordinator = Arc::clone(&coordinator);
            let runtime_thread = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .max_blocking_threads(1)
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let recovery_gate = Arc::clone(&runtime_gate);
                    let recovery_coordinator = Arc::clone(&runtime_coordinator);
                    let recovery = tokio::spawn(async move {
                        if pending {
                            recover_pending_rollups(&recovery_gate, Some(&recovery_coordinator))
                                .await
                        } else {
                            rollup_env_once(
                                "prod",
                                &env,
                                &PublicationClaims::default(),
                                "2GB",
                                Some(&recovery_coordinator),
                                Some(recovery_gate),
                            )
                            .await
                            .map(|_| ())
                        }
                    });
                    paused_rx.await.unwrap();
                    recovery.abort();
                    let cancelled = recovery.await.is_err_and(|error| error.is_cancelled());
                    let readers_blocked =
                        tokio::time::timeout(Duration::from_millis(20), runtime_gate.read())
                            .await
                            .is_err();
                    let cutover_blocked = tokio::time::timeout(
                        Duration::from_millis(20),
                        runtime_coordinator.cutover_guard(),
                    )
                    .await
                    .is_err();
                    checked_tx
                        .send((cancelled, readers_blocked, cutover_blocked))
                        .unwrap();
                    drop(
                        tokio::time::timeout(Duration::from_secs(30), runtime_gate.read())
                            .await
                            .unwrap()
                            .unwrap(),
                    );
                    drop(
                        tokio::time::timeout(
                            Duration::from_secs(30),
                            runtime_coordinator.cutover_guard(),
                        )
                        .await
                        .unwrap(),
                    );
                });
            });

            // Control the pause outside Tokio so a regression that blocks its
            // only worker fails within a deadline and still releases recovery.
            let entered_result = entered.recv_timeout(Duration::from_secs(30));
            let _ = paused_tx.send(());
            let checks = checked_rx.recv_timeout(Duration::from_secs(5));
            let marker_pending = gate.pending_rollup_markers() == vec![marker.clone()];
            let hourly_retained = hourly.exists();
            let _ = release.send(());
            let completed = runtime_thread.join();
            entered_result.unwrap();
            completed.unwrap();
            assert_eq!(checks.unwrap(), (true, true, true), "pending={pending}");
            assert!(marker_pending && hourly_retained);
            assert!(!marker.exists());
            assert!(!hourly.exists());
            assert_eq!(read_strings(&daily, "msg"), vec!["one"]);
        }
    }

    #[tokio::test]
    async fn failed_marker_stage_preserves_complete_list_and_stage_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let env = root.join("prod");
        let old = r#"{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"nginx","msg":"old"}"#;
        let new = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"new"}"#;
        let first = write_hourly_parquet(&env, "2026-01-15", "00", "nginx", &[old]);
        let second = write_hourly_parquet(&env, "2026-01-15", "01", "nginx", &[new]);
        let combined = write_hourly_parquet(&env, "2026-01-15", "02", "nginx", &[old, new]);
        let day = env.join("2026-01-15");
        std::fs::rename(combined, day.join("nginx.parquet")).unwrap();
        write_rollup_marker(&day, "nginx", &[first.clone(), second.clone()]).unwrap();
        let marker = rollup_marker_path(&day, "nginx");
        let complete = std::fs::read(&marker).unwrap();
        let stage = day.join(format!("..rollup-nginx.next.{}", std::process::id()));
        std::fs::create_dir(&stage).unwrap();
        assert!(write_rollup_marker(&day, "nginx", std::slice::from_ref(&first)).is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), complete);
        std::fs::remove_dir(&stage).unwrap();
        std::fs::write(&stage, b"partial staged list").unwrap();
        let gate = PublicationGate::new();
        gate.initialize(&root);
        assert_eq!(gate.pending_rollup_markers(), vec![marker]);
        {
            let _writer = gate.write().await;
            recover_rollup_markers_coordinated(&day, Some(&gate)).unwrap();
        }
        assert!(!first.exists());
        assert!(!second.exists());
        assert_eq!(std::fs::read(stage).unwrap(), b"partial staged list");
        assert!(gate.read().await.is_ok());
        assert_eq!(
            read_strings(&day.join("nginx.parquet"), "msg"),
            vec!["old", "new"]
        );
    }

    #[tokio::test]
    async fn failed_invalid_tmp_quarantine_cannot_retire_unpublished_rows_on_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let env = root.join("prod");
        let old = r#"{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"nginx","msg":"old"}"#;
        let new = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"new"}"#;
        let first = write_hourly_parquet(&env, "2026-01-15", "00", "nginx", &[old]);
        let hourly = write_hourly_parquet(&env, "2026-01-15", "01", "nginx", &[new]);
        let day = env.join("2026-01-15");
        let canonical = day.join("nginx.parquet");
        std::fs::rename(first, &canonical).unwrap();
        let invalid_tmp = day.join("nginx.parquet.tmp");
        std::fs::create_dir(&invalid_tmp).unwrap();
        write_rollup_marker(&day, "nginx", std::slice::from_ref(&hourly)).unwrap();
        let gate = PublicationGate::new();
        gate.initialize(&root);
        {
            let _writer = gate.write().await;
            assert!(recover_rollup_markers_coordinated(&day, Some(&gate)).is_err());
            assert!(!rollup_marker_path(&day, "nginx").exists());
            assert!(invalid_tmp.is_dir());
            recover_rollup_markers_coordinated(&day, Some(&gate)).unwrap();
        }
        assert_eq!(read_strings(&canonical, "msg"), vec!["old"]);
        assert_eq!(read_strings(&hourly, "msg"), vec!["new"]);
        assert!(gate.read().await.is_ok());
    }

    #[tokio::test]
    async fn pending_rollup_recovers_before_fresh_wal_even_when_rollup_disabled() {
        for with_hot in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            let env = data.join("prod");
            let wal = tmp.path().join("wal");
            let env_wal = wal.join("prod");
            std::fs::create_dir_all(&env_wal).unwrap();
            let now = chrono::Utc::now();
            let partition_day = now.format("%Y-%m-%d").to_string();
            let hour = now.format("%H").to_string();
            let old = r#"{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"nginx","msg":"old"}"#;
            let fresh = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"fresh"}"#;
            let hourly = write_hourly_parquet(&env, &partition_day, &hour, "nginx", &[old]);
            let day = env.join(&partition_day);
            let daily = day.join("nginx.parquet");
            std::fs::copy(&hourly, &daily).unwrap();
            write_rollup_marker(&day, "nginx", std::slice::from_ref(&hourly)).unwrap();
            let fresh_wal = write_wal_file(&env_wal, "nginx", &[fresh]);
            let hot = with_hot.then(|| {
                Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
                    max_events: 100,
                    max_bytes: 100_000,
                }))
            });
            if let Some(buf) = &hot {
                let id = format!("prod/{}", fresh_wal.file_stem().unwrap().to_str().unwrap());
                buf.insert_for_test(Arc::new(crate::bus::IngestBatch {
                    batch_id: id.into(),
                    service: "nginx".into(),
                    events: vec![serde_json::from_str(fresh).unwrap()],
                    byte_size: fresh.len(),
                }));
            }
            let errors = compact_once(
                &wal,
                &data,
                Duration::ZERO,
                false,
                hot.as_ref(),
                DEFAULT_CHUNK_SIZE,
                "2GB",
                None,
            )
            .await
            .unwrap();
            assert_eq!(errors, 0);
            assert!(!rollup_marker_path(&day, "nginx").exists());
            assert!(!fresh_wal.exists());
            assert_eq!(read_strings(&daily, "msg"), vec!["old"]);
            let mut rows = Vec::new();
            for file in find_files_by_ext(&data, "parquet") {
                rows.extend(read_strings(&file, "msg"));
            }
            rows.sort();
            assert_eq!(rows, vec!["fresh", "old"]);
            if let Some(buf) = hot {
                assert_eq!(buf.event_count(), 0);
                assert!(buf.publication().read().await.is_ok());
            }
        }
    }

    #[test]
    fn publication_blocks_reader_until_hot_batch_is_drained() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        let record = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"nginx","message":"one"}"#;
        let file = write_wal_file(&wal.join("prod"), "nginx", &[record]);
        let hot = Arc::new(HotBuffer::new(crate::hot_buffer::HotBufferConfig {
            max_events: 100,
            max_bytes: 100_000,
        }));
        hot.insert_for_test(Arc::new(crate::bus::IngestBatch {
            batch_id: "prod/batch".into(),
            service: "nginx".into(),
            events: vec![serde_json::from_str(record).unwrap()],
            byte_size: record.len(),
        }));
        let gate = hot.publication();
        let (entered, release) = gate.pause_next_publication_for_test();
        let writer_hot = Arc::clone(&hot);
        let writer_data = data.clone();
        let writer_wal = wal.clone();
        let writer = std::thread::spawn(move || {
            let mut quarantined = Vec::new();
            let prep = prepare_service_batch(
                &[file],
                &writer_data,
                "nginx",
                "2GB",
                &mut quarantined,
                &HashMap::new(),
            )
            .unwrap()
            .unwrap();
            let pins = local_pins(&prep.proposals);
            conform_and_publish(
                prep,
                &pins,
                &PublishTarget {
                    data_dir: &writer_data.join("prod"),
                    wal_dir: &writer_wal,
                    env: "prod",
                },
                "nginx",
                Some(&writer_hot),
                &["prod/batch".to_owned()],
            )
            .unwrap();
        });
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(hot.event_count(), 1);
        assert_eq!(find_files_by_ext(&data, "parquet").len(), 1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), gate.read())
                    .await
                    .is_err()
            );
        });
        release.send(()).unwrap();
        writer.join().unwrap();
        runtime.block_on(async {
            let _reader = gate.read().await.unwrap();
            assert_eq!(hot.event_count(), 0);
            let files = find_files_by_ext(&data, "parquet");
            assert_eq!(read_strings(&files[0], "message"), vec!["one"]);
        });
    }

    #[test]
    fn rollup_publication_blocks_readers_until_hourly_retirement() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let old = r#"{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"nginx","msg":"old"}"#;
        let new = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"new"}"#;
        let first = write_hourly_parquet(&data, "2026-01-15", "00", "nginx", &[old]);
        let hourly = write_hourly_parquet(&data, "2026-01-15", "01", "nginx", &[new]);
        let day = data.join("2026-01-15");
        let daily = day.join("nginx.parquet");
        std::fs::rename(first, &daily).unwrap();
        let gate = Arc::new(PublicationGate::new());
        let (entered, release) = gate.pause_next_publication_for_test();
        let writer_gate = Arc::clone(&gate);
        let writer_day = day.clone();
        let writer_hourly = hourly.clone();
        let writer = std::thread::spawn(move || {
            rollup_day_coordinated(
                &writer_day,
                "nginx",
                &[writer_hourly],
                "2GB",
                Some(&writer_gate),
            )
            .result
            .unwrap();
        });
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(hourly.exists());
        assert_eq!(read_strings(&daily, "msg"), vec!["old", "new"]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), gate.read())
                    .await
                    .is_err()
            );
        });
        release.send(()).unwrap();
        writer.join().unwrap();
        runtime.block_on(async {
            let _reader = gate.read().await.unwrap();
            assert!(!hourly.exists());
            assert!(!rollup_marker_path(&day, "nginx").exists());
            assert_eq!(find_files_by_ext(&data, "parquet"), vec![daily.clone()]);
            assert_eq!(read_strings(&daily, "msg"), vec!["old", "new"]);
        });
    }

    #[test]
    fn recovery_promotes_tmp_over_old_daily_before_retiring_hourlies() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let row = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"new"}"#;
        let old = r#"{"_time":"2026-01-15T00:00:00Z","_ingested":"2026-01-15T00:00:00Z","service":"nginx","msg":"old"}"#;
        let previous = write_hourly_parquet(&data, "2026-01-15", "00", "nginx", &[old]);
        let combined = write_hourly_parquet(&data, "2026-01-15", "02", "nginx", &[old, row]);
        let hourly = write_hourly_parquet(&data, "2026-01-15", "01", "nginx", &[row]);
        let day = data.join("2026-01-15");
        let canonical = day.join("nginx.parquet");
        let staged = day.join("nginx.parquet.tmp");
        std::fs::rename(previous, &canonical).unwrap();
        std::fs::rename(combined, &staged).unwrap();
        write_rollup_marker(&day, "nginx", std::slice::from_ref(&hourly)).unwrap();
        remove_stale_tmp(&staged, Duration::ZERO);
        assert!(
            staged.exists(),
            "cleanup must preserve recovery's prepared file"
        );
        recover_rollup_markers(&day).unwrap();
        assert_eq!(read_strings(&canonical, "msg"), vec!["old", "new"]);
        assert!(!hourly.exists());
        assert!(!staged.exists());
    }

    #[test]
    fn corrupt_tmp_preserves_old_daily_and_hourlies() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path();
        let canonical = day.join("nginx.parquet");
        let hourly = day.join("hourly.parquet");
        std::fs::write(&canonical, b"old daily").unwrap();
        std::fs::write(&hourly, b"hourly input").unwrap();
        std::fs::write(day.join("nginx.parquet.tmp"), b"truncated").unwrap();
        write_rollup_marker(day, "nginx", std::slice::from_ref(&hourly)).unwrap();
        recover_rollup_markers(day).unwrap();
        assert_eq!(std::fs::read(canonical).unwrap(), b"old daily");
        assert!(hourly.exists());
    }

    #[tokio::test]
    async fn failed_rollup_recovery_keeps_readers_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path();
        let hourly = day.join("hourly.parquet");
        std::fs::create_dir(&hourly).unwrap();
        std::fs::write(hourly.join("child"), b"occupied").unwrap();
        let aside = day.join("hourly.parquet.merged");
        std::fs::create_dir(&aside).unwrap();
        std::fs::write(aside.join("child"), b"occupied").unwrap();
        std::fs::write(day.join("nginx.parquet"), b"daily").unwrap();
        write_rollup_marker(day, "nginx", &[hourly]).unwrap();
        let gate = Arc::new(PublicationGate::new());
        {
            let _writer = gate.write().await;
            gate.mark_rollup(&rollup_marker_path(day, "nginx"));
        }
        assert!(recover_pending_rollups(&gate, None).await.is_err());
        assert!(gate.read().await.is_err());
        assert!(rollup_marker_path(day, "nginx").exists());
    }

    /// The pass-through shortcut is a claim about the conform, not about
    /// the physical type: it holds only where a column already of that
    /// type needs nothing done to it.
    ///
    /// SEVERITY is the exception, and it must be: its domain is the 1-24
    /// ladder, so a BIGINT column carrying `99` still has to be nulled —
    /// otherwise the corpus holds a value whose canonical token text is
    /// NULL, and `_severity=warn*` and the results table disagree about
    /// what is in the column (ADR-0013).
    #[test]
    fn the_conform_pass_through_excludes_the_severity_range_guard() {
        for pin in [
            CanonicalType::BigInt,
            CanonicalType::Double,
            CanonicalType::Boolean,
            CanonicalType::Timestamp,
            CanonicalType::Varchar,
        ] {
            assert_eq!(
                conform_expr("\"c\"", pin.as_duckdb(), pin),
                None,
                "{pin:?} already IS its pin"
            );
        }
        let sql = conform_expr("\"c\"", "BIGINT", CanonicalType::Severity)
            .expect("SEVERITY still guards the ladder range");
        assert!(sql.contains("BETWEEN 1 AND 24"), "{sql}");
        // …and a non-matching physical type conforms as any other pin does.
        assert!(conform_expr("\"c\"", "VARCHAR", CanonicalType::Severity).is_some());
    }

    #[test]
    fn extract_service_simple() {
        assert_eq!(
            extract_service_from_filename("nginx_1234567890_abcd"),
            "nginx"
        );
    }

    #[test]
    fn extract_service_with_underscores() {
        assert_eq!(
            extract_service_from_filename("my_cool_service_1234567890_abcd"),
            "my_cool_service"
        );
    }

    #[test]
    fn group_by_service_groups_correctly() {
        let files = vec![
            PathBuf::from("/wal/nginx_100_aaaa.ndjson"),
            PathBuf::from("/wal/nginx_200_bbbb.ndjson"),
            PathBuf::from("/wal/postgres_100_cccc.ndjson"),
        ];
        let groups = group_by_service(files);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["nginx"].len(), 2);
        assert_eq!(groups["postgres"].len(), 1);
    }

    /// Helper: write an ndjson WAL file with the given records.
    fn write_wal_file(dir: &Path, service: &str, records: &[&str]) -> PathBuf {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let path = dir.join(format!("{service}_{millis}_test.ndjson"));
        std::fs::write(&path, records.join("\n")).unwrap();
        path
    }

    /// Recursively find files matching an extension under a directory.
    fn find_files_by_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
        let mut result = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    result.extend(find_files_by_ext(&path, ext));
                } else if path.extension().is_some_and(|e| e == ext) {
                    result.push(path);
                }
            }
        }
        result
    }

    #[test]
    fn compact_creates_canonical_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let record = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"nginx","message":"hello"}"#;
        let wal_files = vec![write_wal_file(&wal_dir, "nginx", &[record])];

        compact_service_blocking(&wal_files, &data_dir, "nginx", "2GB").unwrap();

        // Should create {date}/{hour}/nginx.parquet (canonical name).
        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);
        assert_eq!(
            parquet_files[0].file_name().unwrap().to_str().unwrap(),
            "nginx.parquet",
            "expected canonical filename"
        );

        // No .tmp files left behind.
        let tmp_files = find_files_by_ext(&data_dir, "tmp");
        assert!(tmp_files.is_empty(), "stale .tmp files found");
    }

    #[test]
    fn compact_merges_with_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // First compaction: write initial data.
        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"nginx","message":"first"}"#;
        let files1 = vec![write_wal_file(&wal_dir, "nginx", &[r1])];
        compact_service_blocking(&files1, &data_dir, "nginx", "2GB").unwrap();

        // Second compaction: merge new data into existing file.
        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"nginx","message":"second"}"#;
        let files2 = vec![write_wal_file(&wal_dir, "nginx", &[r2])];
        compact_service_blocking(&files2, &data_dir, "nginx", "2GB").unwrap();

        // Still only one parquet file.
        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        // Verify merged file contains both rows.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "merged file should contain 2 rows");
    }

    /// `field_services.row_count` is accumulated by `touch_services`, so
    /// the report must carry this batch's rows — the merged file's total
    /// would re-add every earlier batch on every tick. And the observed
    /// fields must be the post-conform set: a deferred-pin column is
    /// dropped from the parquet, so claiming it as observed is a lie.
    #[test]
    fn write_report_counts_only_this_batch_and_written_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let report_for = |records: &[&str]| {
            let files = vec![write_wal_file(&wal_dir, "nginx", records)];
            let mut quarantined = Vec::new();
            let prep = prepare_service_batch(
                &files,
                &data_dir,
                "nginx",
                "2GB",
                &mut quarantined,
                &HashMap::new(),
            )
            .unwrap()
            .expect("batch survives");
            let pins = local_pins(&prep.proposals);
            conform_and_write(prep, &pins, &data_dir, "nginx").unwrap()
        };

        // `deferred` is all-null, so its pin defers and conform drops it.
        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"nginx","message":"one","deferred":null}"#;
        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"nginx","message":"two","deferred":null}"#;
        let first = report_for(&[r1, r2]);
        assert_eq!(first.batch_rows, 2);
        assert!(first.observed_fields.iter().any(|f| f == "message"));
        assert!(
            !first.observed_fields.iter().any(|f| f == "deferred"),
            "a dropped deferred-pin column was never written: {:?}",
            first.observed_fields
        );

        let r3 = r#"{"_time":"2026-01-01T00:00:02Z","_ingested":"2026-01-01T00:00:02Z","service":"nginx","message":"three"}"#;
        let second = report_for(&[r3]);
        assert_eq!(
            second.batch_rows, 1,
            "the merge tick must report its own rows, not the file total"
        );

        // The file really did merge to 3 rows — so 1 is the batch count,
        // not an artefact of a failed merge.
        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3, "merged file holds both batches");
    }

    #[test]
    fn compact_sorts_rows_by_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Ingest out-of-order timestamps in both the fresh-write and
        // merge paths to verify both sort.
        let late = r#"{"_time":"2026-01-01T00:00:10Z","_ingested":"2026-01-01T00:00:10Z","service":"nginx","msg":"late"}"#;
        let early = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"nginx","msg":"early"}"#;
        compact_service_blocking(
            &[write_wal_file(&wal_dir, "nginx", &[late, early])],
            &data_dir,
            "nginx",
            "2GB",
        )
        .unwrap();

        // Second batch merges into the existing file; include a timestamp
        // that should sort between the two above.
        let middle = r#"{"_time":"2026-01-01T00:00:05Z","_ingested":"2026-01-01T00:00:05Z","service":"nginx","msg":"middle"}"#;
        compact_service_blocking(
            &[write_wal_file(&wal_dir, "nginx", &[middle])],
            &data_dir,
            "nginx",
            "2GB",
        )
        .unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        // Read rows in file order (no ORDER BY in the query) — they should
        // already be timestamp-ascending because of sort-on-compaction.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT msg FROM read_parquet('{}')",
                parquet_files[0].display()
            ))
            .unwrap();
        let msgs: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            msgs,
            vec!["early", "middle", "late"],
            "hourly parquet should be timestamp-sorted"
        );
    }

    #[test]
    fn compact_handles_heterogeneous_schemas() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Two records with different key sets — simulates the real scenario
        // where services emit log lines with varying JSON shapes.
        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"test","message":"base record"}"#;
        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"test","message":"extra","extra_field":"surprise","error":"oh no"}"#;

        let f1 = write_wal_file(&wal_dir, "test", &[r1]);
        let f2 = write_wal_file(&wal_dir, "test", &[r2]);

        compact_service_blocking(&[f1, f2], &data_dir, "test", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        // Verify both rows are present in the output.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "both records should be present despite different schemas"
        );
    }

    #[test]
    fn compact_handles_nested_json() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Simulates the real data shape: outer envelope + nested json object
        // with varying subkeys. The `json` object has different keys across
        // records, and `k8s_namespace` at the top level coexists with
        // `namespace` inside the nested object.
        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"test","message":"startup","k8s_namespace":"default","json":{"level":"info","msg":"starting","namespace":"kube-system","build":{"version":"1.0","commit":"abc123"}}}"#;
        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"test","message":"runtime","k8s_namespace":"default","json":{"level":"warn","msg":"something happened"}}"#;

        let f1 = write_wal_file(&wal_dir, "test", &[r1]);
        let f2 = write_wal_file(&wal_dir, "test", &[r2]);

        compact_service_blocking(&[f1, f2], &data_dir, "test", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "nested JSON records should compact without duplicate key errors"
        );
    }

    #[test]
    fn compact_handles_duplicate_key_across_nesting_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Simulates flux-operator: `k8s_namespace` at top level, `namespace`
        // inside the `json` sub-object. With unlimited depth, DuckDB would
        // flatten `json.namespace` and collide with the top-level key.
        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"flux-op","message":"reconcile","k8s_namespace":"flux-system","json":{"controller":"fluxinstance","level":"info","msg":"Reconciliation finished","name":"flux","namespace":"flux-system"}}"#;
        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"flux-op","message":"sync","k8s_namespace":"flux-system","json":{"controller":"kustomization","level":"info","msg":"Applied revision","namespace":"default"}}"#;

        let f1 = write_wal_file(&wal_dir, "flux-op", &[r1]);
        let f2 = write_wal_file(&wal_dir, "flux-op", &[r2]);

        compact_service_blocking(&[f1, f2], &data_dir, "flux-op", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "duplicate nested key names should not cause compaction failure"
        );
    }

    #[test]
    fn compact_chunked_merges_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Create 3 WAL files. Compact them in two calls to simulate chunking:
        // first call processes 2 files, second call processes 1 file and
        // merges with the existing parquet from the first call.
        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"test","message":"one"}"#;
        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"test","message":"two"}"#;
        let r3 = r#"{"_time":"2026-01-01T00:00:02Z","_ingested":"2026-01-01T00:00:02Z","service":"test","message":"three"}"#;

        let f1 = write_wal_file(&wal_dir, "test", &[r1]);
        let f2 = write_wal_file(&wal_dir, "test", &[r2]);
        let f3 = write_wal_file(&wal_dir, "test", &[r3]);

        // Chunk 1: first two files.
        compact_service_blocking(&[f1, f2], &data_dir, "test", "2GB").unwrap();

        // Chunk 2: third file merges into existing parquet.
        compact_service_blocking(&[f3], &data_dir, "test", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1, "should still be one canonical file");

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 3,
            "all three rows should be present after incremental merge"
        );
    }

    #[test]
    fn compact_merges_despite_type_conflict() {
        // The k8s containerID scenario: first batch carries `container_id`
        // as a JSON object, second as a plain string. The conform pipeline
        // pins the column VARCHAR at the first write, so the second batch
        // merges with no conflict at all — no cast fallback involved.
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // First batch: container_id is a JSON object.
        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"kubelet","message":"start","container_id":{"id":"abc123","runtime":"containerd"}}"#;
        let f1 = write_wal_file(&wal_dir, "kubelet", &[r1]);
        compact_service_blocking(&[f1], &data_dir, "kubelet", "2GB").unwrap();

        // Second batch: container_id is a plain string.
        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"kubelet","message":"running","container_id":"def456"}"#;
        let f2 = write_wal_file(&wal_dir, "kubelet", &[r2]);

        compact_service_blocking(&[f2], &data_dir, "kubelet", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "both rows should be present despite type conflict on container_id"
        );

        // Verify the conflicting column was cast to VARCHAR.
        let col_type: String = conn
            .query_row(
                &format!(
                    "SELECT typeof(container_id) FROM read_parquet('{}') LIMIT 1",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            col_type, "VARCHAR",
            "conflicting column should be cast to VARCHAR"
        );
    }

    #[test]
    fn compaction_coerces_complex_fields_to_varchar() {
        // An object-valued field must be written as VARCHAR so hourly files
        // never disagree on its physical type. Scalars keep their inferred
        // types.
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let r = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"k","containerID":{"id":"abc"},"count":5}"#;
        let f = write_wal_file(&wal_dir, "k", &[r]);
        compact_service_blocking(std::slice::from_ref(&f), &data_dir, "k", "2GB").unwrap();

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let conn = duckdb::Connection::open_in_memory().unwrap();

        // The object field is coerced to VARCHAR, preserving the JSON text.
        let (ty, val): (String, String) = conn
            .query_row(
                &format!(
                    "SELECT typeof(\"containerID\"), \"containerID\" \
                     FROM read_parquet('{}') LIMIT 1",
                    parquet[0].display()
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ty, "VARCHAR", "object field must be coerced to VARCHAR");
        assert!(val.contains("abc"), "JSON text should be preserved: {val}");

        // A scalar field keeps its inferred numeric type.
        let count_ty: String = conn
            .query_row(
                &format!(
                    "SELECT typeof(\"count\") FROM read_parquet('{}') LIMIT 1",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(count_ty, "VARCHAR", "scalar field must keep its type");
    }

    /// The envelope instants land in parquet as `DuckDB` `TIMESTAMP`: INT64
    /// microseconds whose logical type is NOT adjusted to UTC. The schema
    /// sample renderer keys its `Z` suffix off that flag, so the web UI and
    /// TUI fixtures depend on this annotation, on the fresh write and on the
    /// merge write alike.
    #[test]
    fn compaction_writes_envelope_instants_as_non_utc_micros() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let annotations = |parquet: &Path| -> Vec<(String, String, String)> {
            let conn = duckdb::Connection::open_in_memory().unwrap();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT name, converted_type, logical_type::VARCHAR \
                     FROM parquet_schema('{}') \
                     WHERE name IN ('_time', '_ingested') ORDER BY name",
                    parquet.display()
                ))
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let expected = |name: &str| {
            (
                name.to_owned(),
                "TIMESTAMP_MICROS".to_owned(),
                "TimestampType(isAdjustedToUTC=0, unit=TimeUnit(MILLIS=<null>, \
                 MICROS=MicroSeconds(), NANOS=<null>))"
                    .to_owned(),
            )
        };

        let r1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"nginx","message":"first"}"#;
        let files1 = vec![write_wal_file(&wal_dir, "nginx", &[r1])];
        compact_service_blocking(&files1, &data_dir, "nginx", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        assert_eq!(
            annotations(&parquet[0]),
            vec![expected("_ingested"), expected("_time")],
            "fresh write"
        );

        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"nginx","message":"second"}"#;
        let files2 = vec![write_wal_file(&wal_dir, "nginx", &[r2])];
        compact_service_blocking(&files2, &data_dir, "nginx", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        assert_eq!(
            annotations(&parquet[0]),
            vec![expected("_ingested"), expected("_time")],
            "merge write"
        );
    }

    /// Read `(column_type, non_null_count, total_count)` for one column of a
    /// parquet file.
    fn column_stats(parquet: &Path, column: &str) -> (String, i64, i64) {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let dtype: String = {
            let mut stmt = conn
                .prepare(&format!(
                    "DESCRIBE SELECT \"{column}\" FROM read_parquet('{}')",
                    parquet.display()
                ))
                .unwrap();
            stmt.query_row([], |row| row.get(1)).unwrap()
        };
        let (nn, total): (i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT count(\"{column}\")::BIGINT, count(*)::BIGINT \
                     FROM read_parquet('{}')",
                    parquet.display()
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        (dtype, nn, total)
    }

    /// One canonical envelope record with a custom field value spliced in.
    fn record_with(field: &str, value: &str, i: usize) -> String {
        format!(
            "{{\"_time\":\"2026-01-01T00:00:{:02}Z\",\"_ingested\":\"2026-01-01T00:00:{:02}Z\",\
             \"service\":\"svc\",\"message\":\"m{i}\",\"{field}\":{value}}}",
            i % 60,
            i % 60,
        )
    }

    /// The SEVERITY ladder guard must not turn every already-conformed file
    /// into a rewrite: `_severity` is in every parquet trawl writes, so a
    /// plan whose only cast is that guard is the identity unless the source
    /// actually holds an out-of-ladder number — and the tally is what proves
    /// it, per file, before the in-place rewrite is paid for.
    #[test]
    fn a_guard_only_plan_over_an_in_range_source_nulls_nothing() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE wal_batch AS \
             SELECT * FROM (VALUES (9::BIGINT, 'm'), (17::BIGINT, 'm')) t(_severity, message)",
        )
        .unwrap();
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [
            ("_severity".to_owned(), CanonicalType::Severity),
            ("message".to_owned(), CanonicalType::Varchar),
        ]
        .into_iter()
        .collect();

        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        assert_eq!(plan.cast_count(), 1, "only the severity guard casts");
        assert!(plan.is_guard_only(), "the guard is the plan's only work");
        assert!(
            plan.tally_conflicts(&conn, "wal_batch", "svc")
                .unwrap()
                .is_empty(),
            "1-24 values are already conformant: nothing to rewrite"
        );

        // …and an out-of-ladder value is still caught, so the skip is
        // decided by the data rather than by the pin.
        conn.execute_batch("INSERT INTO wal_batch VALUES (99, 'm')")
            .unwrap();
        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].rows_nulled, 1);
    }

    /// A plan that also renames or drops is never guard-only, whatever its
    /// casts are: those changes are invisible to the tally.
    #[test]
    fn a_rename_alongside_the_guard_is_not_guard_only() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE wal_batch AS \
             SELECT * FROM (VALUES (9::BIGINT, 'm')) t(_severity, \"Msg\")",
        )
        .unwrap();
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [
            ("_severity".to_owned(), CanonicalType::Severity),
            ("msg".to_owned(), CanonicalType::Varchar),
        ]
        .into_iter()
        .collect();
        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        assert!(!plan.is_noop());
        assert!(!plan.is_guard_only(), "the case-fold rename is real work");
    }

    /// Only a cast that nulled something is conflict evidence.
    /// `field_conflicts` is append-only and compaction ticks every few
    /// seconds, so recording a lossless conform (here: a VARCHAR-pinned
    /// field whose batch happened to carry only numbers) would append a row
    /// per (field, service) forever for a sender losing nothing.
    #[test]
    fn lossless_conform_is_not_recorded_as_a_conflict() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE wal_batch AS \
             SELECT * FROM (VALUES (7, '8'), (9, 'x')) t(num, txt)",
        )
        .unwrap();
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [
            ("num".to_owned(), CanonicalType::Varchar),
            ("txt".to_owned(), CanonicalType::BigInt),
        ]
        .into_iter()
        .collect();

        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        assert_eq!(plan.cast_count(), 2, "both columns disagree with their pin");

        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        assert_eq!(
            conflicts.len(),
            1,
            "only the lossy cast is evidence: {conflicts:?}"
        );
        assert_eq!(conflicts[0].field, "txt");
        assert_eq!(conflicts[0].rows_nulled, 1, "'x' is the only nulled value");
    }

    /// A plan wider than one aggregate chunk must blame the column the rows
    /// were actually nulled in: the tally runs in batched passes, so a
    /// slot-mapping slip would hand a lossless column its neighbour's
    /// evidence. Alternating lossy/lossless across the boundary catches that.
    #[test]
    fn tally_conflicts_spans_chunk_boundary_without_crossing_columns() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let cols = AGG_CHUNK_COLS + 5;
        let lossy = |i: usize| i.is_multiple_of(3);
        let projection = (0..cols)
            .map(|i| {
                let value = if lossy(i) { "'x'" } else { "'1'" };
                format!("{value} AS f{i}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!("CREATE TABLE wal_batch AS SELECT {projection}"))
            .unwrap();

        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = (0..cols)
            .map(|i| (format!("f{i}"), CanonicalType::BigInt))
            .collect();
        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        assert_eq!(plan.cast_count(), cols, "every VARCHAR column casts");

        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        let fields: Vec<String> = conflicts.iter().map(|c| c.field.clone()).collect();
        let expected: Vec<String> = (0..cols)
            .filter(|i| lossy(*i))
            .map(|i| format!("f{i}"))
            .collect();
        assert_eq!(
            fields, expected,
            "only the unconvertible columns are evidence"
        );
        assert!(
            conflicts.iter().all(|c| c.rows_nulled == 1),
            "each lossy column nulled its one row"
        );
    }

    /// The evidence a conflict row carries beyond its counts: which values
    /// the pin is actually costing. Distinct, capped, and only ever values
    /// the cast nulled — a lossless column's convergence is not evidence.
    #[test]
    fn conflict_samples_are_the_distinct_nulled_values_capped_at_five() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE wal_batch AS SELECT * FROM (VALUES \
             ('a', '1'), ('b', '2'), ('c', '3'), ('d', '4'), \
             ('e', '5'), ('f', '6'), ('a', '7'), (NULL, '8')) t(bad, good)",
        )
        .unwrap();
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [
            ("bad".to_owned(), CanonicalType::BigInt),
            ("good".to_owned(), CanonicalType::BigInt),
        ]
        .into_iter()
        .collect();

        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        assert_eq!(conflicts.len(), 1, "only `bad` is lossy: {conflicts:?}");
        let samples = &conflicts[0].samples;
        assert_eq!(
            samples.len(),
            MAX_CONFLICT_SAMPLES,
            "six distinct misfits, five slots: {samples:?}"
        );
        let mut sorted = samples.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), samples.len(), "distinct: {samples:?}");
        assert!(
            samples
                .iter()
                .all(|s| ["a", "b", "c", "d", "e", "f"].contains(&s.as_str())),
            "never a converting value, never the NULL row: {samples:?}"
        );
    }

    /// A misfit value is client text of client-chosen length and content, so
    /// the sample is cut to a byte budget and stripped of control characters
    /// at capture — before it reaches postgres, a response body or a
    /// terminal. The substitution runs first (U+FFFD is three bytes where the
    /// character it replaces is one), and the cut lands on a `char` boundary.
    #[test]
    fn conflict_samples_are_control_sanitised_then_byte_capped() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE wal_batch (v VARCHAR)")
            .unwrap();
        let astral = "\u{1F600}".repeat(100);
        let controls = "\u{7}".repeat(300);
        // A bidi override: category Cf, invisible to `char::is_control`,
        // and the lever that makes a sample rewrite the line after it.
        let trojan = "ok\u{202e}drop table".to_owned();
        for value in [&astral, &controls, &"a\u{7}b".to_owned(), &trojan] {
            conn.execute("INSERT INTO wal_batch VALUES (?)", [value])
                .unwrap();
        }
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [("v".to_owned(), CanonicalType::BigInt)]
            .into_iter()
            .collect();

        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        let samples = &conflicts[0].samples;
        assert_eq!(samples.len(), 4);
        assert!(
            samples.contains(&"ok\u{fffd}drop table".to_owned()),
            "a format character is neutralised like a control one: {samples:?}"
        );
        for sample in samples {
            assert!(
                sample.len() <= MAX_CONFLICT_SAMPLE_BYTES,
                "{} bytes exceeds the cap",
                sample.len()
            );
            assert!(
                !sample.chars().any(char::is_control),
                "control characters survived capture"
            );
        }
        assert!(
            samples.contains(&"a\u{FFFD}b".to_owned()),
            "a control character is replaced, not dropped: {samples:?}"
        );
        // 400 bytes of 4-byte characters cut to exactly 64 of them; 300
        // control characters become 900 bytes of 3-byte U+FFFD, whose last
        // whole character ends at 255 — the boundary walk, not the cap.
        let mut lengths: Vec<usize> = samples.iter().map(String::len).collect();
        lengths.sort_unstable();
        assert_eq!(lengths, vec![5, 15, 255, 256]);
    }

    /// `DISTINCT` runs in `DuckDB` over the raw values, but what is stored
    /// is the sanitised, byte-capped form — and two raw misfits can collapse
    /// into one of those. The stored set is deduplicated after both
    /// transforms, so "at most five DISTINCT samples" describes the samples
    /// rather than the values they came from.
    #[test]
    fn conflict_samples_are_distinct_after_sanitisation() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE wal_batch (v VARCHAR)")
            .unwrap();
        // Two values distinct only in a control character, plus one that is
        // distinct after the byte cap collapses a shared 256-byte prefix.
        let long_a = format!("{}A", "x".repeat(MAX_CONFLICT_SAMPLE_BYTES));
        let long_b = format!("{}B", "x".repeat(MAX_CONFLICT_SAMPLE_BYTES));
        for value in ["a\u{1}b", "a\u{2}b", &long_a, &long_b, "plain"] {
            conn.execute("INSERT INTO wal_batch VALUES (?)", [value])
                .unwrap();
        }
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [("v".to_owned(), CanonicalType::BigInt)]
            .into_iter()
            .collect();

        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        let mut samples = conflicts[0].samples.clone();
        samples.sort();
        assert_eq!(
            samples,
            vec![
                "a\u{fffd}b".to_owned(),
                "plain".to_owned(),
                "x".repeat(MAX_CONFLICT_SAMPLE_BYTES),
            ],
            "five raw misfits, three distinct samples"
        );
    }

    /// A capture that fails must cost the samples, never the batch.
    ///
    /// The failure is real, not mocked: `list(DISTINCT …)` accumulates every
    /// distinct misfit and `DuckDB` does not spill it, so a memory limit the
    /// two scalar tally aggregates fit inside comfortably is one the capture
    /// exhausts. Propagating it would fail the conform, retain the WAL, and
    /// have the next tick retry a bigger batch — an ingestion stall that
    /// tightens itself with every retry.
    #[test]
    fn a_failed_sample_capture_still_records_the_conflict() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE wal_batch AS \
             SELECT 'v' || repeat('x', 200) || i::VARCHAR AS v FROM range(200000) s(i)",
        )
        .unwrap();
        // Applied after the fixture: the settings are for the queries under
        // test. The margin is deliberately an order of magnitude wide — the
        // tally's two scalar aggregates need single-digit MB whatever the
        // row count, while the capture must hold ~40MB of distinct values —
        // so the outcome does not turn on how loaded the machine is. One
        // thread for the same reason.
        conn.execute_batch("SET threads=1").unwrap();
        // No spilling: an over-limit query must fail, which is the
        // production shape (a compactor that silently offloads 40MB of
        // sample candidates to disk is its own problem) and keeps the test
        // off DuckDB's temp-file path.
        conn.execute_batch("SET temp_directory=''").unwrap();
        conn.execute_batch("SET memory_limit='64MB'").unwrap();

        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [("v".to_owned(), CanonicalType::BigInt)]
            .into_iter()
            .collect();
        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);

        assert!(
            plan.capture_samples(&conn, "wal_batch", &[0]).is_err(),
            "fixture must actually exhaust the capture, or this test proves nothing"
        );

        let conflicts = plan
            .tally_conflicts(&conn, "wal_batch", "svc")
            .expect("a capture failure must not fail the conform");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].rows_nulled, 200_000, "the counts are exact");
        assert!(
            conflicts[0].samples.is_empty(),
            "evidence degrades to counts, and the values remain in _raw"
        );
    }

    /// A batch that conflicts on nothing runs no sample query at all — the
    /// capture is a second aggregate pass over client-width data, and a
    /// healthy install must not pay it every tick.
    ///
    /// Observed rather than assumed, through the one signal a swallowed
    /// capture leaves: both fixtures are wide enough that a capture over
    /// them exhausts the connection's memory limit, so a capture that ran
    /// would bump the failure counter. The conflicting fixture proves the
    /// probe has teeth; the clean one proves the capture never ran.
    /// (Widening the sampled set from "the columns that conflicted" to
    /// "every cast column" fails the second assertion.)
    #[test]
    fn a_batch_with_no_conflicts_runs_no_sample_query() {
        // Distinct 200-byte values: enough that `list(DISTINCT …)` cannot
        // fit inside the limit, while the two scalar tally aggregates fit
        // inside it with an order of magnitude to spare (so the outcome
        // does not turn on machine load). Settings are applied after the
        // fixture: they are for the queries under test, not their setup.
        let capture_ran = |projection: &str, pin: CanonicalType| {
            let conn = duckdb::Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE wal_batch AS SELECT {projection} AS v FROM range(200000) s(i)"
            ))
            .unwrap();
            conn.execute_batch("SET threads=1").unwrap();
            conn.execute_batch("SET temp_directory=''").unwrap();
            conn.execute_batch("SET memory_limit='64MB'").unwrap();

            let schema = describe_source(&conn, "wal_batch").unwrap();
            let pins: HashMap<String, CanonicalType> =
                [("v".to_owned(), pin)].into_iter().collect();
            let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
            assert_eq!(plan.cast_count(), 1, "the column must be a cast column");

            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let conflicts = metrics::with_local_recorder(&recorder, || {
                plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap()
            });
            let ran = recorder
                .handle()
                .render()
                .contains(crate::metrics::CATALOG_SAMPLE_CAPTURE_FAILURES_TOTAL);
            (conflicts, ran)
        };

        // Unreadable under a BIGINT pin: every row conflicts, so the capture
        // runs — and fails on this fixture, which is what makes the probe
        // meaningful.
        let (conflicts, ran) = capture_ran(
            "'v' || repeat('x', 200) || i::VARCHAR",
            CanonicalType::BigInt,
        );
        assert_eq!(conflicts.len(), 1);
        assert!(ran, "the fixture must be wide enough to exhaust a capture");

        // Same width, but a JSON source under a VARCHAR pin converts whole:
        // a cast column that nulls nothing is not evidence, and must not be
        // sampled.
        let (conflicts, ran) = capture_ran(
            "to_json('v' || repeat('x', 200) || i::VARCHAR)",
            CanonicalType::Varchar,
        );
        assert!(
            conflicts.is_empty(),
            "the conform is lossless: {conflicts:?}"
        );
        assert!(!ran, "a clean batch must not run the capture at all");
    }

    /// At the sampled-column cap the evidence degrades, never the tally: a
    /// batch conflicting on more columns than one capture pass may cover
    /// still records every conflict, and only the columns past the cap lose
    /// their samples. Which columns those are is decided by name, so a wide
    /// batch samples the same fields whatever order it describes in.
    #[test]
    fn the_sampled_column_cap_drops_samples_not_conflicts() {
        let cols = MAX_SAMPLED_CONFLICT_COLUMNS + 1;
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let projection = (0..cols)
            .map(|i| format!("'nope' AS f{i:03}"))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!("CREATE TABLE wal_batch AS SELECT {projection}"))
            .unwrap();

        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = (0..cols)
            .map(|i| (format!("f{i:03}"), CanonicalType::BigInt))
            .collect();
        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);

        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        assert_eq!(conflicts.len(), cols, "every conflict is still counted");
        assert!(conflicts.iter().all(|c| c.rows_nulled == 1));

        let sampled = conflicts.iter().filter(|c| !c.samples.is_empty()).count();
        assert_eq!(sampled, MAX_SAMPLED_CONFLICT_COLUMNS);
        let unsampled: Vec<&str> = conflicts
            .iter()
            .filter(|c| c.samples.is_empty())
            .map(|c| c.field.as_str())
            .collect();
        assert_eq!(
            unsampled,
            vec![format!("f{:03}", cols - 1)],
            "the last column BY NAME is the one that loses its samples"
        );
    }

    // --- the pin ladder is deterministic over mixed batches (ADR-0009) ---

    #[test]
    fn pin_ladder_one_outlier_among_integers_pins_bigint() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let mut records: Vec<String> = (0..20)
            .map(|i| record_with("duration", &i.to_string(), i))
            .collect();
        records.push(record_with("duration", "\"N/A\"", 20));
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);

        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (dtype, nn, total) = column_stats(&parquet[0], "duration");
        assert_eq!(dtype, "BIGINT", "1-of-21 outlier must pin BIGINT");
        assert_eq!(total, 21);
        assert_eq!(nn, 20, "the outlier nulls (recoverable from _raw)");
    }

    #[test]
    fn pin_ladder_even_split_pins_varchar_unquoted() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let mut records: Vec<String> = (0..10)
            .map(|i| record_with("duration", &i.to_string(), i))
            .collect();
        records.extend((10..20).map(|i| record_with("duration", "\"n/a\"", i)));
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);

        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (dtype, nn, total) = column_stats(&parquet[0], "duration");
        assert_eq!(dtype, "VARCHAR", "a 50/50 batch honestly pins VARCHAR");
        assert_eq!((nn, total), (20, 20), "nothing nulls on a VARCHAR pin");

        // The JSON-typed source column must land UNQUOTED: '5' and 'n/a',
        // never '"n/a"'.
        let values = read_strings(&parquet[0], "DISTINCT duration");
        assert!(
            values.contains(&"n/a".to_owned()),
            "unquoted string: {values:?}"
        );
        assert!(
            values.contains(&"5".to_owned()),
            "stringified number: {values:?}"
        );
    }

    #[test]
    fn pin_ladder_u64_range_batch_pins_double() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let records: Vec<String> = (0..3)
            .map(|i| record_with("duration", "18446744073709551615", i))
            .collect();
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);

        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (dtype, nn, total) = column_stats(&parquet[0], "duration");
        assert_eq!(
            dtype, "DOUBLE",
            "u64-range values pin DOUBLE via the ladder"
        );
        assert_eq!((nn, total), (3, 3));
    }

    #[test]
    fn pin_ladder_huge_outlier_among_integers_pins_bigint() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let mut records: Vec<String> = (0..20)
            .map(|i| record_with("duration", &i.to_string(), i))
            .collect();
        records.push(record_with("duration", "18446744073709551615", 20));
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);

        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (dtype, nn, total) = column_stats(&parquet[0], "duration");
        assert_eq!(
            dtype, "BIGINT",
            "one huge value among twenty integers pins BIGINT"
        );
        assert_eq!(total, 21);
        assert_eq!(nn, 20, "the out-of-range outlier nulls");
    }

    /// 19 fractional values plus one string infer JSON. Bare
    /// `count(TRY_CAST(...))` scoring would count `1.5 → 2` as a BIGINT
    /// success (19/20 ≥ 90%), pinning BIGINT and rounding every value on
    /// write with no `field_conflicts` row; round-trip scoring fails that
    /// rung because rounding is not lossless, so the batch pins DOUBLE.
    #[test]
    fn pin_ladder_fractional_majority_pins_double_not_bigint() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let mut records: Vec<String> = (0..19).map(|i| record_with("duration", "1.5", i)).collect();
        records.push(record_with("duration", "\"n/a\"", 19));
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);

        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (dtype, nn, total) = column_stats(&parquet[0], "duration");
        assert_eq!(
            dtype, "DOUBLE",
            "a fractional majority must pin DOUBLE — BIGINT would round"
        );
        assert_eq!(total, 20);
        assert_eq!(nn, 19, "only the string nulls (recoverable from _raw)");
        let mut values = read_strings(
            &parquet[0],
            "DISTINCT COALESCE(CAST(duration AS VARCHAR), '<null>')",
        );
        values.sort_unstable();
        assert_eq!(
            values,
            vec!["1.5".to_owned(), "<null>".to_owned()],
            "the fractional values survive unrounded"
        );
    }

    /// The >2^53 boundary, where a wire number's fraction stops being a
    /// fraction: above 2^53 the gap between adjacent doubles exceeds 1, so
    /// `read_json` parses `1735689600123456710.7` into an integer-valued
    /// double before any conform expression exists to see it. The text
    /// both lanes then read (`1735689600123456800.0`) is integral, so the
    /// batch pins BIGINT and stores that integer — and a >2^53 INTEGER
    /// batch still pins BIGINT bit-exactly (JSON keeps integer tokens
    /// integral, so nothing rounds it at all).
    ///
    /// Text-first scoring (ADR-0011) takes both sides of the guard from one
    /// text, which is what makes this deterministic. Comparing the DOUBLE
    /// column's own BIGINT cast (`…768`, the double's exact value) against
    /// the column's 17-significant-digit rendering (`…800`) would refuse the
    /// pin over the gap between two spellings of one double, and answer
    /// differently for any `read_json` class that renders that double
    /// differently. Below 2^53 nothing moves: a real fraction survives
    /// into the text and is still refused
    /// ([`Self::pin_ladder_fractional_majority_pins_double_not_bigint`]),
    /// as is a fractional value that arrives as TEXT at any magnitude
    /// ([`Self::conform_to_bigint_pin_nulls_beyond_2_pow_53_fractionals`]).
    #[test]
    fn pin_ladder_beyond_2_pow_53_integral_doubles_pin_bigint() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let mut records: Vec<String> = (0..19)
            .map(|i| record_with("ns_frac", "1735689600123456710.7", i))
            .collect();
        records.push(record_with("ns_frac", "\"n/a\"", 19));
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);
        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (dtype, nn, total) = column_stats(&parquet[0], "ns_frac");
        assert_eq!(
            dtype, "BIGINT",
            "the parsed double is integral, so the BIGINT rung round-trips"
        );
        assert_eq!((nn, total), (19, 20));
        let values = read_strings(
            &parquet[0],
            "DISTINCT COALESCE(CAST(ns_frac AS VARCHAR), '<null>')",
        );
        assert!(
            values.contains(&"1735689600123456800".to_owned()),
            "the stored integer is the double's own rendering, not the wire \
             text DuckDB never held: {values:?}"
        );

        // Same magnitude, integral: exact in DECIMAL space, pins BIGINT.
        let tmp2 = tempfile::tempdir().unwrap();
        let (wal_dir2, data_dir2) = (tmp2.path().join("wal"), tmp2.path().join("data"));
        std::fs::create_dir_all(&wal_dir2).unwrap();
        let mut records: Vec<String> = (0..19)
            .map(|i| record_with("ns_int", "1735689600123456710", i))
            .collect();
        records.push(record_with("ns_int", "\"n/a\"", 19));
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir2, "svc", &refs);
        compact_service_blocking(&[f], &data_dir2, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir2, "parquet");
        let (dtype, nn, _) = column_stats(&parquet[0], "ns_int");
        assert_eq!(dtype, "BIGINT", ">2^53 integers are exact and pin BIGINT");
        assert_eq!(nn, 19);
        let values = read_strings(
            &parquet[0],
            "DISTINCT COALESCE(CAST(ns_int AS VARCHAR), '<null>')",
        );
        assert!(
            values.contains(&"1735689600123456710".to_owned()),
            "the integer survives bit-exact, no double round-off: {values:?}"
        );
    }

    /// Conform-time twin of the boundary case: against an existing BIGINT
    /// pin, a >2^53 fractional value must NULL and tally — never write the
    /// rounded `...711`.
    #[test]
    fn conform_to_bigint_pin_nulls_beyond_2_pow_53_fractionals() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE wal_batch AS \
             SELECT * FROM (VALUES ('1735689600123456710.7'), ('1735689600123456710')) t(ns)",
        )
        .unwrap();
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [("ns".to_owned(), CanonicalType::BigInt)]
            .into_iter()
            .collect();

        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].rows_nulled, 1,
            "exactly the fractional row is a recorded loss"
        );

        let rows: Vec<Option<i64>> = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {} FROM wal_batch ORDER BY 1 NULLS FIRST",
                    plan.select_list.join(", ")
                ))
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            rows,
            vec![None, Some(1_735_689_600_123_456_710)],
            "the fractional writes NULL (never ...711); the integer is exact"
        );
    }

    /// Lossless scoring is what makes the BOOLEAN rung reachable at all:
    /// plain counting scores a boolean batch ≥90% on the earlier BIGINT rung
    /// (`TRY_CAST(true AS BIGINT)` = 1), so BIGINT would always win first.
    #[test]
    fn pin_ladder_boolean_majority_pins_boolean() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let mut records: Vec<String> = (0..19)
            .map(|i| record_with("healthy", if i % 2 == 0 { "true" } else { "false" }, i))
            .collect();
        records.push(record_with("healthy", "\"n/a\"", 19));
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);

        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (dtype, nn, total) = column_stats(&parquet[0], "healthy");
        assert_eq!(
            dtype, "BOOLEAN",
            "a ≥90%-boolean batch must pin BOOLEAN — the rung is reachable now"
        );
        assert_eq!(total, 20);
        assert_eq!(nn, 19, "only the string nulls");
    }

    /// The catalog holds ASCII-folded names only, so the conform plan must
    /// fold a mixed-case column onto its pin — a type-matching column still
    /// gets a rename (a rewrite, not a no-op), and a cast column lands under
    /// the folded alias with its conflict evidence attributed to the catalog
    /// key.
    #[test]
    fn conform_plan_folds_mixed_case_columns_onto_their_pins() {
        let schema = vec![
            ColInfo {
                name: "Dur".to_owned(),
                dtype: "BIGINT".to_owned(),
            },
            ColInfo {
                name: "Note".to_owned(),
                dtype: "BIGINT".to_owned(),
            },
        ];
        let pins: HashMap<String, CanonicalType> = [
            ("dur".to_owned(), CanonicalType::BigInt),
            ("note".to_owned(), CanonicalType::Varchar),
        ]
        .into_iter()
        .collect();

        let plan = ConformPlan::build(
            &schema,
            &pins,
            &ConformPolicy::StandingFile {
                time_fallback: chrono::Utc::now(),
            },
        );
        assert!(!plan.is_noop(), "a rename-only plan is still a rewrite");
        assert_eq!(plan.cast_count(), 1, "only `Note` needs a cast");
        assert_eq!(plan.retained, vec!["dur", "note"]);
        assert_eq!(plan.select_list[0], r#""Dur" AS "dur""#);
        assert!(
            plan.select_list[1].ends_with(r#" AS "note""#),
            "the cast lands under the folded alias: {}",
            plan.select_list[1]
        );

        // WalBatch: an unpinned mixed-case column is judged by its folded
        // name — pinned under `dur`, so it is conformed, not dropped.
        let wal_plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        assert!(wal_plan.dropped.is_empty(), "{:?}", wal_plan.dropped);
        assert_eq!(wal_plan.retained, vec!["dur", "note"]);
    }

    /// The repin policy (ADR-0011): the target column routes through the
    /// resurrection expression — stored guarded reading first, then the
    /// `_raw` re-extraction — while every already-conformant column passes
    /// through untouched.
    #[test]
    fn repin_policy_routes_the_target_through_resurrection() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE f AS SELECT * FROM (VALUES \
             (404::BIGINT, 'svc', '{\"status\":404}'), \
             (NULL::BIGINT, 'svc', '{\"status\":\"accepted\"}')) \
             t(status, service, _raw)",
        )
        .unwrap();
        let schema = describe_source(&conn, "f").unwrap();
        // The one-entry-flipped pin map: status now VARCHAR.
        let pins: HashMap<String, CanonicalType> = [
            ("status".to_owned(), CanonicalType::Varchar),
            ("service".to_owned(), CanonicalType::Varchar),
            ("_raw".to_owned(), CanonicalType::Varchar),
        ]
        .into_iter()
        .collect();

        let plan = ConformPlan::build(
            &schema,
            &pins,
            &ConformPolicy::Repin {
                resurrect_field: "status".to_owned(),
                time_fallback: chrono::Utc::now(),
                target: RepinTarget::otel(CanonicalType::Varchar),
            },
        );
        assert!(!plan.is_noop());
        assert_eq!(plan.cast_count(), 1, "only the target column is cast");
        assert!(
            plan.select_list.iter().any(|s| s == "\"service\""),
            "conformant columns pass through untouched: {:?}",
            plan.select_list
        );

        let rows: Vec<(Option<String>, Option<String>)> = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {} FROM f ORDER BY _raw",
                    plan.select_list.join(", ")
                ))
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(2)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(
            rows,
            vec![
                (
                    Some("accepted".to_owned()),
                    Some("{\"status\":\"accepted\"}".to_owned())
                ),
                (Some("404".to_owned()), Some("{\"status\":404}".to_owned())),
            ],
            "the stored value keeps its text and the shelved value resurrects"
        );
    }

    /// The forced resurrection-only pass (`to == current` + force): the
    /// column's physical type already IS the pin, which the ordinary
    /// conform would treat as a noop — the repin target must still be
    /// rewritten so shelved values come back.
    #[test]
    fn repin_policy_fires_even_when_the_dtype_already_matches_the_pin() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE f AS SELECT * FROM (VALUES \
             (NULL::BIGINT, '{\"dur\":7}')) t(dur, _raw)",
        )
        .unwrap();
        let schema = describe_source(&conn, "f").unwrap();
        let pins: HashMap<String, CanonicalType> = [
            ("dur".to_owned(), CanonicalType::BigInt),
            ("_raw".to_owned(), CanonicalType::Varchar),
        ]
        .into_iter()
        .collect();

        let plan = ConformPlan::build(
            &schema,
            &pins,
            &ConformPolicy::Repin {
                resurrect_field: "dur".to_owned(),
                time_fallback: chrono::Utc::now(),
                target: RepinTarget::otel(CanonicalType::BigInt),
            },
        );
        assert!(!plan.is_noop(), "a resurrection-only pass is a rewrite");
        assert_eq!(plan.cast_count(), 1);
        let got: Option<i64> = conn
            .query_row(
                &format!("SELECT {} FROM f", plan.select_list[0]),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(got, Some(7), "the shelved value comes back");
    }

    /// Defensive: an affected file that (against the envelope invariant)
    /// carries no `_raw` column falls back to the plain guarded conform —
    /// a resurrection arm referencing a missing column would fail the
    /// whole rewrite.
    #[test]
    fn repin_policy_without_raw_column_falls_back_to_plain_conform() {
        let schema = vec![ColInfo {
            name: "status".to_owned(),
            dtype: "BIGINT".to_owned(),
        }];
        let pins: HashMap<String, CanonicalType> = [("status".to_owned(), CanonicalType::Varchar)]
            .into_iter()
            .collect();
        let plan = ConformPlan::build(
            &schema,
            &pins,
            &ConformPolicy::Repin {
                resurrect_field: "status".to_owned(),
                time_fallback: chrono::Utc::now(),
                target: RepinTarget::otel(CanonicalType::Varchar),
            },
        );
        assert_eq!(plan.cast_count(), 1);
        assert!(
            !plan.select_list[0].contains("_raw"),
            "no resurrection arm without a _raw column: {}",
            plan.select_list[0]
        );
    }

    /// Two sampling aggregates, one promise. The conform's own evidence
    /// accumulates every distinct misfit and slices — safe over a bounded
    /// WAL batch — while the repin scan reads a whole corpus and takes a
    /// fixed-size sketch instead, because the distinct count there is a
    /// sender's free text and nothing bounds it. What they share is
    /// what a sample is: the same subject (a value the column carries whose
    /// reading is NULL), the same character cap, and the same
    /// sanitise-then-cap-then-dedup decode.
    #[test]
    fn both_sampling_aggregates_share_the_subject_and_the_decode() {
        let quoted = quote_ident("dur");
        let text = trawl_core::conform::untyped_text(&quoted);
        let unbounded = distinct_misfit_samples_sql(&quoted, &text, "TARGET");
        let bounded = bounded_misfit_samples_sql(&quoted, &text, "TARGET");

        // The conform's shape is unchanged (its input is a bounded batch).
        assert_eq!(
            unbounded,
            "to_json(array_slice(list(DISTINCT \
             left(json_extract_string(to_json(\"dur\"), '$'), 256)) \
             FILTER (WHERE \"dur\" IS NOT NULL AND (TARGET) IS NULL), 1, 10))::VARCHAR"
        );
        let cast = CastEntry {
            name: "dur".to_owned(),
            dtype: "VARCHAR".to_owned(),
            pin: CanonicalType::BigInt,
            expr: "TARGET".to_owned(),
            guard_only: false,
        };
        assert_eq!(sample_expr(&cast), unbounded);

        // One subject, one cap, in both.
        for sql in [&unbounded, &bounded] {
            assert!(
                sql.contains("FILTER (WHERE \"dur\" IS NOT NULL AND (TARGET) IS NULL)"),
                "a sample is a carried value the target cannot read: {sql}"
            );
            assert!(sql.contains(&format!("left({text}, {MAX_CONFLICT_SAMPLE_BYTES})")));
            assert!(sql.starts_with("to_json(") && sql.ends_with(")::VARCHAR"));
        }
        // The repin's memory is a function of k, never of the corpus: the
        // sketch is the whole point, and `list(DISTINCT …)` must not appear.
        assert!(
            bounded.contains(&format!(
                "approx_top_k(left({text}, {MAX_CONFLICT_SAMPLE_BYTES}), {MAX_CONFLICT_SAMPLES})"
            )),
            "{bounded}"
        );
        assert!(!bounded.contains("list(DISTINCT"), "{bounded}");
    }

    /// The bounded aggregate, executed against the shape that exhausts the
    /// unbounded one: a column whose every value is a distinct misfit. The
    /// scan must survive and return at most five samples, all of them real
    /// values from the corpus.
    #[test]
    fn the_bounded_sampler_survives_a_high_cardinality_misfit_column() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
            .unwrap();
        // 200k DISTINCT unreadable values — every row its own misfit.
        conn.execute_batch(
            "CREATE TABLE f AS SELECT ('gold-' || i::VARCHAR) AS level FROM range(200000) t(i)",
        )
        .unwrap();
        let quoted = quote_ident("level");
        let text = trawl_core::conform::untyped_text(&quoted);
        let target = trawl_core::conform::guarded_cast(&text, CanonicalType::Severity);
        let sql = format!(
            "SELECT {} FROM f",
            bounded_misfit_samples_sql(&quoted, &text, &target)
        );
        let rendered: Option<String> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        let samples = decode_misfit_samples(rendered.as_deref()).unwrap();
        assert!(!samples.is_empty(), "the sketch reports what it saw");
        assert!(samples.len() <= MAX_CONFLICT_SAMPLES, "{samples:?}");
        for sample in &samples {
            assert!(sample.starts_with("gold-"), "{sample} is not a real value");
        }

        // And a column with nothing to sample answers nothing, not an error.
        conn.execute_batch("CREATE TABLE clean AS SELECT 'error' AS level")
            .unwrap();
        let sql = format!(
            "SELECT {} FROM clean",
            bounded_misfit_samples_sql(&quoted, &text, &target)
        );
        let rendered: Option<String> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        assert!(
            decode_misfit_samples(rendered.as_deref())
                .unwrap()
                .is_empty()
        );
    }

    /// Decoding caps, sanitises and de-duplicates in that order: `left()`
    /// bounds what `DuckDB` accumulates in characters, the byte cap and the
    /// control-character substitution happen in Rust, and DISTINCT is
    /// re-applied after both because sanitising can collapse two raw values
    /// into one sample.
    #[test]
    fn decoding_samples_caps_sanitises_then_dedups() {
        assert_eq!(decode_misfit_samples(None).unwrap(), Vec::<String>::new());
        // Two values SQL calls distinct that sanitise to one sample, plus
        // enough tail to prove the cap.
        let json = "[\"a\\u0001b\",\"x\\u0001\",\"x\\u0002\",\"gold\",\"1\",\"2\",\"3\",\"4\"]";
        let kept = decode_misfit_samples(Some(json)).unwrap();
        assert_eq!(
            kept,
            vec!["a\u{fffd}b", "x\u{fffd}", "gold", "1", "2"],
            "control chars fold to U+FFFD, the collapsing pair is one sample, five kept"
        );
        assert_eq!(kept.len(), MAX_CONFLICT_SAMPLES);
        assert!(decode_misfit_samples(Some("not json")).is_err());
    }

    /// The counting expressions over a SEVERITY target, executed: a sender's
    /// `level` column repinned onto the ladder. Tokens map dialect-free,
    /// `gold` maps nowhere, and the numeral `3` maps under both dialects to
    /// different rungs — which is the whole ambiguity notion, counted
    /// whatever the job asserted.
    #[test]
    fn repin_counts_over_a_severity_target_separate_loss_from_ambiguity() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE f AS SELECT * FROM (VALUES \
             ('error', '{\"level\":\"error\"}'), \
             ('ERROR', '{\"level\":\"ERROR\"}'), \
             ('error2', '{\"level\":\"error2\"}'), \
             ('3', '{\"level\":\"3\"}'), \
             ('gold', '{\"level\":\"gold\"}')) t(level, _raw)",
        )
        .unwrap();
        let count = |dialect| {
            let reading =
                RepinReading::new(CanonicalType::Varchar, CanonicalType::Severity, dialect);
            let exprs = repin_count_exprs("\"level\"", true, "level", reading);
            let sql = format!(
                "SELECT {}::BIGINT, {}::BIGINT, {}::BIGINT FROM f",
                exprs.carrying, exprs.kept, exprs.ambiguous
            );
            conn.query_row(&sql, [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
        };

        // OTel: `3` reads as trace3, `gold` reads as nothing.
        assert_eq!(count(Dialect::Otel), (5, 4, 1));
        // Syslog: `3` reads as err (17) instead — still mapped, still the
        // one ambiguous row, and `gold` is still the only loss. The COUNT
        // does not depend on the assertion; only the gate does.
        assert_eq!(count(Dialect::Syslog), (5, 4, 1));

        // The values themselves, to prove the dialect actually changes what
        // the rewrite would write.
        let read = |dialect| {
            let reading =
                RepinReading::new(CanonicalType::Varchar, CanonicalType::Severity, dialect);
            let expr = repin_target_expr("\"level\"", true, "level", reading.written);
            let mut stmt = conn
                .prepare(&format!("SELECT {expr} FROM f WHERE level = '3'"))
                .unwrap();
            stmt.query_row([], |row| row.get::<_, Option<i64>>(0))
                .unwrap()
        };
        assert_eq!(read(Dialect::Otel), Some(3));
        assert_eq!(read(Dialect::Syslog), Some(17));

        // A non-SEVERITY target has no dialect-sensitive rung at all, so
        // the ambiguity count is the constant 0 rather than a pair of
        // expressions DuckDB evaluates to prove they agree.
        let reading = RepinReading::new(
            CanonicalType::Varchar,
            CanonicalType::BigInt,
            Dialect::Syslog,
        );
        let exprs = repin_count_exprs("\"level\"", true, "level", reading);
        assert_eq!(exprs.ambiguous, "0");
    }

    /// A repin whose SOURCE is already `SEVERITY` reads its stored column
    /// dialect-free: the values are canonical ladder positions, and
    /// re-reading a stored `3` as syslog would silently corrupt it to 17.
    /// Only the `_raw` arm — the sender's own wire text — takes the
    /// assertion (ADR-0013).
    #[test]
    fn a_severity_source_keeps_its_stored_arm_dialect_free() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE f AS SELECT * FROM (VALUES \
             (3::BIGINT, '{\"sev\":\"3\"}'), \
             (NULL::BIGINT, '{\"sev\":\"3\"}')) t(sev, _raw)",
        )
        .unwrap();
        let reading = RepinReading::new(
            CanonicalType::Severity,
            CanonicalType::Severity,
            Dialect::Syslog,
        );
        let expr = repin_target_expr("\"sev\"", true, "sev", reading.written);
        let mut stmt = conn.prepare(&format!("SELECT {expr} FROM f")).unwrap();
        let got: Vec<Option<i64>> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            got,
            vec![Some(3), Some(17)],
            "the stored canonical 3 is preserved; only the resurrected wire \
             `3` reads as syslog err"
        );

        // And the ambiguity variants keep that arm fixed too, so a stored
        // canonical value is not reported as ambiguous.
        let exprs = repin_count_exprs("\"sev\"", true, "sev", reading);
        let ambiguous: i64 = conn
            .query_row(
                &format!("SELECT {}::BIGINT FROM f", exprs.ambiguous),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            ambiguous, 1,
            "only the row resurrecting a wire numeral is ambiguous"
        );
    }

    /// Conform-time lossless guarantee: a batch disagreeing with an
    /// existing typed pin must null (and tally) every value the cast would
    /// alter — never write a silently rounded one. Numeric-space tolerance
    /// keeps genuinely lossless drift (`4.0 → 4`).
    #[test]
    fn conform_to_bigint_pin_nulls_rounded_values_and_records_them() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        // A DOUBLE column (a sender flip-flopping JSON number encodings)
        // and a VARCHAR column, both against a BIGINT pin.
        conn.execute_batch(
            "CREATE TABLE wal_batch AS \
             SELECT * FROM (VALUES (1.5::DOUBLE, '1.5'), (4.0::DOUBLE, '42')) t(frac, txt)",
        )
        .unwrap();
        let schema = describe_source(&conn, "wal_batch").unwrap();
        let pins: HashMap<String, CanonicalType> = [
            ("frac".to_owned(), CanonicalType::BigInt),
            ("txt".to_owned(), CanonicalType::BigInt),
        ]
        .into_iter()
        .collect();

        let plan = ConformPlan::build(&schema, &pins, &ConformPolicy::WalBatch);
        assert_eq!(plan.cast_count(), 2);

        // The tally counts the rounded values as NULLED — they are losses.
        let conflicts = plan.tally_conflicts(&conn, "wal_batch", "svc").unwrap();
        let nulled: HashMap<&str, u64> = conflicts
            .iter()
            .map(|c| (c.field.as_str(), c.rows_nulled))
            .collect();
        assert_eq!(
            nulled,
            HashMap::from([("frac", 1), ("txt", 1)]),
            "each column's fractional value is evidence: {conflicts:?}"
        );

        // And the written values match: NULL where rounding would have
        // altered, the exact integer where the round trip holds.
        let select = plan.select_list.join(", ");
        let rows: Vec<(Option<i64>, Option<i64>)> = {
            let mut stmt = conn
                .prepare(&format!("SELECT {select} FROM wal_batch ORDER BY txt"))
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            rows,
            vec![(Some(4), Some(42)), (None, None)],
            "1.5 and '1.5' must write NULL, never 2; 4.0 and '42' round-trip"
        );
    }

    /// A batch wider than one ladder chunk must pin every column to the type
    /// its own values support: the ladder runs in batched aggregate passes,
    /// so a slot-mapping slip would silently hand a column its neighbour's
    /// verdict. Alternating verdicts across the chunk boundary catch that.
    #[test]
    fn pin_ladder_spans_chunk_boundary_without_crossing_verdicts() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Every column is mixed-typed (so every one takes the ladder), but
        // every third is an even split that can only honestly pin VARCHAR.
        let cols = AGG_CHUNK_COLS + 5;
        let varchar_col = |i: usize| i.is_multiple_of(3);
        let records: Vec<String> = (0..21)
            .map(|row| {
                let fields: String = (0..cols)
                    .map(|i| {
                        let numeric = if varchar_col(i) { row < 10 } else { row < 20 };
                        if numeric {
                            format!(",\"f{i}\":{row}")
                        } else {
                            format!(",\"f{i}\":\"n/a\"")
                        }
                    })
                    .collect();
                format!(
                    "{{\"_time\":\"2026-01-01T00:00:{row:02}Z\",\
                     \"_ingested\":\"2026-01-01T00:00:{row:02}Z\",\
                     \"service\":\"svc\"{fields}}}"
                )
            })
            .collect();
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let f = write_wal_file(&wal_dir, "svc", &refs);

        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");

        // Probe both sides of the boundary, including the columns straddling
        // it, plus the very first and last column of the batch.
        let probes = [0, 1, 2, 254, 255, 256, 257, 258, cols - 2, cols - 1];
        for i in probes {
            let (dtype, _, total) = column_stats(&parquet[0], &format!("f{i}"));
            let expected = if varchar_col(i) { "VARCHAR" } else { "BIGINT" };
            assert_eq!(dtype, expected, "column f{i} pinned the wrong type");
            assert_eq!(total, 21);
        }
    }

    #[test]
    fn all_null_unpinned_column_defers_then_later_batch_pins() {
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Batch 1: `maybe` is all-null and unpinned → the column must be
        // absent from the file (union_by_name reads absent as NULL; typed
        // NULLs would pre-empt the field's real type).
        let r1 = record_with("maybe", "null", 0);
        let f1 = write_wal_file(&wal_dir, "svc", &[r1.as_str()]);
        compact_service_blocking(&[f1], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let cols: Vec<String> = {
            let mut stmt = conn
                .prepare(&format!(
                    "DESCRIBE SELECT * FROM read_parquet('{}')",
                    parquet[0].display()
                ))
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert!(
            !cols.contains(&"maybe".to_owned()),
            "an all-null unpinned column defers (absent from the file): {cols:?}"
        );

        // Batch 2: real values arrive → the field pins and both batches
        // read cleanly through one union.
        let r2 = record_with("maybe", "7", 1);
        let f2 = write_wal_file(&wal_dir, "svc", &[r2.as_str()]);
        compact_service_blocking(&[f2], &data_dir, "svc", "2GB").unwrap();
        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let (dtype, nn, total) = column_stats(&parquet[0], "maybe");
        assert_eq!(dtype, "BIGINT");
        assert_eq!(
            (nn, total),
            (1, 2),
            "deferred rows read as NULL after the pin"
        );
    }

    #[test]
    fn nested_object_keeps_batchmates_columns_and_stays_reachable() {
        // A nested object (hand-written WAL — ingest stringifies) infers
        // STRUCT at depth 2 and must conform to VARCHAR JSON text without
        // dropping any batch-mate's custom column.
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let r1 = record_with("k8s", "{\"pod\":\"x\"}", 0);
        let r2 = record_with("status", "418", 1);
        let f = write_wal_file(&wal_dir, "svc", &[r1.as_str(), r2.as_str()]);
        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();

        let parquet = find_files_by_ext(&data_dir, "parquet");
        let (k8s_ty, k8s_nn, _) = column_stats(&parquet[0], "k8s");
        assert_eq!(k8s_ty, "VARCHAR", "nested object conforms to VARCHAR");
        assert_eq!(k8s_nn, 1);
        let (status_ty, status_nn, total) = column_stats(&parquet[0], "status");
        assert_eq!(status_ty, "BIGINT", "batch-mate custom column survives");
        assert_eq!((status_nn, total), (1, 2));

        // The nested value stays reachable as JSON text.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let pod: String = conn
            .query_row(
                &format!(
                    "SELECT json_extract_string(k8s, '$.pod') \
                     FROM read_parquet('{}') WHERE k8s IS NOT NULL",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pod, "x", "json_extract_string must reach the nested value");
    }

    #[test]
    fn duplicate_name_collision_drains_at_depth_1_keeping_all_columns() {
        // Case-colliding keys inside a nested object are the one shape that
        // still raises "Duplicate name" at depth 2 (verified by execution).
        // The depth-1 retry reads every top-level field as JSON and the
        // conform step types them — no column is dropped.
        let tmp = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (tmp.path().join("wal"), tmp.path().join("data"));
        std::fs::create_dir_all(&wal_dir).unwrap();

        let r1 = record_with("k8s", "{\"Pod\":\"x\",\"pod\":\"y\"}", 0);
        let r2 = record_with("status", "418", 1);
        let f = write_wal_file(&wal_dir, "svc", &[r1.as_str(), r2.as_str()]);
        compact_service_blocking(&[f], &data_dir, "svc", "2GB").unwrap();

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1, "the batch must drain");
        let (status_ty, status_nn, total) = column_stats(&parquet[0], "status");
        assert_eq!(
            (status_ty.as_str(), status_nn, total),
            ("BIGINT", 1, 2),
            "batch-mates' custom columns survive the depth-1 rung"
        );
        let (k8s_ty, k8s_nn, _) = column_stats(&parquet[0], "k8s");
        assert_eq!(k8s_ty, "VARCHAR");
        assert_eq!(k8s_nn, 1);
        let (msg_ty, msg_nn, _) = column_stats(&parquet[0], "message");
        assert_eq!(msg_ty, "VARCHAR", "envelope pin holds at depth 1");
        assert_eq!(msg_nn, 2);
    }

    #[test]
    fn cleanup_removes_stale_tmp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let hour_dir = data_dir.join("2026-01-01").join("00");
        std::fs::create_dir_all(&hour_dir).unwrap();

        // Create the "stale" file first, then sleep so it ages past the threshold.
        let stale = hour_dir.join("nginx.parquet.tmp");
        std::fs::write(&stale, b"stale").unwrap();

        std::thread::sleep(Duration::from_millis(50));

        // Create a fresh .tmp file that must not be removed.
        let fresh = hour_dir.join("postgres.parquet.tmp");
        std::fs::write(&fresh, b"fresh").unwrap();

        // Threshold between stale (50ms+ old) and fresh (~0ms old).
        cleanup_stale_tmp_files(
            "prod",
            &data_dir,
            Duration::from_millis(25),
            &PublicationClaims::default(),
        );

        assert!(!stale.exists(), "stale .tmp should be removed");
        assert!(fresh.exists(), "fresh .tmp should be kept");
    }

    #[test]
    fn cleanup_removes_day_level_stale_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let day_dir = data_dir.join("2026-01-01");
        std::fs::create_dir_all(&day_dir).unwrap();

        // Day-level stale .tmp from interrupted rollup.
        let stale = day_dir.join("nginx.parquet.tmp");
        std::fs::write(&stale, b"stale").unwrap();

        std::thread::sleep(Duration::from_millis(50));

        cleanup_stale_tmp_files(
            "prod",
            &data_dir,
            Duration::from_millis(25),
            &PublicationClaims::default(),
        );
        assert!(!stale.exists(), "day-level stale .tmp should be removed");
    }

    #[test]
    fn looks_like_date_valid() {
        assert!(looks_like_date("2026-02-13"));
        assert!(looks_like_date("2025-01-01"));
    }

    #[test]
    fn looks_like_date_invalid() {
        assert!(!looks_like_date("wal"));
        assert!(!looks_like_date("00"));
        assert!(!looks_like_date("2026-1-01"));
        assert!(!looks_like_date(""));
    }

    /// Helper: create a parquet file with the given rows in a specific hour-dir.
    fn write_hourly_parquet(
        data_dir: &Path,
        date: &str,
        hour: &str,
        service: &str,
        records: &[&str],
    ) -> PathBuf {
        let wal_dir = data_dir.join("_wal_tmp");
        std::fs::create_dir_all(&wal_dir).unwrap();
        // One WAL file preserves distinct rows even when fixture creation
        // happens within the same timestamp used by write_wal_file.
        let wal_files = [write_wal_file(&wal_dir, service, records)];

        // Use DuckDB directly to write parquet (simpler than going through compact).
        let hour_dir = data_dir.join(date).join(hour);
        std::fs::create_dir_all(&hour_dir).unwrap();
        let out = hour_dir.join(format!("{service}.parquet"));

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let file_list = wal_files
            .iter()
            .map(|p| format!("'{}'", p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM read_json([{file_list}], format='newline_delimited', \
             records=true, auto_detect=true, union_by_name=true, \
             field_appearance_threshold=0, maximum_depth=2)) \
             TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY)",
            out.to_string_lossy(),
        ))
        .unwrap();

        // Clean up temp wal files.
        let _ = std::fs::remove_dir_all(&wal_dir);

        out
    }

    #[test]
    fn rollup_day_blocking_merges_hourly_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let r2 = r#"{"_time":"2026-01-15T02:30:00Z","_ingested":"2026-01-15T02:30:00Z","service":"nginx","msg":"b"}"#;
        let r3 = r#"{"_time":"2026-01-15T02:45:00Z","_ingested":"2026-01-15T02:45:00Z","service":"nginx","msg":"c"}"#;

        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let f2 = write_hourly_parquet(&data_dir, date, "02", "nginx", &[r2, r3]);

        let day_dir = data_dir.join(date);
        rollup_day_blocking(&day_dir, "nginx", &[f1.clone(), f2.clone()], "2GB")
            .result
            .unwrap();

        // Day-level file should exist.
        let daily = day_dir.join("nginx.parquet");
        assert!(daily.exists(), "daily parquet should exist");

        // Hourly files should be deleted.
        assert!(!f1.exists(), "hourly file 1 should be deleted");
        assert!(!f2.exists(), "hourly file 2 should be deleted");

        // Verify row count.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3, "daily file should contain all 3 rows");
    }

    #[test]
    fn rollup_day_blocking_merges_with_existing_daily() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // First rollup: create initial daily file.
        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let day_dir = data_dir.join(date);
        rollup_day_blocking(&day_dir, "nginx", &[f1], "2GB")
            .result
            .unwrap();

        // Late-arriving data creates a new hourly file.
        let r2 = r#"{"_time":"2026-01-15T03:00:00Z","_ingested":"2026-01-15T03:00:00Z","service":"nginx","msg":"late"}"#;
        let f2 = write_hourly_parquet(&data_dir, date, "03", "nginx", &[r2]);

        // Second rollup: should merge existing daily + new hourly.
        rollup_day_blocking(&day_dir, "nginx", &[f2], "2GB")
            .result
            .unwrap();

        let daily = day_dir.join("nginx.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "daily file should contain original + late row");
    }

    #[test]
    fn rollup_day_blocking_sorts_by_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // Write records in reverse order across hours.
        let r1 = r#"{"_time":"2026-01-15T23:00:00Z","_ingested":"2026-01-15T23:00:00Z","service":"nginx","msg":"late"}"#;
        let r2 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"early"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "23", "nginx", &[r1]);
        let f2 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r2]);

        let day_dir = data_dir.join(date);
        rollup_day_blocking(&day_dir, "nginx", &[f1, f2], "2GB")
            .result
            .unwrap();

        // Verify rows are sorted by timestamp (ascending).
        let daily = day_dir.join("nginx.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let first_msg: String = conn
            .query_row(
                &format!(
                    "SELECT msg FROM read_parquet('{}') ORDER BY \"_time\" LIMIT 1",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_msg, "early", "rows should be sorted by timestamp");
    }

    #[test]
    fn rollup_of_nonconformant_hourlies_errors_loudly_and_retains_inputs() {
        // Hour 01 carries `offset` as a STRUCT, hour 02 as a plain string —
        // a mix write-time conformance cannot produce, so it can only mean
        // foreign parquet dropped into the tree (ADR-0009). The rollup must
        // fail loudly and retain the hourly inputs for the operator (retried
        // next tick), never half-produce a daily file and never rewrite both
        // sides with a silent VARCHAR cast.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"ctrl","offset":{"v":1,"u":"x"}}"#;
        let r2 = r#"{"_time":"2026-01-15T02:00:00Z","_ingested":"2026-01-15T02:00:00Z","service":"ctrl","offset":"540.203µs"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "ctrl", &[r1]);
        let f2 = write_hourly_parquet(&data_dir, date, "02", "ctrl", &[r2]);

        let day_dir = data_dir.join(date);
        let outcome = rollup_day_blocking(&day_dir, "ctrl", &[f1.clone(), f2.clone()], "2GB");
        assert!(
            outcome.result.is_err(),
            "a nonconformant hourly set must fail the rollup loudly"
        );
        assert!(
            f1.exists() && f2.exists(),
            "the hourly inputs must be retained for repair/retry"
        );
        assert!(
            !day_dir.join("ctrl.parquet").exists(),
            "no daily file may be produced from a nonconformant set"
        );
    }

    #[test]
    fn merge_into_foreign_nonconformant_parquet_errors_and_retains_wal() {
        // The canonical hourly file already holds `container_id` as a STRUCT
        // (foreign parquet — the conform pipeline always writes VARCHAR for
        // it). Merging a conformant WAL batch into it hits a union type
        // conflict, and there is no cast-and-retry: the batch errors (logged
        // as catalog_invariant_violation), the WAL file survives for the next
        // tick, and the existing file is untouched.
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Foreign file at the canonical path the batch will target — the
        // output dir is derived from the compaction instant, so place it at
        // today's date/hour with a genuinely STRUCT-typed column.
        let now = chrono::Utc::now();
        let existing = write_hourly_parquet(
            &data_dir,
            &now.format("%Y-%m-%d").to_string(),
            &now.format("%H").to_string(),
            "kubelet",
            &[
                r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"kubelet","message":"start","container_id":{"id":"abc123","runtime":"containerd"}}"#,
            ],
        );
        let before = std::fs::metadata(&existing).unwrap().len();

        let r2 = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"kubelet","message":"running","container_id":"def456"}"#;
        let f2 = write_wal_file(&wal_dir, "kubelet", &[r2]);

        let err = compact_service_blocking(std::slice::from_ref(&f2), &data_dir, "kubelet", "2GB")
            .expect_err("merging into a foreign nonconformant file must error");
        assert!(
            err.contains("merge"),
            "the error must surface from the merge step: {err}"
        );
        assert!(f2.exists(), "the WAL file must survive for the next tick");
        assert_eq!(
            std::fs::metadata(&existing).unwrap().len(),
            before,
            "the existing parquet must be untouched"
        );
    }

    #[test]
    fn collect_hour_dirs_finds_valid_hours() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(day.join("00")).unwrap();
        std::fs::create_dir_all(day.join("14")).unwrap();
        std::fs::create_dir_all(day.join("23")).unwrap();
        // Not an hour directory.
        std::fs::write(day.join("nginx.parquet"), b"data").unwrap();

        let dirs = collect_hour_dirs(&day);
        assert_eq!(dirs.len(), 3, "should find 3 hour directories");
    }

    #[cfg(unix)]
    #[test]
    fn operational_hour_metadata_controls_preserve_selection_and_count_each_error() {
        use std::os::unix::fs::symlink;

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026-01-15");
        assert!(collect_hour_dirs(&day).is_empty());
        std::fs::create_dir_all(day.join("00")).unwrap();
        std::fs::create_dir(day.join("invalid")).unwrap();
        std::fs::write(day.join("01"), b"ordinary file").unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, day.join("02")).unwrap();
        symlink(tmp.path().join("missing"), day.join("03")).unwrap();
        assert_eq!(
            day.join("03").metadata().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let expected = vec![day.join("00"), day.join("02")];
        let mut selected = collect_hour_dirs(&day);
        selected.sort();
        assert_eq!(selected, expected);
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            0
        );

        // Each independently failed metadata lookup is observed, unlike the
        // once-per-root aggregation used by the outer environment listing.
        for hour in ["04", "05"] {
            symlink(hour, day.join(hour)).unwrap();
            assert_ne!(
                day.join(hour).metadata().unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
        }
        let mut selected = collect_hour_dirs(&day);
        selected.sort();
        assert_eq!(selected, expected);
        assert_eq!(
            operation_count(&handle, CompactionOperation::DailyRollupScan),
            2
        );
        for operation in CompactionOperation::ALL {
            if operation != CompactionOperation::DailyRollupScan {
                assert_eq!(operation_count(&handle, operation), 0);
            }
        }
    }

    #[test]
    fn collect_service_files_groups_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let h00 = tmp.path().join("00");
        let h01 = tmp.path().join("01");
        std::fs::create_dir_all(&h00).unwrap();
        std::fs::create_dir_all(&h01).unwrap();
        std::fs::write(h00.join("nginx.parquet"), b"data").unwrap();
        std::fs::write(h00.join("postgres.parquet"), b"data").unwrap();
        std::fs::write(h01.join("nginx.parquet"), b"data").unwrap();

        let groups = collect_service_files(&[h00, h01]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["nginx"].len(), 2);
        assert_eq!(groups["postgres"].len(), 1);
    }

    #[tokio::test]
    async fn rollup_runs_without_wal_files() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Create hourly parquet files for a historical date (not today).
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let r1 = format!(
            r#"{{"_time":"{yesterday}T01:00:00Z","_ingested":"{yesterday}T01:00:00Z","service":"nginx","msg":"a"}}"#
        );
        write_hourly_parquet(&data_dir.join("prod"), &yesterday, "01", "nginx", &[&r1]);

        // WAL dir is empty — compact_once should still run rollup.
        compact_once(
            &wal_dir,
            &data_dir,
            Duration::from_secs(1),
            true,
            None,
            DEFAULT_CHUNK_SIZE,
            "2GB",
            None,
        )
        .await
        .unwrap();

        // Day-level file should exist from rollup (under the env root).
        let daily = data_dir.join("prod").join(&yesterday).join("nginx.parquet");
        assert!(
            daily.exists(),
            "rollup should run even when WAL dir is empty"
        );
    }

    /// The repin job's rollup interlock (ADR-0011): while the coordinator
    /// holds a rollup pause, a compaction tick drains WAL but never
    /// relocates hourly files into dailies — the shadow build's catch-up
    /// diff must stay additive. Dropping the pause resumes consolidation on
    /// the next tick.
    #[tokio::test]
    async fn rollup_is_suppressed_while_a_repin_pause_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let r1 = format!(
            r#"{{"_time":"{yesterday}T01:00:00Z","_ingested":"{yesterday}T01:00:00Z","service":"nginx","msg":"a"}}"#
        );
        let hourly =
            write_hourly_parquet(&data_dir.join("prod"), &yesterday, "01", "nginx", &[&r1]);

        let coordinator = Arc::new(RepinCoordinator::new());
        let pause = coordinator.pause_rollup();
        compact_once_coordinated(
            &wal_dir,
            &data_dir,
            Duration::from_secs(1),
            Duration::from_secs(1),
            true,
            None,
            DEFAULT_CHUNK_SIZE,
            "2GB",
            None,
            Some(&coordinator),
        )
        .await
        .unwrap();
        let daily = data_dir.join("prod").join(&yesterday).join("nginx.parquet");
        assert!(
            !daily.exists() && hourly.exists(),
            "no file may relocate while the repin pause is held"
        );

        drop(pause);
        compact_once_coordinated(
            &wal_dir,
            &data_dir,
            Duration::from_secs(1),
            Duration::from_secs(1),
            true,
            None,
            DEFAULT_CHUNK_SIZE,
            "2GB",
            None,
            Some(&coordinator),
        )
        .await
        .unwrap();
        assert!(
            daily.exists(),
            "consolidation resumes on the first tick after the job ends"
        );
    }

    /// The other half of that interlock (ADR-0011): a rollup pass that was
    /// already running when the job started. The pause flag alone only stops
    /// a pass that has not begun — a pass in flight would keep relocating
    /// files straight through `swap_envs` and republish pre-repin types into
    /// the new generation. So every relocating unit takes the corpus gate:
    /// the cutover waits it out, and the rest of the pass stands down.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_rollup_pass_is_gated_by_the_cutover_and_stands_down() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let r1 = format!(
            r#"{{"_time":"{yesterday}T01:00:00Z","_ingested":"{yesterday}T01:00:00Z","service":"nginx","msg":"a"}}"#
        );
        let hourly =
            write_hourly_parquet(&data_dir.join("prod"), &yesterday, "01", "nginx", &[&r1]);
        let daily = data_dir.join("prod").join(&yesterday).join("nginx.parquet");

        let coordinator = Arc::new(RepinCoordinator::new());

        // The cutover holds the corpus gate — as it does across the swap.
        let cutover = coordinator.cutover_guard().await;

        let c = Arc::clone(&coordinator);
        let (w, d) = (wal_dir.clone(), data_dir.clone());
        let tick = tokio::spawn(async move {
            compact_once_coordinated(
                &w,
                &d,
                Duration::from_secs(1),
                Duration::from_secs(1),
                true,
                None,
                DEFAULT_CHUNK_SIZE,
                "2GB",
                None,
                Some(&c),
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !tick.is_finished() && !daily.exists(),
            "a relocating rollup unit may not run under the cutover's guard"
        );

        // The job claims the rollup while that pass sits on the gate, then
        // the cutover completes.
        let pause = coordinator.pause_rollup();
        drop(cutover);
        tick.await.unwrap().unwrap();
        assert!(
            !daily.exists() && hourly.exists(),
            "the pass in flight must stand down, not relocate a pre-repin \
             file into the generation the cutover just published"
        );

        drop(pause);
        compact_once_coordinated(
            &wal_dir,
            &data_dir,
            Duration::from_secs(1),
            Duration::from_secs(1),
            true,
            None,
            DEFAULT_CHUNK_SIZE,
            "2GB",
            None,
            Some(&coordinator),
        )
        .await
        .unwrap();
        assert!(
            daily.exists(),
            "consolidation resumes on the first tick after the job ends"
        );
    }

    #[test]
    fn rollup_marker_written_and_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let marker = day_dir.join(".rollup-nginx");

        rollup_day_blocking(&day_dir, "nginx", &[f1], "2GB")
            .result
            .unwrap();

        // Marker should be cleaned up after successful rollup.
        assert!(!marker.exists(), "marker should be removed after rollup");
        // Canonical file should exist.
        assert!(day_dir.join("nginx.parquet").exists());
    }

    #[test]
    fn rollup_recovery_after_rename_crash() {
        // Simulate crash after rename but before hourly deletion:
        // canonical exists, marker exists, hourly files still present.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        let marker = day_dir.join(".rollup-nginx");

        // Simulate: canonical was written (via rename), marker exists, hourly survives.
        std::fs::write(&canonical, b"consolidated data").unwrap();
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&f1)).unwrap();
        assert!(marker.exists());
        assert!(f1.exists());

        // Recovery should delete hourlies and marker without touching canonical.
        recover_rollup_markers(&day_dir).unwrap();

        assert!(canonical.exists(), "canonical should survive recovery");
        assert!(!f1.exists(), "hourly file should be deleted by recovery");
        assert!(!marker.exists(), "marker should be removed after recovery");
    }

    #[test]
    fn rollup_recovery_after_tmp_crash() {
        // Simulate crash after .tmp write but before rename:
        // .tmp exists, marker exists, no canonical yet.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        let tmp_file = day_dir.join("nginx.parquet.tmp");
        let marker = day_dir.join(".rollup-nginx");

        // Simulate: a COMPLETE .tmp was written, marker exists, no canonical
        // yet. A real interrupted-after-write tmp is a valid parquet, so use
        // f1's bytes (recovery validates before promoting — see
        // `recovery_discards_truncated_tmp` for the truncated case).
        std::fs::copy(&f1, &tmp_file).unwrap();
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&f1)).unwrap();
        assert!(!canonical.exists());
        assert!(tmp_file.exists());

        // Recovery should rename .tmp to canonical, delete hourlies and marker.
        recover_rollup_markers(&day_dir).unwrap();

        assert!(canonical.exists(), "canonical should exist after recovery");
        assert!(!tmp_file.exists(), ".tmp should be gone after recovery");
        assert!(!f1.exists(), "hourly file should be deleted by recovery");
        assert!(!marker.exists(), "marker should be removed after recovery");
    }

    #[test]
    fn rollup_recovery_stale_marker() {
        // Stale marker: neither canonical nor .tmp exists.
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();

        let marker = day_dir.join(".rollup-nginx");
        std::fs::write(&marker, "/nonexistent/path.parquet").unwrap();

        recover_rollup_markers(&day_dir).unwrap();
        assert!(!marker.exists(), "stale marker should be removed");
    }

    #[test]
    fn is_valid_parquet_detects_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // A real parquet file is valid.
        let data_dir = dir.join("data");
        let r = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"x","m":"y"}"#;
        let good = write_hourly_parquet(&data_dir, "2026-01-15", "01", "x", &[r]);
        assert!(is_valid_parquet(&good), "real parquet should be valid");

        let empty = dir.join("empty.parquet");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_valid_parquet(&empty), "zero-byte file is invalid");

        let trunc = dir.join("trunc.parquet");
        std::fs::write(&trunc, b"PAR1\x00\x00").unwrap();
        assert!(!is_valid_parquet(&trunc), "truncated file is invalid");

        let nomagic = dir.join("nomagic.parquet");
        std::fs::write(&nomagic, b"definitely not a parquet file, but long enough").unwrap();
        assert!(!is_valid_parquet(&nomagic), "missing PAR1 magic is invalid");

        assert!(
            !is_valid_parquet(&dir.join("does-not-exist.parquet")),
            "missing file is invalid"
        );
    }

    #[test]
    fn is_valid_ndjson_detects_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let good = dir.join("good.ndjson");
        std::fs::write(&good, b"{\"service\":\"x\",\"message\":\"hi\"}\n").unwrap();
        assert!(is_valid_ndjson(&good), "real ndjson should be valid");

        // Pure-NUL file: the torn-write crash signature (unflushed blocks read
        // back as 0x00). 662 bytes mirrors the live homelab debris.
        let nul = dir.join("nul.ndjson");
        std::fs::write(&nul, [0u8; 662]).unwrap();
        assert!(!is_valid_ndjson(&nul), "all-NUL file is corrupt");

        // A valid record followed by an embedded NUL (partial torn write).
        let partial = dir.join("partial.ndjson");
        std::fs::write(&partial, b"{\"a\":1}\n\x00\x00\x00").unwrap();
        assert!(!is_valid_ndjson(&partial), "embedded NUL is corrupt");

        // Zero-byte: read_json(records=true) errors on empty input, so an
        // empty WAL file would wedge compaction just like a NUL one.
        let empty = dir.join("empty.ndjson");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_valid_ndjson(&empty), "zero-byte file is invalid");

        // Non-UTF-8 garbage (no NUL byte) exercises the UTF-8 branch.
        let binary = dir.join("binary.ndjson");
        std::fs::write(&binary, [0xff, 0xfe, 0xfd, 0x01]).unwrap();
        assert!(!is_valid_ndjson(&binary), "non-UTF-8 is invalid");

        assert!(
            !is_valid_ndjson(&dir.join("does-not-exist.ndjson")),
            "missing file is invalid"
        );
    }

    #[test]
    fn compact_quarantines_nul_ndjson_keeps_good() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let good = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"nginx","message":"hello"}"#;
        let good_file = write_wal_file(&wal_dir, "nginx", &[good]);

        // A pure-NUL WAL file (the torn-write crash signature: unflushed
        // blocks read back as 0x00). write_wal_file only writes text, so write
        // it directly; keep the `nginx_` prefix so it groups with the good
        // file's service. 662 bytes mirrors the live homelab debris.
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let bad_file = wal_dir.join(format!("nginx_{millis}_bad.ndjson"));
        std::fs::write(&bad_file, [0u8; 662]).unwrap();

        let quarantined =
            compact_service_blocking(&[good_file, bad_file.clone()], &data_dir, "nginx", "2GB")
                .unwrap();
        assert_eq!(quarantined, 1, "the NUL file should be quarantined");

        // Good data still compacts to exactly one parquet.
        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1, "good WAL must still produce a parquet");

        // Corrupt file renamed aside; bytes preserved for forensics.
        assert!(!bad_file.exists(), "corrupt WAL renamed away");
        let mut corrupt = bad_file.into_os_string();
        corrupt.push(".corrupt");
        assert!(
            PathBuf::from(corrupt).exists(),
            "corrupt WAL quarantined to .corrupt"
        );

        // Only the good row landed.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only the valid row should be present");
    }

    #[test]
    fn compact_all_nul_is_data_loss_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let a = wal_dir.join(format!("svc_{millis}_a.ndjson"));
        let b = wal_dir.join(format!("svc_{millis}_b.ndjson"));
        std::fs::write(&a, [0u8; 128]).unwrap();
        std::fs::write(&b, [0u8; 128]).unwrap();

        // All inputs corrupt: no parquet, no error (nothing readable to retry),
        // but both are counted as quarantined data-loss.
        let quarantined =
            compact_service_blocking(&[a.clone(), b.clone()], &data_dir, "svc", "2GB").unwrap();
        assert_eq!(quarantined, 2, "both corrupt inputs counted");

        assert!(
            find_files_by_ext(&data_dir, "parquet").is_empty(),
            "no parquet from all-corrupt batch"
        );
        assert!(!a.exists() && !b.exists(), "both corrupt files quarantined");
    }

    #[test]
    fn compact_isolates_malformed_textual_ndjson_keeps_good() {
        // A malformed-but-textual WAL file (valid UTF-8, no NUL, but truncated
        // mid-JSON) slips past the byte sniff and fails read_json. Per-file
        // read isolation must quarantine it (alongside a NUL file) while still
        // compacting the good data — one poison file must not wedge the batch.
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let good = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"web","message":"ok"}"#;
        let good_file = write_wal_file(&wal_dir, "web", &[good]);

        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        // Valid UTF-8, no NUL byte, but a truncated JSON object — read_json
        // errors on it, yet is_valid_ndjson's byte sniff passes it.
        let malformed = wal_dir.join(format!("web_{millis}_trunc.ndjson"));
        std::fs::write(
            &malformed,
            b"{\"_time\":\"2026-01-01T00:00:01Z\",\"_ingested\":\"2026-01-01T00:00:01Z\",\"service\":\"web\",\"message\":",
        )
        .unwrap();

        // A pure-NUL file too, so both quarantine paths run in one batch
        // (sniff for the NUL, read-isolation for the malformed-textual one).
        let nul = wal_dir.join(format!("web_{millis}_nul.ndjson"));
        std::fs::write(&nul, [0u8; 64]).unwrap();

        let quarantined = compact_service_blocking(
            &[good_file, malformed.clone(), nul.clone()],
            &data_dir,
            "web",
            "2GB",
        )
        .unwrap();
        assert_eq!(quarantined, 2, "malformed + NUL files both quarantined");

        // Good data still compacts to exactly one parquet, one row.
        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1, "good WAL still produces a parquet");

        assert!(!malformed.exists(), "malformed file renamed away");
        assert!(!nul.exists(), "NUL file renamed away");
        let mut m_corrupt = malformed.into_os_string();
        m_corrupt.push(".corrupt");
        assert!(
            PathBuf::from(m_corrupt).exists(),
            "malformed file quarantined to .corrupt"
        );

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only the good row should be present");
    }

    #[test]
    fn compact_keeps_quarantine_count_when_a_later_step_fails() {
        // A NUL file is quarantined (count=1) and then a step after the
        // quarantine errors (output dir can't be created because data_dir is a
        // regular file). The count must survive the Err — a naive `?` would
        // drop it, and since the file is already renamed `.corrupt` a retry
        // can't re-count it. Mirrors the rollup
        // `rollup_keeps_quarantine_count_when_merge_errors` guarantee.
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // data_dir is a regular file, so create_dir_all of the output dir (and
        // any DuckDB write) downstream of the quarantine fails.
        let data_file = tmp.path().join("data_is_a_file");
        std::fs::write(&data_file, b"not a directory").unwrap();

        let good = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"svc","message":"ok"}"#;
        let good_file = write_wal_file(&wal_dir, "svc", &[good]);

        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let nul_file = wal_dir.join(format!("svc_{millis}_bad.ndjson"));
        std::fs::write(&nul_file, [0u8; 128]).unwrap();

        let mut quarantined = Vec::new();
        let result = prepare_service_batch(
            &[good_file, nul_file.clone()],
            &data_file,
            "svc",
            "2GB",
            &mut quarantined,
            &HashMap::new(),
        )
        .and_then(|prep| {
            let prep = prep.expect("the good file survives");
            let pins = local_pins(&prep.proposals);
            conform_and_write(prep, &pins, &data_file, "svc").map(|_| ())
        });

        assert!(result.is_err(), "a step after the quarantine must error");
        assert_eq!(
            quarantined,
            std::slice::from_ref(&nul_file),
            "the quarantine must survive the Err, not be dropped"
        );
        assert!(!nul_file.exists(), "the NUL file was really quarantined");
    }

    #[test]
    fn rollup_quarantines_truncated_hourly_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"ok"}"#;
        let good = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        // A truncated parquet in another hour (crash mid-COPY).
        let bad_dir = data_dir.join(date).join("02");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let bad = bad_dir.join("nginx.parquet");
        std::fs::write(&bad, b"PAR1\x00\x00").unwrap();

        let day_dir = data_dir.join(date);
        let outcome = rollup_day_blocking(&day_dir, "nginx", &[good.clone(), bad.clone()], "2GB");
        assert!(
            outcome.result.is_ok(),
            "partial-corrupt rollup should still produce a daily file"
        );
        assert_eq!(outcome.quarantined, 1, "one corrupt input quarantined");

        // Good data rolled up; corrupt file quarantined, not read.
        let daily = day_dir.join("nginx.parquet");
        assert!(daily.exists(), "daily file built from the valid hourly");
        assert!(!good.exists(), "consumed valid hourly should be deleted");
        assert!(!bad.exists(), "corrupt file should be renamed away");

        let mut corrupt = bad.into_os_string();
        corrupt.push(".corrupt");
        assert!(
            PathBuf::from(corrupt).exists(),
            "corrupt file should be quarantined to .corrupt"
        );

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only the valid row should be present");
    }

    #[test]
    fn rollup_skips_when_all_inputs_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let bad_dir = data_dir.join(date).join("00");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let bad = bad_dir.join("svc.parquet");
        std::fs::write(&bad, b"").unwrap(); // zero-byte

        let day_dir = data_dir.join(date);
        // No error (nothing recoverable to retry), no daily file produced,
        // but the quarantine count surfaces the data-loss.
        let outcome = rollup_day_blocking(&day_dir, "svc", std::slice::from_ref(&bad), "2GB");
        assert!(
            outcome.result.is_ok(),
            "all-corrupt rollup returns Ok — nothing readable to retry"
        );
        assert_eq!(outcome.quarantined, 1, "the all-corrupt input is counted");

        assert!(
            !day_dir.join("svc.parquet").exists(),
            "no daily file from all-corrupt input"
        );
        assert!(!bad.exists(), "corrupt file should be quarantined");
    }

    #[test]
    fn rollup_keeps_quarantine_count_when_merge_errors() {
        // One input is corrupt (quarantined, count=1); a second input passes the
        // magic-byte sniff but is unreadable by read_parquet (valid PAR1
        // bookends, bogus footer length), so the merge COPY hard-errors. The
        // quarantine count must still ride out on the outcome: a naive
        // `return Err` would drop it, and since the quarantined file is already
        // renamed `.corrupt`, a retry tick could never re-count it.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // Corrupt (zero-byte) input → quarantined.
        let corrupt_dir = data_dir.join(date).join("00");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        let corrupt = corrupt_dir.join("nginx.parquet");
        std::fs::write(&corrupt, b"").unwrap();

        // Sniff-valid but unreadable input → hard COPY error. 12 bytes: PAR1
        // header + a 0xFFFFFFFF "footer length" (points way past the file) +
        // PAR1 trailer. Passes is_valid_parquet; read_parquet cannot parse it.
        let unreadable_dir = data_dir.join(date).join("01");
        std::fs::create_dir_all(&unreadable_dir).unwrap();
        let unreadable = unreadable_dir.join("nginx.parquet");
        std::fs::write(&unreadable, b"PAR1\xff\xff\xff\xffPAR1").unwrap();

        let day_dir = data_dir.join(date);
        let outcome = rollup_day_blocking(
            &day_dir,
            "nginx",
            &[corrupt.clone(), unreadable.clone()],
            "2GB",
        );
        assert!(
            outcome.result.is_err(),
            "an unreadable surviving input must fail the merge"
        );
        assert_eq!(
            outcome.quarantined, 1,
            "the quarantine count must survive the hard merge error, not be dropped"
        );
    }

    #[test]
    fn recovery_discards_truncated_tmp() {
        // Crash mid-COPY: a truncated .tmp must not be promoted to canonical,
        // and the hourly files must be retained for a fresh rollup.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        let tmp_file = day_dir.join("nginx.parquet.tmp");

        std::fs::write(&tmp_file, b"PAR1trunc").unwrap();
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&f1)).unwrap();

        recover_rollup_markers(&day_dir).unwrap();

        assert!(
            !canonical.exists(),
            "must NOT promote a truncated tmp to canonical"
        );
        assert!(!tmp_file.exists(), "truncated tmp should be moved aside");
        assert!(f1.exists(), "hourly file retained for re-rollup");
        assert!(
            !day_dir.join(".rollup-nginx").exists(),
            "marker should be removed"
        );
    }

    #[test]
    fn compact_once_counts_quarantine_as_error() {
        // Counter wiring: an all-corrupt service/day must surface its
        // quarantine count through rollup_once → compact_once's returned u64
        // (which spawn_compaction folds into stats.total_errors).
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Historical (non-today) day with one all-corrupt service.
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let bad_dir = data_dir.join("prod").join(&yesterday).join("00");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("svc.parquet"), b"").unwrap(); // zero-byte

        let rt = tokio::runtime::Runtime::new().unwrap();
        let errors = rt
            .block_on(compact_once(
                &wal_dir,
                &data_dir,
                Duration::from_secs(1),
                true,
                None,
                DEFAULT_CHUNK_SIZE,
                "2GB",
                None,
            ))
            .unwrap();

        assert!(
            errors > 0,
            "quarantine data-loss should flow out as a rollup failure/data-loss tally, got {errors}"
        );
        // No daily file from the all-corrupt input.
        assert!(
            !data_dir
                .join("prod")
                .join(&yesterday)
                .join("svc.parquet")
                .exists()
        );
    }

    #[test]
    fn retire_merged_input_deletes_when_possible() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("nginx.parquet");
        std::fs::write(&f, b"data").unwrap();

        retire_merged_input(&f).unwrap();

        assert!(!f.exists(), "file should be deleted on the happy path");
        let mut aside = f.into_os_string();
        aside.push(".merged");
        assert!(
            !PathBuf::from(aside).exists(),
            "no .merged sibling when delete succeeds"
        );
    }

    #[test]
    fn retire_merged_input_renames_aside_when_delete_fails() {
        // remove_file on a directory fails → fall through to rename-aside.
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().join("nginx.parquet");
        std::fs::create_dir(&dir_path).unwrap();

        retire_merged_input(&dir_path).unwrap();

        assert!(!dir_path.exists(), "original should be renamed away");
        let mut aside = dir_path.into_os_string();
        aside.push(".merged");
        assert!(
            PathBuf::from(aside).exists(),
            "should be renamed aside with .merged suffix"
        );
    }

    #[test]
    fn merged_suffix_is_not_picked_up_by_collect_service_files() {
        // A retired hourly (`.merged`) must never be re-merged: the glob keys
        // off the `parquet` extension, and `nginx.parquet.merged` has the
        // `merged` extension.
        let tmp = tempfile::tempdir().unwrap();
        let hour = tmp.path().join("00");
        std::fs::create_dir_all(&hour).unwrap();
        std::fs::write(hour.join("nginx.parquet.merged"), b"retired").unwrap();
        std::fs::write(hour.join("postgres.parquet"), b"live").unwrap();

        let groups = collect_service_files(&[hour]);
        assert!(
            !groups.contains_key("nginx.parquet"),
            "retired .merged file must not be collected"
        );
        assert!(
            !groups.contains_key("nginx"),
            "retired .merged file must not be collected"
        );
        assert_eq!(groups.len(), 1, "only the live parquet should be collected");
        assert!(groups.contains_key("postgres"));
    }

    #[test]
    fn rollup_does_not_duplicate_rows_when_hourly_survives() {
        // Simulate a delete that fails on the first rollup (the hourly is
        // retired aside as `.merged`), then run a second rollup over the same
        // day. The retired `.merged` file must not be re-merged, so the daily
        // row count stays put rather than doubling.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let day_dir = data_dir.join(date);

        // First rollup builds the daily file and retires the hourly.
        rollup_day_blocking(&day_dir, "nginx", std::slice::from_ref(&f1), "2GB")
            .result
            .unwrap();

        let daily = day_dir.join("nginx.parquet");
        assert!(daily.exists());

        // Simulate a "delete failed → retired aside" hourly that survived as
        // `.merged` (what retire_merged_input leaves behind on a bad delete).
        // It must not be re-collected nor re-merged.
        let stranded = day_dir.join("01").join("nginx.parquet.merged");
        std::fs::create_dir_all(stranded.parent().unwrap()).unwrap();
        std::fs::copy(&daily, &stranded).unwrap();

        // Re-discover service files for the day exactly as rollup_once does.
        let hour_dirs = collect_hour_dirs(&day_dir);
        let service_files = collect_service_files(&hour_dirs);

        // The stranded `.merged` file must not appear as an input.
        assert!(
            service_files.get("nginx").is_none_or(Vec::is_empty),
            "retired .merged hourly must not be re-collected for rollup"
        );

        // Second rollup over the canonical only (no live hourlies) — must not
        // double the count even though `.merged` bytes sit alongside.
        rollup_day_blocking(&day_dir, "nginx", &[], "2GB")
            .result
            .unwrap();

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "row count must not double from a stranded hourly");
    }

    #[test]
    fn recovery_keeps_marker_when_hourly_retire_fails() {
        // Canonical present, marker lists an hourly that can be neither
        // deleted nor renamed aside. recover_rollup_markers must return Err
        // and leave the marker in place so the next pass retries.
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();

        let canonical = day_dir.join("nginx.parquet");
        std::fs::write(&canonical, b"consolidated").unwrap();

        // The "hourly" is a non-empty directory: remove_file fails (EISDIR),
        // and rename-aside fails too because a non-empty `.merged` directory
        // already occupies the target (ENOTEMPTY).
        let hourly = day_dir.join("01").join("nginx.parquet");
        std::fs::create_dir_all(&hourly).unwrap();
        std::fs::write(hourly.join("blocker"), b"x").unwrap();
        let aside = day_dir.join("01").join("nginx.parquet.merged");
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("blocker"), b"x").unwrap();

        let marker = day_dir.join(".rollup-nginx");
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&hourly)).unwrap();
        assert!(marker.exists());

        let result = recover_rollup_markers(&day_dir);
        assert!(
            result.is_err(),
            "recovery should fail when an hourly can be neither removed nor retired"
        );
        assert!(
            marker.exists(),
            "marker must survive so the next recovery pass retries"
        );
    }

    #[test]
    fn retire_merged_input_is_idempotent_on_missing_file() {
        // Retiring an already-gone hourly is a no-op success. The goal
        // ("this file is no longer a re-mergeable hourly") is already met, so a
        // missing source must not propagate as Err (which would wedge recovery).
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("never-existed.parquet");
        assert!(!gone.exists());

        retire_merged_input(&gone).expect("missing hourly should retire as Ok");

        let mut aside = gone.into_os_string();
        aside.push(".merged");
        assert!(
            !PathBuf::from(aside).exists(),
            "must not conjure a .merged sibling for a phantom-missing source"
        );
    }

    #[test]
    fn recovery_completes_after_partial_hourly_cleanup() {
        // A crash mid-cleanup-loop deletes some hourlies but leaves the
        // marker, which lists all of them. The next recovery pass replays the
        // marker verbatim, so retiring an already-gone hourly must be a no-op
        // success: an Err on the first phantom short-circuits before the
        // marker delete and wedges recovery permanently.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // Two hourlies were originally merged; the canonical already exists.
        let r1 = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let r2 = r#"{"_time":"2026-01-15T02:00:00Z","_ingested":"2026-01-15T02:00:00Z","service":"nginx","msg":"b"}"#;
        let already_deleted = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let still_present = write_hourly_parquet(&data_dir, date, "02", "nginx", &[r2]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        std::fs::write(&canonical, b"consolidated").unwrap();

        // Marker lists both (written before the cleanup loop ran).
        write_rollup_marker(
            &day_dir,
            "nginx",
            &[already_deleted.clone(), still_present.clone()],
        )
        .unwrap();
        let marker = day_dir.join(".rollup-nginx");
        assert!(marker.exists());

        // Simulate the partial cleanup: the first hourly was deleted before the
        // crash, the second was not.
        std::fs::remove_file(&already_deleted).unwrap();
        assert!(!already_deleted.exists());
        assert!(still_present.exists());

        recover_rollup_markers(&day_dir).expect("recovery must tolerate a phantom-missing hourly");

        // The surviving hourly is retired (deleted, or renamed aside as .merged).
        let mut aside = still_present.clone().into_os_string();
        aside.push(".merged");
        assert!(
            !still_present.exists() || PathBuf::from(aside).exists(),
            "the present hourly must be retired (gone or .merged)"
        );
        // The marker must be removed so later compaction ticks don't replay it.
        assert!(
            !marker.exists(),
            "marker must be removed after recovery completes"
        );
    }

    // --- malformed-timestamp repair tests (ADR-0008) -----------------------

    /// Read a single-column query over a parquet file into strings.
    fn read_strings(parquet: &Path, select: &str) -> Vec<String> {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {select} FROM read_parquet('{}')",
                parquet.display()
            ))
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows
    }

    /// A hand-written WAL file with a malformed timestamp compacts, and the
    /// row's timestamp comes from the filename's unix-millis segment rather
    /// than wedging the batch.
    #[test]
    fn compact_repairs_malformed_timestamp_from_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Known instant: 2024-10-27 03:33:20 UTC.
        let known_millis: i64 = 1_730_000_000_000;
        let wal = wal_dir.join(format!("svc_{known_millis}_abcd.ndjson"));
        std::fs::write(
            &wal,
            b"{\"_time\":\"not-a-date\",\"_ingested\":\"not-a-date\",\"service\":\"svc\",\"message\":\"wedged\"}\n",
        )
        .unwrap();

        let quarantined =
            compact_service_blocking(&[wal], &data_dir, "svc", "2GB").expect("must not wedge");
        assert_eq!(quarantined, 0, "a bad timestamp is repair, not quarantine");

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let ts = read_strings(&parquet[0], "CAST(\"_time\" AS VARCHAR)");
        assert_eq!(
            ts,
            vec!["2024-10-27 03:33:20".to_owned()],
            "timestamp must be recovered from the WAL filename's unix millis"
        );
    }

    /// Each row's filename fallback comes from its own WAL file, never a
    /// batch-level value: one batch spanning two files with different
    /// unix-millis values recovers two different instants.
    #[test]
    fn compact_filename_fallback_is_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let a = wal_dir.join("svc_1730000000000_aaaa.ndjson");
        let b = wal_dir.join("svc_1730000060000_bbbb.ndjson");
        std::fs::write(
            &a,
            b"{\"_time\":\"not-a-date\",\"_ingested\":\"not-a-date\",\"service\":\"svc\",\"message\":\"a\"}\n",
        )
        .unwrap();
        std::fs::write(
            &b,
            b"{\"_time\":\"not-a-date\",\"_ingested\":\"not-a-date\",\"service\":\"svc\",\"message\":\"b\"}\n",
        )
        .unwrap();

        compact_service_blocking(&[a, b], &data_dir, "svc", "2GB").unwrap();

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let rows = read_strings(&parquet[0], "message || '@' || CAST(\"_time\" AS VARCHAR)");
        assert_eq!(
            rows,
            vec![
                "a@2024-10-27 03:33:20".to_owned(),
                "b@2024-10-27 03:34:20".to_owned(),
            ],
            "each row must recover its own file's ingest instant"
        );
    }

    /// All four malformed-`_time` shapes compact without error and land with
    /// a non-NULL timestamp.
    #[test]
    fn compact_repairs_all_trigger_variants() {
        let variants = [
            r#""not-a-date""#,
            r#""2026-13-45T99:99:99Z""#,
            r#"{"nested":1}"#,
            "12345",
        ];
        for (i, variant) in variants.iter().enumerate() {
            let tmp = tempfile::tempdir().unwrap();
            let wal_dir = tmp.path().join("wal");
            let data_dir = tmp.path().join("data");
            std::fs::create_dir_all(&wal_dir).unwrap();

            let wal = wal_dir.join("svc_1730000000000_abcd.ndjson");
            std::fs::write(
                &wal,
                format!(r#"{{"_time":{variant},"_ingested":"2026-01-01T00:00:00Z","service":"svc","message":"v{i}"}}"#),
            )
            .unwrap();

            compact_service_blocking(&[wal], &data_dir, "svc", "2GB")
                .unwrap_or_else(|e| panic!("variant {variant} must compact: {e}"));

            let parquet = find_files_by_ext(&data_dir, "parquet");
            assert_eq!(parquet.len(), 1, "variant {variant} must produce parquet");
            let conn = duckdb::Connection::open_in_memory().unwrap();
            let nulls: i64 = conn
                .query_row(
                    &format!(
                        "SELECT count(*)::BIGINT FROM read_parquet('{}') WHERE \"_time\" IS NULL",
                        parquet[0].display()
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                nulls, 0,
                "variant {variant}: no parquet row may have a NULL timestamp"
            );
        }
    }

    /// A repaired event past `DuckDB`'s default JSON sample window still
    /// reaches parquet with its `_repairs` intact.
    ///
    /// `_repairs` is sparse by construction — only repaired events carry it
    /// — so auto-detection over a bounded sample would miss it on any WAL
    /// file bigger than the sample and drop the column with no error at all,
    /// defeating ADR-0008's promise that the evidence reaches the operator.
    #[test]
    fn compact_preserves_repair_column_past_the_sample_window() {
        use std::fmt::Write as _;

        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Larger than DuckDB's default JSON sample (~20480 rows), with the
        // only repaired event as the very last line.
        let mut lines = String::new();
        for i in 0..30_000 {
            writeln!(
                lines,
                "{{\"_time\":\"2026-01-01T00:00:00Z\",\"_ingested\":\"2026-01-01T00:00:00Z\",\"service\":\"svc\",\"message\":\"m{i}\"}}"
            )
            .unwrap();
        }
        lines.push_str(
            "{\"_time\":\"2026-01-01T00:00:00Z\",\"_ingested\":\"2026-01-01T00:00:00Z\",\"service\":\"svc\",\
             \"message\":\"repaired\",\"_repairs\":\"time.from_ingest\"}\n",
        );
        let wal = wal_dir.join("svc_1730000000000_abcd.ndjson");
        std::fs::write(&wal, lines).unwrap();

        compact_service_blocking(&[wal], &data_dir, "svc", "2GB").expect("must compact");

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let preserved = read_strings(&parquet[0], "COALESCE(string_agg(_repairs), 'MISSING')");
        assert_eq!(
            preserved,
            vec!["time.from_ingest".to_owned()],
            "the sparse repair column must survive a WAL file larger than \
             the JSON sample window"
        );
    }

    /// The two path dimensions (ADR-0009): compaction writes
    /// `data/{env}/{date}/{HH}/{service}.parquet`, never merges across
    /// envs, and carries service names verbatim so `api.v2` and `api_v2`
    /// land in distinct files.
    #[tokio::test]
    async fn compact_once_partitions_by_env_and_verbatim_service() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");

        let row = |env: &str, msg: &str| {
            format!(
                r#"{{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","env":"{env}","service":"svc","message":"{msg}"}}"#
            )
        };
        std::fs::create_dir_all(wal_dir.join("prod")).unwrap();
        std::fs::create_dir_all(wal_dir.join("lab")).unwrap();
        std::fs::write(
            wal_dir.join("prod").join("svc_1730000000000_aaaa.ndjson"),
            row("prod", "prod-row"),
        )
        .unwrap();
        std::fs::write(
            wal_dir.join("lab").join("svc_1730000000000_bbbb.ndjson"),
            row("lab", "lab-row"),
        )
        .unwrap();
        // Dotted vs underscored service in the same env: distinct files.
        std::fs::write(
            wal_dir.join("prod").join("api.v2_1730000000000_cccc.ndjson"),
            r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","env":"prod","service":"api.v2","message":"dotted"}"#,
        )
        .unwrap();
        std::fs::write(
            wal_dir.join("prod").join("api_v2_1730000000000_dddd.ndjson"),
            r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","env":"prod","service":"api_v2","message":"underscored"}"#,
        )
        .unwrap();

        let errors = compact_once(
            &wal_dir,
            &data_dir,
            Duration::ZERO,
            false,
            None,
            500,
            "2GB",
            None,
        )
        .await
        .expect("compaction tick must succeed");
        assert_eq!(errors, 0);

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let hour = chrono::Utc::now().format("%H").to_string();
        let prod_svc = data_dir
            .join("prod")
            .join(&today)
            .join(&hour)
            .join("svc.parquet");
        let lab_svc = data_dir
            .join("lab")
            .join(&today)
            .join(&hour)
            .join("svc.parquet");
        assert!(prod_svc.exists(), "expected {}", prod_svc.display());
        assert!(lab_svc.exists(), "expected {}", lab_svc.display());
        assert_eq!(
            read_strings(&prod_svc, "message"),
            vec!["prod-row".to_owned()],
            "prod parquet must not absorb the lab env's rows"
        );
        assert_eq!(
            read_strings(&lab_svc, "message"),
            vec!["lab-row".to_owned()],
            "lab parquet must not absorb the prod env's rows"
        );

        let hour_dir = data_dir.join("prod").join(&today).join(&hour);
        assert!(hour_dir.join("api.v2.parquet").exists(), "dotted file");
        assert!(hour_dir.join("api_v2.parquet").exists(), "underscored file");
        assert_eq!(
            read_strings(&hour_dir.join("api.v2.parquet"), "message"),
            vec!["dotted".to_owned()]
        );
        assert_eq!(
            read_strings(&hour_dir.join("api_v2.parquet"), "message"),
            vec!["underscored".to_owned()]
        );
    }

    /// One unreadable env WAL directory is isolated to its own env: it is
    /// counted and skipped, never propagated. Envs are walked in sorted
    /// order, so `broken` is scanned before `prod` — a propagated error
    /// would stall `prod`'s WAL drain (and the rollup) indefinitely.
    #[cfg(unix)]
    #[tokio::test]
    async fn compact_once_isolates_unreadable_env_wal_dir() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");

        let broken = wal_dir.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::create_dir_all(wal_dir.join("prod")).unwrap();
        let prod_wal = wal_dir.join("prod").join("svc_1730000000000_aaaa.ndjson");
        std::fs::write(
            &prod_wal,
            r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","env":"prod","service":"svc","message":"prod-row"}"#,
        )
        .unwrap();

        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&broken).is_ok() {
            // Running as root: the mode bits are not enforced, so there is
            // no unreadable directory to isolate. Nothing to assert.
            return;
        }

        let errors = compact_once(
            &wal_dir,
            &data_dir,
            Duration::ZERO,
            true,
            None,
            500,
            "2GB",
            None,
        )
        .await
        .expect("one unreadable env must not fail the whole cycle");
        let _ = std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755));

        assert_eq!(
            errors, 1,
            "the unreadable env is counted once as a failure, not propagated"
        );

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let hour = chrono::Utc::now().format("%H").to_string();
        assert!(
            data_dir
                .join("prod")
                .join(&today)
                .join(&hour)
                .join("svc.parquet")
                .exists(),
            "a later env must still compact"
        );
        assert!(!prod_wal.exists(), "a later env's WAL must still drain");
    }

    /// An unreadable WAL *root* is not an empty WAL root: it must surface as
    /// an error, never as a clean cycle. Swallowing it iterates no envs, so
    /// the WAL never drains, the hot buffer fills until admission refuses
    /// new events, and the error counter stays at zero: a stall no counter
    /// explains (ADR-0008).
    #[cfg(unix)]
    #[tokio::test]
    async fn compact_once_errors_on_unreadable_wal_root() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(wal_dir.join("prod")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        std::fs::set_permissions(&wal_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&wal_dir).is_ok() {
            // Running as root: the mode bits are not enforced, so there is no
            // unreadable root to report. Nothing to assert.
            let _ = std::fs::set_permissions(&wal_dir, std::fs::Permissions::from_mode(0o755));
            return;
        }

        let result = compact_once(
            &wal_dir,
            &data_dir,
            Duration::ZERO,
            true,
            None,
            500,
            "2GB",
            None,
        )
        .await;
        // Teardown first: a leaked 0o000 dir would break tempdir cleanup.
        std::fs::set_permissions(&wal_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.expect_err("an unreadable WAL root must not read as a clean cycle");
        assert!(
            err.contains("WAL env directories"),
            "error must name the failing listing, got: {err}"
        );
    }

    /// The other half of the pair: a WAL root that does not exist yet is a
    /// legitimate cold start, and must stay a silent, clean, zero-error run.
    #[tokio::test]
    async fn compact_once_is_clean_when_wal_root_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let errors = compact_once(
            &wal_dir,
            &data_dir,
            Duration::ZERO,
            true,
            None,
            500,
            "2GB",
            None,
        )
        .await
        .expect("a missing WAL root is a cold start, not a failure");
        assert_eq!(errors, 0, "a cold start reports no errors");
    }

    /// One bad event does not affect its batch-mates: 1 bad + 2 good in one
    /// WAL file all land, the WAL directory drains, and `compact_once`
    /// reports zero errors — no poison pill.
    #[tokio::test]
    async fn compact_once_drains_bad_timestamp_without_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let good1 = r#"{"_time":"2026-01-01T00:00:00Z","_ingested":"2026-01-01T00:00:00Z","service":"nginx","message":"good1"}"#;
        let bad =
            r#"{"_time":"not-a-date","_ingested":"not-a-date","service":"nginx","message":"bad"}"#;
        let good2 = r#"{"_time":"2026-01-01T00:00:02Z","_ingested":"2026-01-01T00:00:02Z","service":"nginx","message":"good2"}"#;
        std::fs::create_dir_all(wal_dir.join("prod")).unwrap();
        let wal = wal_dir.join("prod").join("nginx_1730000000000_abcd.ndjson");
        std::fs::write(&wal, [good1, bad, good2].join("\n")).unwrap();

        let errors = compact_once(
            &wal_dir,
            &data_dir,
            Duration::ZERO,
            false,
            None,
            500,
            "2GB",
            None,
        )
        .await
        .expect("compaction tick must succeed");
        assert_eq!(errors, 0, "a bad timestamp must not count as an error");

        assert!(!wal.exists(), "consumed WAL file must be deleted");
        let leftover = find_files_by_ext(&wal_dir, "ndjson");
        assert!(leftover.is_empty(), "WAL directory must be empty");

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3, "all three events (incl. the repaired one) land");
    }

    /// A user event legitimately carrying a field named `filename` keeps all
    /// its columns — the synthetic WAL-provenance column uses a reserved
    /// name precisely so it cannot collide.
    #[test]
    fn compact_keeps_user_field_named_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let wal = wal_dir.join("svc_1730000000000_abcd.ndjson");
        std::fs::write(
            &wal,
            b"{\"_time\":\"2026-01-01T00:00:00Z\",\"_ingested\":\"2026-01-01T00:00:00Z\",\"service\":\"svc\",\"filename\":\"user.txt\",\"message\":\"m\"}\n",
        )
        .unwrap();

        compact_service_blocking(&[wal], &data_dir, "svc", "2GB").unwrap();

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let filenames = read_strings(&parquet[0], "\"filename\"");
        assert_eq!(
            filenames,
            vec!["user.txt".to_owned()],
            "the user's own `filename` column must survive"
        );
    }

    /// A WAL file carrying the reserved provenance key still drains, and
    /// drains losslessly: a row literally carrying `_trawl_wal_file` collides
    /// with the synthetic provenance column, which must route to the renamed
    /// provenance retry rather than failing the read forever (the isolation
    /// path cannot save it — `probe_ndjson` omits `filename=`, so the file
    /// parses cleanly and is kept as a survivor). Its innocent batch-mate
    /// must land too, and every user field outside the vector envelope must
    /// survive the rename.
    #[tokio::test]
    async fn compact_once_drains_wal_carrying_reserved_provenance_key() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        // Malformed timestamp on the poison row: the repair must fall back to
        // the file's own name — never to the client-controlled value.
        let poison = r#"{"_time":"not-a-date","_ingested":"not-a-date","service":"nginx","message":"poison","_trawl_wal_file":"/etc/passwd","request_id":"deadbeef"}"#;
        let innocent = r#"{"_time":"2026-01-01T00:00:01Z","_ingested":"2026-01-01T00:00:01Z","service":"nginx","message":"innocent","trace_id":"t1"}"#;
        std::fs::create_dir_all(wal_dir.join("prod")).unwrap();
        let wal = wal_dir.join("prod").join("nginx_1730000000000_abcd.ndjson");
        std::fs::write(&wal, [poison, innocent].join("\n")).unwrap();

        let errors = compact_once(
            &wal_dir,
            &data_dir,
            Duration::ZERO,
            false,
            None,
            500,
            "2GB",
            None,
        )
        .await
        .expect("compaction tick must succeed");
        assert_eq!(errors, 0, "a reserved-key row must not count as an error");

        assert!(!wal.exists(), "consumed WAL file must be deleted");
        assert!(
            find_files_by_ext(&wal_dir, "ndjson").is_empty(),
            "WAL directory must drain"
        );

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let mut messages = read_strings(&parquet[0], "message");
        messages.sort();
        assert_eq!(
            messages,
            vec!["innocent".to_owned(), "poison".to_owned()],
            "both events land, including the batch-mate"
        );

        // Non-envelope user fields survive on both rows.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let (request_id, wal_file_val, trace_id): (String, String, String) = conn
            .query_row(
                &format!(
                    "SELECT max(request_id), max({WAL_FILE_COL}), max(trace_id) \
                     FROM read_parquet('{}')",
                    parquet[0].display()
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("non-envelope user fields must survive the drain");
        assert_eq!(request_id, "deadbeef");
        assert_eq!(trace_id, "t1");
        assert_eq!(
            wal_file_val, "/etc/passwd",
            "the client's literal column is preserved as ordinary data"
        );

        // The malformed timestamp was repaired from the file's own name, not
        // from the client-controlled `_trawl_wal_file` value.
        let repaired: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}') \
                     WHERE message = 'poison' \
                       AND \"_time\" = epoch_ms(1730000000000)",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            repaired, 1,
            "the poison row's timestamp must come from its WAL filename"
        );
    }

    /// The synthetic `_trawl_wal_file` provenance column never reaches the
    /// parquet schema.
    #[test]
    fn compact_excludes_wal_provenance_column() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let wal = wal_dir.join("svc_1730000000000_abcd.ndjson");
        std::fs::write(
            &wal,
            b"{\"_time\":\"not-a-date\",\"_ingested\":\"not-a-date\",\"service\":\"svc\",\"message\":\"m\"}\n",
        )
        .unwrap();

        compact_service_blocking(&[wal], &data_dir, "svc", "2GB").unwrap();

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "DESCRIBE SELECT * FROM read_parquet('{}')",
                parquet[0].display()
            ))
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            !cols.iter().any(|c| c == "_trawl_wal_file"),
            "provenance column must be excluded from parquet, got {cols:?}"
        );
    }

    /// A WAL filename that does not conform to `{service}_{millis}_{hex4}`
    /// degrades to the compaction-instant arm — never a NULL timestamp.
    #[test]
    fn compact_nonconforming_filename_falls_back_to_now() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let wal = wal_dir.join("svc_oddname.ndjson");
        std::fs::write(
            &wal,
            b"{\"_time\":\"not-a-date\",\"_ingested\":\"not-a-date\",\"service\":\"svc\",\"message\":\"m\"}\n",
        )
        .unwrap();

        let before = chrono::Utc::now() - chrono::Duration::minutes(5);
        compact_service_blocking(&[wal], &data_dir, "svc", "2GB").unwrap();
        let after = chrono::Utc::now() + chrono::Duration::minutes(5);

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let ts = read_strings(&parquet[0], "strftime(\"_time\", '%Y-%m-%dT%H:%M:%S.%fZ')");
        assert_eq!(ts.len(), 1);
        let got = chrono::DateTime::parse_from_rfc3339(&ts[0])
            .unwrap_or_else(|e| panic!("parquet timestamp {} must parse: {e}", ts[0]))
            .with_timezone(&chrono::Utc);
        assert!(
            got > before && got < after,
            "non-conforming filename must land at compaction time, got {got}"
        );
    }

    /// A recurring path (`{env}/{date}/{HH}/{service}.parquet` is stable
    /// across ticks) corrupting twice must leave two artifacts: `rename`
    /// clobbers an existing destination, so a blind second quarantine would
    /// destroy the first one's bytes.
    #[test]
    fn repeated_quarantine_keeps_every_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("svc.parquet");

        std::fs::write(&bad, b"first corruption").unwrap();
        let first = quarantine_file(&bad, "svc", "rollup_quarantine", QuarantineKind::Parquet)
            .expect("first quarantine must succeed");

        // Same path corrupts again on a later tick.
        std::fs::write(&bad, b"second corruption").unwrap();
        let second = quarantine_file(&bad, "svc", "rollup_quarantine", QuarantineKind::Parquet)
            .expect("second quarantine must succeed");

        assert_eq!(first, tmp.path().join("svc.parquet.corrupt"));
        assert_eq!(second, tmp.path().join("svc.parquet.corrupt.1"));
        assert_ne!(first, second, "the second artifact must take a new name");

        assert_eq!(std::fs::read(&first).unwrap(), b"first corruption");
        assert_eq!(std::fs::read(&second).unwrap(), b"second corruption");
        assert!(!bad.exists(), "both originals were renamed away");

        // Both names stay inert to the scan globs.
        for p in [&first, &second] {
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert!(
                !["parquet", "ndjson", "tmp"].contains(&ext.as_str()),
                "quarantined name {} must not match a scan glob",
                p.display()
            );
        }
    }

    /// A failed reservation is a hard error, not a swallowed log — the
    /// bad file still matches the scan glob and would re-wedge every tick.
    #[test]
    #[cfg(unix)]
    fn quarantine_file_signals_reservation_failure() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("locked");
        std::fs::create_dir(&dir).unwrap();
        let bad = dir.join("svc.parquet");
        std::fs::write(&bad, b"corrupt").unwrap();

        // Read+execute but not write: the reservation's `create_new` open
        // cannot make a new entry in this directory.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::File::create(dir.join(".probe")).is_ok() {
            // Running as root: mode bits are not enforced.
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
            eprintln!(
                "skipped: running as root, cannot make {} unwritable",
                dir.display()
            );
            return;
        }

        let result = quarantine_file(&bad, "svc", "rollup_quarantine", QuarantineKind::Parquet);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.is_err(),
            "quarantine must surface a reservation failure as Err"
        );
        assert!(bad.exists(), "original stays put when quarantine fails");
    }

    /// A failed rename is a hard error too, and it cleans up the reservation,
    /// so a retry re-uses the bare `.corrupt` name instead of stepping over
    /// zero-byte debris.
    #[test]
    fn quarantine_file_signals_rename_failure() {
        let tmp = tempfile::tempdir().unwrap();
        // No source file: the reservation at `<path>.corrupt` succeeds and
        // the rename then fails ENOENT.
        let missing = tmp.path().join("svc.parquet");

        let result = quarantine_file(
            &missing,
            "svc",
            "rollup_quarantine",
            QuarantineKind::Parquet,
        );
        assert!(
            result.is_err(),
            "quarantine must surface a rename failure as Err"
        );
        assert!(
            !tmp.path().join("svc.parquet.corrupt").exists(),
            "a failed quarantine must not leave an empty reservation behind"
        );
        assert!(
            std::fs::read_dir(tmp.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".corrupt")),
            "no .corrupt debris in the directory"
        );
    }

    /// A postgres outage does not fail fast — each write blocks on the pool's
    /// acquire timeout — so the retry must be bounded by wall clock, not by
    /// attempt count, or one batch's bookkeeping outlasts the compaction
    /// interval and the WAL stops draining.
    #[tokio::test(start_paused = true)]
    async fn bookkeeping_budget_bounds_a_hung_write() {
        let start = tokio::time::Instant::now();
        budgeted_bookkeeping("svc", &InFlight::new(), std::future::pending::<()>()).await;
        assert_eq!(
            tokio::time::Instant::now() - start,
            BOOKKEEPING_BUDGET,
            "a bookkeeping write that never returns must cost exactly the budget"
        );
    }

    /// The blips the retry exists for fail in milliseconds, so the budget
    /// must not cost them their attempts.
    #[tokio::test(start_paused = true)]
    async fn budget_leaves_room_for_every_fast_failing_attempt() {
        let calls = std::cell::Cell::new(0_u32);
        let start = tokio::time::Instant::now();
        budgeted_bookkeeping(
            "svc",
            &InFlight::new(),
            retry_bookkeeping(BookkeepingWrite::Observations, "svc", || {
                calls.set(calls.get() + 1);
                std::future::ready(Err(crate::store::StoreError::Unavailable(
                    sqlx::Error::PoolTimedOut,
                )))
            }),
        )
        .await;
        assert_eq!(
            calls.get(),
            BOOKKEEPING_ATTEMPTS,
            "instant failures must still get every attempt"
        );
        assert!(
            tokio::time::Instant::now() - start < BOOKKEEPING_BUDGET,
            "the full backoff ladder must fit inside the budget"
        );
    }

    /// A one-thread runtime with time paused, so a budget window costs no
    /// wall clock and the whole test stays on the recorder's thread.
    fn paused_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("current-thread runtime")
    }

    /// Run `body` against a LOCAL prometheus recorder and return the scrape.
    ///
    /// `metrics::with_local_recorder` installs on the current thread only,
    /// which is why the runtime above is current-thread: increments made
    /// inside `block_on` land in this recorder rather than the process-wide
    /// one another test may have installed.
    fn under_local_recorder(body: impl FnOnce()) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, body);
        handle.render()
    }

    fn timeout_series(write: BookkeepingWrite, count: u64) -> String {
        format!(
            "{}{{write=\"{}\"}} {count}",
            crate::metrics::CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL,
            write.label()
        )
    }

    /// The budget covers both writes, so the counter is only useful if it
    /// says which one was in flight. A hung conflicts write is the first
    /// half.
    #[test]
    fn a_hung_conflicts_write_counts_against_conflicts() {
        let rendered = under_local_recorder(|| {
            paused_runtime().block_on(async {
                let in_flight = InFlight::new();
                let writes = async {
                    in_flight.set(BookkeepingWrite::Conflicts);
                    std::future::pending::<()>().await;
                };
                budgeted_bookkeeping("svc", &in_flight, writes).await;
            });
        });
        assert!(
            rendered.contains(&timeout_series(BookkeepingWrite::Conflicts, 1)),
            "the abandoned write must name itself: {rendered}"
        );
        assert!(
            !rendered.contains("write=\"observations\""),
            "the write that never started must not be blamed: {rendered}"
        );
    }

    /// And the second half: conflicts finishes, observations hangs, and the
    /// cursor has moved.
    #[test]
    fn a_hung_observations_write_counts_against_observations() {
        let rendered = under_local_recorder(|| {
            paused_runtime().block_on(async {
                let in_flight = InFlight::new();
                let writes = async {
                    in_flight.set(BookkeepingWrite::Conflicts);
                    retry_bookkeeping(BookkeepingWrite::Conflicts, "svc", || {
                        std::future::ready(Ok(()))
                    })
                    .await;
                    in_flight.set(BookkeepingWrite::Observations);
                    std::future::pending::<()>().await;
                };
                budgeted_bookkeeping("svc", &in_flight, writes).await;
            });
        });
        assert!(
            rendered.contains(&timeout_series(BookkeepingWrite::Observations, 1)),
            "the cursor must follow the writes: {rendered}"
        );
        assert!(
            !rendered.contains("write=\"conflicts\""),
            "a write that succeeded must not be blamed: {rendered}"
        );
    }

    /// Retry exhaustion is not a timeout. Both writes fail fast, spend
    /// every attempt, and finish well inside the budget: that failure has
    /// its own log line and must leave this counter alone, or an alert on
    /// it fires for a failure whose remedy is different.
    #[test]
    fn fast_retry_exhaustion_counts_no_timeout() {
        let rendered = under_local_recorder(|| {
            paused_runtime().block_on(async {
                let in_flight = InFlight::new();
                let fail = || {
                    std::future::ready(Err(crate::store::StoreError::Unavailable(
                        sqlx::Error::PoolTimedOut,
                    )))
                };
                let writes = async {
                    in_flight.set(BookkeepingWrite::Conflicts);
                    retry_bookkeeping(BookkeepingWrite::Conflicts, "svc", fail).await;
                    in_flight.set(BookkeepingWrite::Observations);
                    retry_bookkeeping(BookkeepingWrite::Observations, "svc", fail).await;
                };
                budgeted_bookkeeping("svc", &in_flight, writes).await;
            });
        });
        assert!(
            !rendered.contains(crate::metrics::CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL),
            "no series at all, on either label: {rendered}"
        );
    }

    // -- hot-buffer pressure (ADR-0043) ---------------------------------------

    use crate::hot_buffer::{Charge, HotBufferConfig};
    use crate::ingest::pipeline::{AdmittedGroup, PipelineWriter, ServiceBatch};
    use crate::ingest::producer::ProducerKind;
    use crate::ingest::wal::WalWriter;

    /// A hot buffer whose pressure threshold is 50 events.
    fn pressure_buffer() -> Arc<HotBuffer> {
        Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 1024 * 1024,
        }))
    }

    /// Reserve and build one `(prod, service)` group of `count` events.
    fn admitted_group(
        pipeline: &PipelineWriter,
        producer: ProducerKind,
        service: &str,
        count: usize,
    ) -> AdmittedGroup {
        let now = chrono::Utc::now().to_rfc3339();
        let mut batch = ServiceBatch::default();
        for id in 0..count {
            batch.push(
                serde_json::json!({
                    "_time": now,
                    "_ingested": now,
                    "env": "prod",
                    "service": service,
                    "message": format!("event {id}"),
                })
                .as_object()
                .unwrap()
                .clone(),
            );
        }
        let reservation = pipeline
            .reserve(producer, batch.charge())
            .expect("the group fits");
        AdmittedGroup {
            key: ("prod".to_owned(), service.to_owned()),
            batch,
            reservation,
        }
    }

    /// Write admitted groups through the real WAL + hot-buffer path.
    async fn write_groups(pipeline: &Arc<PipelineWriter>, groups: Vec<AdmittedGroup>) -> usize {
        let pipeline = Arc::clone(pipeline);
        tokio::task::spawn_blocking(move || pipeline.write(groups))
            .await
            .unwrap()
    }

    struct Loop {
        handle: tokio::task::JoinHandle<()>,
        shutdown: watch::Sender<bool>,
        stats: Arc<CompactionStats>,
    }

    impl Loop {
        fn spawn(tmp: &Path, interval: Duration, daily_rollup: bool, hot: &Arc<HotBuffer>) -> Self {
            let (shutdown, shutdown_rx) = watch::channel(false);
            let stats = Arc::new(CompactionStats::default());
            let handle = spawn_compaction(
                tmp.join("wal"),
                tmp.join("data"),
                interval,
                daily_rollup,
                DEFAULT_CHUNK_SIZE,
                "2GB".to_owned(),
                Some(Arc::clone(hot)),
                Some(Arc::clone(&stats)),
                None,
                None,
                shutdown_rx,
            );
            Self {
                handle,
                shutdown,
                stats,
            }
        }

        async fn stop(self) {
            self.shutdown.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(60), self.handle)
                .await
                .expect("compaction stops on shutdown")
                .unwrap();
        }
    }

    /// Poll `done` every 20 ms until it holds or `limit` passes.
    async fn eventually(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        done()
    }

    /// With a one-hour interval, the normal pass never fires during
    /// the test, and the WAL files are seconds old. Only a pressure pass
    /// (zero WAL age) can publish them and drain the buffer.
    ///
    /// The loop must be waiting before the reservation enters pressure.
    /// Otherwise its startup check can see `Pressure` before the WAL file
    /// exists, run a pass that drains nothing and cool down for the whole
    /// interval, and the test would exercise startup detection instead of
    /// the insert's pressure wake.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pressure_wakes_compaction_and_drains_young_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let hot = pressure_buffer();
        let pipeline = Arc::new(PipelineWriter::new(
            Arc::new(WalWriter::new(tmp.path().join("wal"))),
            Some(Arc::clone(&hot)),
            None,
        ));
        let compaction = Loop::spawn(tmp.path(), Duration::from_secs(3600), false, &hot);
        let stats = Arc::clone(&compaction.stats);
        let waiting = eventually(Duration::from_secs(30), || {
            stats.waits.load(Ordering::Acquire) == 1
        })
        .await;
        assert!(waiting, "the loop reaches its first wait");
        assert_eq!(
            stats.total_runs.load(Ordering::Relaxed),
            0,
            "an Open start runs no pass before it waits"
        );

        let group = admitted_group(&pipeline, ProducerKind::Syslog, "svc", 60);
        assert_eq!(
            hot.admission_state(),
            AdmissionState::Pressure,
            "60 of 100 events charged is at or above one half"
        );
        assert_eq!(write_groups(&pipeline, vec![group]).await, 60);

        let wal_env = tmp.path().join("wal").join("prod");
        let drained = eventually(Duration::from_secs(30), || {
            hot.event_count() == 0 && find_files_by_ext(&wal_env, "ndjson").is_empty()
        })
        .await;
        assert!(
            drained,
            "a pressure pass must publish the young WAL and drain the buffer; \
             {} events still resident, {} runs",
            hot.event_count(),
            compaction.stats.total_runs.load(Ordering::Relaxed),
        );
        assert_eq!(hot.charged(), Charge::ZERO);
        assert_eq!(hot.admission_state(), AdmissionState::Open);
        assert_eq!(hot.drained_batches(), 1);
        // Only the pressure wake can end the first wait: the normal
        // deadline is an hour out and shutdown is not sent. The loop counts
        // the pass before it waits again.
        let waiting_again = eventually(Duration::from_secs(30), || {
            stats.waits.load(Ordering::Acquire) == 2
        })
        .await;
        assert!(waiting_again, "the loop waits again after its pass");
        assert_eq!(
            stats.total_runs.load(Ordering::Relaxed),
            1,
            "one pass, woken from the first wait, drained the young WAL"
        );
        let published = find_files_by_ext(&tmp.path().join("data").join("prod"), "parquet");
        assert_eq!(published.len(), 1, "one hourly parquet: {published:?}");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let rows: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    published[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 60);

        compaction.stop().await;
    }

    /// A reservation above one half starts the loop under pressure, so its
    /// first pass runs before the WAL file exists, drains nothing and cools
    /// down. The insert that follows ends the cooldown: with a one-hour
    /// interval, only that can publish the young WAL within the test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_after_an_empty_pressure_pass_ends_the_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let hot = pressure_buffer();
        let pipeline = Arc::new(PipelineWriter::new(
            Arc::new(WalWriter::new(tmp.path().join("wal"))),
            Some(Arc::clone(&hot)),
            None,
        ));
        let group = admitted_group(&pipeline, ProducerKind::Syslog, "svc", 60);
        assert_eq!(hot.admission_state(), AdmissionState::Pressure);

        let compaction = Loop::spawn(tmp.path(), Duration::from_secs(3600), false, &hot);
        let stats = Arc::clone(&compaction.stats);
        let cooling = eventually(Duration::from_secs(30), || {
            stats.waits.load(Ordering::Acquire) == 1
        })
        .await;
        assert!(cooling, "the loop waits after its first pass");
        assert_eq!(
            stats.total_runs.load(Ordering::Relaxed),
            1,
            "a start under pressure runs one pass before it waits"
        );
        assert_eq!(hot.drained_batches(), 0, "that pass found no WAL");

        assert_eq!(write_groups(&pipeline, vec![group]).await, 60);
        let wal_env = tmp.path().join("wal").join("prod");
        let drained = eventually(Duration::from_secs(30), || {
            hot.event_count() == 0 && find_files_by_ext(&wal_env, "ndjson").is_empty()
        })
        .await;
        assert!(
            drained,
            "the insert must end the cooldown and a pass drain the young WAL; \
             {} events still resident, {} runs",
            hot.event_count(),
            stats.total_runs.load(Ordering::Relaxed),
        );
        assert_eq!(hot.drained_batches(), 1);
        assert_eq!(hot.admission_state(), AdmissionState::Open);

        compaction.stop().await;
    }

    /// Every pass fails to publish (the env data path is a file), so
    /// none drains anything, while `Full` refusals advance the pressure
    /// generation every few milliseconds. The cooldown keeps the loop to
    /// about one pass per interval instead of one per refusal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_pressure_passes_stay_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data").join("prod"), b"not a directory").unwrap();
        let hot = pressure_buffer();
        let pipeline = Arc::new(PipelineWriter::new(
            Arc::new(WalWriter::new(tmp.path().join("wal"))),
            Some(Arc::clone(&hot)),
            None,
        ));
        // Self-telemetry may fill the whole cap; external producers now
        // have no free space at all.
        let group = admitted_group(&pipeline, ProducerKind::Trawld, "svc", 100);
        assert_eq!(write_groups(&pipeline, vec![group]).await, 100);

        let refusing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let refusals = {
            let hot = Arc::clone(&hot);
            let refusing = Arc::clone(&refusing);
            std::thread::spawn(move || {
                let mut count = 0_u64;
                while refusing.load(Ordering::Relaxed) {
                    assert_eq!(
                        hot.ensure_free_space(ProducerKind::Http),
                        Err(crate::hot_buffer::Refusal::Full)
                    );
                    count += 1;
                    std::thread::sleep(Duration::from_millis(5));
                }
                count
            })
        };

        let interval = Duration::from_secs(1);
        let start = Instant::now();
        let compaction = Loop::spawn(tmp.path(), interval, false, &hot);
        tokio::time::sleep(Duration::from_secs(6)).await;
        let runs = compaction.stats.total_runs.load(Ordering::Relaxed);
        let elapsed = start.elapsed();
        refusing.store(false, Ordering::Relaxed);
        let refused = refusals.join().unwrap();
        compaction.stop().await;

        assert_eq!(hot.event_count(), 100, "no pass drained anything");
        assert_eq!(hot.drained_batches(), 0);
        assert_eq!(hot.admission_state(), AdmissionState::Refusing);
        assert!(refused > 100, "refusals kept coming: {refused}");
        let bound = elapsed.as_secs_f64() / interval.as_secs_f64() + 2.0;
        assert!(
            runs >= 2,
            "the loop kept running passes ({runs} in {elapsed:?})"
        );
        #[allow(clippy::cast_precision_loss, reason = "a pass count far below 2^52")]
        let runs_f = runs as f64;
        assert!(
            runs_f <= bound,
            "{runs} passes in {elapsed:?} under {refused} refusals exceeds {bound}"
        );
    }

    #[test]
    fn zero_age_scan_takes_future_mtime_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_wal_file(tmp.path(), "svc", &[OPERATIONAL_ROW]);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();

        assert_eq!(
            scan_wal_files(tmp.path(), Duration::ZERO).unwrap(),
            vec![path],
            "a zero age takes every WAL file, a future mtime included"
        );
        assert!(
            scan_wal_files(tmp.path(), Duration::from_secs(1))
                .unwrap()
                .is_empty(),
            "a positive age still reads a future mtime as too young"
        );
    }

    /// Continuous pressure (a reservation held above one half) makes the
    /// loop run a pressure pass as soon as it starts. That pass must leave
    /// the historical hourly files alone, and the normal deadline must
    /// still roll them up while the buffer stays under pressure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pressure_pass_skips_rollup_and_the_normal_deadline_still_rolls_up() {
        let tmp = tempfile::tempdir().unwrap();
        let env_data = tmp.path().join("data").join("prod");
        let date = "2026-01-15";
        let row = r#"{"_time":"2026-01-15T01:00:00Z","_ingested":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let hourly = [
            write_hourly_parquet(&env_data, date, "01", "nginx", &[row]),
            write_hourly_parquet(&env_data, date, "02", "nginx", &[row]),
        ];
        let daily = env_data.join(date).join("nginx.parquet");

        let hot = pressure_buffer();
        let pipeline = Arc::new(PipelineWriter::new(
            Arc::new(WalWriter::new(tmp.path().join("wal"))),
            Some(Arc::clone(&hot)),
            None,
        ));
        let hold = pipeline
            .reserve(
                ProducerKind::Trawld,
                Charge {
                    events: 60,
                    bytes: 60,
                },
            )
            .unwrap();
        assert_eq!(hot.admission_state(), AdmissionState::Pressure);

        // Written before the loop starts: a loop that starts under pressure
        // runs a pressure pass at once, and one that found nothing would
        // cool down past the normal deadline.
        let group = admitted_group(&pipeline, ProducerKind::Syslog, "svc", 5);
        assert_eq!(write_groups(&pipeline, vec![group]).await, 5);
        let interval = Duration::from_secs(5);
        let start = Instant::now();
        let compaction = Loop::spawn(tmp.path(), interval, true, &hot);

        let pressure_ran = eventually(interval, || hot.drained_batches() == 1).await;
        let observed_at = start.elapsed();
        assert!(pressure_ran, "the insert under pressure woke a pass");
        assert!(
            observed_at < interval,
            "the pressure pass must finish before the normal deadline for this \
             test to separate them (took {observed_at:?})"
        );
        assert!(
            hourly.iter().all(|path| path.exists()) && !daily.exists(),
            "a pressure pass must not roll up"
        );

        let rolled = eventually(interval + Duration::from_secs(30), || daily.exists()).await;
        assert!(rolled, "the normal deadline rolls up under pressure");
        assert!(hourly.iter().all(|path| !path.exists()));
        assert_ne!(
            hot.admission_state(),
            AdmissionState::Open,
            "the pressure lasted through the rollup"
        );
        assert!(start.elapsed() >= interval);

        compaction.stop().await;
        drop(hold);
    }

    /// The cadence alone, on a synthetic clock: a stalled drain under a
    /// pressure wake at every step runs about one pass per interval, and
    /// the normal deadline moves only with normal passes.
    #[test]
    fn cadence_bounds_stalled_pressure_and_keeps_the_normal_deadline() {
        let interval = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut cadence = Cadence::new(t0, interval);
        let mut passes = 0_u32;
        let mut normals = 0_u32;
        for ms in (0..100_000).step_by(100) {
            let now = t0 + Duration::from_millis(ms);
            let kind = cadence.due(now, AdmissionState::Refusing, 0).or_else(|| {
                cadence
                    .accepts_pressure(now, 0)
                    .then_some(PassKind::Pressure)
            });
            if let Some(kind) = kind {
                passes += 1;
                normals += u32::from(kind == PassKind::Normal);
                cadence.finished(kind, now, false, AdmissionState::Refusing, 0);
            }
        }
        assert!(passes <= 100 / 10 + 2, "{passes} passes in 100 s");
        assert!(normals >= 9, "the normal deadline kept firing: {normals}");

        // Progress under pressure reruns at once, without moving the deadline.
        let mut cadence = Cadence::new(t0, interval);
        let now = t0 + Duration::from_secs(1);
        cadence.finished(PassKind::Pressure, now, true, AdmissionState::Pressure, 0);
        assert_eq!(
            cadence.due(now, AdmissionState::Pressure, 0),
            Some(PassKind::Pressure)
        );
        assert_eq!(cadence.next_normal, t0 + interval);
        // No progress: cooldown, and the normal deadline still fires.
        cadence.finished(PassKind::Pressure, now, false, AdmissionState::Pressure, 0);
        assert_eq!(cadence.due(now, AdmissionState::Pressure, 0), None);
        assert!(!cadence.accepts_pressure(now, 0));
        assert_eq!(
            cadence.due(t0 + interval, AdmissionState::Pressure, 0),
            Some(PassKind::Normal)
        );
    }

    /// A pass that drains nothing cools down, but an insert after the pass
    /// started brings WAL a new pass can drain: the cooldown ends at once.
    /// Refusals do not move the insert count, so they stay muted.
    #[test]
    fn cadence_cooldown_ends_on_a_fresh_insert() {
        let interval = Duration::from_secs(3600);
        let t0 = Instant::now();
        let mut cadence = Cadence::new(t0, interval);
        let now = t0 + Duration::from_secs(1);
        // The pass started with 7 inserts and drained nothing.
        cadence.finished(PassKind::Pressure, now, false, AdmissionState::Pressure, 7);
        assert_eq!(cadence.due(now, AdmissionState::Pressure, 7), None);
        assert!(
            !cadence.accepts_pressure(now, 7),
            "no insert: still cooling"
        );

        let later = now + Duration::from_millis(10);
        assert_eq!(
            cadence.due(later, AdmissionState::Pressure, 8),
            Some(PassKind::Pressure),
            "an insert after the pass started ends the cooldown"
        );
        assert!(cadence.accepts_pressure(later, 8));
        assert_eq!(
            cadence.due(later, AdmissionState::Open, 8),
            None,
            "an Open buffer needs no pressure pass"
        );
        assert_eq!(cadence.next_normal, t0 + interval);

        // A normal pass that finds only young WAL cools down the same way.
        let mut cadence = Cadence::new(t0, interval);
        let deadline = t0 + interval;
        cadence.finished(
            PassKind::Normal,
            deadline,
            false,
            AdmissionState::Pressure,
            3,
        );
        assert_eq!(cadence.due(deadline, AdmissionState::Pressure, 3), None);
        assert_eq!(
            cadence.due(deadline, AdmissionState::Pressure, 4),
            Some(PassKind::Pressure)
        );
    }

    #[test]
    fn pass_plans_split_wal_age_from_rollup() {
        let interval = Duration::from_secs(60);
        let plan = |kind, state| PassPlan::new(kind, state, interval, true);
        assert_eq!(
            plan(PassKind::Normal, AdmissionState::Open),
            PassPlan {
                wal_min_age: interval,
                daily_rollup: true
            }
        );
        for state in [AdmissionState::Pressure, AdmissionState::Refusing] {
            assert_eq!(
                plan(PassKind::Normal, state),
                PassPlan {
                    wal_min_age: Duration::ZERO,
                    daily_rollup: true
                }
            );
        }
        for state in [
            AdmissionState::Open,
            AdmissionState::Pressure,
            AdmissionState::Refusing,
        ] {
            assert_eq!(
                plan(PassKind::Pressure, state),
                PassPlan {
                    wal_min_age: Duration::ZERO,
                    daily_rollup: false
                }
            );
        }
    }
}
