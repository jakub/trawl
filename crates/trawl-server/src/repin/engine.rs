// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin job lifecycle (ADR-0011 slice B): validation → claim → scan
//! → (dry-run report | force gate | background build) → additive catch-up
//! → exclusion-guarded cutover → sweep.
//!
//! Every job — dry or real — runs the same scan with the same expressions
//! the rewrite writes; "mandatory dry run" and the force gate are one code
//! path. The gate is then re-asked of the FINISHED shadow under the
//! cutover exclusion, because the scan describes a corpus that ingest and
//! compaction keep changing underneath the build: a file written after
//! the scan can carry values the new pin cannot read, and only the
//! shadow's own accounting can be what the omitted force flag governs.
//!
//! The build stages the new generation in a SIBLING shadow root
//! (`marker.rs` explains why it cannot live inside the data root), the
//! catch-up loop folds in files compaction writes meanwhile (additive by
//! construction: the rollup is paused for the whole job), and the cutover
//! holds BOTH exclusion primitives — the corpus gate against compaction
//! and pool exclusivity against every parquet-reading query lane — across
//! the final increment, the per-env swap and the pin flip. The M1 probes
//! are why this is not optional: a mixed-type corpus does not error, it
//! silently promotes.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use trawl_core::schema::CanonicalType;

use crate::catalog::FieldCatalog;
use crate::catalog::conform::open_bounded_connection;
use crate::error::ServerError;
use crate::pool::ExecutorPool;
use crate::repin::cutover::{swap_envs, sweep_dir};
use crate::repin::gate::RepinCoordinator;
use crate::repin::marker::{
    RepinMarker, RepinPhase, aside_root, remove_marker, shadow_root, write_marker,
};
use crate::repin::plan::{ScanCounts, scan};
use crate::repin::rewrite::{FileSig, ProcessTally, process_file, snapshot_env_files};
use crate::store::{CatalogStore, FieldConflict, RepinJob, RepinJobStatus, RepinStore};

/// Catch-up passes before the job gives up (steadily-shrinking deltas
/// converge in two or three; a delta that refuses to shrink under real
/// ingest volume must fail cleanly rather than loop with the rollup
/// suppressed forever).
const MAX_CATCHUP_PASSES: usize = 8;

/// How long the cutover waits for every query permit before aborting to
/// `blocked` (a wedged query holds a permit; an unbounded wait starves the
/// cutover with retention suppressed).
const CUTOVER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Attempts at the post-swap postgres flip before giving up the process
/// (the marker replay completes it at the next boot).
const FLIP_ATTEMPTS: u32 = 3;

/// Test-only per-file delay in the build pass, so integration tests can
/// ingest through a deliberately slowed rewrite.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_FILE_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What `start` decided.
#[derive(Debug)]
pub enum StartOutcome {
    /// A dry run: the scan report, job already terminal (`succeeded`).
    DryRun(RepinJob),
    /// A lossy repin without force: terminal `refused_needs_force`, plan
    /// attached — the caller answers 409 with it.
    Refused(RepinJob),
    /// The rewrite is running in the background; poll the status surface.
    Started(RepinJob),
}

/// The repin engine — one per ingest-enabled daemon.
#[derive(Debug, Clone)]
pub struct RepinEngine {
    store: RepinStore,
    catalog_store: CatalogStore,
    cache: Arc<FieldCatalog>,
    coordinator: Arc<RepinCoordinator>,
    pool: ExecutorPool,
    data_dir: PathBuf,
    memory_limit: String,
    min_free_disk_bytes: u64,
}

impl RepinEngine {
    /// Assemble the engine.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: RepinStore,
        catalog_store: CatalogStore,
        cache: Arc<FieldCatalog>,
        coordinator: Arc<RepinCoordinator>,
        pool: ExecutorPool,
        data_dir: PathBuf,
        memory_limit: String,
        min_free_disk_bytes: u64,
    ) -> Self {
        Self {
            store,
            catalog_store,
            cache,
            coordinator,
            pool,
            data_dir,
            memory_limit,
            min_free_disk_bytes,
        }
    }

    /// The job store (status surface reads).
    #[must_use]
    pub fn store(&self) -> &RepinStore {
        &self.store
    }

    /// Validate, claim, scan — then report (dry run), refuse (lossy
    /// without force), or start the background rewrite.
    ///
    /// Refusals are side-effect-free on the corpus; every claim leaves a
    /// job row (the dry-run report IS the row).
    #[allow(clippy::too_many_lines)] // one decision ladder, clearer unsplit
    pub async fn start(
        self: &Arc<Self>,
        field: &str,
        to: &str,
        dry_run: bool,
        force: bool,
        requested_by: Option<&str>,
    ) -> Result<StartOutcome, ServerError> {
        let field = field.to_ascii_lowercase();
        let to = CanonicalType::from_duckdb(&to.to_ascii_uppercase()).ok_or_else(|| {
            ServerError::BadRequest(format!(
                "unknown repin target type {to:?} — the candidate ladder is \
                 BIGINT, DOUBLE, TIMESTAMP, BOOLEAN, VARCHAR"
            ))
        })?;
        if trawl_core::schema::ENVELOPE_TYPES
            .iter()
            .any(|(name, _)| *name == field)
        {
            return Err(ServerError::BadRequest(format!(
                "{field:?} is a declared envelope field — its type is part \
                 of the event contract and cannot be repinned"
            )));
        }
        let Some(from) = self.cache.get(&field) else {
            return Err(ServerError::BadRequest(format!(
                "{field:?} is not a pinned field, so there is nothing to repin"
            )));
        };
        if from == to && !force {
            return Err(ServerError::BadRequest(format!(
                "{field:?} is already pinned {}; pass force to run a \
                 resurrection-only rewrite that re-extracts shelved values \
                 from _raw under the same pin",
                to.as_duckdb()
            )));
        }

        // The one-running slot: a second request 409s here.
        let job_id = self
            .store
            .claim(&field, from, to, dry_run, force, requested_by)
            .await?;

        tracing::info!(
            event_type = "repin_start",
            job_id,
            field = %field,
            from = from.as_duckdb(),
            to = to.as_duckdb(),
            dry_run,
            force,
            "repin job claimed; scanning the corpus"
        );

        let counts = match self.run_scan(&field, to).await {
            Ok(counts) => counts,
            Err(e) => {
                self.finish(job_id, RepinJobStatus::Failed, Some(&e)).await;
                return Err(ServerError::Internal(format!("repin scan failed: {e}")));
            }
        };
        if let Err(e) = self
            .store
            .record_plan(
                job_id,
                i64::try_from(counts.files_total).unwrap_or(i64::MAX),
                i64::try_from(counts.rows_carrying).unwrap_or(i64::MAX),
                i64::try_from(counts.projected_nulls).unwrap_or(i64::MAX),
                i64::try_from(counts.resurrectable).unwrap_or(i64::MAX),
                i64::try_from(counts.affected_bytes).unwrap_or(i64::MAX),
            )
            .await
        {
            self.finish(job_id, RepinJobStatus::Failed, Some(&e.to_string()))
                .await;
            return Err(ServerError::Store(e));
        }

        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::CATALOG_REPIN_FILES_TOTAL).set(counts.files_total as f64);

        if dry_run {
            self.finish(job_id, RepinJobStatus::Succeeded, None).await;
            return Ok(StartOutcome::DryRun(self.job(job_id).await?));
        }
        if counts.projected_nulls > 0 && !force {
            self.finish(job_id, RepinJobStatus::RefusedNeedsForce, None)
                .await;
            return Ok(StartOutcome::Refused(self.job(job_id).await?));
        }

        // Free-space pre-flight: the job holds the affected bytes TWICE
        // until the aside sweep, and retention is suppressed for its whole
        // life — it must not create pressure retention cannot relieve.
        let available = fs4::available_space(&self.data_dir)
            .map_err(|e| ServerError::Internal(format!("failed to check free disk space: {e}")))?;
        let needed = counts
            .affected_bytes
            .saturating_add(self.min_free_disk_bytes);
        if available < needed {
            let msg = format!(
                "insufficient free space for the shadow build: {available} \
                 bytes available, {} affected bytes to double-hold plus the \
                 {} byte retention floor (retention is suppressed while a \
                 repin runs)",
                counts.affected_bytes, self.min_free_disk_bytes
            );
            self.finish(job_id, RepinJobStatus::Failed, Some(&msg))
                .await;
            return Err(ServerError::BadRequest(msg));
        }

        let engine = Arc::clone(self);
        tokio::spawn(async move {
            engine.run_job(job_id, field, from, to, force).await;
        });
        Ok(StartOutcome::Started(self.job(job_id).await?))
    }

    async fn run_scan(&self, field: &str, to: CanonicalType) -> Result<ScanCounts, String> {
        let data_dir = self.data_dir.clone();
        let memory_limit = self.memory_limit.clone();
        let field = field.to_owned();
        tokio::task::spawn_blocking(move || scan(&data_dir, &memory_limit, &field, to))
            .await
            .map_err(|e| format!("repin scan task panicked: {e}"))?
    }

    async fn job(&self, job_id: i64) -> Result<RepinJob, ServerError> {
        self.store
            .get(job_id)
            .await
            .map_err(ServerError::Store)?
            .ok_or_else(|| ServerError::Internal("repin job row vanished".into()))
    }

    async fn finish(&self, job_id: i64, status: RepinJobStatus, error: Option<&str>) {
        if let Err(e) = self.store.finish(job_id, status, error).await {
            tracing::error!(
                event_type = "repin_store_error",
                job_id,
                error = %e,
                "failed to record repin job outcome"
            );
        }
        metrics::counter!(
            crate::metrics::CATALOG_REPIN_JOBS_TOTAL,
            "outcome" => status.as_str()
        )
        .increment(1);
    }

    /// The background half: build, catch up, cut over, sweep.
    async fn run_job(
        self: Arc<Self>,
        job_id: i64,
        field: String,
        from: CanonicalType,
        to: CanonicalType,
        force: bool,
    ) {
        let started = std::time::Instant::now();
        let _rollup_pause = self.coordinator.pause_rollup();
        metrics::gauge!(crate::metrics::CATALOG_REPIN_RUNNING).set(1.0);

        let outcome = self.run_job_inner(job_id, &field, from, to, force).await;
        metrics::gauge!(crate::metrics::CATALOG_REPIN_RUNNING).set(0.0);
        metrics::histogram!(crate::metrics::CATALOG_REPIN_DURATION_SECONDS)
            .record(started.elapsed().as_secs_f64());

        match outcome {
            Ok(()) => {
                self.finish(job_id, RepinJobStatus::Succeeded, None).await;
                tracing::info!(
                    event_type = "repin_complete",
                    job_id,
                    field = %field,
                    to = to.as_duckdb(),
                    duration_ms = started.elapsed().as_millis(),
                    "repin job complete: the corpus and the pin now agree"
                );
            }
            Err(JobAbort::Blocked(msg)) => {
                self.abandon_build(job_id, RepinJobStatus::Blocked, &msg)
                    .await;
            }
            Err(JobAbort::RefusedNeedsForce(msg)) => {
                self.abandon_build(job_id, RepinJobStatus::RefusedNeedsForce, &msg)
                    .await;
            }
            Err(JobAbort::Failed(msg)) => {
                self.abandon_build(job_id, RepinJobStatus::Failed, &msg)
                    .await;
            }
        }
    }

    /// Abandon a job whose corpus is still untouched (pre-swap): sweep the
    /// disposable shadow, drop the marker, record the outcome.
    async fn abandon_build(&self, job_id: i64, status: RepinJobStatus, msg: &str) {
        tracing::warn!(
            event_type = "repin_abandoned",
            job_id,
            status = status.as_str(),
            error = %msg,
            "repin job abandoned before any visible change; corpus untouched"
        );
        // The marker is what licenses the next boot to delete this shadow:
        // removing it over a failed sweep strands the staging root, which
        // suppresses retention forever. Keep it and let the replay retry.
        if sweep_dir(&shadow_root(&self.data_dir), "abandoned shadow") {
            if let Err(e) = remove_marker(&self.data_dir) {
                tracing::warn!(event_type = "repin_marker_error", error = %e, "marker removal failed");
            }
        } else {
            tracing::warn!(
                event_type = "repin_recovery_incomplete",
                job_id,
                "the abandoned shadow survived its sweep; keeping the marker \
                 so the next boot retries the cleanup"
            );
        }
        self.finish(job_id, status, Some(msg)).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn run_job_inner(
        &self,
        job_id: i64,
        field: &str,
        from: CanonicalType,
        to: CanonicalType,
        force: bool,
    ) -> Result<(), JobAbort> {
        let marker = RepinMarker {
            job_id,
            field: field.to_owned(),
            from_type: from.as_duckdb().to_owned(),
            to_type: to.as_duckdb().to_owned(),
            phase: RepinPhase::Building,
        };
        write_marker(&self.data_dir, &marker).map_err(JobAbort::Failed)?;

        let shadow = shadow_root(&self.data_dir);
        sweep_dir(&shadow, "stale shadow");
        std::fs::create_dir_all(&shadow)
            .map_err(|e| JobAbort::Failed(format!("failed to create shadow root: {e}")))?;

        // The one-entry-flipped pin map every rewrite conforms against.
        let mut flipped = self.cache.snapshot();
        flipped.insert(field.to_owned(), to);
        let flipped = Arc::new(flipped);

        // Build + additive catch-up: pass 0 processes everything, later
        // passes only the (dev,ino,len,mtime) delta.
        let mut state = BuildState::default();
        let mut converged = false;
        for pass in 0..MAX_CATCHUP_PASSES {
            let changed = self
                .run_pass(field, to, &flipped, &mut state)
                .await
                .map_err(JobAbort::Failed)?;
            self.publish_progress(job_id, &state).await;
            tracing::info!(
                event_type = "repin_pass",
                job_id,
                pass,
                changed,
                files_done = state.files_rewritten(),
                "repin build pass complete"
            );
            if changed == 0 {
                converged = true;
                break;
            }
        }
        if !converged {
            return Err(JobAbort::Failed(format!(
                "catch-up did not converge within {MAX_CATCHUP_PASSES} passes \
                 (ingest volume kept the delta alive); re-run the repin when \
                 ingest is quieter"
            )));
        }

        // The narrow pause: exclusive against compaction batches AND every
        // parquet-reading query lane, bounded.
        let corpus_gate = self.coordinator.cutover_guard().await;
        let pool_guard = match self.pool.exclusive(CUTOVER_DRAIN_TIMEOUT).await {
            Ok(guard) => guard,
            Err(ServerError::Timeout) => {
                drop(corpus_gate);
                return Err(JobAbort::Blocked(format!(
                    "cutover could not drain in-flight queries within \
                     {CUTOVER_DRAIN_TIMEOUT:?}; the corpus is untouched — \
                     retry when the wedged query is gone"
                )));
            }
            Err(e) => {
                drop(corpus_gate);
                return Err(JobAbort::Failed(format!("cutover drain failed: {e}")));
            }
        };

        // Final increment under exclusion: nothing can write or read the
        // corpus now, so this pass is the last word.
        self.run_pass(field, to, &flipped, &mut state)
            .await
            .map_err(JobAbort::Failed)?;
        self.publish_progress(job_id, &state).await;

        // The AUTHORITATIVE loss gate. The pre-build scan only describes
        // the corpus as it stood before the build; ingest and compaction
        // run for the whole job, so a file written after the scan can
        // carry values the new pin cannot read. Deciding on the finished
        // shadow's OWN accounting is the only check the operator's
        // omitted force flag can actually govern — and it is safe to
        // refuse here because nothing visible has moved yet.
        let (_, _, nulled, _) = state.totals();
        if nulled > 0 && !force {
            return Err(JobAbort::RefusedNeedsForce(format!(
                "the completed rewrite nulled {nulled} stored value(s) the \
                 pre-build scan did not project — data ingested after the \
                 scan cannot be read as {}; the cutover is refused and the \
                 corpus stands at its pre-repin generation. Re-run the dry \
                 run for the current plan, then pass force to accept the loss",
                to.as_duckdb()
            )));
        }

        // Point of no return.
        let marker = RepinMarker {
            phase: RepinPhase::Cutover,
            ..marker
        };
        write_marker(&self.data_dir, &marker).map_err(JobAbort::Failed)?;
        if let Err(e) = swap_envs(&self.data_dir, &shadow, &aside_root(&self.data_dir)) {
            // Forward is the only direction past the marker: some envs may
            // already serve the new generation. A process that released
            // its exclusion guards here would serve the silently-promoting
            // mixed corpus the whole design exists to prevent — so die
            // crash-consistent and let the marker replay finish the swap.
            tracing::error!(
                event_type = "repin_cutover_fatal",
                job_id,
                error = %e,
                "per-env swap failed mid-cutover; terminating so the boot \
                 marker replay completes it rather than serving a \
                 mixed-type corpus"
            );
            std::process::exit(1);
        }

        // The pin flip, transactional with the job's completion. Postgres
        // trouble here gets bounded retries, then the same forward-only
        // exit: the corpus already IS the new generation.
        let mut flipped_ok = false;
        for attempt in 1..=FLIP_ATTEMPTS {
            match self.store.finish_cutover(job_id, field, to).await {
                Ok(()) => {
                    flipped_ok = true;
                    break;
                }
                Err(e) if attempt < FLIP_ATTEMPTS => {
                    tracing::warn!(
                        event_type = "repin_flip_retry",
                        job_id,
                        attempt,
                        error = %e,
                        "pin flip failed; retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
                }
                Err(e) => {
                    tracing::error!(
                        event_type = "repin_cutover_fatal",
                        job_id,
                        error = %e,
                        "pin flip unreachable after the swap; terminating so \
                         the boot marker replay completes it"
                    );
                }
            }
        }
        if !flipped_ok {
            std::process::exit(1);
        }
        self.cache.repin(field, to);

        let marker = RepinMarker {
            phase: RepinPhase::Cleanup,
            ..marker
        };
        if let Err(e) = write_marker(&self.data_dir, &marker) {
            tracing::warn!(event_type = "repin_marker_error", error = %e, "cleanup marker write failed (boot replay covers it)");
        }
        drop(pool_guard);
        drop(corpus_gate);

        // Evidence + metrics for what the rewrite actually did.
        self.record_outcome(job_id, field, from, to, &state).await;

        // Sweep: disk-only from here. A failed sweep of EITHER staging root
        // keeps the marker so the boot replay retries it — a leftover root
        // suppresses retention until it is gone.
        let aside_swept = sweep_dir(&aside_root(&self.data_dir), "aside");
        let shadow_swept = sweep_dir(&shadow, "shadow remnant");
        if aside_swept && shadow_swept {
            remove_marker(&self.data_dir).map_err(JobAbort::Failed)?;
        }
        Ok(())
    }

    /// One build/catch-up pass on the blocking pool. Returns how many
    /// source files were (re)processed or retired.
    async fn run_pass(
        &self,
        field: &str,
        to: CanonicalType,
        flipped: &Arc<HashMap<String, CanonicalType>>,
        state: &mut BuildState,
    ) -> Result<usize, String> {
        let data_dir = self.data_dir.clone();
        let shadow = shadow_root(&self.data_dir);
        let memory_limit = self.memory_limit.clone();
        let field = field.to_owned();
        let flipped = Arc::clone(flipped);
        let mut taken = std::mem::take(state);
        let (returned, changed) = tokio::task::spawn_blocking(move || {
            let changed = run_pass_blocking(
                &data_dir,
                &shadow,
                &memory_limit,
                &field,
                to,
                &flipped,
                &mut taken,
            )?;
            Ok::<_, String>((taken, changed))
        })
        .await
        .map_err(|e| format!("repin pass task panicked: {e}"))??;
        *state = returned;
        Ok(changed)
    }

    async fn publish_progress(&self, job_id: i64, state: &BuildState) {
        let (files_done, rows, nulled, resurrected) = state.totals();
        if let Err(e) = self
            .store
            .record_progress(
                job_id,
                i64::try_from(files_done).unwrap_or(i64::MAX),
                i64::try_from(rows).unwrap_or(i64::MAX),
                i64::try_from(nulled).unwrap_or(i64::MAX),
                i64::try_from(resurrected).unwrap_or(i64::MAX),
            )
            .await
        {
            tracing::warn!(event_type = "repin_store_error", job_id, error = %e, "progress write failed");
        }
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::CATALOG_REPIN_FILES_DONE).set(files_done as f64);
    }

    /// Final tallies: counters, and — for a forced lossy repin — the same
    /// `field_conflicts` evidence rows a lossy conform writes (best
    /// effort, like compaction's bookkeeping).
    async fn record_outcome(
        &self,
        job_id: i64,
        field: &str,
        from: CanonicalType,
        to: CanonicalType,
        state: &BuildState,
    ) {
        let (_, _, nulled, resurrected) = state.totals();
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_NULLED_TOTAL).increment(nulled);
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_RESURRECTED_TOTAL)
            .increment(resurrected);
        self.publish_progress(job_id, state).await;

        let conflicts: Vec<FieldConflict> = state
            .nulled_by_service()
            .into_iter()
            .map(|(service, rows_nulled)| FieldConflict {
                field: field.to_owned(),
                service,
                observed_type: from.as_duckdb().to_owned(),
                expected_type: to,
                rows_nulled,
            })
            .collect();
        if !conflicts.is_empty()
            && let Err(e) = self.catalog_store.record_conflicts(&conflicts).await
        {
            tracing::warn!(
                event_type = "catalog_bookkeeping_error",
                error = %e,
                "repin failed to record field_conflicts evidence"
            );
        }
    }
}

/// Why a job stopped short of the swap.
enum JobAbort {
    Failed(String),
    Blocked(String),
    /// The finished shadow nulled values the pre-build scan did not
    /// project (concurrent ingest), and the request carried no force.
    RefusedNeedsForce(String),
}

/// Everything a pass needs to remember between passes: which source file
/// versions the shadow already reflects, and what each contributed.
#[derive(Debug, Default)]
struct BuildState {
    processed: BTreeMap<PathBuf, FileSig>,
    results: BTreeMap<PathBuf, ProcessTally>,
}

impl BuildState {
    fn files_rewritten(&self) -> u64 {
        self.results.values().filter(|t| t.rewritten).count() as u64
    }

    /// `(files_done, rows, nulled, resurrected)` over the CURRENT shadow
    /// contents — a caught-up replacement supersedes its earlier tally, so
    /// totals never double-count a reprocessed file.
    fn totals(&self) -> (u64, u64, u64, u64) {
        let mut rows = 0u64;
        let mut nulled = 0u64;
        let mut resurrected = 0u64;
        for tally in self.results.values().filter(|t| t.rewritten) {
            rows = rows.saturating_add(tally.rows);
            nulled = nulled.saturating_add(tally.nulled);
            resurrected = resurrected.saturating_add(tally.resurrected);
        }
        (self.files_rewritten(), rows, nulled, resurrected)
    }

    fn nulled_by_service(&self) -> HashMap<String, u64> {
        let mut out: HashMap<String, u64> = HashMap::new();
        for tally in self.results.values() {
            if tally.nulled > 0
                && let Some(service) = &tally.service
            {
                *out.entry(service.clone()).or_default() += tally.nulled;
            }
        }
        out
    }
}

/// One pass, blocking: diff the source tree against what the shadow
/// already reflects, (re)process the delta, retire removals.
fn run_pass_blocking(
    data_dir: &Path,
    shadow: &Path,
    memory_limit: &str,
    field: &str,
    to: CanonicalType,
    flipped: &HashMap<String, CanonicalType>,
    state: &mut BuildState,
) -> Result<usize, String> {
    let sources = snapshot_env_files(data_dir)?;

    // Retirements: a source file that vanished (nothing should remove one
    // while retention and the rollup are suppressed, but an operator can)
    // must not resurrect through the swap.
    let gone: Vec<PathBuf> = state
        .processed
        .keys()
        .filter(|rel| !sources.contains_key(*rel))
        .cloned()
        .collect();
    for rel in &gone {
        let _ = std::fs::remove_file(shadow.join(rel));
        state.processed.remove(rel);
        state.results.remove(rel);
    }

    let todo: Vec<(PathBuf, FileSig)> = sources
        .into_iter()
        .filter(|(rel, sig)| state.processed.get(rel) != Some(sig))
        .collect();
    if todo.is_empty() && gone.is_empty() {
        return Ok(0);
    }

    let conn = open_bounded_connection(data_dir, memory_limit)?;
    let changed = todo.len() + gone.len();
    for (rel, sig) in todo {
        #[cfg(any(test, feature = "test-support"))]
        {
            let delay = TEST_FILE_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
        }
        let tally = process_file(&conn, data_dir, shadow, &rel, field, to, flipped)?;
        state.results.insert(rel.clone(), tally);
        state.processed.insert(rel, sig);
    }
    Ok(changed)
}
