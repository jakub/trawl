// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin job lifecycle: validation → permit → staging check → claim →
//! scan → (dry-run report | force gate | background build) → additive
//! catch-up → exclusion-guarded cutover → sweep.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use trawl_core::schema::CanonicalType;

use crate::catalog::FieldCatalog;
use crate::catalog::conform::open_bounded_connection;
use crate::deadline::Deadline;
use crate::error::{CauseKind, ServerError};
use crate::ingest::compaction::RepinReading;
use crate::pool::ExecutorPool;
use crate::publication::CorpusUnsettled;
use crate::repin::cancel::{
    CancelActor, CancelHandle, CancelRegistry, CancelVerdict, Commit, PassStop, STAGE_BUILD,
    STAGE_FINAL_GATE, STAGE_SCAN, Settlement, audit_cancel_refused, audit_cancel_requested,
    audit_cancelled,
};
use crate::repin::ceiling::{self, ForceTerms, RequestedCeilings};
use crate::repin::cutover::{
    finish_post_swap_staging, prepare_shadow_root, swap_envs, sweep_pre_swap_staging,
};
use crate::repin::gate::{RepinCoordinator, RollupPause};
use crate::repin::jobs::JobGeneration;
use crate::repin::marker::{
    ASIDE_SUFFIX, RepinMarker, RepinPhase, StagingEvidence, aside_root, read_marker, remove_marker,
    shadow_root, staging_evidence, write_marker,
};
use crate::repin::plan::{ScanCounts, ScanTallies, scan};
use crate::repin::rewrite::{FileSig, ProcessTally, process_file, snapshot_env_files};
use crate::store::{
    CatalogStore, JobTotals, MAX_CONFLICTS_PER_FIELD, RepinJob, RepinJobStatus, RepinStore,
    StoreError,
};

const MAX_CATCHUP_PASSES: usize = 8;

/// How long admission waits for an active rollup, or cutover waits for
/// every query permit, before aborting to `blocked`.
const CUTOVER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Attempts at the post-swap postgres flip before giving up the process
/// (the marker replay completes it at the next boot).
const FLIP_ATTEMPTS: u32 = 3;

const FINISH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

const fn repin_blocked(reason: CorpusUnsettled) -> &'static str {
    match reason {
        CorpusUnsettled::RollupPending => {
            "repin is blocked while rollup publication is incomplete; retry after recovery"
        }
        CorpusUnsettled::RestartBacklog => {
            "repin is blocked while the server finishes loading data from before its restart; \
             retry once it settles"
        }
    }
}

/// Why repin admission stops while a finished rollup's marker unlink still
/// owes its day-directory fsync. Reads still answer, so this is not a
/// `corpus_recovering` refusal; the compaction pass retries the fsync.
const REMOVAL_SYNC_OWED: &str = "repin is blocked while a finished rollup's marker removal is not yet \
     durable; retry after the next compaction pass";

#[derive(Debug)]
enum AdmissionStop {
    Cancelled {
        stage: &'static str,
    },
    /// The job ends `blocked`. `row` is the text its job row carries, and
    /// `refusal` is what the request answers. An unsettled corpus keeps its
    /// typed refusal, so the request answers 503 `corpus_recovering` as
    /// every other corpus read does (ADR-0041).
    Blocked {
        row: String,
        refusal: ServerError,
    },
}

impl From<PassStop> for AdmissionStop {
    fn from(stop: PassStop) -> Self {
        match stop {
            PassStop::Cancelled { stage } => Self::Cancelled { stage },
            PassStop::Failed(row) => Self::Blocked {
                refusal: ServerError::ServiceUnavailable(row.clone()),
                row,
            },
        }
    }
}

/// Wait out an active rollup, validate its completed publication, then claim
/// the pause before another unit can start. The corpus guard comes first,
/// matching the rollup's lock order. Neither lock survives this admission.
async fn claim_rollup_pause(
    coordinator: &Arc<RepinCoordinator>,
    publication: &crate::publication::PublicationGate,
    cancel: &CancelHandle,
) -> Result<RollupPause, AdmissionStop> {
    cancel.check(STAGE_SCAN)?;
    let admission = async {
        let _corpus = coordinator.cutover_guard().await;
        let _publication = publication.read().await.map_err(|error| match error {
            ServerError::CorpusRecovering(reason) => AdmissionStop::Blocked {
                row: repin_blocked(reason).to_owned(),
                refusal: ServerError::CorpusRecovering(reason),
            },
            other => PassStop::Failed(other.safe_message()).into(),
        })?;
        if publication.any_removal_sync_owed() {
            return Err(AdmissionStop::Blocked {
                row: REMOVAL_SYNC_OWED.to_owned(),
                refusal: ServerError::ServiceUnavailable(REMOVAL_SYNC_OWED.to_owned()),
            });
        }
        cancel.check(STAGE_SCAN)?;
        Ok::<_, AdmissionStop>(coordinator.pause_rollup())
    };
    tokio::pin!(admission);
    let deadline = tokio::time::sleep(CUTOVER_DRAIN_TIMEOUT);
    tokio::pin!(deadline);
    let mut cancellation = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            result = &mut admission => return result,
            _ = cancellation.tick() => cancel.check(STAGE_SCAN)?,
            () = &mut deadline => return Err(PassStop::Failed(
                "repin could not wait out active rollup publication within 30 seconds; retry after it finishes"
                    .to_owned(),
            ).into()),
        }
    }
}

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

/// Test-only per-file delay in the scan (`plan::scan`), so integration
/// tests can walk away from a request while the scan is still running.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_SCAN_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only barrier on the build's first pass, arming a happened-before
/// ordering a delay alone cannot give: pass 0 takes its source snapshot,
/// publishes [`TEST_SNAPSHOT_TAKEN`], and then waits for
/// [`TEST_RELEASE_BUILD`].
#[cfg(any(test, feature = "test-support"))]
pub static TEST_BARRIER_FIRST_PASS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_SNAPSHOT_TAKEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_BUILD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hold at the first published progress, pinning the one state a
/// mid-job observer needs: the job row `running`, the running gauge up, and
/// `files_done` already ≥ 1, with no exclusion primitive held, so queries
/// and ingest still work exactly as they do mid-build.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_AFTER_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_PROGRESS_PUBLISHED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_JOB: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hold immediately past the point of no return: the latch is
/// taken and the Cutover marker is published, but no env has been swapped
/// yet.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_AFTER_NO_RETURN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_PAST_NO_RETURN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_CUTOVER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hold inside the scan, at the first file's boundary.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_IN_SCAN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_SCAN_HELD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_SCAN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only panic at the start of the scan (`plan::scan`), armed once.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_PANIC_IN_SCAN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hold at the finished-shadow force refusal, after the gate has
/// decided to refuse and before the job settles that verdict against a
/// pending cancel.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_AT_FORCE_REFUSAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_FORCE_REFUSAL_REACHED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_FORCE_REFUSAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Debug)]
pub enum StartOutcome {
    /// A dry run: the scan report, job already terminal (`succeeded`).
    DryRun(RepinJob),
    /// A lossy repin without force: terminal `refused_needs_force`, plan
    /// attached — the caller answers 409 with it.
    Refused(RepinJob),
    /// The rewrite is running in the background; poll the status surface.
    Started(RepinJob),
    /// An operator cancelled the job before it reached its point of no
    /// return, and this request's own ladder observed the cancel. The
    /// terminal row is already written; the caller answers 200 with it.
    Cancelled(RepinJob),
}

#[derive(Debug)]
pub enum BoundedStart {
    Decided(StartOutcome),
    /// The bound passed first. The row is the claimed job as the claim
    /// wrote it, and the job carries on detached.
    Running(RepinJob),
}

const UNCLAIMED_AT_BOUND: &str = "the repin request could not claim a job within [server] \
                                  timeout_secs, so no repin runs from it; retry the repin";

const CLAIM_OUTLIVED_REQUEST: &str = "the request that claimed this job was answered or \
                                      abandoned before the claim landed, so the job stopped \
                                      before its scan and changed nothing; retry the repin";

const PERMIT_HELD: &str = "the previous repin is still running or still cleaning up its \
                           staging; retry when it finishes";

const SHADOW_LEFT: &str =
    "a repin shadow root remains with no marker; restart trawld and boot removes it";

const STAGING_UNREADABLE: &str = "the repin staging state could not be read, so no repin can \
                                  start; the server log names the error";

fn marker_left(marker: &RepinMarker) -> String {
    format!(
        "repin job {} did not finish cleaning up (its marker stands in phase {}); fix the \
         cause the server log reports, then restart trawld so boot replays the cleanup; \
         trawld --doctor reports the marker under server.recovery.repin",
        marker.job_id,
        marker.phase.as_str()
    )
}

fn aside_left() -> String {
    format!(
        "a repin aside root remains with no marker, and trawld never deletes one without a \
         marker; remove the data root's {ASIDE_SUFFIX} sibling by hand, then retry"
    )
}

/// One share of the engine's repin permit ([`RepinEngine::permit`]). The
/// permit is released when the last share drops, an unwinding drop
/// included.
///
/// A detached helper that outlives the job's work never takes a share: the
/// terminal-write retry and the cancel recorder touch only their own job's
/// row.
#[derive(Debug, Clone)]
struct RepinPermit {
    _held: Arc<tokio::sync::OwnedSemaphorePermit>,
}

#[derive(Debug)]
struct ClaimHandoff {
    /// Hears the claimed row. A send that fails means nobody is waiting
    /// for it: the request answered at its bound, or its caller went away.
    claimed: tokio::sync::oneshot::Sender<RepinJob>,
}

fn decided(
    joined: Result<Result<StartOutcome, ServerError>, tokio::task::JoinError>,
) -> Result<BoundedStart, ServerError> {
    joined
        .map_err(|e| ServerError::from_join("repin request", e))?
        .map(BoundedStart::Decided)
}

/// The ceilings here are the operator's numbers, not the job's: resolution
/// needs the scan, which has not run yet at claim time.
#[derive(Debug, Clone)]
struct RequestTerms {
    force: bool,
    ceilings: RequestedCeilings,
    /// The requesting key's display name, as the claim recorded it.
    requested_by: Option<String>,
}

/// The build reads its terms from here and never from postgres: the numbers
/// were resolved from this job's own scan, and re-reading the row at cutover
/// would let a concurrent write change what the operator agreed to.
#[derive(Debug, Clone)]
struct JobTerms {
    force: ForceTerms,
    /// What the pre-build scan projected, for the audit record: the whole
    /// point of the finished-shadow gate is that these two can differ.
    scanned: ceiling::Counts,
    requested_by: Option<String>,
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
    wal_dir: PathBuf,
    memory_limit: String,
    min_free_disk_bytes: u64,
    cancel: Arc<CancelRegistry>,
    jobs: Arc<JobGeneration>,
    permit: Arc<tokio::sync::Semaphore>,
    #[cfg(any(test, feature = "test-support"))]
    holds: Arc<crate::repin::holds::JobHolds>,
}

impl RepinEngine {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: RepinStore,
        catalog_store: CatalogStore,
        cache: Arc<FieldCatalog>,
        coordinator: Arc<RepinCoordinator>,
        pool: ExecutorPool,
        data_dir: PathBuf,
        wal_dir: PathBuf,
        memory_limit: String,
        min_free_disk_bytes: u64,
        jobs: Arc<JobGeneration>,
    ) -> Self {
        Self {
            store,
            catalog_store,
            cache,
            coordinator,
            pool,
            data_dir,
            wal_dir,
            memory_limit,
            min_free_disk_bytes,
            cancel: Arc::new(CancelRegistry::default()),
            jobs,
            permit: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(any(test, feature = "test-support"))]
            holds: Arc::default(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn holds(&self) -> &crate::repin::holds::JobHolds {
        &self.holds
    }

    /// Test-only: whether the repin permit is free, so the next repin can
    /// be admitted. It only reads the permit and never takes it, so a test
    /// that polls it cannot change the admission it observes.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn repin_idle(&self) -> bool {
        self.permit.available_permits() == 1
    }

    #[must_use]
    pub fn store(&self) -> &RepinStore {
        &self.store
    }

    /// The request runs in its own task, as [`Self::start_within`]'s does,
    /// so the permit never lives in the caller's future: a caller dropped
    /// mid-request leaves the task to settle and disarm what it owns.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        self: &Arc<Self>,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
        requested: RequestedCeilings,
        caller: Option<&crate::store::Requester>,
    ) -> Result<StartOutcome, ServerError> {
        self.spawn_request(field, to, dialect, dry_run, force, requested, caller, None)
            .await
            .map_err(|e| ServerError::from_join("repin request", e))?
    }

    /// [`Self::start`], answering within `bound` of this call.
    ///
    /// One deadline, stamped on entry (ADR-0024), covers everything the
    /// answer waits on: validation, the claim, the scan and the decision's
    /// own reads. Past it:
    ///
    /// - A claimed job answers [`BoundedStart::Running`] with the row the
    ///   claim wrote, and nothing more is read: a store slow enough to hold
    ///   the decision past its bound would hold a read-back as well.
    /// - A request with no claim yet answers 503 `service_unavailable`. Its
    ///   claim may still land, but then it finds nobody waiting and ends
    ///   the job `blocked` before any scan.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_within(
        self: &Arc<Self>,
        bound: Duration,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
        requested: RequestedCeilings,
        caller: Option<&crate::store::Requester>,
    ) -> Result<BoundedStart, ServerError> {
        let deadline = Deadline::after(bound);
        let unclaimed = || ServerError::ServiceUnavailable(UNCLAIMED_AT_BOUND.to_owned());
        let (claimed, mut claimed_rx) = tokio::sync::oneshot::channel();
        let mut request = self.spawn_request(
            field,
            to,
            dialect,
            dry_run,
            force,
            requested,
            caller,
            Some(ClaimHandoff { claimed }),
        );
        if let Ok(joined) = deadline.run(&mut request).await {
            return decided(joined);
        }
        // Past the bound, and nothing below waits. Closing the receiver
        // settles the race with the claim: a claim that lands from here on
        // finds it closed and abandons its job, and one that landed first
        // is already in the channel.
        claimed_rx.close();
        let claimed = claimed_rx.try_recv().ok();
        if request.is_finished() {
            return decided(request.await);
        }
        // Dropping `request` detaches it. Past a claim everything already
        // runs detached and terminalizes the row itself; short of one, the
        // task can only claim a job it then abandons (see `run_request`).
        claimed.map(BoundedStart::Running).ok_or_else(unclaimed)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_request(
        self: &Arc<Self>,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
        requested: RequestedCeilings,
        caller: Option<&crate::store::Requester>,
        handoff: Option<ClaimHandoff>,
    ) -> tokio::task::JoinHandle<Result<StartOutcome, ServerError>> {
        let engine = Arc::clone(self);
        let field = field.to_owned();
        let to = to.to_owned();
        let dialect = dialect.map(str::to_owned);
        let caller = caller.cloned();
        tokio::spawn(async move {
            engine
                .run_request(
                    &field,
                    &to,
                    dialect.as_deref(),
                    dry_run,
                    force,
                    requested,
                    caller.as_ref(),
                    handoff,
                )
                .await
        })
    }

    /// `handoff`, from a bounded request, hears the claimed row once the
    /// claim has landed and the cancel registry is armed for it.
    // Long because the panic arm's ordering is the point, and splitting it
    // would move the registry settlement away from the writes it precedes.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn run_request(
        self: &Arc<Self>,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
        requested: RequestedCeilings,
        caller: Option<&crate::store::Requester>,
        handoff: Option<ClaimHandoff>,
    ) -> Result<StartOutcome, ServerError> {
        let field = field.to_ascii_lowercase();
        let to = parse_target(to)?;
        let dialect = resolve_dialect(to, dialect)?;
        let (max_nulled_rows, max_ambiguous_rows) = validate_ceilings(force, requested)?;
        // A predicate, not a list of envelope names: the whole `_` prefix
        // is trawl's (`schema::is_contract_typed`), so a contract slot
        // added later is refused the day it exists rather than the day
        // somebody remembers this check.
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

        let permit = self.take_permit()?;
        self.refuse_leftover_staging()?;
        let claimed = self
            .store
            .claim_job(crate::store::RepinClaim {
                field: &field,
                from_type: from,
                to_type: to,
                dialect,
                dry_run,
                force,
                max_nulled_rows,
                max_ambiguous_rows,
                requester: caller,
            })
            .await?;
        let job_id = claimed.id;
        // Arm the cancel registry with no `await` between it and the claim
        // that created the job. Anything awaited here would be a window in
        // which the one-running slot is held by a job nothing can cancel:
        // the request would answer "no job running" while the scan burns
        // through the corpus.
        let cancel = self.cancel.arm(job_id);
        if let Some(ClaimHandoff { claimed: answer }) = handoff
            && answer.send(claimed).is_err()
        {
            // Nobody is waiting for this job: the request answered 503 at
            // its bound, or its caller went away, before the claim landed.
            // Nobody can follow it either, so it ends here, short of the
            // scan, as a pre-scan refusal does. The permit drops as this
            // returns, after the disarm.
            tracing::warn!(
                event_type = "repin_claim_unanswered",
                job_id,
                "the repin request was answered or abandoned before its \
                 claim landed; the job ends blocked before its scan"
            );
            self.settle_pre_cutover(
                job_id,
                STAGE_SCAN,
                RepinJobStatus::Blocked,
                Some(CLAIM_OUTLIVED_REQUEST),
            )
            .await;
            self.cancel.disarm(job_id);
            return Err(ServerError::ServiceUnavailable(
                UNCLAIMED_AT_BOUND.to_owned(),
            ));
        }

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

        // Everything past the claim runs in a detached task, never in the
        // caller's future: axum drops the handler future the moment the
        // connection goes away, and cancelled between the claim and the
        // terminal transition, the unique running slot would be stranded
        // until a daemon restart.
        //
        // The reading rule is built once here because this is the only place
        // that knows the old pin (see `RepinReading`). Everything downstream
        // (scan, rewrite, both force gates) reads it rather than re-deriving
        // a dialect from the target.
        let reading = RepinReading::new(from, to, dialect.unwrap_or_default());
        let engine = Arc::clone(self);
        let req = RequestTerms {
            force,
            ceilings: requested,
            requested_by: caller.map(|r| r.name.clone()),
        };
        let decided = {
            let permit = permit.clone();
            tokio::spawn(async move {
                engine
                    .decide(job_id, field, from, reading, dry_run, req, cancel, permit)
                    .await
            })
        };
        let outcome = match decided.await {
            Ok(outcome) => outcome,
            Err(e) => {
                // Settle first, under the registry lock and before any store
                // I/O: `finish` rides out postgres trouble for seconds, and an
                // entry still armed through all of that answers 202 to
                // cancels of a job whose task no longer exists.
                //
                // The request is written HERE, synchronously, before the
                // terminal status: `record_cancel_request` only matches a row
                // whose status is still `running`.
                if let Settlement::Cancelled(actor) = self.cancel.settle(job_id) {
                    tracing::warn!(
                        event_type = "repin_cancel_unobserved",
                        job_id,
                        actor = %actor.name(),
                        actor_key_prefix = %actor.key_prefix(),
                        "the repin task died before any boundary observed the \
                         pending cancel; the job is recorded failed, not \
                         cancelled"
                    );
                    if let Err(store_err) =
                        self.store.record_cancel_request(job_id, actor.name()).await
                    {
                        tracing::error!(
                            event_type = "repin_store_error",
                            job_id,
                            error_class = store_err.class(),
                            "the pending cancel request could not be recorded \
                             before the failed terminal; the job row will not \
                             name who asked"
                        );
                    }
                }
                let msg = StepDied::from_join("repin job", &e).to_string();
                self.finish(job_id, RepinJobStatus::Failed, Some(&msg), None)
                    .await;
                Err(ServerError::from_join("repin job", e))
            }
        };
        // The party that owns the outcome is the party that disarms. `run_job`
        // owns the background half and disarms itself at its end; every other
        // outcome is owned here. These are the only two disarm call sites.
        if !matches!(outcome, Ok(StartOutcome::Started(_))) {
            self.cancel.disarm(job_id);
        }
        drop(permit);
        outcome
    }

    fn take_permit(&self) -> Result<RepinPermit, ServerError> {
        Arc::clone(&self.permit)
            .try_acquire_owned()
            .map(|held| RepinPermit {
                _held: Arc::new(held),
            })
            // The permit is never closed, so a failure means it is held.
            .map_err(|_| ServerError::Conflict(PERMIT_HELD.to_owned()))
    }

    fn refuse_leftover_staging(&self) -> Result<(), ServerError> {
        let unreadable = || ServerError::ServiceUnavailable(STAGING_UNREADABLE.to_owned());
        let found = staging_evidence(&self.data_dir).map_err(|e| {
            let refusal = unreadable();
            tracing::debug!(
                event_type = "repin_staging_unreadable",
                error = %e,
                "the repin staging state could not be read; refusing the repin"
            );
            tracing::error!(
                event_type = "repin_staging_unreadable",
                error_class = refusal.error_class(),
                cause_kind = CauseKind::of_io(&e).as_str(),
                "the repin staging state could not be read; refusing the repin"
            );
            refusal
        })?;
        match found {
            None => Ok(()),
            Some(StagingEvidence::Marker) => match read_marker(&self.data_dir) {
                Ok(Some(marker)) => Err(ServerError::Conflict(marker_left(&marker))),
                // A corrupt marker names no job and no phase to refuse
                // with, and nothing this process runs removes a marker
                // while admission holds the permit. Neither has a remedy
                // to name, so both answer as unreadable staging.
                Ok(None) => {
                    tracing::error!(
                        event_type = "repin_staging_unreadable",
                        "the repin marker vanished while admission read it; \
                         refusing the repin"
                    );
                    Err(unreadable())
                }
                Err(e) => {
                    // The text mixes the read's I/O wording with serde's
                    // parse wording and keeps no typed source.
                    tracing::debug!(
                        event_type = "repin_staging_unreadable",
                        error = %e,
                        "the repin marker could not be read; refusing the repin"
                    );
                    tracing::error!(
                        event_type = "repin_staging_unreadable",
                        error_class = "internal",
                        cause_kind = CauseKind::Unknown.as_str(),
                        "the repin marker could not be read; refusing the repin"
                    );
                    Err(unreadable())
                }
            },
            Some(StagingEvidence::ShadowRoot) => Err(ServerError::Conflict(SHADOW_LEFT.to_owned())),
            Some(StagingEvidence::AsideRoot) => Err(ServerError::Conflict(aside_left())),
        }
    }

    /// Detached because a cancel must not be split by a client disconnect.
    /// The in-process flag is already set when this returns, so the effect
    /// site can fire before the request row lands; that is safe, because
    /// the effect site records the request itself before writing a
    /// `cancelled` terminal status.
    pub fn cancel(self: &Arc<Self>, actor: &CancelActor) -> CancelVerdict {
        let decision = self.cancel.request(actor);
        match decision.verdict {
            CancelVerdict::Cancelling {
                job_id,
                already_requested,
            } => {
                let engine = Arc::clone(self);
                let caller = actor.clone();
                // The registry's retained actor, never the caller's: a
                // repeat is audited under the name that asked, but the
                // durable row may only ever carry the first asker.
                let recorded = decision
                    .retained
                    .unwrap_or_else(|| actor.clone())
                    .name()
                    .to_owned();
                tokio::spawn(async move {
                    if let Err(e) = engine.store.record_cancel_request(job_id, &recorded).await {
                        // The flag is the authority for the verdict the
                        // operator already holds; a store that cannot
                        // record the request costs the audit row, and the
                        // effect site downgrades the outcome to `failed`
                        // when its own write fails too.
                        tracing::error!(
                            event_type = "repin_store_error",
                            job_id,
                            error_class = e.class(),
                            "failed to record the repin cancel request; the \
                             cancellation itself is unaffected"
                        );
                    }
                    audit_cancel_requested(job_id, &caller, already_requested);
                });
            }
            CancelVerdict::PastPointOfNoReturn { job_id } => audit_cancel_refused(job_id, actor),
            // A 404 has no job to name, so it emits no audit event.
            CancelVerdict::NoJobRunning => {}
        }
        decision.verdict
    }

    /// The claimed job's decision ladder: scan → report (dry run) → refuse
    /// (lossy without force) → pre-flight → start the background rewrite.
    // The ladder is long because it is a ladder: every rung terminalizes
    // the claimed job itself.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn decide(
        self: Arc<Self>,
        job_id: i64,
        field: String,
        from: CanonicalType,
        reading: RepinReading,
        dry_run: bool,
        req: RequestTerms,
        cancel: CancelHandle,
        permit: RepinPermit,
    ) -> Result<StartOutcome, ServerError> {
        // A pending rollup's marker and temporary output are one recovery
        // operation. The shadow walk excludes temporary files, so it must
        // never copy that marker into a generation without its output.
        // Claim before the scan too, so a dry run cannot report mixed rows.
        let publication = self.pool.publication();
        let rollup_pause = match claim_rollup_pause(&self.coordinator, &publication, &cancel).await
        {
            Ok(pause) => pause,
            Err(AdmissionStop::Cancelled { stage }) => {
                let actor = self.settle_cancel(job_id, stage);
                self.finish_cancelled(job_id, stage, actor).await;
                return self.cancelled_outcome(job_id).await;
            }
            Err(AdmissionStop::Blocked { row, refusal }) => {
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Blocked, Some(&row))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(refusal);
            }
        };
        // Test-only: a decision task that dies holding its rollup pause,
        // its claimed row and its armed cancel entry (see
        // `JobHolds::panic_next_decision`). Armed once.
        #[cfg(any(test, feature = "test-support"))]
        assert!(
            !self.holds.take_decision_panic(),
            "test-support: the armed repin decision panic"
        );

        // Layout pre-flight, ahead of the minutes-long scan. It gates the dry
        // run too: answering yes to a layout whose cutover can only exit the
        // process would be a lie. Post-claim, so the refusal is a terminal
        // job row the status surface reports rather than a stranded running
        // slot.
        let data_dir = self.data_dir.clone();
        match on_blocking_pool("repin staging pre-flight", move || {
            crate::repin::marker::check_staging_filesystem(&data_dir)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&msg))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(ServerError::BadRequest(msg));
            }
            // The row gets the fixed text; the request gets the typed
            // failure, so a panic answers as one.
            Err(died) => {
                if self
                    .settle_pre_cutover(
                        job_id,
                        STAGE_SCAN,
                        RepinJobStatus::Failed,
                        Some(&died.to_string()),
                    )
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(died.into());
            }
        }

        let (counts, tallies, samples) = match self.run_scan(&field, reading, &cancel).await {
            Ok(Ok(measured)) => measured,
            // A cancel observed inside the scan: nothing has been staged
            // and no marker exists, so the whole unwind is the terminal
            // write.
            Ok(Err(PassStop::Cancelled { stage })) => {
                let actor = self.settle_cancel(job_id, stage);
                self.finish_cancelled(job_id, stage, actor).await;
                return self.cancelled_outcome(job_id).await;
            }
            Ok(Err(PassStop::Failed(e))) => {
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&e))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(ServerError::Internal(format!("repin scan failed: {e}")));
            }
            // As the pre-flight: fixed text for the row, the typed failure
            // for the request.
            Err(died) => {
                if self
                    .settle_pre_cutover(
                        job_id,
                        STAGE_SCAN,
                        RepinJobStatus::Failed,
                        Some(&died.to_string()),
                    )
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(died.into());
            }
        };
        let liveness = self.field_liveness(&field).await;
        let clamp = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        // The one resolution point. A job's ceilings can only come from its
        // own scan, so they are settled here, once, and persisted in the same
        // statement as the counts they were derived from. Both gates and the
        // audit record read this value; nothing re-reads the row.
        let ceilings = ceiling::resolve(
            counts.projected_nulls,
            counts.ambiguous_numerals,
            req.ceilings,
        );
        let terms = if req.force {
            ForceTerms::forced(ceilings)
        } else {
            ForceTerms::unforced()
        };
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
                    accepted_max_nulled_rows: terms.ceilings().map(|c| clamp(c.max_nulled)),
                    accepted_max_ambiguous_rows: terms.ceilings().map(|c| clamp(c.max_ambiguous)),
                },
            )
            .await
        {
            let err = ServerError::Store(e);
            let row = fault_row(job_id, "could not record the repin plan", &err);
            if self
                .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&row))
                .await
            {
                return self.cancelled_outcome(job_id).await;
            }
            return Err(err);
        }

        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::CATALOG_REPIN_FILES_TOTAL).set(counts.files_total as f64);

        if dry_run {
            // Even a completed dry run is arbitrated: a cancel that landed
            // after the last file's post-check, while the plan was being
            // written, is a request an operator made of a job that was
            // still running. Reporting `succeeded` there would answer a
            // cancel with the very report it was meant to stop.
            if self
                .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Succeeded, None)
                .await
            {
                return self.cancelled_outcome(job_id).await;
            }
            return Ok(StartOutcome::DryRun(self.job(job_id).await?));
        }
        // The scan gate: the same decision as the finished-shadow gate
        // below, one function. The plan rides back as the 409 body and the
        // reason rides with it, because a refusal over ambiguity with zero
        // projected nulls is otherwise a plan an operator cannot read the
        // verdict off.
        if let Some(reason) = force_refusal(
            reading.written.pin,
            Some(reading.written.raw),
            counts.projected_nulls,
            counts.ambiguous_numerals,
            terms,
        ) {
            if self
                .settle_pre_cutover(
                    job_id,
                    STAGE_SCAN,
                    RepinJobStatus::RefusedNeedsForce,
                    Some(&reason),
                )
                .await
            {
                return self.cancelled_outcome(job_id).await;
            }
            return Ok(StartOutcome::Refused(self.job(job_id).await?));
        }

        let available = match fs4::available_space(&self.data_dir) {
            Ok(available) => available,
            Err(e) => {
                let row = disk_check_row(job_id, &e);
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&row))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(ServerError::Internal(row));
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
            if self
                .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&msg))
                .await
            {
                return self.cancelled_outcome(job_id).await;
            }
            return Err(ServerError::BadRequest(msg));
        }

        // Read the row the caller is answered with BEFORE any detached work
        // exists. A read failure here settles like any other pre-cutover
        // candidate, with nothing detached to strand.
        let job = match self.job(job_id).await {
            Ok(job) => job,
            Err(e) => {
                let row = fault_row(job_id, "could not read the claimed repin job row", &e);
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&row))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(e);
            }
        };

        let engine = Arc::clone(&self);
        let tallies = Arc::new(tallies);
        let job_terms = JobTerms {
            force: terms,
            scanned: ceiling::Counts {
                nulled: counts.projected_nulls,
                ambiguous: counts.ambiguous_numerals,
            },
            requested_by: req.requested_by,
        };
        #[cfg(any(test, feature = "test-support"))]
        let holds = Arc::clone(&engine.holds);
        tokio::spawn(async move {
            engine
                .run_job(
                    job_id,
                    field,
                    from,
                    reading,
                    job_terms,
                    tallies,
                    cancel,
                    rollup_pause,
                )
                .await;
            #[cfg(any(test, feature = "test-support"))]
            holds.before_permit_release.reach(job_id).await;
            drop(permit);
        });
        Ok(StartOutcome::Started(job))
    }

    async fn run_scan(
        &self,
        field: &str,
        reading: RepinReading,
        cancel: &CancelHandle,
    ) -> Result<Result<(ScanCounts, ScanTallies, Vec<String>), PassStop>, StepDied> {
        let data_dir = self.data_dir.clone();
        let memory_limit = self.memory_limit.clone();
        let field = field.to_owned();
        let cancel = cancel.clone();
        on_blocking_pool("repin scan", move || {
            scan(&data_dir, &memory_limit, &field, reading, &cancel)
        })
        .await
    }

    /// Is anything still writing this field? The newest observation inside
    /// [`crate::repin::LIVENESS_WINDOW`], with one service behind it.
    async fn field_liveness(&self, field: &str) -> Option<(chrono::DateTime<chrono::Utc>, String)> {
        let (rows, _) = self
            .catalog_store
            .field_services(field, None, 1)
            .await
            .map_err(|e| {
                log_store_fault(
                    Loudness::Warn,
                    StoreFault {
                        event_type: "catalog_bookkeeping_error",
                        field: Some(field),
                        ..StoreFault::default()
                    },
                    e,
                    "could not read repin subject liveness; the report omits it",
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

    /// The `repin_jobs_one_running` slot is unique, so a `running` row
    /// whose terminal write is lost would 409 every later repin until a
    /// restart's boot reconciliation. A blip gets bounded fast retries; a
    /// real outage hands the write to a detached slow loop that retries
    /// until it lands. The outcome counter increments only when the write
    /// lands.
    async fn finish(
        &self,
        job_id: i64,
        status: RepinJobStatus,
        error: Option<&str>,
        totals: Option<JobTotals>,
    ) {
        const FAST_ATTEMPTS: u32 = 3;
        for attempt in 1..=FAST_ATTEMPTS {
            match self
                .store
                .finish_if_running(job_id, status, error, totals)
                .await
            {
                // The write is conditional (never clobber a terminal
                // verdict), but the metric is not gated on it: the success
                // path arrives here with the row ALREADY `succeeded`,
                // terminalized inside `finish_cutover`'s transaction.
                Ok(_) => {
                    count_outcome(status);
                    return;
                }
                Err(e) if attempt < FAST_ATTEMPTS => {
                    log_store_fault(
                        Loudness::Warn,
                        StoreFault {
                            event_type: "repin_store_retry",
                            job_id: Some(job_id),
                            attempt: Some(attempt),
                            ..StoreFault::default()
                        },
                        e,
                        "failed to record repin job outcome; retrying",
                    );
                    tokio::time::sleep(Duration::from_millis(100 << (attempt - 1))).await;
                }
                Err(e) => {
                    log_store_fault(
                        Loudness::Error,
                        StoreFault {
                            event_type: "repin_store_error",
                            job_id: Some(job_id),
                            ..StoreFault::default()
                        },
                        e,
                        "failed to record repin job outcome; handing the \
                         terminal write to a background retry so the \
                         one-running slot cannot stay wedged",
                    );
                }
            }
        }
        let store = self.store.clone();
        let error = error.map(str::to_owned);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(FINISH_RETRY_INTERVAL).await;
                match store
                    .finish_if_running(job_id, status, error.as_deref(), totals)
                    .await
                {
                    Ok(_) => {
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
                        log_store_fault(
                            Loudness::Warn,
                            StoreFault {
                                event_type: "repin_store_retry",
                                job_id: Some(job_id),
                                ..StoreFault::default()
                            },
                            e,
                            "repin job outcome write still failing; will retry",
                        );
                    }
                }
            }
        });
    }

    async fn cancelled_outcome(&self, job_id: i64) -> Result<StartOutcome, ServerError> {
        Ok(StartOutcome::Cancelled(self.job(job_id).await?))
    }

    /// Terminalize a job that is still short of the point of no return,
    /// arbitrating `candidate` against a pending cancel. Returns `true`
    /// when the cancel won, in which case the row is already written and
    /// the caller must not write its own verdict.
    async fn settle_pre_cutover(
        &self,
        job_id: i64,
        stage: &'static str,
        candidate: RepinJobStatus,
        error: Option<&str>,
    ) -> bool {
        let Some(actor) = self.settle_cancel(job_id, stage) else {
            self.finish(job_id, candidate, error, None).await;
            return false;
        };
        self.finish_cancelled(job_id, stage, Some(actor)).await;
        true
    }

    /// Latch this job's pre-cutover outcome and, when a cancel already
    /// holds it, audit that before any unwinding starts. `None` means the
    /// caller's own verdict stands and no later request can contradict it.
    /// [`CancelRegistry::settle`] makes the choice under the same lock the
    /// request takes, so the two answers cannot disagree.
    fn settle_cancel(&self, job_id: i64, stage: &'static str) -> Option<CancelActor> {
        match self.cancel.settle(job_id) {
            Settlement::Cancelled(actor) => {
                audit_cancelled(job_id, &actor, stage);
                Some(actor)
            }
            Settlement::Candidate => None,
        }
    }

    /// The order is the database's requirement: a `cancelled` row without a
    /// recorded request violates the CHECK, so the terminal write is
    /// unreachable until `record_cancel_request` has landed. If it fails
    /// too, the job ends `failed` naming the store trouble.
    async fn finish_cancelled(&self, job_id: i64, stage: &'static str, actor: Option<CancelActor>) {
        let Some(actor) = actor else {
            // Only reachable if the registry stopped naming this job
            // between the check that decided to cancel and this call.
            // `cancelled` would be a claim about a request nothing can
            // point at, so it is `failed`.
            let msg = format!(
                "the repin was stopped during {stage} but its cancel \
                 request could no longer be read; the live corpus was \
                 never touched"
            );
            self.finish(job_id, RepinJobStatus::Failed, Some(&msg), None)
                .await;
            return;
        };
        match self.store.record_cancel_request(job_id, actor.name()).await {
            Ok(_) => {
                let msg = cancelled_error(actor.name(), stage);
                self.finish(job_id, RepinJobStatus::Cancelled, Some(&msg), None)
                    .await;
            }
            Err(e) => {
                tracing::error!(
                    event_type = "repin_store_error",
                    job_id,
                    error_class = e.class(),
                    "the cancel request could not be recorded, so the job \
                     cannot be terminalized as cancelled; recording it as \
                     failed instead"
                );
                let msg = format!(
                    "cancelled by {} during {stage}, but the cancel \
                     request could not be recorded in the job store, so the \
                     outcome is failed rather than cancelled; the live \
                     corpus was never touched",
                    actor.name()
                );
                self.finish(job_id, RepinJobStatus::Failed, Some(&msg), None)
                    .await;
            }
        }
    }

    /// The job generation's begin and end live here, the one function
    /// every job that writes under the data root runs through.
    #[allow(clippy::too_many_arguments)]
    async fn run_job(
        self: Arc<Self>,
        job_id: i64,
        field: String,
        from: CanonicalType,
        reading: RepinReading,
        terms: JobTerms,
        scanned: Arc<ScanTallies>,
        cancel: CancelHandle,
        rollup_pause: RollupPause,
    ) {
        let _job = self.jobs.begin();
        let started = std::time::Instant::now();
        let _rollup_pause = rollup_pause;
        metrics::gauge!(crate::metrics::CATALOG_REPIN_RUNNING).set(1.0);

        let to = reading.written.pin;
        let outcome = self
            .run_job_inner(job_id, &field, from, reading, &terms, &scanned, &cancel)
            .await;
        metrics::gauge!(crate::metrics::CATALOG_REPIN_RUNNING).set(0.0);
        metrics::histogram!(crate::metrics::CATALOG_REPIN_DURATION_SECONDS)
            .record(started.elapsed().as_secs_f64());

        match outcome {
            Ok(()) => {
                self.finish(job_id, RepinJobStatus::Succeeded, None, None)
                    .await;
                tracing::info!(
                    event_type = "repin_complete",
                    job_id,
                    field = %field,
                    to = to.as_catalog(),
                    duration_ms = started.elapsed().as_millis(),
                    "repin job complete: the corpus and the pin now agree"
                );
            }
            Err(JobAbort::Cancelled { stage }) => {
                let actor = self.settle_cancel(job_id, stage);
                self.abandon_cancelled(job_id, stage, actor).await;
            }
            Err(JobAbort::RefusedNeedsForce { msg, totals }) => {
                // The refusal decided on these numbers, so they ride the
                // terminal write: the periodic progress write is
                // best-effort, and a store blip just before the gate would
                // otherwise leave the wire reporting the refusal beside a
                // stale `rows_nulled`. A cancel that wins the settlement
                // drops them — the verdict is then `cancelled`, and those
                // tallies are progress, not the evidence behind a refusal.
                self.abandon_build(
                    job_id,
                    RepinJobStatus::RefusedNeedsForce,
                    &msg,
                    Some(totals),
                )
                .await;
            }
            Err(JobAbort::Blocked(msg)) => {
                self.abandon_build(job_id, RepinJobStatus::Blocked, &msg, None)
                    .await;
            }
            Err(JobAbort::Failed(msg)) => {
                self.abandon_build(job_id, RepinJobStatus::Failed, &msg, None)
                    .await;
            }
        }

        // Until this line a cancel request is answered 409 rather than 404.
        // `arm` itself refuses nothing; the repin permit keeps a fresh repin
        // out while this job still deletes its staging roots.
        self.cancel.disarm(job_id);
    }

    /// `actor` comes from [`Self::settle_cancel`], which audited it before
    /// any unwinding started, so the log reads request, effect, sweep.
    /// `None` means the registry stopped naming this job between the
    /// boundary that observed the cancel and the settlement, which
    /// [`Self::finish_cancelled`] records as `failed`.
    async fn abandon_cancelled(
        &self,
        job_id: i64,
        stage: &'static str,
        actor: Option<CancelActor>,
    ) {
        let msg = actor.as_ref().map_or_else(
            || format!("cancelled during {stage}"),
            |actor| cancelled_error(actor.name(), stage),
        );
        self.unwind_staging(job_id, RepinJobStatus::Cancelled, &msg)
            .await;
        self.finish_cancelled(job_id, stage, actor).await;
    }

    /// Every pre-cutover terminal candidate settles under the registry lock,
    /// and there is no other door. The synchronous half's door is
    /// [`Self::settle_pre_cutover`], same rule.
    ///
    /// Settling before the sweep starts is the point. Sweeping a whole
    /// shadow generation takes minutes on a real archive, and had the
    /// verdict been chosen first, a cancel landing during those minutes
    /// would be answered 202 and then lose to a decision already made.
    async fn abandon_build(
        &self,
        job_id: i64,
        candidate: RepinJobStatus,
        msg: &str,
        totals: Option<JobTotals>,
    ) {
        if let Some(actor) = self.settle_cancel(job_id, STAGE_BUILD) {
            self.abandon_cancelled(job_id, STAGE_BUILD, Some(actor))
                .await;
            return;
        }
        self.unwind_staging(job_id, candidate, msg).await;
        self.finish(job_id, candidate, Some(msg), totals).await;
    }

    /// A cancelled job takes the same unwind but a different terminal write:
    /// `cancelled` is only legal once the request row exists (see
    /// [`Self::finish_cancelled`]).
    async fn unwind_staging(&self, job_id: i64, status: RepinJobStatus, msg: &str) {
        if status == RepinJobStatus::Failed {
            // A failed step's message can carry DuckDB, serde or I/O
            // wording, and no typed source is left to classify it by.
            tracing::debug!(
                event_type = "repin_abandoned",
                job_id,
                status = status.as_str(),
                error = %msg,
                "repin job abandoned before any visible change; corpus untouched"
            );
            tracing::warn!(
                event_type = "repin_abandoned",
                job_id,
                status = status.as_str(),
                error_class = "internal",
                cause_kind = CauseKind::Unknown.as_str(),
                "repin job abandoned before any visible change; corpus untouched"
            );
        } else {
            // A blocked, refused or cancelled job's message is a sentence
            // trawl wrote.
            tracing::warn!(
                event_type = "repin_abandoned",
                job_id,
                status = status.as_str(),
                error = %msg,
                "repin job abandoned before any visible change; corpus untouched"
            );
        }
        // The marker is what licenses the next boot to delete the staging
        // roots. Keep it over a failed sweep: the boot replay retries it.
        let data_dir = self.data_dir.clone();
        let swept = on_blocking_pool("repin abandon sweep", move || {
            sweep_pre_swap_staging(&data_dir)
        })
            .await
            .unwrap_or_else(|died| {
                // A panicked sweep is a sweep that did not finish: keep the
                // marker, exactly as a failed one does.
                tracing::warn!(event_type = "repin_sweep_failed", error = %died, "abandon sweep task failed");
                false
            });
        if swept {
            if let Err(e) = remove_marker(&self.data_dir) {
                // `remove_marker` hands back text, so the kind is unknown here.
                tracing::debug!(event_type = "repin_marker_error", error = %e, "marker removal failed");
                tracing::warn!(
                    event_type = "repin_marker_error",
                    cause_kind = CauseKind::Unknown.as_str(),
                    "marker removal failed"
                );
            }
        } else {
            tracing::warn!(
                event_type = "repin_recovery_incomplete",
                job_id,
                "a repin staging root survived the abandoned job's sweep; \
                 keeping the marker so the next boot retries the cleanup"
            );
        }
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn run_job_inner(
        &self,
        job_id: i64,
        field: &str,
        from: CanonicalType,
        reading: RepinReading,
        terms: &JobTerms,
        scanned: &Arc<ScanTallies>,
        cancel: &CancelHandle,
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

        let data_dir = self.data_dir.clone();
        let shadow = on_blocking_pool("repin shadow prepare", move || {
            prepare_shadow_root(&data_dir)
        })
        .await
        .map_err(|died| JobAbort::Failed(died.to_string()))?
        .map_err(JobAbort::Failed)?;

        let mut flipped = self.cache.snapshot();
        flipped.insert(field.to_owned(), to);
        let flipped = Arc::new(flipped);

        let mut state = BuildState::default();
        let mut converged = false;
        for pass in 0..MAX_CATCHUP_PASSES {
            let changed = self
                .run_pass(field, reading, &flipped, scanned, &mut state, cancel)
                .await?;
            self.publish_progress(job_id, &state).await;

            // Test-only: pin the mid-job state for an observer (see
            // `TEST_HOLD_AFTER_PROGRESS`).
            #[cfg(any(test, feature = "test-support"))]
            if TEST_HOLD_AFTER_PROGRESS.swap(false, std::sync::atomic::Ordering::SeqCst) {
                TEST_PROGRESS_PUBLISHED.store(true, std::sync::atomic::Ordering::SeqCst);
                while !TEST_RELEASE_JOB.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }

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

        let corpus_gate = self.coordinator.cutover_guard().await;
        if let Some(reason) = self.pending_publication(job_id).await {
            drop(corpus_gate);
            return Err(JobAbort::Blocked(reason));
        }
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
                return Err(JobAbort::Failed(fault_row(
                    job_id,
                    "cutover drain failed",
                    &e,
                )));
            }
        };

        #[cfg(any(test, feature = "test-support"))]
        self.coordinator.hold_cutover_for_tests().await;

        // Final increment under exclusion: nothing can write or read the
        // corpus now, so this pass is the last word. Still cancellable —
        // the exclusion guards are held, but nothing visible has moved, and
        // dropping them on the unwind costs the corpus nothing.
        self.run_pass(field, reading, &flipped, scanned, &mut state, cancel)
            .await?;

        let staged_totals = staged_totals(&state).map_err(JobAbort::Failed)?;
        let staged_tallies = staged_tallies(&state).map_err(JobAbort::Failed)?;
        self.store
            .stage_cutover_input(job_id, staged_totals, &staged_tallies)
            .await
            .map_err(|e| {
                log_store_fault(
                    Loudness::Error,
                    StoreFault {
                        event_type: "repin_store_error",
                        job_id: Some(job_id),
                        ..StoreFault::default()
                    },
                    e,
                    "staging the cutover tallies failed; aborting before the marker",
                );
                JobAbort::Failed(
                    "could not stage the cutover tallies (app-state store \
                     error; details in the server log). Nothing has moved — \
                     the corpus stands at its pre-repin generation; retry \
                     the repin"
                        .to_owned(),
                )
            })?;
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::CATALOG_REPIN_FILES_DONE)
            .set(staged_totals.files_done as f64);

        let totals = state.totals();
        if let Some(reason) = force_refusal(
            reading.written.pin,
            Some(reading.written.raw),
            totals.nulled,
            totals.ambiguous,
            terms.force,
        ) {
            // Test-only: pin the window between a decided refusal and its
            // settlement (see `TEST_HOLD_AT_FORCE_REFUSAL`).
            #[cfg(any(test, feature = "test-support"))]
            if TEST_HOLD_AT_FORCE_REFUSAL.swap(false, std::sync::atomic::Ordering::SeqCst) {
                TEST_FORCE_REFUSAL_REACHED.store(true, std::sync::atomic::Ordering::SeqCst);
                while !TEST_RELEASE_FORCE_REFUSAL.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            let remedy = if matches!(terms.force, ForceTerms::Forced(_)) {
                "re-run the dry run for the current plan and force it with \
                 ceilings that cover it"
            } else {
                "re-run the dry run for the current plan, then pass force to \
                 accept it"
            };
            return Err(JobAbort::RefusedNeedsForce {
                msg: format!(
                    "the completed rewrite is not what the pre-build scan \
                     projected — data ingested after the scan carries values \
                     the plan never saw: {reason}. The cutover is refused and \
                     the corpus stands at its pre-repin generation; {remedy}"
                ),
                totals: job_totals(totals),
            });
        }

        // The point of no return: refusing a pending cancel, publishing the
        // Cutover marker and latching are one operation under the registry
        // lock. Everything below this block is forward-only.
        let marker = RepinMarker {
            phase: RepinPhase::Cutover,
            ..marker
        };
        let data_dir = &self.data_dir;
        // The closure is synchronous and stays that way: it runs while the
        // registry's mutex is held.
        match self
            .cancel
            .commit(job_id, || write_marker(data_dir, &marker))
        {
            Commit::Latched => {}
            Commit::Refused => {
                return Err(JobAbort::Cancelled {
                    stage: STAGE_FINAL_GATE,
                });
            }
            Commit::Failed(msg) => return Err(JobAbort::Failed(msg)),
        }

        // Emitted once the latch is held and before the swap: a marker write
        // that fails takes the cutover with it, and a cancel that won the
        // latch stops it, so neither reaches this line.
        if let ForceTerms::Forced(ceilings) = terms.force {
            tracing::info!(
                event_type = "repin_force_accepted",
                job_id,
                field = %field,
                from = from.as_catalog(),
                to = to.as_catalog(),
                dialect = reading.written.raw.token(),
                accepted_max_nulled_rows = ceilings.max_nulled,
                accepted_max_ambiguous_rows = ceilings.max_ambiguous,
                rows_nulled = totals.nulled,
                ambiguous_numerals = totals.ambiguous,
                scanned_projected_nulls = terms.scanned.nulled,
                scanned_ambiguous_numerals = terms.scanned.ambiguous,
                requested_by = terms.requested_by.as_deref(),
                "forced repin cutover within the ceilings it accepted"
            );
        }

        // Test-only: pin the window a cancel can only be refused in (see
        // `TEST_HOLD_AFTER_NO_RETURN`).
        #[cfg(any(test, feature = "test-support"))]
        if TEST_HOLD_AFTER_NO_RETURN.swap(false, std::sync::atomic::Ordering::SeqCst) {
            TEST_PAST_NO_RETURN.store(true, std::sync::atomic::Ordering::SeqCst);
            while !TEST_RELEASE_CUTOVER.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        if let Err(e) = swap_envs(&self.data_dir, &shadow, &aside_root(&self.data_dir)) {
            // Forward is the only direction past the marker: die
            // crash-consistent and let the marker replay finish the swap.
            // The swap's text names host paths and I/O errors, and no typed
            // cause comes with it, so it goes to DEBUG only.
            tracing::debug!(
                event_type = "repin_cutover_fatal",
                job_id,
                error = %e,
                "per-env swap failed mid-cutover; terminating so the boot \
                 marker replay completes it rather than serving a \
                 mixed-type corpus"
            );
            tracing::error!(
                event_type = "repin_cutover_fatal",
                job_id,
                error_class = "internal",
                cause_kind = CauseKind::Unknown.as_str(),
                "per-env swap failed mid-cutover; terminating so the boot \
                 marker replay completes it rather than serving a \
                 mixed-type corpus"
            );
            std::process::exit(1);
        }

        // The pin flip, transactional with the job's completion. Postgres
        // trouble here gets bounded retries, then the same forward-only
        // exit: the corpus already is the new generation.
        let mut flipped_ok = false;
        for attempt in 1..=FLIP_ATTEMPTS {
            match self.store.finish_cutover(job_id, field, to).await {
                Ok(outcome) => {
                    if outcome.cleared_ack {
                        tracing::info!(
                            event_type = "field_degraded_ack_cleared",
                            field = %field,
                            reason = "repin",
                            job_id,
                            "the acknowledged pin was repinned; the \
                             acknowledgement went with the evidence"
                        );
                    }
                    flipped_ok = true;
                    break;
                }
                Err(e) if attempt < FLIP_ATTEMPTS => {
                    log_store_fault(
                        Loudness::Warn,
                        StoreFault {
                            event_type: "repin_flip_retry",
                            job_id: Some(job_id),
                            attempt: Some(attempt),
                            ..StoreFault::default()
                        },
                        e,
                        "pin flip failed; retrying",
                    );
                    tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
                }
                Err(e) => {
                    log_store_fault(
                        Loudness::Error,
                        StoreFault {
                            event_type: "repin_cutover_fatal",
                            job_id: Some(job_id),
                            ..StoreFault::default()
                        },
                        e,
                        "pin flip unreachable after the swap; terminating so \
                         the boot marker replay completes it",
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
            // The text mixes serde's serialize wording with the staged
            // write's I/O wording and keeps no typed source.
            tracing::debug!(event_type = "repin_marker_error", error = %e, "cleanup marker write failed (boot replay covers it)");
            tracing::warn!(
                event_type = "repin_marker_error",
                error_class = "internal",
                cause_kind = CauseKind::Unknown.as_str(),
                "cleanup marker write failed (boot replay covers it)"
            );
        }
        drop(pool_guard);
        drop(corpus_gate);

        Self::record_outcome(&state);

        let data_dir = self.data_dir.clone();
        #[cfg(any(test, feature = "test-support"))]
        let holds = Arc::clone(&self.holds);
        if let Err(died) = on_blocking_pool("repin post-swap sweep", move || {
            #[cfg(any(test, feature = "test-support"))]
            holds.before_post_swap_sweep.reach_blocking(job_id);
            finish_post_swap_staging(&data_dir);
        })
        .await
        {
            tracing::warn!(event_type = "repin_sweep_failed", error = %died, "post-swap sweep task failed");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_pass(
        &self,
        field: &str,
        reading: RepinReading,
        flipped: &Arc<HashMap<String, CanonicalType>>,
        scanned: &Arc<ScanTallies>,
        state: &mut BuildState,
        cancel: &CancelHandle,
    ) -> Result<usize, JobAbort> {
        let data_dir = self.data_dir.clone();
        let shadow = shadow_root(&self.data_dir);
        let memory_limit = self.memory_limit.clone();
        let field = field.to_owned();
        let flipped = Arc::clone(flipped);
        let scanned = Arc::clone(scanned);
        let cancel = cancel.clone();
        let mut taken = std::mem::take(state);
        // A cancelled pass returns its state like any other, so the work
        // already staged is still described when the sweep runs.
        let (returned, changed) = on_blocking_pool("repin pass", move || {
            let changed = run_pass_blocking(
                &data_dir,
                &shadow,
                &memory_limit,
                &field,
                reading,
                &flipped,
                &scanned,
                &mut taken,
                &cancel,
            );
            (taken, changed)
        })
        .await
        .map_err(|died| JobAbort::Failed(died.to_string()))?;
        *state = returned;
        Ok(changed?)
    }

    /// Mid-build progress, best effort and deliberately lossy at the top of
    /// the range: this row is a display, so a count too large to store is
    /// better shown clamped than not shown at all. The staging write that
    /// freezes the same counts at the end refuses instead — see
    /// [`staged_totals`].
    async fn publish_progress(&self, job_id: i64, state: &BuildState) {
        let totals = state.totals();
        if let Err(e) = self.store.record_progress(job_id, job_totals(totals)).await {
            log_store_fault(
                Loudness::Warn,
                StoreFault {
                    event_type: "repin_store_error",
                    job_id: Some(job_id),
                    ..StoreFault::default()
                },
                e,
                "progress write failed",
            );
        }
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::CATALOG_REPIN_FILES_DONE).set(totals.files_done as f64);
    }

    fn record_outcome(state: &BuildState) {
        let totals = state.totals();
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_NULLED_TOTAL).increment(totals.nulled);
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_RESURRECTED_TOTAL)
            .increment(totals.resurrected);
    }
}

/// The calling task holds the repin permit across the await, so no timeout
/// may wrap a staging step (ADR-0011).
async fn on_blocking_pool<T, F>(what: &'static str, f: F) -> Result<T, StepDied>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StepDied::from_join(what, &e))
}

/// Typed rather than text so a request-facing step can answer a panic as
/// [`ServerError::Panicked`]: the detached decision task completes normally
/// around a panicked blocking step, so its own join never reports one.
/// Neither variant holds the payload: `JoinError`'s `Display` quotes it,
/// so the join error is read for its kind and dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepDied {
    Panicked(&'static str),
    Cancelled(&'static str),
}

impl StepDied {
    fn from_join(what: &'static str, e: &tokio::task::JoinError) -> Self {
        if e.is_panic() {
            Self::Panicked(what)
        } else {
            Self::Cancelled(what)
        }
    }
}

impl std::fmt::Display for StepDied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked(what) => write!(f, "{what} task panicked"),
            Self::Cancelled(what) => write!(f, "{what} task was cancelled"),
        }
    }
}

impl From<StepDied> for ServerError {
    fn from(died: StepDied) -> Self {
        match died {
            StepDied::Panicked(what) => Self::Panicked(what),
            StepDied::Cancelled(_) => Self::Internal(died.to_string()),
        }
    }
}

fn job_totals(totals: BuildTotals) -> JobTotals {
    let clamp = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    JobTotals {
        files_done: clamp(totals.files_done),
        rows_rewritten: clamp(totals.rows),
        rows_nulled: clamp(totals.nulled),
        rows_resurrected: clamp(totals.resurrected),
        ambiguous_numerals: clamp(totals.ambiguous),
    }
}

impl RepinEngine {
    /// Why the cutover must not swap while publication markers are pending
    /// (ADR-0041), or `None` when there are none. Call under the cutover
    /// guard.
    ///
    /// A marker proves its publish by the canonical output's identity. The
    /// swap replaces every canonical file with its rewritten copy, so after
    /// it recovery could no longer tell a finished publish from an
    /// unfinished one.
    async fn pending_publication(&self, job_id: i64) -> Option<String> {
        let wal_dir = self.wal_dir.clone();
        let scan = on_blocking_pool("repin publication claims", move || {
            crate::ingest::publication_marker::scan_claims(&wal_dir)
        })
        .await;
        let refusal = match scan {
            Ok(Ok(claims)) if !claims.any() => return None,
            Ok(Ok(_)) => "publication_marker_pending",
            Ok(Err(e)) => {
                // `scan_claims` hands back text, so the kind is unknown here.
                tracing::debug!(
                    event_type = "repin_publication_claims_unreadable",
                    job_id,
                    error = %e,
                    "cannot read publication markers at cutover"
                );
                tracing::error!(
                    event_type = "repin_publication_claims_unreadable",
                    job_id,
                    cause_kind = CauseKind::Unknown.as_str(),
                    "cannot read publication markers at cutover"
                );
                "publication_markers_unreadable"
            }
            Err(died) => {
                tracing::error!(
                    event_type = "repin_publication_claims_unreadable",
                    job_id,
                    error = %died,
                    "the publication marker scan did not return at cutover"
                );
                "publication_markers_unreadable"
            }
        };
        tracing::warn!(
            event_type = "repin_cutover_refused",
            job_id,
            reason = refusal,
            "cutover refused while publication markers are pending or unreadable"
        );
        Some(format!(
            "cutover refused ({refusal}): a compaction publish may be waiting \
             for recovery, and swapping the corpus now would leave it \
             unprovable. The corpus stands at its pre-repin generation; check \
             trawl_publication_recovery_total and the publication_recovery_failed \
             log, then retry the repin once recovery completes"
        ))
    }
}

enum JobAbort {
    Failed(String),
    Blocked(String),
    /// The finished shadow nulled values the pre-build scan did not
    /// project (concurrent ingest), or more than the accepted ceilings
    /// allow. Carries the tallies the gate decided on so the terminal
    /// write can persist the verdict and its evidence together.
    RefusedNeedsForce {
        msg: String,
        totals: JobTotals,
    },
    Cancelled {
        stage: &'static str,
    },
}

impl From<PassStop> for JobAbort {
    fn from(stop: PassStop) -> Self {
        match stop {
            PassStop::Cancelled { stage } => Self::Cancelled { stage },
            PassStop::Failed(msg) => Self::Failed(msg),
        }
    }
}

fn cancelled_error(actor: &str, stage: &str) -> String {
    format!("cancelled by {actor} during {stage}; the live corpus was never touched")
}

#[derive(Debug, Clone, Copy)]
enum Loudness {
    Warn,
    Error,
}

#[derive(Debug, Default, Clone, Copy)]
struct StoreFault<'a> {
    event_type: &'static str,
    job_id: Option<i64>,
    attempt: Option<u32>,
    field: Option<&'a str>,
}

/// A pinned field's name can come from an ingest sender, so it goes to DEBUG
/// only.
fn log_store_fault(level: Loudness, at: StoreFault<'_>, err: StoreError, what: &'static str) {
    let StoreFault {
        event_type,
        job_id,
        attempt,
        field,
    } = at;
    tracing::debug!(event_type, job_id, attempt, field, error = %err, "{what}");
    let fault = ServerError::from(err);
    let (error_class, cause_kind) = (fault.error_class(), fault.cause_kind().as_str());
    match level {
        Loudness::Warn => tracing::warn!(
            event_type,
            job_id,
            attempt,
            error_class,
            cause_kind,
            "{what}"
        ),
        Loudness::Error => tracing::error!(
            event_type,
            job_id,
            attempt,
            error_class,
            cause_kind,
            "{what}"
        ),
    }
}

fn fault_row(job_id: i64, what: &'static str, err: &ServerError) -> String {
    tracing::debug!(
        event_type = "repin_job_failed",
        job_id,
        error = %err,
        "{what}"
    );
    err.safe_message()
}

fn disk_check_row(job_id: i64, err: &std::io::Error) -> String {
    tracing::debug!(
        event_type = "repin_job_failed",
        job_id,
        error = %err,
        "could not check free disk space for the repin"
    );
    format!(
        "could not check free disk space for the repin ({})",
        err.kind()
    )
}

#[derive(Debug, Clone, Copy, Default)]
struct BuildTotals {
    files_done: u64,
    rows: u64,
    nulled: u64,
    resurrected: u64,
    ambiguous: u64,
}

#[derive(Debug, Default)]
struct BuildState {
    processed: BTreeMap<PathBuf, FileSig>,
    results: BTreeMap<PathBuf, ProcessTally>,
}

impl BuildState {
    fn files_rewritten(&self) -> u64 {
        self.results.values().filter(|t| t.rewritten).count() as u64
    }

    /// The shadow's tallies over its current contents: a caught-up
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

/// One count as the job row stores it, refusing rather than clamping.
fn checked_count(what: &str, value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| {
        format!("the rewrite's {what} count ({value}) does not fit the job row; refusing to stage")
    })
}

fn staged_totals(state: &BuildState) -> Result<JobTotals, String> {
    let totals = state.totals();
    Ok(JobTotals {
        files_done: checked_count("files_done", totals.files_done)?,
        rows_rewritten: checked_count("rows_rewritten", totals.rows)?,
        rows_nulled: checked_count("rows_nulled", totals.nulled)?,
        rows_resurrected: checked_count("rows_resurrected", totals.resurrected)?,
        ambiguous_numerals: checked_count("ambiguous_numerals", totals.ambiguous)?,
    })
}

/// The per-service null tallies the cutover stages, worst first.
fn staged_tallies(state: &BuildState) -> Result<Vec<(String, i64)>, String> {
    let mut tallies: Vec<(String, u64)> = state.nulled_by_service().into_iter().collect();
    tallies.sort_by(|(a_service, a_rows), (b_service, b_rows)| {
        b_rows.cmp(a_rows).then_with(|| a_service.cmp(b_service))
    });
    tallies.truncate(usize::try_from(MAX_CONFLICTS_PER_FIELD).unwrap_or(usize::MAX));
    tallies
        .into_iter()
        .map(|(service, rows)| Ok((service, checked_count("per-service rows_nulled", rows)?)))
        .collect()
}

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
    cancel: &CancelHandle,
) -> Result<usize, PassStop> {
    cancel.check(STAGE_BUILD)?;
    let sources = snapshot_env_files(data_dir)?;

    // Test-only happened-before edge. This is a std thread inside a
    // blocking pass, so the test's drop guard must free it when the test
    // fails: the runtime's blocking pool waits for it on teardown.
    #[cfg(any(test, feature = "test-support"))]
    if TEST_BARRIER_FIRST_PASS.swap(false, std::sync::atomic::Ordering::SeqCst) {
        TEST_SNAPSHOT_TAKEN.store(true, std::sync::atomic::Ordering::SeqCst);
        while !TEST_RELEASE_BUILD.load(std::sync::atomic::Ordering::SeqCst) {
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
        cancel.check(STAGE_BUILD)?;
        let _ = std::fs::remove_file(shadow.join(rel));
        state.processed.remove(rel);
        state.results.remove(rel);
        cancel.check(STAGE_BUILD)?;
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
        cancel.check(STAGE_BUILD)?;
        #[cfg(any(test, feature = "test-support"))]
        {
            let delay = TEST_FILE_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
        }
        let precounted = scanned
            .get(&rel)
            .and_then(|(scanned_sig, effect)| (*scanned_sig == sig).then_some(*effect));
        let tally = process_file(
            &conn, data_dir, shadow, &rel, field, reading, flipped, precounted,
        )?;
        state.results.insert(rel.clone(), tally);
        state.processed.insert(rel, sig);
        cancel.check(STAGE_BUILD)?;
    }
    Ok(changed)
}

/// Takes the pin and the asserted dialect rather than a `RepinTarget`,
/// because the wire asker reads a stored job row: `RepinTarget` is the
/// engine's to construct (it alone knows the old pin), and re-deriving one
/// from a row would be exactly the second opinion this function exists to
/// prevent.
pub(crate) fn force_refusal(
    pin: CanonicalType,
    dialect: Option<trawl_core::severity::Dialect>,
    nulled: u64,
    ambiguous: u64,
    terms: ForceTerms,
) -> Option<String> {
    if let ForceTerms::Forced(ceilings) = terms {
        return ceiling::exceeds(pin, dialect, ceilings, nulled, ambiguous)
            .map(|over| over.to_string());
    }
    if nulled > 0 {
        return Some(format!(
            "{nulled} stored value(s) cannot be read as {} and would be \
             nulled (the originals stay findable in _raw)",
            pin.as_catalog()
        ));
    }
    if ceiling::ambiguity_binds(pin, dialect) && ambiguous > 0 {
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

fn validate_ceilings(
    force: bool,
    requested: RequestedCeilings,
) -> Result<(Option<i64>, Option<i64>), ServerError> {
    if !force && (requested.max_nulled.is_some() || requested.max_ambiguous.is_some()) {
        return Err(ServerError::BadRequest(
            "a ceiling only means something beside force: max_nulled_rows \
             and max_ambiguous_rows state what a forced repin accepts, and \
             an unforced repin accepts no loss at all"
                .into(),
        ));
    }
    let fit = |value: Option<u64>, name: &str| {
        value
            .map(|v| {
                i64::try_from(v).map_err(|_| {
                    ServerError::BadRequest(format!(
                        "{name} of {v} is larger than the largest row count \
                         trawl can record ({})",
                        i64::MAX
                    ))
                })
            })
            .transpose()
    };
    Ok((
        fit(requested.max_nulled, "max_nulled_rows")?,
        fit(requested.max_ambiguous, "max_ambiguous_rows")?,
    ))
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

    #[tokio::test]
    async fn a_panicked_blocking_step_never_carries_its_payload() {
        let died = on_blocking_pool::<(), _>("repin probe", || panic!("zz_repin_payload_sentinel"))
            .await
            .expect_err("the step panics");
        assert_eq!(died, StepDied::Panicked("repin probe"));
        assert_eq!(died.to_string(), "repin probe task panicked");
        let err = ServerError::from(died);
        assert!(
            matches!(err, ServerError::Panicked("repin probe")),
            "got {err:?}"
        );
        assert!(!format!("{died:?} {err} {err:?}").contains("zz_repin_payload_sentinel"));
    }

    #[tokio::test]
    async fn a_panicked_decision_task_never_carries_its_payload() {
        let panicked = tokio::spawn(async { panic!("zz_repin_payload_sentinel") })
            .await
            .expect_err("the task panics");
        let msg = StepDied::from_join("repin job", &panicked).to_string();
        assert!(!msg.contains("zz_repin_payload_sentinel"), "{msg}");
        assert_eq!(msg, "repin job task panicked");

        let pending = tokio::spawn(std::future::pending::<()>());
        pending.abort();
        let cancelled = pending.await.expect_err("the task is cancelled");
        let died = StepDied::from_join("repin job", &cancelled);
        assert_eq!(died.to_string(), "repin job task was cancelled");
        assert!(
            matches!(ServerError::from(died), ServerError::Internal(msg) if msg == "repin job task was cancelled")
        );
    }

    async fn assert_rollup_admission_waits_for_unit(failed: bool) {
        let coordinator = Arc::new(RepinCoordinator::new());
        let publication = crate::publication::PublicationGate::new();
        let registry = Arc::new(CancelRegistry::default());
        let cancel = registry.arm(1);
        let active = coordinator.rollup_unit_guard().await.unwrap();
        let mut admission = Box::pin(claim_rollup_pause(&coordinator, &publication, &cancel));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), admission.as_mut())
                .await
                .is_err(),
            "repin must wait for the active unit before claiming its pause"
        );
        assert!(!coordinator.rollup_paused());
        let root = tempfile::tempdir().unwrap();
        if failed {
            let _writer = publication.write().await;
            let marker = root.path().join(".rollup-api");
            std::fs::write(&marker, "unfinished").unwrap();
            drop(publication.mark_rollup(&marker));
        }
        drop(active);
        let result = tokio::time::timeout(Duration::from_secs(1), admission)
            .await
            .expect("admission must observe the completed unit");
        if failed {
            assert!(matches!(
                result,
                Err(AdmissionStop::Blocked {
                    refusal: ServerError::CorpusRecovering(CorpusUnsettled::RollupPending),
                    ..
                })
            ));
            assert!(!coordinator.rollup_paused());
        } else {
            let pause = result.unwrap();
            assert!(coordinator.rollup_unit_guard().await.is_none());
            drop(pause);
            assert!(coordinator.rollup_unit_guard().await.is_some());
        }
    }

    #[tokio::test]
    async fn repin_admission_waits_for_successful_rollup_before_pausing() {
        assert_rollup_admission_waits_for_unit(false).await;
    }

    #[tokio::test]
    async fn repin_admission_observes_rollup_failure_before_pausing() {
        assert_rollup_admission_waits_for_unit(true).await;
    }

    #[tokio::test]
    async fn repin_admission_names_why_the_corpus_is_unsettled() {
        let admit = |publication: crate::publication::PublicationGate| async move {
            let coordinator = Arc::new(RepinCoordinator::new());
            let registry = Arc::new(CancelRegistry::default());
            let cancel = registry.arm(1);
            let result = claim_rollup_pause(&coordinator, &publication, &cancel).await;
            assert!(!coordinator.rollup_paused());
            match result {
                Err(AdmissionStop::Blocked {
                    row,
                    refusal: ServerError::CorpusRecovering(reason),
                }) => {
                    assert_eq!(row, repin_blocked(reason));
                    row
                }
                Err(other) => panic!("expected the corpus refusal, got {other:?}"),
                Ok(_) => panic!("an unsettled corpus admitted a repin"),
            }
        };
        let restart = "repin is blocked while the server finishes loading data from before its \
                       restart; retry once it settles";
        let rollup =
            "repin is blocked while rollup publication is incomplete; retry after recovery";

        assert_eq!(
            admit(crate::publication::PublicationGate::starting()).await,
            restart
        );
        let overhang = crate::publication::PublicationGate::starting();
        overhang.finish_hydration(true).unwrap();
        assert_eq!(admit(overhang).await, restart);

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".rollup-api");
        std::fs::write(&marker, "unfinished").unwrap();
        for starting in [false, true] {
            let gate = if starting {
                crate::publication::PublicationGate::starting()
            } else {
                crate::publication::PublicationGate::new()
            };
            {
                let _writer = gate.write().await;
                drop(gate.mark_rollup(&marker));
            }
            assert_eq!(admit(gate).await, rollup, "starting: {starting}");
        }
    }

    #[tokio::test]
    async fn rollup_unlink_sync_retry_refuses_repin_admission() {
        let coordinator = Arc::new(RepinCoordinator::new());
        let publication = crate::publication::PublicationGate::new();
        let registry = Arc::new(CancelRegistry::default());
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("prod/2026-11-06");
        std::fs::create_dir_all(&day).unwrap();
        let marker = day.join(".rollup-api");
        {
            let _writer = publication.write().await;
            let removal = publication.owe_removal_sync(&marker);
            assert!(!marker.exists(), "the marker path is already gone");
            assert!(
                removal
                    .settle(|_| Err(std::io::ErrorKind::StorageFull.into()))
                    .is_err()
            );
        }
        assert!(publication.read().await.is_ok(), "reads answer while owed");

        let cancel = registry.arm(1);
        match claim_rollup_pause(&coordinator, &publication, &cancel).await {
            Err(AdmissionStop::Blocked {
                row,
                refusal: ServerError::ServiceUnavailable(text),
            }) => {
                assert_eq!(row, REMOVAL_SYNC_OWED);
                assert_eq!(text, REMOVAL_SYNC_OWED);
                assert!(!text.contains(dir.path().to_str().unwrap()));
            }
            Err(other) => panic!("expected the removal-sync refusal, got {other:?}"),
            Ok(_) => panic!("an owed removal sync admitted a repin"),
        }
        assert!(!coordinator.rollup_paused(), "no pause was claimed");
        assert!(coordinator.rollup_unit_guard().await.is_some());

        assert_eq!(publication.retry_removal_syncs(crate::epoch::fsync_dir), 0);
        let pause = claim_rollup_pause(&coordinator, &publication, &cancel)
            .await
            .expect("a settled obligation admits the repin");
        assert!(coordinator.rollup_paused());
        drop(pause);
    }

    #[tokio::test]
    async fn repin_admission_can_cancel_while_waiting_for_publication() {
        let coordinator = Arc::new(RepinCoordinator::new());
        let publication = crate::publication::PublicationGate::new();
        let registry = Arc::new(CancelRegistry::default());
        let cancel = registry.arm(1);
        let _writer = publication.write().await;
        let mut admission = Box::pin(claim_rollup_pause(&coordinator, &publication, &cancel));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), admission.as_mut())
                .await
                .is_err()
        );
        registry.request(&CancelActor::new("operator", "test-key"));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), admission)
                .await
                .unwrap(),
            Err(AdmissionStop::Cancelled { stage: STAGE_SCAN })
        ));
        assert!(!coordinator.rollup_paused());
        let _corpus = tokio::time::timeout(Duration::from_secs(1), coordinator.compaction_guard())
            .await
            .expect("cancellation releases the corpus guard while publication is still held");
    }

    #[tokio::test(start_paused = true)]
    async fn repin_admission_wait_has_a_deadline() {
        let coordinator = Arc::new(RepinCoordinator::new());
        let publication = crate::publication::PublicationGate::new();
        let registry = Arc::new(CancelRegistry::default());
        let cancel = registry.arm(1);
        let _active = coordinator.rollup_unit_guard().await.unwrap();
        let started = tokio::time::Instant::now();
        assert!(matches!(
            claim_rollup_pause(&coordinator, &publication, &cancel).await,
            Err(AdmissionStop::Blocked {
                refusal: ServerError::ServiceUnavailable(_),
                ..
            })
        ));
        assert_eq!(started.elapsed(), CUTOVER_DRAIN_TIMEOUT);
        assert!(!coordinator.rollup_paused());
    }

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

    #[test]
    fn the_force_gate_refuses_loss_always_and_ambiguity_under_otel_only() {
        use trawl_core::severity::Dialect;

        const SEVERITY: CanonicalType = CanonicalType::Severity;
        const VARCHAR: CanonicalType = CanonicalType::Varchar;
        let unforced = ForceTerms::unforced();
        let bounded = ForceTerms::forced(ceiling::Ceilings {
            max_nulled: 3,
            max_ambiguous: 2,
        });

        assert!(
            force_refusal(VARCHAR, None, 3, 0, unforced)
                .is_some_and(|m| m.contains("cannot be read as VARCHAR")),
        );
        assert_eq!(force_refusal(VARCHAR, None, 3, 0, bounded), None);
        assert!(force_refusal(SEVERITY, Some(Dialect::Syslog), 3, 0, unforced).is_some());

        let refusal = force_refusal(SEVERITY, Some(Dialect::Otel), 0, 2, unforced)
            .expect("an OTel repin over the 1-7 overlap must refuse");
        assert!(refusal.contains("2 row(s)"), "{refusal}");
        assert!(refusal.contains("dialect=syslog"), "{refusal}");
        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Syslog), 0, 2, unforced),
            None,
            "asserting syslog IS the answer to the ambiguity"
        );
        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Otel), 0, 2, bounded),
            None
        );
        // A row with no recorded dialect at all reads as the OTel default:
        // the gate must not go silent because a column is NULL.
        assert!(force_refusal(SEVERITY, None, 0, 2, unforced).is_some());

        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Otel), 0, 0, unforced),
            None
        );
        assert_eq!(force_refusal(VARCHAR, None, 0, 5, unforced), None);

        // Loss is reported first: it is the larger hazard, and a plan
        // carrying both needs one force flag, not a ladder of them.
        let both = force_refusal(SEVERITY, Some(Dialect::Otel), 1, 1, unforced).unwrap();
        assert!(both.contains("cannot be read as SEVERITY"), "{both}");
    }

    #[test]
    fn a_forced_job_is_held_to_the_ceilings_it_accepted() {
        use crate::repin::ceiling::Ceilings;
        use trawl_core::severity::Dialect;

        const SEVERITY: CanonicalType = CanonicalType::Severity;
        const VARCHAR: CanonicalType = CanonicalType::Varchar;
        let terms = ForceTerms::forced(Ceilings {
            max_nulled: 10,
            max_ambiguous: 2,
        });

        assert_eq!(
            force_refusal(VARCHAR, None, 10, 0, terms),
            None,
            "the accepted number itself passes"
        );
        let over = force_refusal(VARCHAR, None, 11, 0, terms).expect("one row above refuses");
        assert!(over.contains("accepted 10"), "{over}");
        assert!(
            over.contains("11 nulled row(s)"),
            "the sentence names the actual too: {over}"
        );

        assert!(force_refusal(SEVERITY, Some(Dialect::Otel), 0, 3, terms).is_some());
        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Syslog), 0, 3, terms),
            None
        );
    }

    #[test]
    fn a_ceiling_needs_force_and_must_fit_the_column() {
        let stated = RequestedCeilings {
            max_nulled: Some(5),
            max_ambiguous: None,
        };
        let err = validate_ceilings(false, stated).expect_err("a ceiling without force is refused");
        assert!(
            matches!(&err, ServerError::BadRequest(m) if m.contains("max_nulled_rows")),
            "{err:?}"
        );
        assert!(
            matches!(
                validate_ceilings(
                    false,
                    RequestedCeilings {
                        max_nulled: None,
                        max_ambiguous: Some(0),
                    }
                ),
                Err(ServerError::BadRequest(_))
            ),
            "either dimension alone is enough to contradict an unforced request"
        );

        assert_eq!(
            validate_ceilings(true, stated).expect("forced and in range"),
            (Some(5), None)
        );
        assert_eq!(
            validate_ceilings(true, RequestedCeilings::default()).expect("nothing stated"),
            (None, None)
        );

        let huge = RequestedCeilings {
            max_nulled: Some(u64::MAX),
            max_ambiguous: None,
        };
        let err = validate_ceilings(true, huge).expect_err("past i64 is refused, never clamped");
        assert!(
            matches!(&err, ServerError::BadRequest(m) if m.contains("larger than")),
            "{err:?}"
        );
    }

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

    #[test]
    fn decide_fetches_the_response_row_before_spawning_the_background_half() {
        const SOURCE: &str = include_str!("engine.rs");
        let start = SOURCE
            .find("    async fn decide(")
            .expect("decide is still a method on the engine");
        let end = SOURCE[start..]
            .find("    async fn run_scan(")
            .expect("run_scan still follows decide")
            + start;
        let body = &SOURCE[start..end];

        let spawn = body
            .find(".run_job(")
            .expect("decide still spawns the background half");
        let fetch = body
            .find("self.job(job_id).await")
            .expect("decide still reads the response row");
        assert!(
            fetch < spawn,
            "the response row must be read before the background half is \
             spawned, so a failed read has no detached job to strand"
        );
        assert!(
            !body[spawn..].contains("self.job("),
            "nothing may read the job row after the spawn: a failure there \
             is reported as a synchronous one and disarms a running job"
        );
    }

    #[test]
    fn a_panicked_decision_settles_before_it_touches_the_store() {
        const SOURCE: &str = include_str!("engine.rs");
        let start = SOURCE
            .find("    pub async fn start(")
            .expect("start is still a method on the engine");
        let end = SOURCE[start..]
            .find("    pub fn cancel(")
            .expect("the cancel entry point still follows start")
            + start;
        let body = &SOURCE[start..end];
        let panic_arm = body
            .find("let outcome = match decided.await")
            .expect("start still joins the detached decision task");
        let arm = &body[panic_arm..];

        let settle = arm
            .find("self.cancel.settle(job_id)")
            .expect("the panic arm still settles the registry");
        let finish = arm
            .find("self.finish(job_id")
            .expect("the panic arm still terminalizes the row");
        assert!(
            settle < finish,
            "the registry must be settled before the terminal write, which \
             can spend seconds retrying a struggling store"
        );
        assert!(
            !arm[..finish].contains("audit_cancelled("),
            "a panic is an unobserved failure: no `repin_cancelled` event, \
             because no boundary saw the request and no unwind ran"
        );
    }

    #[test]
    fn a_panicked_decision_records_a_pending_cancel_before_the_terminal_write() {
        const SOURCE: &str = include_str!("engine.rs");
        let start = SOURCE
            .find("    pub async fn start(")
            .expect("start is still a method on the engine");
        let end = SOURCE[start..]
            .find("    pub fn cancel(")
            .expect("the cancel entry point still follows start")
            + start;
        let body = &SOURCE[start..end];
        let panic_arm = body
            .find("let outcome = match decided.await")
            .expect("start still joins the detached decision task");
        let arm = &body[panic_arm..];

        let record = arm
            .find("self.store.record_cancel_request(job_id, actor.name()).await")
            .expect("the panic arm still records a pending cancel request");
        let finish = arm
            .find("self.finish(job_id")
            .expect("the panic arm still terminalizes the row");
        assert!(
            record < finish,
            "the request must be recorded before the terminal write: a \
             `failed` row takes no later request, and the detached recorder \
             would silently update nothing"
        );
    }

    #[test]
    fn a_pass_stop_keeps_cancellation_and_failure_distinct() {
        assert!(matches!(
            JobAbort::from(PassStop::Cancelled { stage: STAGE_BUILD }),
            JobAbort::Cancelled { stage } if stage == STAGE_BUILD
        ));
        assert!(matches!(
            JobAbort::from(PassStop::Failed("read_parquet failed".to_owned())),
            JobAbort::Failed(msg) if msg == "read_parquet failed"
        ));
    }

    #[test]
    fn repin_terminal_rows_store_public_messages() {
        use trawl_engine::error::EngineError;

        const MARKER: &str = "/var/lib/trawl/data/MARKER-repin-row";
        const SOURCE: &str = include_str!("engine.rs");
        let faults = [
            ServerError::Store(StoreError::Unavailable(sqlx::Error::Protocol(
                MARKER.into(),
            ))),
            ServerError::Engine(EngineError::Io(std::io::Error::other(MARKER))),
            ServerError::Internal(format!("read {MARKER} failed")),
        ];
        for fault in &faults {
            let row = fault_row(7, "could not record the repin plan", fault);
            assert_eq!(row, fault.safe_message(), "{fault:?}");
            assert!(!row.contains("MARKER"), "{fault:?}: {row}");
        }

        let io = std::io::Error::other(MARKER);
        let row = disk_check_row(7, &io);
        assert_eq!(
            row,
            "could not check free disk space for the repin (other error)"
        );
        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, MARKER);
        let row = disk_check_row(7, &denied);
        assert!(row.contains("permission denied"), "{row}");
        assert!(!row.contains("MARKER"), "{row}");

        let start = SOURCE
            .find("    async fn decide(")
            .expect("decide is still a method on the engine");
        let end = SOURCE[start..]
            .find("    async fn run_scan(")
            .expect("run_scan still follows decide")
            + start;
        let decide = &SOURCE[start..end];
        // The positive control: the range is the one that builds the rows,
        // so the absence checks below can see a leak if one is there.
        assert!(
            decide.contains("disk_check_row(") && decide.contains(" fault_row("),
            "decide must build its terminal rows through the two helpers"
        );
        for leak in [
            "Some(&e.to_string())",
            "free disk space: {e}",
            "claimed repin job row: {e}",
        ] {
            assert!(
                !decide.contains(leak),
                "decide builds a terminal row from an error's Display: {leak}"
            );
        }
        assert!(
            !SOURCE.contains(concat!("cutover drain failed: {", "e}")),
            "the cutover drain row is built from the pool error's Display"
        );
    }

    #[test]
    fn repin_store_fault_events_carry_no_error_text() {
        use crate::test_events::events_of;
        use tracing::Level;

        const MARKER: &str = "/var/lib/trawl/data/MARKER-repin-store";
        // A pinned field's name can come from an ingest sender, so it is
        // content: DEBUG only, like the driver text.
        const FIELD: &str = "zz_field_marker";
        const SOURCE: &str = include_str!("engine.rs");
        let planted = || StoreError::Unavailable(sqlx::Error::Protocol(MARKER.into()));
        let expected = ServerError::from(planted());
        let (class, cause) = (expected.error_class(), expected.cause_kind().as_str());

        for (loudness, level, at) in [
            (
                Loudness::Warn,
                Level::WARN,
                StoreFault {
                    event_type: "catalog_bookkeeping_error",
                    field: Some(FIELD),
                    ..StoreFault::default()
                },
            ),
            (
                Loudness::Warn,
                Level::WARN,
                StoreFault {
                    event_type: "repin_store_retry",
                    job_id: Some(7),
                    attempt: Some(1),
                    ..StoreFault::default()
                },
            ),
            (
                Loudness::Error,
                Level::ERROR,
                StoreFault {
                    event_type: "repin_store_error",
                    job_id: Some(7),
                    ..StoreFault::default()
                },
            ),
        ] {
            let (event_type, at_field) = (at.event_type, at.field.is_some());
            let events = events_of(|| log_store_fault(loudness, at, planted(), "a repin fault"));
            let loud: Vec<_> = events.iter().filter(|e| e.is_default_level()).collect();
            assert_eq!(loud.len(), 1, "{event_type}: {events:?}");
            let event = loud[0];
            assert_eq!(event.level, level, "{event:?}");
            assert_eq!(
                event.fields.get("event_type").map(String::as_str),
                Some(event_type),
                "{event:?}"
            );
            assert_eq!(
                event.fields.get("error_class").map(String::as_str),
                Some(class),
                "{event:?}"
            );
            assert_eq!(
                event.fields.get("cause_kind").map(String::as_str),
                Some(cause),
                "{event:?}"
            );
            assert!(!event.fields.contains_key("error"), "{event:?}");
            assert!(!event.text().contains("MARKER"), "{event:?}");
            assert!(!event.fields.contains_key("field"), "{event:?}");
            assert!(!event.text().contains(FIELD), "{event:?}");

            let debug = events
                .iter()
                .find(|e| e.level == Level::DEBUG)
                .unwrap_or_else(|| panic!("{event_type}: no DEBUG event: {events:?}"));
            assert!(debug.text().contains(MARKER), "{debug:?}");
            if at_field {
                assert!(debug.text().contains(FIELD), "{debug:?}");
            }
        }

        for (from, to) in [
            ("    async fn field_liveness(", "    async fn job("),
            ("    async fn finish(", "    async fn cancelled_outcome("),
        ] {
            let start = SOURCE.find(from).expect(from);
            let end = SOURCE[start..].find(to).expect(to) + start;
            let body = &SOURCE[start..end];
            assert!(
                body.contains("log_store_fault("),
                "{from} must log its store faults through the helper"
            );
            assert!(
                !body.contains(concat!("error = ", "%"))
                    && !body.contains(concat!("error = ", "?")),
                "{from} logs a store error's text itself"
            );
        }
    }

    /// Pinned because this sentence is what the CLI prints and what an
    /// integration test asserts on the row.
    #[test]
    fn the_cancelled_row_names_the_actor_the_stage_and_the_untouched_corpus() {
        assert_eq!(
            cancelled_error("ops-key", STAGE_BUILD),
            "cancelled by ops-key during build; the live corpus was never touched"
        );
        for stage in [STAGE_SCAN, STAGE_BUILD, STAGE_FINAL_GATE] {
            let msg = cancelled_error("ops-key", stage);
            assert!(msg.contains(stage), "{msg}");
            assert!(msg.contains("never touched"), "{msg}");
        }
    }

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
