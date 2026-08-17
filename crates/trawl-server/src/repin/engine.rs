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
use crate::ingest::compaction::RepinReading;
use crate::pool::ExecutorPool;
use crate::repin::cutover::{
    finish_post_swap_staging, prepare_shadow_root, swap_envs, sweep_pre_swap_staging,
};
use crate::repin::gate::RepinCoordinator;
use crate::repin::marker::{
    RepinMarker, RepinPhase, aside_root, remove_marker, shadow_root, write_marker,
};
use crate::repin::plan::{ScanCounts, ScanTallies, scan};
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

/// Cadence of the detached terminal-write retry after the fast attempts
/// in [`RepinEngine::finish`] are exhausted (a real store outage).
const FINISH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// The terminal-outcome counter, incremented ONLY once the terminal write
/// has actually landed — see [`RepinEngine::finish`].
fn count_outcome(status: RepinJobStatus) {
    metrics::counter!(
        crate::metrics::CATALOG_REPIN_JOBS_TOTAL,
        "outcome" => status.as_str()
    )
    .increment(1);
}

/// Test-only per-file delay in the build pass, so integration tests can
/// ingest through a deliberately slowed rewrite.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_FILE_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only per-file delay in the SCAN (`plan::scan`), so integration
/// tests can walk away from a request while the scan is still running.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_SCAN_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only BARRIER on the build's first pass, arming a
/// happened-before ordering a delay alone cannot give: pass 0 takes its
/// source snapshot, publishes [`TEST_SNAPSHOT_TAKEN`], and then waits for
/// [`TEST_RELEASE_BUILD`].
///
/// A test that ingests while the build merely runs SLOWLY proves nothing
/// about catch-up: pass 0's own snapshot may already have seen the new
/// file, and the assertion would hold even if catch-up passes read the
/// wrong dialect. With the barrier the file provably lands AFTER the
/// snapshot, so only a catch-up pass can carry it into the shadow.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_BARRIER_FIRST_PASS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the build once pass 0 has snapshotted the source tree.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_SNAPSHOT_TAKEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the test to let the barriered pass proceed.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_BUILD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    #[allow(clippy::too_many_arguments)] // one flag bundle per request field
    pub async fn start(
        self: &Arc<Self>,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
        requested_by: Option<&str>,
    ) -> Result<StartOutcome, ServerError> {
        let field = field.to_ascii_lowercase();
        let to = parse_target(to)?;
        let dialect = resolve_dialect(to, dialect)?;
        // A PREDICATE, not the envelope list: the whole `_` prefix is
        // trawl's (`schema::is_contract_typed`), so a contract slot added
        // later is refused the day it exists rather than the day somebody
        // remembers this check.
        if trawl_core::schema::is_contract_typed(&field) {
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
                to.as_catalog()
            )));
        }

        // The one-running slot: a second request 409s here.
        let job_id = self
            .store
            .claim(crate::store::RepinClaim {
                field: &field,
                from_type: from,
                to_type: to,
                dialect,
                dry_run,
                force,
                requested_by,
            })
            .await?;

        tracing::info!(
            event_type = "repin_start",
            job_id,
            field = %field,
            from = from.as_catalog(),
            to = to.as_catalog(),
            dialect = dialect.map(trawl_core::severity::Dialect::token),
            dry_run,
            force,
            "repin job claimed; scanning the corpus"
        );

        // Everything past the claim runs in a DETACHED task, never in the
        // caller's future. The scan is a full-corpus DuckDB pass — minutes
        // on a real archive, well past `trawl-client`'s two-minute
        // timeout and any proxy's — and axum drops the handler future the
        // moment the connection goes away. Cancelled between the claim and
        // the terminal transition, the unique running slot would be
        // stranded until a daemon restart (only boot reconciliation ever
        // clears it), 409ing every later repin and reporting a phantom
        // running job. Detached, the ladder always terminalizes; a caller
        // that walked away merely loses the response and reads the verdict
        // from `/schema/repin/status`.
        // The reading rule, built ONCE here because this is the only place
        // that knows the OLD pin (see `RepinReading`). Everything downstream
        // — scan, rewrite, both force gates — reads it rather than
        // re-deriving a dialect from the target.
        let reading = RepinReading::new(from, to, dialect.unwrap_or_default());
        let engine = Arc::clone(self);
        let decided = tokio::spawn(async move {
            engine
                .decide(job_id, field, from, reading, dry_run, force)
                .await
        });
        match decided.await {
            Ok(outcome) => outcome,
            Err(e) => {
                let msg = format!("repin job task failed: {e}");
                self.finish(job_id, RepinJobStatus::Failed, Some(&msg))
                    .await;
                Err(ServerError::Internal(msg))
            }
        }
    }

    /// The claimed job's decision ladder: scan → report (dry run) → refuse
    /// (lossy without force) → pre-flight → start the background rewrite.
    ///
    /// Runs detached from the request (see `start`), so every exit path
    /// terminalizes the job row itself.
    async fn decide(
        self: Arc<Self>,
        job_id: i64,
        field: String,
        from: CanonicalType,
        reading: RepinReading,
        dry_run: bool,
        force: bool,
    ) -> Result<StartOutcome, ServerError> {
        // Layout pre-flight, ahead of the minutes-long scan: the staging
        // siblings must share the data root's filesystem, because both
        // halves of this engine are renames and hardlinks across that
        // boundary. It gates the dry run too — "can this repin run here"
        // is exactly what a dry run is asked, and answering yes to a
        // layout whose cutover can only exit the process would be a lie.
        // Post-claim, so the refusal is a terminal job row the status
        // surface reports rather than a stranded running slot.
        let data_dir = self.data_dir.clone();
        match on_blocking_pool("staging pre-flight", move || {
            crate::repin::marker::check_staging_filesystem(&data_dir)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                self.finish(job_id, RepinJobStatus::Failed, Some(&msg))
                    .await;
                return Err(ServerError::BadRequest(msg));
            }
            Err(msg) => {
                self.finish(job_id, RepinJobStatus::Failed, Some(&msg))
                    .await;
                return Err(ServerError::Internal(msg));
            }
        }

        let (counts, tallies, samples) = match self.run_scan(&field, reading).await {
            Ok(measured) => measured,
            Err(e) => {
                self.finish(job_id, RepinJobStatus::Failed, Some(&e)).await;
                return Err(ServerError::Internal(format!("repin scan failed: {e}")));
            }
        };
        let liveness = self.field_liveness(&field).await;
        let clamp = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        if let Err(e) = self
            .store
            .record_plan(
                job_id,
                crate::store::RepinPlan {
                    files_total: clamp(counts.files_total),
                    rows_carrying: clamp(counts.rows_carrying),
                    projected_nulls: clamp(counts.projected_nulls),
                    resurrectable: clamp(counts.resurrectable),
                    affected_bytes: clamp(counts.affected_bytes),
                    ambiguous_numerals: clamp(counts.ambiguous_numerals),
                    unmapped_samples: samples,
                    field_last_seen: liveness.as_ref().map(|(at, _)| *at),
                    field_last_service: liveness.map(|(_, service)| service),
                },
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
        // The SCAN gate. Same decision as the finished-shadow gate below,
        // one function — the plan rides back as the 409 body, and the reason
        // rides with it: a refusal over ambiguity with zero projected nulls
        // is otherwise a plan an operator cannot read the verdict off.
        if let Some(reason) = force_refusal(
            reading.written.pin,
            Some(reading.written.raw),
            counts.projected_nulls,
            counts.ambiguous_numerals,
            force,
        ) {
            self.finish(job_id, RepinJobStatus::RefusedNeedsForce, Some(&reason))
                .await;
            return Ok(StartOutcome::Refused(self.job(job_id).await?));
        }

        // Free-space pre-flight: the job holds the affected bytes TWICE
        // until the aside sweep, and retention is suppressed for its whole
        // life — it must not create pressure retention cannot relieve.
        // A post-claim failure must terminalize the claimed job: the
        // running slot is unique, so an early return would 409 every
        // later repin until a restart reconciles the orphan.
        let available = match fs4::available_space(&self.data_dir) {
            Ok(available) => available,
            Err(e) => {
                let msg = format!("failed to check free disk space: {e}");
                self.finish(job_id, RepinJobStatus::Failed, Some(&msg))
                    .await;
                return Err(ServerError::Internal(msg));
            }
        };
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

        let engine = Arc::clone(&self);
        let tallies = Arc::new(tallies);
        tokio::spawn(async move {
            engine
                .run_job(job_id, field, from, reading, force, tallies)
                .await;
        });
        Ok(StartOutcome::Started(self.job(job_id).await?))
    }

    async fn run_scan(
        &self,
        field: &str,
        reading: RepinReading,
    ) -> Result<(ScanCounts, ScanTallies, Vec<String>), String> {
        let data_dir = self.data_dir.clone();
        let memory_limit = self.memory_limit.clone();
        let field = field.to_owned();
        tokio::task::spawn_blocking(move || scan(&data_dir, &memory_limit, &field, reading))
            .await
            .map_err(|e| format!("repin scan task panicked: {e}"))?
    }

    /// Is anything still WRITING this field? The newest observation inside
    /// [`crate::repin::LIVENESS_WINDOW`], with one service behind it.
    ///
    /// One indexed row (`field_services (field, …)`, ordered `last_seen`
    /// DESC — the read the schema surface already pages through), so no new
    /// store method and no scan. Best-effort by design: liveness is
    /// ADVISORY, and a repin must not fail because an observation table was
    /// briefly unreadable — a warning that cannot be produced is a missing
    /// warning, not a missing repin.
    async fn field_liveness(&self, field: &str) -> Option<(chrono::DateTime<chrono::Utc>, String)> {
        let (rows, _) = self
            .catalog_store
            .field_services(field, None, 1)
            .await
            .inspect_err(|e| {
                tracing::warn!(
                    event_type = "catalog_bookkeeping_error",
                    field = %field,
                    error = %e,
                    "could not read repin subject liveness; the report omits it"
                );
            })
            .ok()?;
        let newest = rows.into_iter().next()?;
        let cutoff = chrono::Utc::now()
            - chrono::Duration::from_std(crate::repin::LIVENESS_WINDOW).unwrap_or_default();
        (newest.last_seen >= cutoff).then_some((newest.last_seen, newest.service))
    }

    async fn job(&self, job_id: i64) -> Result<RepinJob, ServerError> {
        self.store
            .get(job_id)
            .await
            .map_err(ServerError::Store)?
            .ok_or_else(|| ServerError::Internal("repin job row vanished".into()))
    }

    /// Terminalize a claimed job row, riding out postgres trouble.
    ///
    /// The `repin_jobs_one_running` slot is unique, so a `running` row
    /// whose terminal write is lost would 409 every later repin until a
    /// restart's boot reconciliation — a wedge the daemon must not carry
    /// while it lives. A blip gets bounded fast retries (the catalog
    /// bookkeeping cadence); a real outage hands the write to a detached
    /// slow loop that retries until it lands. While the store is down no
    /// new claim can succeed either, so the slot is honestly busy rather
    /// than wedged, and it frees within one tick of the store returning.
    /// A daemon that dies with the loop still trying falls back to boot
    /// reconciliation, as before. The outcome counter increments only
    /// when the write lands: a row still `running` must not be metered
    /// as a terminal outcome.
    async fn finish(&self, job_id: i64, status: RepinJobStatus, error: Option<&str>) {
        const FAST_ATTEMPTS: u32 = 3;
        for attempt in 1..=FAST_ATTEMPTS {
            match self.store.finish(job_id, status, error).await {
                Ok(()) => {
                    count_outcome(status);
                    return;
                }
                Err(e) if attempt < FAST_ATTEMPTS => {
                    tracing::warn!(
                        event_type = "repin_store_retry",
                        job_id,
                        attempt,
                        error = %e,
                        "failed to record repin job outcome; retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(100 << (attempt - 1))).await;
                }
                Err(e) => {
                    tracing::error!(
                        event_type = "repin_store_error",
                        job_id,
                        error = %e,
                        "failed to record repin job outcome; handing the \
                         terminal write to a background retry so the \
                         one-running slot cannot stay wedged"
                    );
                }
            }
        }
        let store = self.store.clone();
        let error = error.map(str::to_owned);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(FINISH_RETRY_INTERVAL).await;
                match store.finish(job_id, status, error.as_deref()).await {
                    Ok(()) => {
                        count_outcome(status);
                        tracing::info!(
                            event_type = "repin_store_recovered",
                            job_id,
                            "repin job outcome recorded after store recovery; \
                             the one-running slot is free again"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            event_type = "repin_store_retry",
                            job_id,
                            error = %e,
                            "repin job outcome write still failing; will retry"
                        );
                    }
                }
            }
        });
    }

    /// The background half: build, catch up, cut over, sweep.
    ///
    /// `scanned` carries the mandatory pre-build scan's per-file readings
    /// so the build does not immediately re-measure a corpus nothing has
    /// touched (see [`ScanTallies`]).
    async fn run_job(
        self: Arc<Self>,
        job_id: i64,
        field: String,
        from: CanonicalType,
        reading: RepinReading,
        force: bool,
        scanned: Arc<ScanTallies>,
    ) {
        let started = std::time::Instant::now();
        let _rollup_pause = self.coordinator.pause_rollup();
        metrics::gauge!(crate::metrics::CATALOG_REPIN_RUNNING).set(1.0);

        let to = reading.written.pin;
        let outcome = self
            .run_job_inner(job_id, &field, from, reading, force, &scanned)
            .await;
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
                    to = to.as_catalog(),
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
    /// disposable shadow and any leftover aside, drop the marker, record
    /// the outcome.
    async fn abandon_build(&self, job_id: i64, status: RepinJobStatus, msg: &str) {
        tracing::warn!(
            event_type = "repin_abandoned",
            job_id,
            status = status.as_str(),
            error = %msg,
            "repin job abandoned before any visible change; corpus untouched"
        );
        // The marker is what licenses the next boot to delete the staging
        // roots — BOTH of them, since a leftover aside from an earlier
        // job's failed sweep outlives its own marker (see
        // `sweep_pre_swap_staging`). Removing the marker over a failed
        // sweep strands whichever root survived, which suppresses
        // retention forever. Keep it and let the replay retry.
        let data_dir = self.data_dir.clone();
        let swept = on_blocking_pool("abandon sweep", move || sweep_pre_swap_staging(&data_dir))
            .await
            .unwrap_or_else(|msg| {
                // A panicked sweep is a sweep that did not finish: keep the
                // marker, exactly as a failed one does.
                tracing::warn!(event_type = "repin_sweep_failed", error = %msg, "abandon sweep task failed");
                false
            });
        if swept {
            if let Err(e) = remove_marker(&self.data_dir) {
                tracing::warn!(event_type = "repin_marker_error", error = %e, "marker removal failed");
            }
        } else {
            tracing::warn!(
                event_type = "repin_recovery_incomplete",
                job_id,
                "a repin staging root survived the abandoned job's sweep; \
                 keeping the marker so the next boot retries the cleanup"
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
        reading: RepinReading,
        force: bool,
        scanned: &Arc<ScanTallies>,
    ) -> Result<(), JobAbort> {
        let to = reading.written.pin;
        let marker = RepinMarker {
            job_id,
            field: field.to_owned(),
            from_type: from.as_catalog().to_owned(),
            to_type: to.as_catalog().to_owned(),
            phase: RepinPhase::Building,
        };
        write_marker(&self.data_dir, &marker).map_err(JobAbort::Failed)?;

        // A shadow root that outlived an earlier job's sweep is NOT a
        // disk-only problem here: building into it would publish that
        // job's files — and rows retention has since deleted — into the
        // live corpus at the swap. Refuse rather than layer.
        let data_dir = self.data_dir.clone();
        let shadow = on_blocking_pool("shadow prepare", move || prepare_shadow_root(&data_dir))
            .await
            .map_err(JobAbort::Failed)?
            .map_err(JobAbort::Failed)?;

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
                .run_pass(field, reading, &flipped, scanned, &mut state)
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

        // Test-only widening of the pause, so a test can act inside the
        // one window that stops WAL draining.
        #[cfg(any(test, feature = "test-support"))]
        self.coordinator.hold_cutover_for_tests().await;

        // Final increment under exclusion: nothing can write or read the
        // corpus now, so this pass is the last word.
        self.run_pass(field, reading, &flipped, scanned, &mut state)
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
        let totals = state.totals();
        if let Some(reason) = force_refusal(
            reading.written.pin,
            Some(reading.written.raw),
            totals.nulled,
            totals.ambiguous,
            force,
        ) {
            return Err(JobAbort::RefusedNeedsForce(format!(
                "the completed rewrite is not what the pre-build scan \
                 projected — data ingested after the scan carries values the \
                 plan never saw: {reason}. The cutover is refused and the \
                 corpus stands at its pre-repin generation; re-run the dry \
                 run for the current plan, then pass force to accept it"
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

        // Sweep: disk-only from here, and infallible by type. A failed
        // sweep of EITHER staging root keeps the marker so the boot replay
        // retries it — a leftover root suppresses retention until it is
        // gone — but nothing past the point of no return may be reported
        // as a failure of the JOB: the corpus is the new generation and
        // the pin is flipped, so an undeletable marker is leftover disk,
        // not a repin that "left the corpus untouched".
        let data_dir = self.data_dir.clone();
        if let Err(msg) = on_blocking_pool("post-swap sweep", move || {
            finish_post_swap_staging(&data_dir);
        })
        .await
        {
            tracing::warn!(event_type = "repin_sweep_failed", error = %msg, "post-swap sweep task failed");
        }
        Ok(())
    }

    /// One build/catch-up pass on the blocking pool. Returns how many
    /// source files were (re)processed or retired.
    async fn run_pass(
        &self,
        field: &str,
        reading: RepinReading,
        flipped: &Arc<HashMap<String, CanonicalType>>,
        scanned: &Arc<ScanTallies>,
        state: &mut BuildState,
    ) -> Result<usize, String> {
        let data_dir = self.data_dir.clone();
        let shadow = shadow_root(&self.data_dir);
        let memory_limit = self.memory_limit.clone();
        let field = field.to_owned();
        let flipped = Arc::clone(flipped);
        let scanned = Arc::clone(scanned);
        let mut taken = std::mem::take(state);
        let (returned, changed) = tokio::task::spawn_blocking(move || {
            let changed = run_pass_blocking(
                &data_dir,
                &shadow,
                &memory_limit,
                &field,
                reading,
                &flipped,
                &scanned,
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
        let totals = state.totals();
        let clamp = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        if let Err(e) = self
            .store
            .record_progress(
                job_id,
                clamp(totals.files_done),
                clamp(totals.rows),
                clamp(totals.nulled),
                clamp(totals.resurrected),
                clamp(totals.ambiguous),
            )
            .await
        {
            tracing::warn!(event_type = "repin_store_error", job_id, error = %e, "progress write failed");
        }
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::CATALOG_REPIN_FILES_DONE).set(totals.files_done as f64);
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
        let totals = state.totals();
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_NULLED_TOTAL).increment(totals.nulled);
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_RESURRECTED_TOTAL)
            .increment(totals.resurrected);
        self.publish_progress(job_id, state).await;

        let conflicts: Vec<FieldConflict> = state
            .nulled_by_service()
            .into_iter()
            .map(|(service, rows_nulled)| FieldConflict {
                field: field.to_owned(),
                service,
                // The CATALOG spelling: evidence a repin authors must name
                // the pin the values were stored under, and `as_duckdb` is
                // not injective — a SEVERITY source would indict itself as
                // BIGINT, a pin the field never had.
                observed_type: from.as_catalog().to_owned(),
                expected_type: to,
                rows_nulled,
                // The rewrite counts what it nulled per service; it never
                // materialises the values (a rewrite that carried them back
                // would be a second full pass over the corpus for evidence
                // the operator asked for this repin in spite of).
                samples: Vec::new(),
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

/// Run one whole-corpus filesystem step on the blocking pool.
///
/// These steps are proportional to the SIZE OF THE ARCHIVE, not to the
/// repin: the staging pre-flight lstats every file under every env dir,
/// and each sweep is a `remove_dir_all` over a whole corpus generation —
/// tens of seconds to minutes on a multi-hundred-thousand-file archive.
/// Called inline from an async fn, each parks a tokio worker thread for
/// that whole time and degrades unrelated request handling, which is the
/// same reason the scan and every build pass already go through
/// `spawn_blocking`. A panicked task surfaces as an `Err` here so the
/// caller decides, rather than vanishing into a dropped `JoinHandle`.
async fn on_blocking_pool<T, F>(what: &'static str, f: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("repin {what} task panicked: {e}"))
}

/// Why a job stopped short of the swap.
enum JobAbort {
    Failed(String),
    Blocked(String),
    /// The finished shadow nulled values the pre-build scan did not
    /// project (concurrent ingest), and the request carried no force.
    RefusedNeedsForce(String),
}

/// What the shadow generation holds so far — the numbers the progress
/// record, the metrics and the cutover's force gate all read.
#[derive(Debug, Clone, Copy, Default)]
struct BuildTotals {
    /// Affected files actually rewritten.
    files_done: u64,
    /// Rows written through the rewrite.
    rows: u64,
    /// Stored values the new pin could not keep.
    nulled: u64,
    /// Values recovered from `_raw`.
    resurrected: u64,
    /// Rows the new pin reads differently in each dialect.
    ambiguous: u64,
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

    /// The shadow's tallies over its CURRENT contents — a caught-up
    /// replacement supersedes its earlier tally, so totals never
    /// double-count a reprocessed file.
    fn totals(&self) -> BuildTotals {
        let mut totals = BuildTotals {
            files_done: self.files_rewritten(),
            ..BuildTotals::default()
        };
        for tally in self.results.values().filter(|t| t.rewritten) {
            totals.rows = totals.rows.saturating_add(tally.rows);
            totals.nulled = totals.nulled.saturating_add(tally.nulled);
            totals.resurrected = totals.resurrected.saturating_add(tally.resurrected);
            totals.ambiguous = totals.ambiguous.saturating_add(tally.ambiguous);
        }
        totals
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
#[allow(clippy::too_many_arguments)]
fn run_pass_blocking(
    data_dir: &Path,
    shadow: &Path,
    memory_limit: &str,
    field: &str,
    reading: RepinReading,
    flipped: &HashMap<String, CanonicalType>,
    scanned: &ScanTallies,
    state: &mut BuildState,
) -> Result<usize, String> {
    let sources = snapshot_env_files(data_dir)?;

    // Test-only happened-before edge: this snapshot is taken, and nothing
    // moves until the test has written the file it wants a CATCH-UP pass to
    // carry. Bounded so a mis-driven test fails rather than hangs.
    #[cfg(any(test, feature = "test-support"))]
    if TEST_BARRIER_FIRST_PASS.swap(false, std::sync::atomic::Ordering::SeqCst) {
        TEST_SNAPSHOT_TAKEN.store(true, std::sync::atomic::Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !TEST_RELEASE_BUILD.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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
        // The scan measured this very file moments ago; reuse its reading
        // when the source is still byte-identical, so the build does not
        // pay a second full aggregate scan per file before it has
        // rewritten anything. A signature the scan never saw — or one it
        // saw at other bytes — recounts.
        let precounted = scanned
            .get(&rel)
            .and_then(|(scanned_sig, effect)| (*scanned_sig == sig).then_some(*effect));
        let tally = process_file(
            &conn, data_dir, shadow, &rel, field, reading, flipped, precounted,
        )?;
        state.results.insert(rel.clone(), tally);
        state.processed.insert(rel, sig);
    }
    Ok(changed)
}

/// Resolve a requested repin target to a canonical type.
///
/// The parse is through `CanonicalType::from_catalog` — the CATALOG
/// spelling, the injective one — so `SEVERITY` is admitted (issue #79).
/// It is a target an operator can only reach by naming it: inference still
/// cannot mint it (`DESCRIBE` never says SEVERITY, so
/// `normalize_duckdb_type` can never yield it), and the seed remains the
/// only other installer. What used to make the physical door the gate —
/// "severity is not an operator decision" — is exactly what this slice
/// reverses.
/// The force decision, asked by all THREE askers — the pre-build scan
/// gate, the finished-shadow gate, and the wire (`requires_force` on every
/// job row a client reads) — so a dry run can never report a verdict the
/// executing request would not reach. `Some(reason)` means the job needs an
/// explicit force flag it does not have; the reason is the row's `error`
/// text, and a caller may wrap it in its own context, which is the only
/// thing the askers say differently.
///
/// Takes the PIN and the asserted dialect rather than a `RepinTarget`,
/// because the wire asker reads a stored job row: `RepinTarget` is the
/// engine's to construct (it alone knows the old pin), and re-deriving one
/// from a row would be exactly the second opinion this function exists to
/// prevent.
///
/// Two reasons, both about a value the operator has not knowingly accepted:
///
/// 1. LOSS — values the new pin cannot read at all. Unchanged behaviour;
/// 2. AMBIGUITY — numerals that read as a different severity in each
///    dialect (the 1-7 overlap). Only under the `OTel` reading, because
///    asserting syslog IS the statement about provenance the ambiguity is
///    waiting for (issue #79, AC4). An explicit `dialect=otel` does NOT
///    suppress it: the refusal is about the values, not about how the
///    request was spelled, and force is the one escape.
///
/// The COUNT is taken whatever the dialect — the report says what is there
/// — and only the gate is conditional.
pub(crate) fn force_refusal(
    pin: CanonicalType,
    dialect: Option<trawl_core::severity::Dialect>,
    nulled: u64,
    ambiguous: u64,
    force: bool,
) -> Option<String> {
    if force {
        return None;
    }
    if nulled > 0 {
        return Some(format!(
            "{nulled} stored value(s) cannot be read as {} and would be \
             nulled (the originals stay findable in _raw)",
            pin.as_catalog()
        ));
    }
    let ambiguous_gate = pin == CanonicalType::Severity
        && dialect != Some(trawl_core::severity::Dialect::Syslog)
        && ambiguous > 0;
    if ambiguous_gate {
        return Some(format!(
            "{ambiguous} row(s) carry a numeral 1-7, which the OTel ladder \
             and syslog PRI read as DIFFERENT severities (3 is trace3 to \
             OTel and err to syslog) — no value-shape rule can tell them \
             apart, so trawl will not guess. Re-run with dialect=syslog if \
             the sender speaks syslog PRI, or force to accept the OTel \
             reading"
        ));
    }
    None
}

/// Resolve the asserted numeral dialect for a target (issue #79).
///
/// A `SEVERITY` target always ends up with one — `otel` when the request
/// says nothing, because that is what every other lane reads and a repin
/// that changed the ladder by omission would be the silent mistranslation
/// this whole feature exists to make deliberate.
///
/// Any OTHER target with a dialect is a 400, not an ignored field: the
/// dialect only reaches the SEVERITY rung, so accepting it elsewhere would
/// tell an operator their assertion was honoured when nothing read it.
fn resolve_dialect(
    to: CanonicalType,
    dialect: Option<&str>,
) -> Result<Option<trawl_core::severity::Dialect>, ServerError> {
    use trawl_core::severity::{DIALECT_TOKENS, Dialect};

    match (to, dialect) {
        (CanonicalType::Severity, None) => Ok(Some(Dialect::Otel)),
        (CanonicalType::Severity, Some(token)) => {
            Dialect::from_token(token).map(Some).ok_or_else(|| {
                ServerError::BadRequest(format!(
                    "unknown severity dialect {token:?} — the vocabulary is {}",
                    DIALECT_TOKENS.join(", ")
                ))
            })
        }
        (_, None) => Ok(None),
        (_, Some(token)) => Err(ServerError::BadRequest(format!(
            "dialect {token:?} is only meaningful for a SEVERITY target — \
             the dialect reads NUMERALS onto the severity ladder, and {} \
             has no ladder to read them onto",
            to.as_catalog()
        ))),
    }
}

fn parse_target(to: &str) -> Result<CanonicalType, ServerError> {
    CanonicalType::from_catalog(&to.to_ascii_uppercase()).ok_or_else(|| {
        ServerError::BadRequest(format!(
            "unknown repin target type {to:?} — the candidate ladder is \
             BIGINT, DOUBLE, TIMESTAMP, BOOLEAN, VARCHAR, SEVERITY"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The target vocabulary is the CATALOG's — the injective spelling —
    /// so `SEVERITY` is a target an operator can name (issue #79) and
    /// stays DISTINCT from the `BIGINT` it shares a physical type with:
    /// the two mean different things to every comparison rule, and a parse
    /// that collapsed them would repin a field to a pin nobody asked for.
    #[test]
    fn repin_targets_are_the_catalog_vocabulary_severity_included() {
        for (spelling, expected) in [
            ("bigint", CanonicalType::BigInt),
            ("VARCHAR", CanonicalType::Varchar),
            ("TimeStamp", CanonicalType::Timestamp),
            ("double", CanonicalType::Double),
            ("boolean", CanonicalType::Boolean),
            ("severity", CanonicalType::Severity),
            ("SEVERITY", CanonicalType::Severity),
            ("Severity", CanonicalType::Severity),
        ] {
            assert_eq!(parse_target(spelling).unwrap(), expected, "{spelling}");
        }
        assert_ne!(
            parse_target("severity").unwrap(),
            parse_target("bigint").unwrap(),
            "SEVERITY is a semantic pin over BIGINT, not a synonym for it"
        );
        // Still a closed vocabulary: `JSON` is an inference artifact, never
        // a pin, and an empty target is a client bug.
        for rejected in ["json", "", "sev", "otel", "hugeint"] {
            let err = parse_target(rejected).expect_err("must refuse");
            assert!(
                matches!(err, ServerError::BadRequest(ref m) if m.contains("unknown repin target type")),
                "{rejected}: {err:?}"
            );
        }
    }

    /// The force gate is ONE decision asked at two moments (the pre-build
    /// scan and the finished shadow), so the two can only refuse for the
    /// same reasons: loss always, ambiguity only under the `OTel` reading —
    /// asserting syslog IS the provenance statement the ambiguity waits for
    /// — and `--force` is the single escape from either.
    #[test]
    fn the_force_gate_refuses_loss_always_and_ambiguity_under_otel_only() {
        use trawl_core::severity::Dialect;

        const SEVERITY: CanonicalType = CanonicalType::Severity;
        const VARCHAR: CanonicalType = CanonicalType::Varchar;

        // Loss: any target, any dialect, cleared only by force.
        assert!(
            force_refusal(VARCHAR, None, 3, 0, false)
                .is_some_and(|m| m.contains("cannot be read as VARCHAR")),
        );
        assert_eq!(force_refusal(VARCHAR, None, 3, 0, true), None);
        assert!(force_refusal(SEVERITY, Some(Dialect::Syslog), 3, 0, false).is_some());

        // Ambiguity: refused under OTel, silent under an explicit syslog
        // assertion, and cleared by force either way.
        let refusal = force_refusal(SEVERITY, Some(Dialect::Otel), 0, 2, false)
            .expect("an OTel repin over the 1-7 overlap must refuse");
        assert!(refusal.contains("2 row(s)"), "{refusal}");
        assert!(refusal.contains("dialect=syslog"), "{refusal}");
        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Syslog), 0, 2, false),
            None,
            "asserting syslog IS the answer to the ambiguity"
        );
        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Otel), 0, 2, true),
            None
        );
        // A row with no recorded dialect at all (a legacy job, or one whose
        // scope CHECK predates this slice) reads as the OTel default — the
        // gate must not go silent because a column is NULL.
        assert!(force_refusal(SEVERITY, None, 0, 2, false).is_some());

        // Nothing to refuse, and a non-severity target has no ambiguity
        // notion at all (its count is a constant zero upstream, but the gate
        // must not fire even if one were handed in).
        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Otel), 0, 0, false),
            None
        );
        assert_eq!(force_refusal(VARCHAR, None, 0, 5, false), None);

        // Loss is reported FIRST: it is the older and larger hazard, and a
        // plan carrying both needs one force flag, not a ladder of them.
        let both = force_refusal(SEVERITY, Some(Dialect::Otel), 1, 1, false).unwrap();
        assert!(both.contains("cannot be read as SEVERITY"), "{both}");
    }

    /// The dialect is a SEVERITY-only assertion: a severity target defaults
    /// to `otel` (what every other lane reads), an unknown token names the
    /// vocabulary, and a dialect on any other target is refused rather than
    /// ignored — an ignored assertion is one an operator believes was
    /// honoured.
    #[test]
    fn the_dialect_is_resolved_for_severity_targets_only() {
        use trawl_core::severity::Dialect;

        assert_eq!(
            resolve_dialect(CanonicalType::Severity, None).unwrap(),
            Some(Dialect::Otel),
            "an omitted dialect is the OTel reading, never an absent one"
        );
        for (token, want) in [
            ("otel", Dialect::Otel),
            ("SYSLOG", Dialect::Syslog),
            ("Syslog", Dialect::Syslog),
        ] {
            assert_eq!(
                resolve_dialect(CanonicalType::Severity, Some(token)).unwrap(),
                Some(want),
                "{token}"
            );
        }
        let err = resolve_dialect(CanonicalType::Severity, Some("rfc5424")).expect_err("refuse");
        assert!(
            matches!(err, ServerError::BadRequest(ref m)
                if m.contains("unknown severity dialect") && m.contains("syslog")),
            "{err:?}"
        );

        for pin in [
            CanonicalType::Varchar,
            CanonicalType::BigInt,
            CanonicalType::Double,
            CanonicalType::Timestamp,
            CanonicalType::Boolean,
        ] {
            assert_eq!(resolve_dialect(pin, None).unwrap(), None, "{pin:?}");
            let err = resolve_dialect(pin, Some("syslog")).expect_err("refuse");
            assert!(
                matches!(err, ServerError::BadRequest(ref m)
                    if m.contains("only meaningful for a SEVERITY target")),
                "{pin:?}: {err:?}"
            );
        }
    }

    /// The FIELD side of admission: `start` gates on
    /// `schema::is_contract_typed`, so every contract slot — the sealed `_`
    /// namespace whole, present and future, plus the four sender-asserted
    /// bare names — is refused before a job is ever claimed, while ordinary
    /// sender vocabulary (including the names that used to be envelope
    /// slots) is repinnable.
    #[test]
    fn contract_typed_fields_are_not_repin_subjects() {
        for refused in [
            trawl_core::schema::SEVERITY,
            trawl_core::schema::TIME,
            trawl_core::schema::RAW,
            trawl_core::schema::ENV,
            trawl_core::schema::SERVICE,
            trawl_core::schema::HOST,
            trawl_core::schema::MESSAGE,
            "_x",
            "_not_a_slot_yet",
        ] {
            assert!(
                trawl_core::schema::is_contract_typed(refused),
                "{refused} must be refused by the admission gate"
            );
        }
        for admitted in ["level", "severity", "timestamp", "status", "duration"] {
            assert!(
                !trawl_core::schema::is_contract_typed(admitted),
                "{admitted} is sender vocabulary and repinnable"
            );
        }
    }
}
