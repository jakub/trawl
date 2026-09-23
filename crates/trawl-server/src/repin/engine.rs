// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin job lifecycle: validation → claim → scan → (dry-run report |
//! force gate | background build) → additive catch-up → exclusion-guarded
//! cutover → sweep.
//!
//! Every job, dry or real, runs the same scan with the same expressions the
//! rewrite writes; "mandatory dry run" and the force gate are one code path.
//! The gate is then re-asked of the finished shadow under the cutover
//! exclusion, because the scan describes a corpus that ingest and compaction
//! keep changing underneath the build: a file written after the scan can
//! carry values the new pin cannot read, and only the shadow's own
//! accounting can be what the omitted force flag governs.
//!
//! The build stages the new generation in a sibling shadow root
//! (`marker.rs` explains why it cannot live inside the data root), the
//! catch-up loop folds in files compaction writes meanwhile (additive by
//! construction: the rollup is paused for the whole job), and the cutover
//! holds both exclusion primitives, the corpus gate against compaction and
//! pool exclusivity against every parquet-reading query lane, across the
//! final increment, the per-env swap and the pin flip. That exclusion is
//! not optional: probes showed a mixed-type corpus does not error, it
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
use crate::repin::marker::{
    RepinMarker, RepinPhase, aside_root, remove_marker, shadow_root, write_marker,
};
use crate::repin::plan::{ScanCounts, ScanTallies, scan};
use crate::repin::rewrite::{FileSig, ProcessTally, process_file, snapshot_env_files};
use crate::store::{
    CatalogStore, JobTotals, MAX_CONFLICTS_PER_FIELD, RepinJob, RepinJobStatus, RepinStore,
};

/// Catch-up passes before the job gives up (steadily-shrinking deltas
/// converge in two or three; a delta that refuses to shrink under real
/// ingest volume must fail cleanly rather than loop with the rollup
/// suppressed forever).
const MAX_CATCHUP_PASSES: usize = 8;

/// How long admission waits for an active rollup, or cutover waits for
/// every query permit, before aborting to `blocked`. A wedged operation
/// must not leave a repin waiting indefinitely with retention suppressed.
const CUTOVER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Attempts at the post-swap postgres flip before giving up the process
/// (the marker replay completes it at the next boot).
const FLIP_ATTEMPTS: u32 = 3;

/// Cadence of the detached terminal-write retry after the fast attempts
/// in [`RepinEngine::finish`] are exhausted (a real store outage).
const FINISH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Wait out an active rollup, validate its completed publication, then claim
/// the pause before another unit can start. The corpus guard comes first,
/// matching the rollup's lock order. Neither lock survives this admission.
async fn claim_rollup_pause(
    coordinator: &Arc<RepinCoordinator>,
    publication: &crate::publication::PublicationGate,
    cancel: &CancelHandle,
) -> Result<RollupPause, PassStop> {
    cancel.check(STAGE_SCAN)?;
    let admission = async {
        let _corpus = coordinator.cutover_guard().await;
        let _publication = publication.read().await.map_err(|_| {
            PassStop::Failed(
                "repin is blocked while rollup publication is incomplete; retry after recovery"
                    .to_owned(),
            )
        })?;
        cancel.check(STAGE_SCAN)?;
        Ok(coordinator.pause_rollup())
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
            )),
        }
    }
}

/// The terminal-outcome counter, incremented only once the terminal write
/// has actually landed (see [`RepinEngine::finish`]).
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
///
/// A test that ingests while the build merely runs slowly proves nothing
/// about catch-up: pass 0's own snapshot may already have seen the new
/// file, and the assertion would hold even if catch-up passes read the
/// wrong dialect. With the barrier the file provably lands after the
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

/// Test-only hold at the first published progress, pinning the one state a
/// mid-job observer needs: the job row `running`, the running gauge up, and
/// `files_done` already ≥ 1, with no exclusion primitive held, so queries
/// and ingest still work exactly as they do mid-build.
///
/// Progress is published per pass, not per file, so the window where
/// "running and `files_done` ≥ 1" holds opens only when pass 0 finishes and
/// closes when the job terminalizes. A polling observer can miss it
/// entirely, or find the job already terminal on its first read, which is
/// timing rather than behaviour. Holding the job at that point makes the
/// observation an ordering instead of a race: the state is pinned until the
/// test that wants to see it says so.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_AFTER_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the build once it is holding at published progress.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_PROGRESS_PUBLISHED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the test to let the held job continue to catch-up and cutover.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_JOB: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hold immediately past the point of no return: the latch is
/// taken and the Cutover marker is published, but no env has been swapped
/// yet (#109).
///
/// A cancel arriving in that window must be refused, and the refusal is the
/// one cancellation answer no barrier already reachable can pin. The window
/// is otherwise microseconds wide, two renames per env, so a test aiming
/// at it by timing would be asserting on its own scheduler.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_AFTER_NO_RETURN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the cutover once it is holding past the point of no return.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_PAST_NO_RETURN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the test to let the held cutover swap the corpus.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_CUTOVER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hold inside the scan, at the first file's boundary (#109).
///
/// The scan-stage cancel needs the scan pinned mid-corpus, not merely made
/// slow: a delay leaves "did the cancel land before the last file" to the
/// scheduler, and a scan that finished first publishes a plan, which is
/// exactly the thing the test asserts never happened.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_IN_SCAN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the scan once it is holding at a file boundary.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_SCAN_HELD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the test to let the held scan read its next file.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_SCAN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only hold at the finished-shadow force refusal, after the gate has
/// decided to refuse and before the job settles that verdict against a
/// pending cancel (#109).
///
/// The window runs from the last file boundary of the final increment to
/// the settlement, which no other barrier reaches and which real timing
/// makes microseconds wide. A test that wants "a cancel was pending when
/// the refusal settled" has to be inside it, not near it.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_HOLD_AT_FORCE_REFUSAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the cutover once it is holding on a decided force refusal.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_FORCE_REFUSAL_REACHED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set by the test to let the held refusal reach settlement.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_RELEASE_FORCE_REFUSAL: std::sync::atomic::AtomicBool =
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
    /// An operator cancelled the job before it reached its point of no
    /// return, and this request's own ladder observed the cancel. The
    /// terminal row is already written; the caller answers 200 with it,
    /// never a fourth status code — 409 already means refused-needs-force
    /// to a body-sniffing client, and the job body's `status` says
    /// `cancelled` plainly enough.
    Cancelled(RepinJob),
}

/// What the REQUEST stated, carried from `start` into the detached ladder.
///
/// The ceilings here are the operator's numbers, not the job's: resolution
/// needs the scan, which has not run yet at claim time.
#[derive(Debug, Clone)]
struct RequestTerms {
    /// Whether the request passed force.
    force: bool,
    /// The ceilings the request stated, each dimension separately.
    ceilings: RequestedCeilings,
    /// The requesting key's display name, as the claim recorded it. This is
    /// the only actor identity that reaches the engine: the handler passes
    /// `verified.name` and no key prefix, so the audit event names what the
    /// job row names.
    requested_by: Option<String>,
}

/// The force decision a running job carries, settled once in `decide`.
///
/// The build reads its terms from here and never from postgres: the numbers
/// were resolved from this job's own scan, and re-reading the row at cutover
/// would let a concurrent write change what the operator agreed to.
#[derive(Debug, Clone)]
struct JobTerms {
    /// The flag plus the ceilings it accepted.
    force: ForceTerms,
    /// What the pre-build scan projected, for the audit record: the whole
    /// point of the finished-shadow gate is that these two can differ.
    scanned: ceiling::Counts,
    /// Who asked (audit only).
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
    memory_limit: String,
    min_free_disk_bytes: u64,
    /// The one armed job's cancel state (#109). In-process by design: a
    /// cancel is a request to the daemon doing the work, and the node that
    /// owns the data root is the only one that can stop it.
    cancel: Arc<CancelRegistry>,
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
            cancel: Arc::new(CancelRegistry::default()),
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
    /// job row (the dry-run report is the row).
    #[allow(clippy::too_many_arguments)] // one flag bundle per request field
    pub async fn start(
        self: &Arc<Self>,
        field: &str,
        to: &str,
        dialect: Option<&str>,
        dry_run: bool,
        force: bool,
        requested: RequestedCeilings,
        requested_by: Option<&str>,
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
                max_nulled_rows,
                max_ambiguous_rows,
                requested_by,
            })
            .await?;
        // Arm the cancel registry with no `await` between it and the claim
        // that created the job. Anything awaited here would be a window in
        // which the one-running slot is held by a job nothing can cancel:
        // the request would answer "no job running" while the scan burns
        // through the corpus.
        //
        // The window is narrowed, not closed: the claim commits in
        // postgres and the registry is armed in this process, two
        // synchronisation domains that no lock spans. A status or cancel
        // request landing between the claim's commit and this line sees a
        // running row and an empty registry, and answers 404. We accept
        // that. It is microseconds of straight-line code with no I/O, the
        // 404 is recoverable by retrying (the next request finds the armed
        // entry), and the alternative — reserving a registry slot for a job
        // that has no id yet, then reconciling it against a claim that may
        // fail — buys a correctness property nothing here needs.
        let cancel = self.cancel.arm(job_id);

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
        // caller's future. The scan is a full-corpus DuckDB pass, minutes on
        // a real archive, well past `trawl-client`'s two-minute timeout and
        // any proxy's, and axum drops the handler future the moment the
        // connection goes away. Cancelled between the claim and the terminal
        // transition, the unique running slot would be stranded until a
        // daemon restart (only boot reconciliation ever clears it), 409ing
        // every later repin and reporting a phantom running job. Detached,
        // the ladder always terminalizes; a caller that walked away merely
        // loses the response and reads the verdict from
        // `/schema/repin/status`.
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
            requested_by: requested_by.map(str::to_owned),
        };
        let decided = tokio::spawn(async move {
            engine
                .decide(job_id, field, from, reading, dry_run, req, cancel)
                .await
        });
        let outcome = match decided.await {
            Ok(outcome) => outcome,
            Err(e) => {
                // A panicked decision task observed no boundary, so this is
                // `failed` even with a cancel pending (design decision 5:
                // `cancelled` means the unwind actually ran).
                //
                // Settle first, under the registry lock and before any store
                // I/O: `finish` rides out postgres trouble for seconds and
                // can hand the write to a detached retry loop, and an entry
                // still armed through all of that answers 202 to cancels of
                // a job whose task no longer exists — accepted requests
                // nothing will ever observe. Settling closes the slot the way
                // every other pre-cutover terminal does; the disarm below
                // then clears it.
                //
                // A cancel that was already pending is not relabelled and
                // emits no `repin_cancelled` event: that event means a
                // boundary saw the request and the unwind ran, and neither
                // happened here. What the row must still carry is who
                // asked, and that is why the request is written HERE,
                // synchronously, before the terminal status.
                //
                // The cancel path spawns its own detached
                // `record_cancel_request`, and this arm used to rely on it.
                // It is a race: `record_cancel_request` only matches a row
                // whose status is still `running`, so a terminal write that
                // lands first makes the detached recorder update zero rows
                // and the job ends `failed` with NULL
                // `cancel_requested_at`/`cancelled_by`, after the caller was
                // told 202. Both writes are idempotent and
                // first-writer-preserving, so ordering only matters for the
                // one that must precede the terminal, and doing it here
                // means the detached twin is harmless whichever way it
                // falls. A store failure costs the request fields and
                // nothing else: the job still terminalizes `failed`.
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
                let msg = join_failure("job", &e);
                self.finish(job_id, RepinJobStatus::Failed, Some(&msg), None)
                    .await;
                Err(ServerError::from_join("repin job", e))
            }
        };
        // The registry stays armed for exactly as long as a job is doing
        // work, and the party that owns the outcome is the party that
        // disarms. `run_job` owns the background half and disarms itself at
        // its end (through the post-cutover sweep); every other outcome is
        // owned here, and by construction there is no detached work left
        // when it is: `decide` spawns `run_job` as its last act and cannot
        // fail after that spawn. These are the only two disarm call sites,
        // which is what keeps a running job from being disarmed by somebody
        // who merely failed to describe it.
        if !matches!(outcome, Ok(StartOutcome::Started(_))) {
            self.cancel.disarm(job_id);
        }
        outcome
    }

    /// Ask to cancel the running job. The handler's one call: the verdict
    /// is decided synchronously under the registry lock and the persistence
    /// it implies runs detached.
    ///
    /// Detached because a cancel must not be split by a client disconnect.
    /// The in-process flag is already set when this returns, so the effect
    /// site can fire before the request row lands; that is deliberate and
    /// safe, because the effect site records the request itself (the store
    /// call is idempotent and first-writer-preserving) before writing a
    /// `cancelled` terminal status. This detached write exists so the
    /// status route can show a cancel in flight while the job is still
    /// walking to its next file boundary.
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
                // durable row may only ever carry the first asker. Two
                // detached writes racing with the caller's own name is how
                // the row ends up naming somebody the registry and the
                // effect audit both disagree with.
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
    ///
    /// Runs detached from the request (see `start`), so every exit path
    /// terminalizes the job row itself.
    // The ladder is long because it is a ladder: every rung terminalizes
    // the claimed job itself, and each one now arbitrates its verdict
    // against a pending cancel.
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
    ) -> Result<StartOutcome, ServerError> {
        // A pending rollup's marker and temporary output are one recovery
        // operation. The shadow walk excludes temporary files, so it must
        // never copy that marker into a generation without its output.
        // Claim before the scan too, so a dry run cannot report mixed rows.
        let publication = self.pool.publication();
        let rollup_pause = match claim_rollup_pause(&self.coordinator, &publication, &cancel).await
        {
            Ok(pause) => pause,
            Err(PassStop::Cancelled { stage }) => {
                let actor = self.settle_cancel(job_id, stage);
                self.finish_cancelled(job_id, stage, actor).await;
                return self.cancelled_outcome(job_id).await;
            }
            Err(PassStop::Failed(msg)) => {
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Blocked, Some(&msg))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(ServerError::ServiceUnavailable(msg));
            }
        };

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
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&msg))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(ServerError::BadRequest(msg));
            }
            Err(msg) => {
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&msg))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(ServerError::Internal(msg));
            }
        }

        let (counts, tallies, samples) = match self.run_scan(&field, reading, &cancel).await {
            Ok(measured) => measured,
            // A cancel observed inside the scan: nothing has been staged
            // and no marker exists, so the whole unwind is the terminal
            // write.
            Err(PassStop::Cancelled { stage }) => {
                let actor = self.settle_cancel(job_id, stage);
                self.finish_cancelled(job_id, stage, actor).await;
                return self.cancelled_outcome(job_id).await;
            }
            Err(PassStop::Failed(e)) => {
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&e))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
                return Err(ServerError::Internal(format!("repin scan failed: {e}")));
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
            // An unforced job refuses on any loss at all, so there is no
            // number to hold it to and none is persisted.
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
            if self
                .settle_pre_cutover(
                    job_id,
                    STAGE_SCAN,
                    RepinJobStatus::Failed,
                    Some(&e.to_string()),
                )
                .await
            {
                return self.cancelled_outcome(job_id).await;
            }
            return Err(ServerError::Store(e));
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

        // Free-space pre-flight: the job holds the affected bytes twice
        // until the aside sweep, and retention is suppressed for its whole
        // life, so it must not create pressure retention cannot relieve.
        // A post-claim failure must terminalize the claimed job: the
        // running slot is unique, so an early return would 409 every
        // later repin until a restart reconciles the orphan.
        let available = match fs4::available_space(&self.data_dir) {
            Ok(available) => available,
            Err(e) => {
                let msg = format!("failed to check free disk space: {e}");
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&msg))
                    .await
                {
                    return self.cancelled_outcome(job_id).await;
                }
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
            if self
                .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&msg))
                .await
            {
                return self.cancelled_outcome(job_id).await;
            }
            return Err(ServerError::BadRequest(msg));
        }

        // Read the row the caller is answered with BEFORE any detached work
        // exists. The row is stable here — nothing past the claim has
        // rewritten it — and reading it after the spawn made a transient
        // postgres error indistinguishable from a synchronous failure: the
        // caller returned `Err`, `start` treated that as "no background half"
        // and disarmed the registry, and the running job was left
        // uncancellable, its own terminal write landing on an empty slot. A
        // read failure here settles like any other pre-cutover candidate,
        // with nothing detached to strand.
        let job = match self.job(job_id).await {
            Ok(job) => job,
            Err(e) => {
                let msg = format!("could not read the claimed repin job row: {e}");
                if self
                    .settle_pre_cutover(job_id, STAGE_SCAN, RepinJobStatus::Failed, Some(&msg))
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
        });
        Ok(StartOutcome::Started(job))
    }

    async fn run_scan(
        &self,
        field: &str,
        reading: RepinReading,
        cancel: &CancelHandle,
    ) -> Result<(ScanCounts, ScanTallies, Vec<String>), PassStop> {
        let data_dir = self.data_dir.clone();
        let memory_limit = self.memory_limit.clone();
        let field = field.to_owned();
        let cancel = cancel.clone();
        on_blocking_pool("scan", move || {
            scan(&data_dir, &memory_limit, &field, reading, &cancel)
        })
        .await
        .map_err(PassStop::Failed)?
    }

    /// Is anything still writing this field? The newest observation inside
    /// [`crate::repin::LIVENESS_WINDOW`], with one service behind it.
    ///
    /// One indexed row (`field_services (field, …)` ordered `last_seen`
    /// DESC, the read the schema surface already pages through), so no new
    /// store method and no scan. Best-effort by design: liveness is
    /// advisory, and a repin must not fail because an observation table was
    /// briefly unreadable. A warning that cannot be produced is a missing
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
    /// restart's boot reconciliation, a wedge the daemon must not carry
    /// while it lives. A blip gets bounded fast retries (the catalog
    /// bookkeeping cadence); a real outage hands the write to a detached
    /// slow loop that retries until it lands. While the store is down no
    /// new claim can succeed either, so the slot is honestly busy rather
    /// than wedged, and it frees within one tick of the store returning.
    /// A daemon that dies with the loop still trying falls back to boot
    /// reconciliation. The outcome counter increments only when the write
    /// lands: a row still `running` must not be metered as a terminal
    /// outcome.
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
                // terminalized inside `finish_cutover`'s transaction, so
                // counting only the rows this statement changed would stop
                // metering every completed repin. One increment per landed
                // terminal write, exactly as before.
                Ok(_) => {
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

    /// The terminal row a cancelled job carries, as the caller's outcome.
    async fn cancelled_outcome(&self, job_id: i64) -> Result<StartOutcome, ServerError> {
        Ok(StartOutcome::Cancelled(self.job(job_id).await?))
    }

    /// Terminalize a job that is still short of the point of no return,
    /// arbitrating `candidate` against a pending cancel. Returns `true`
    /// when the cancel won, in which case the row is already written and
    /// the caller must not write its own verdict.
    ///
    /// Every pre-cutover terminal candidate goes through here, success
    /// included, because the interesting race is the quiet one: a cancel
    /// that lands after the last file's post-check but before the verdict
    /// is written. Without arbitration that operator gets a 202 and then
    /// watches the job report `succeeded`, with no way to tell whether the
    /// cancel was too late or simply lost.
    ///
    /// There is no exception. The background half's finished-shadow force
    /// refusal settles the same way, through [`Self::abandon_build`]: an
    /// operator's stop outranks a park, both leave the corpus untouched,
    /// and answering an accepted cancel with `refused_needs_force` would
    /// make the 202 a lie.
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
    ///
    /// The latch is the point. Reading the pending request and then acting
    /// on the answer are two steps, and the unwind between them can take
    /// minutes (a whole shadow generation to sweep); a cancel landing in
    /// that gap used to be told 202 while the sampled verdict won the row.
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

    /// The cancel effect site: record the request, then write the terminal
    /// status the request earned.
    ///
    /// The order is the database's requirement, not a preference. A
    /// `cancelled` row without a recorded request violates migration
    /// 0014's CHECK, so the terminal write is unreachable until
    /// `record_cancel_request` has landed. The detached request path
    /// normally landed it seconds ago and this call is a no-op (it is
    /// idempotent and keeps the first asker's name), but when that write
    /// failed, this one is what makes `cancelled` legal. If it fails too,
    /// the job ends `failed` naming the store trouble: the unwind still
    /// ran and the corpus is still untouched, so the outcome word is the
    /// only thing the operator loses.
    ///
    /// `actor` is the name [`Self::observe_cancel`] already audited, passed
    /// in rather than re-read so the log line and the row's sentence cannot
    /// name two different people.
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

    /// The background half: build, catch up, cut over, sweep.
    ///
    /// `scanned` carries the mandatory pre-build scan's per-file readings
    /// so the build does not immediately re-measure a corpus nothing has
    /// touched (see [`ScanTallies`]).
    #[allow(clippy::too_many_arguments)] // one bundle per claimed job
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
            // Every pre-cutover abort takes the same door, the force
            // refusal included: a cancel that landed while the verdict was
            // being written is still a cancel of a running job, and the
            // operator who asked for a stop gets one. The refusal costs
            // that operator nothing — it was protecting a corpus the cancel
            // leaves untouched anyway.
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

        // The job task is over — through the post-cutover sweep, not merely
        // past the swap. Until this line a cancel request is answered 409
        // rather than 404, which also keeps a fresh repin from arming the
        // registry while this one is still deleting its staging roots.
        self.cancel.disarm(job_id);
    }

    /// The build-phase cancel effect site: audit, sweep the shadow, drop
    /// the marker, then terminalize through the persist-before-terminal
    /// path.
    ///
    /// `actor` comes from [`Self::settle_cancel`], which audited it before
    /// any unwinding started, so the log reads request, effect, sweep. A
    /// sweep of a whole shadow generation takes minutes on a real archive,
    /// and an operator watching the log should see their cancel land before
    /// the cleanup it caused. `None` means the registry stopped naming this
    /// job between the boundary that observed the cancel and the
    /// settlement, which [`Self::finish_cancelled`] records as `failed`.
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

    /// Abandon a job whose corpus is still untouched (pre-swap): settle
    /// `candidate` against a pending cancel, then sweep the disposable
    /// shadow and any leftover aside, drop the marker and record the
    /// outcome. The unwind is identical either way; only the word the job
    /// row carries differs.
    ///
    /// This is the only door out of a pre-cutover terminal candidate in the
    /// background half, and the rule has no exceptions: every pre-cutover
    /// terminal candidate settles under the registry lock, and there is no
    /// other door. A new [`JobAbort`] arm cannot compile its way past
    /// settlement, because the non-arbitrated abandon does not exist. The
    /// synchronous half's door is [`Self::settle_pre_cutover`], same rule.
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

    /// The disk half of abandoning a build, without the terminal write.
    ///
    /// Split out because a cancelled job takes the same unwind but a
    /// different terminal write: `cancelled` is only legal once the request
    /// row exists (see [`Self::finish_cancelled`]).
    async fn unwind_staging(&self, job_id: i64, status: RepinJobStatus, msg: &str) {
        tracing::warn!(
            event_type = "repin_abandoned",
            job_id,
            status = status.as_str(),
            error = %msg,
            "repin job abandoned before any visible change; corpus untouched"
        );
        // The marker is what licenses the next boot to delete the staging
        // roots, both of them, since a leftover aside from an earlier job's
        // failed sweep outlives its own marker (see
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

        // A shadow root that outlived an earlier job's sweep is not a
        // disk-only problem here: building into it would publish that job's
        // files into the live corpus at the swap, including rows retention
        // has since deleted. Refuse rather than layer.
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
                .run_pass(field, reading, &flipped, scanned, &mut state, cancel)
                .await?;
            self.publish_progress(job_id, &state).await;

            // Test-only: pin the mid-job state for an observer (see
            // `TEST_HOLD_AFTER_PROGRESS`). Bounded, so a mis-driven test
            // fails instead of hanging, and armed once — later passes run
            // at full speed.
            #[cfg(any(test, feature = "test-support"))]
            if TEST_HOLD_AFTER_PROGRESS.swap(false, std::sync::atomic::Ordering::SeqCst) {
                TEST_PROGRESS_PUBLISHED.store(true, std::sync::atomic::Ordering::SeqCst);
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !TEST_RELEASE_JOB.load(std::sync::atomic::Ordering::SeqCst)
                    && std::time::Instant::now() < deadline
                {
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

        // The narrow pause: exclusive against compaction batches and every
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
        // corpus now, so this pass is the last word. Still cancellable —
        // the exclusion guards are held, but nothing visible has moved, and
        // dropping them on the unwind costs the corpus nothing.
        self.run_pass(field, reading, &flipped, scanned, &mut state, cancel)
            .await?;

        // Freeze what the finished shadow holds — the five counts and the
        // per-service null tallies — durably, before the Cutover marker and
        // before the force gate that may still refuse. This replaces the
        // final progress write: it stores the same counts and, unlike that
        // write, is not best-effort. `finish_cutover` turns these tallies
        // into the conflict evidence in the transaction that completes the
        // job (issue #137), so a job that reaches the marker without them is
        // a job whose losses nothing will ever record.
        let staged_totals = staged_totals(&state).map_err(JobAbort::Failed)?;
        let staged_tallies = staged_tallies(&state).map_err(JobAbort::Failed)?;
        self.store
            .stage_cutover_input(job_id, staged_totals, &staged_tallies)
            .await
            .map_err(|e| {
                // The full error stays server-side: the abort message is
                // persisted on the job row and served to SchemaRead via the
                // status route, and a StoreError's Display can carry raw
                // postgres diagnostics the wire contract redacts.
                tracing::error!(
                    event_type = "repin_store_error",
                    job_id,
                    error = %e,
                    "staging the cutover tallies failed; aborting before the marker"
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

        // The authoritative loss gate. The pre-build scan only describes
        // the corpus as it stood before the build; ingest and compaction
        // run for the whole job, so a file written after the scan can carry
        // values the new pin cannot read. Deciding on the finished shadow's
        // own accounting is the only check the operator's omitted force
        // flag can actually govern, and it is safe to refuse here because
        // nothing visible has moved yet.
        let totals = state.totals();
        if let Some(reason) = force_refusal(
            reading.written.pin,
            Some(reading.written.raw),
            totals.nulled,
            totals.ambiguous,
            terms.force,
        ) {
            // Test-only: pin the window between a decided refusal and its
            // settlement (see `TEST_HOLD_AT_FORCE_REFUSAL`). Bounded, and
            // armed once.
            #[cfg(any(test, feature = "test-support"))]
            if TEST_HOLD_AT_FORCE_REFUSAL.swap(false, std::sync::atomic::Ordering::SeqCst) {
                TEST_FORCE_REFUSAL_REACHED.store(true, std::sync::atomic::Ordering::SeqCst);
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !TEST_RELEASE_FORCE_REFUSAL.load(std::sync::atomic::Ordering::SeqCst)
                    && std::time::Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            // Both refusals name the same cause — the corpus moved under the
            // build — and differ only in what the operator does next. Without
            // force there is a plan to re-read; with it there is a number to
            // raise, and telling them to "pass force" when they already did
            // would be advice they have taken.
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
        // lock. A cancel accepted before it stops the marker write; one
        // arriving after it is answered 409; and a marker that cannot be
        // published is a job that never crossed at all, so it leaves the
        // entry unlatched and unwinds through the ordinary abandon path —
        // where a cancel that is still pending wins, honestly, instead of
        // being told for the rest of the sweep that the corpus is moving.
        // Everything below this block is forward-only.
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

        // The audit record of a forced cutover, emitted once the latch is
        // held and before the swap. The latched Cutover marker is the
        // durable point where force licensed the mutation: past it the
        // corpus becomes the new generation one way or another, by this
        // process or by the boot replay. A marker write that fails takes the
        // cutover with it, and a cancel that won the latch stops it, so
        // neither reaches this line — announcing an acceptance that never
        // licensed anything would leave a record of a mutation that did not
        // happen. It carries what force accepted beside what the rewrite
        // actually did, so the pair an operator agreed to is legible after
        // the fact without reading a job row that later columns overwrite.
        // Counts only — no sample values, no free text.
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
        // `TEST_HOLD_AFTER_NO_RETURN`). Bounded, and armed once.
        #[cfg(any(test, feature = "test-support"))]
        if TEST_HOLD_AFTER_NO_RETURN.swap(false, std::sync::atomic::Ordering::SeqCst) {
            TEST_PAST_NO_RETURN.store(true, std::sync::atomic::Ordering::SeqCst);
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !TEST_RELEASE_CUTOVER.load(std::sync::atomic::Ordering::SeqCst)
                && std::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

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
        // exit: the corpus already is the new generation.
        let mut flipped_ok = false;
        for attempt in 1..=FLIP_ATTEMPTS {
            match self.store.finish_cutover(job_id, field, to).await {
                Ok(outcome) => {
                    // `cleared_ack` is true only on the call that completed
                    // the job, so a boot replay of an already-finished
                    // cutover never re-announces a clear that happened once.
                    //
                    // One event per OBSERVED clear, which is not the same as
                    // one per clear. The DELETE commits with the pin flip; a
                    // crash between that commit and this line loses the
                    // event, and the row is gone, so nothing is left to
                    // replay it from. Closing that gap needs the durable
                    // outbox ADR-0019 rejected as a receipts table, so the
                    // residual is documented rather than mechanised: the ack
                    // is gone either way, and the audit trail is one line
                    // short.
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

        // Counters for what the rewrite actually did. The evidence itself
        // went in with the flip.
        Self::record_outcome(&state);

        // Sweep: disk-only from here, and infallible by type. A failed
        // sweep of either staging root keeps the marker so the boot replay
        // retries it, since a leftover root suppresses retention until it
        // is gone. But nothing past the point of no return may be reported
        // as a failure of the job: the corpus is the new generation and the
        // pin is flipped, so an undeletable marker is leftover disk, not a
        // repin that "left the corpus untouched".
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
    #[allow(clippy::too_many_arguments)] // the pass's inputs, one each
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
        let (returned, changed) = on_blocking_pool("pass", move || {
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
        .map_err(JobAbort::Failed)?;
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
            tracing::warn!(event_type = "repin_store_error", job_id, error = %e, "progress write failed");
        }
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(crate::metrics::CATALOG_REPIN_FILES_DONE).set(totals.files_done as f64);
    }

    /// What is left to do once the pin is flipped: increment the two
    /// lifetime counters.
    ///
    /// Everything durable was written before the Cutover marker (the counts
    /// and the per-service tallies, staged) or by the flip itself (the
    /// `field_conflicts` evidence those tallies become, inside
    /// `finish_cutover`'s transaction). Nothing may be written here: a
    /// post-terminal write is exactly the window issue #137 closed, where a
    /// client reading `succeeded` sees a repin whose losses are not yet
    /// recorded, and boot recovery never runs this code at all.
    fn record_outcome(state: &BuildState) {
        let totals = state.totals();
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_NULLED_TOTAL).increment(totals.nulled);
        metrics::counter!(crate::metrics::CATALOG_REPIN_ROWS_RESURRECTED_TOTAL)
            .increment(totals.resurrected);
    }
}

/// Run one whole-corpus filesystem step on the blocking pool.
///
/// These steps are proportional to the size of the archive, not to the
/// repin: the staging pre-flight lstats every file under every env dir,
/// and each sweep is a `remove_dir_all` over a whole corpus generation,
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
        .map_err(|e| join_failure(what, &e))
}

/// The fixed text for a repin task that did not return, naming the step.
///
/// This text becomes the stored job error that the status route serves,
/// and `JoinError`'s `Display` quotes a panic payload, so the join error
/// itself is never formatted.
fn join_failure(what: &'static str, e: &tokio::task::JoinError) -> String {
    if e.is_panic() {
        format!("repin {what} task panicked")
    } else {
        format!("repin {what} task was cancelled")
    }
}

/// The store's shape for what the build has tallied so far. Saturating
/// rather than wrapping: a count past `i64::MAX` is not a corpus anyone has,
/// and the honest ceiling beats a negative row count.
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

/// Why a job stopped short of the swap.
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
    /// An operator cancelled the job and a boundary observed the request
    /// before the point of no return (#109).
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

/// The one sentence a cancelled job row carries. The `error` column is the
/// row's only prose slot, so it says who, where, and the fact an operator
/// most wants confirmed: nothing visible changed.
fn cancelled_error(actor: &str, stage: &str) -> String {
    format!("cancelled by {actor} during {stage}; the live corpus was never touched")
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
///
/// The staged numbers are the cutover's input, not a display: they become
/// the report an operator reads and the conflict evidence the flip writes.
/// A count too large for `BIGINT` is unreachable in any real corpus, so
/// meeting one means something upstream is wrong — and failing here costs
/// nothing, because the marker is not down yet and the corpus is untouched.
fn checked_count(what: &str, value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| {
        format!("the rewrite's {what} count ({value}) does not fit the job row; refusing to stage")
    })
}

/// The five final counts, from the shadow's CURRENT results.
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
///
/// Read off the state's CURRENT results, never accumulated across passes: a
/// caught-up file replaces its earlier tally, so summing deltas would count
/// a reprocessed file twice — the same rule [`BuildState::totals`] follows,
/// and the reason these two must be read from one state.
///
/// Ordered rows DESC then service ASC, and capped at
/// [`MAX_CONFLICTS_PER_FIELD`]: the per-field conflict trim would drop the
/// overflow anyway, and dropping it here makes which rows survive a
/// decision (the services that lost the most) rather than an accident of
/// insertion order. Documented residual: a repin lossy across more than 100
/// services records evidence for the worst 100.
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

/// One pass, blocking: diff the source tree against what the shadow
/// already reflects, (re)process the delta, retire removals.
///
/// `cancel` is checked on both sides of every file operation, retirements
/// included: a retirement is a `remove_file` in the shadow, so stopping
/// between two of them leaves a partial shadow the sweep deletes whole. The
/// snapshot walk that opens the pass is not checkpointed (see
/// [`crate::repin::cancel::CANCEL_LATENCY_CONTRACT`]), which is why the
/// pass also checks before taking it.
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

    // Test-only happened-before edge: this snapshot is taken, and nothing
    // moves until the test has written the file it wants a catch-up pass to
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
        cancel.check(STAGE_BUILD)?;
    }
    Ok(changed)
}

/// Whether this repin must be refused for want of an explicit force flag.
///
/// One decision with three askers: the pre-build scan gate, the
/// finished-shadow gate, and the wire (`requires_force` on every job row a
/// client reads), so a dry run can never report a verdict the executing
/// request would not reach. `Some(reason)` is the text stored as the job
/// row's `error`, and a caller may wrap it in its own context, which is the
/// only thing the askers say differently.
///
/// Takes the pin and the asserted dialect rather than a `RepinTarget`,
/// because the wire asker reads a stored job row: `RepinTarget` is the
/// engine's to construct (it alone knows the old pin), and re-deriving one
/// from a row would be exactly the second opinion this function exists to
/// prevent.
///
/// Two reasons, both about a value the operator has not knowingly accepted:
///
/// 1. loss: values the new pin cannot read at all;
/// 2. ambiguity: numerals that read as a different severity in each dialect
///    (the 1-7 overlap). Only under the `OTel` reading, because asserting
///    syslog is itself the statement about provenance the ambiguity is
///    waiting for. An explicit `dialect=otel` does not suppress it: the
///    refusal is about the values, not about how the request was spelled,
///    and force is the one escape.
///
/// The count is taken whatever the dialect, since the report says what is
/// there; only the gate is conditional.
///
/// A forced plan always carries recorded bounds. Both gates refuse counts
/// above those bounds; an unforced plan retains the loss rules above.
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

/// Resolve the asserted numeral dialect for a target.
///
/// A `SEVERITY` target always ends up with one: `otel` when the request says
/// nothing, because that is what every other lane reads and a repin that
/// changed the ladder by omission would be the silent mistranslation this
/// whole feature exists to make deliberate.
///
/// Any other target with a dialect is a 400, not an ignored field: the
/// dialect only reaches the `SEVERITY` rung, so accepting it elsewhere would
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

/// Check the requested ceilings against the request that carries them, and
/// hand back the pair the claim persists.
///
/// A ceiling without force is refused rather than quietly ignored: it is an
/// operator saying "accept up to this much loss" on a request that accepts
/// none, so the two halves contradict each other and only the operator can
/// say which they meant. Values above `i64::MAX` are refused for the same
/// reason a silent clamp is wrong — the column is a BIGINT, and a ceiling
/// stored as something other than what was asked for is a bound nobody set.
/// Every real corpus is many orders of magnitude below that, so this only
/// ever fires on a typo or a probe.
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

/// Resolve a requested repin target to a canonical type.
///
/// The parse is through `CanonicalType::from_catalog`, the catalog spelling
/// and the injective one, so `SEVERITY` is admitted. It is a target an
/// operator can only reach by naming it: inference cannot mint it
/// (`DESCRIBE` never says `SEVERITY`, so `normalize_duckdb_type` can never
/// yield it), and the envelope seed is the only other installer.
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

    /// A panicked blocking step answers fixed text naming the step. Its
    /// payload never enters the string, because that string becomes the
    /// stored job error the status route serves.
    #[tokio::test]
    async fn a_panicked_blocking_step_never_carries_its_payload() {
        let msg = on_blocking_pool::<(), _>("probe", || panic!("zz_repin_payload_sentinel"))
            .await
            .expect_err("the step panics");
        assert!(!msg.contains("zz_repin_payload_sentinel"), "{msg}");
        assert_eq!(msg, "repin probe task panicked");
    }

    /// The same for the detached decision task, whose join failure is both
    /// the stored job error and the start response's error.
    #[tokio::test]
    async fn a_panicked_decision_task_never_carries_its_payload() {
        let panicked = tokio::spawn(async { panic!("zz_repin_payload_sentinel") })
            .await
            .expect_err("the task panics");
        let msg = join_failure("job", &panicked);
        assert!(!msg.contains("zz_repin_payload_sentinel"), "{msg}");
        assert_eq!(msg, "repin job task panicked");

        let pending = tokio::spawn(std::future::pending::<()>());
        pending.abort();
        let cancelled = pending.await.expect_err("the task is cancelled");
        assert_eq!(
            join_failure("job", &cancelled),
            "repin job task was cancelled"
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
            publication.mark_rollup(&marker);
        }
        drop(active);
        let result = tokio::time::timeout(Duration::from_secs(1), admission)
            .await
            .expect("admission must observe the completed unit");
        if failed {
            assert!(matches!(result, Err(PassStop::Failed(_))));
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
            Err(PassStop::Cancelled { stage: STAGE_SCAN })
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
            Err(PassStop::Failed(_))
        ));
        assert_eq!(started.elapsed(), CUTOVER_DRAIN_TIMEOUT);
        assert!(!coordinator.rollup_paused());
    }

    /// The target vocabulary is the catalog's, the injective spelling, so
    /// `SEVERITY` is a target an operator can name and stays distinct from
    /// the `BIGINT` it shares a physical type with: the two mean different
    /// things to every comparison rule, and a parse that collapsed them
    /// would repin a field to a pin nobody asked for.
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

    /// The force gate is one decision asked at two moments (the pre-build
    /// scan and the finished shadow), so the two can only refuse for the
    /// same reasons: loss always, ambiguity only under the `OTel` reading
    /// (asserting syslog is itself the provenance statement the ambiguity
    /// waits for), and `--force` is the single escape from either.
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

        // Loss: any target, any dialect, cleared only by force.
        assert!(
            force_refusal(VARCHAR, None, 3, 0, unforced)
                .is_some_and(|m| m.contains("cannot be read as VARCHAR")),
        );
        assert_eq!(force_refusal(VARCHAR, None, 3, 0, bounded), None);
        assert!(force_refusal(SEVERITY, Some(Dialect::Syslog), 3, 0, unforced).is_some());

        // Ambiguity: refused under OTel, silent under an explicit syslog
        // assertion, and cleared by force either way.
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

        // Nothing to refuse, and a non-severity target has no ambiguity
        // notion at all (its count is a constant zero upstream, but the gate
        // must not fire even if one were handed in).
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

    /// Force with ceilings is force held to a number (#111): the same one
    /// function decides, so the scan gate, the finished-shadow gate and the
    /// wire all report the overrun in the same sentence. The syslog exemption
    /// carries into the ceiling arm through `ambiguity_binds`.
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

        // Ambiguity has its own ceiling, and asserting syslog still answers
        // the question rather than spending the budget.
        assert!(force_refusal(SEVERITY, Some(Dialect::Otel), 0, 3, terms).is_some());
        assert_eq!(
            force_refusal(SEVERITY, Some(Dialect::Syslog), 0, 3, terms),
            None
        );
    }

    /// A ceiling is a statement about what force accepts, so it is refused
    /// without force rather than ignored, and refused above the column's
    /// domain rather than clamped. Both are 400s: only the operator can say
    /// what they meant.
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

    /// The dialect is a `SEVERITY`-only assertion: a severity target
    /// defaults to `otel` (what every other lane reads), an unknown token
    /// names the vocabulary, and a dialect on any other target is refused
    /// rather than ignored, because an ignored assertion is one an operator
    /// believes was honoured.
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

    /// `decide` reads the response row BEFORE it spawns the background half.
    ///
    /// The ordering is the whole fix. Reading it after the spawn made a
    /// transient postgres error look exactly like a synchronous failure:
    /// `decide` returned `Err`, `start` saw a non-`Started` outcome and
    /// disarmed the registry, and the job that was already rewriting the
    /// corpus could no longer be cancelled — its own terminal write then
    /// landed on an empty slot and recorded `failed` with no actor.
    ///
    /// This is a shape assertion over the source, not a behavioural one:
    /// `RepinEngine` holds a concrete `RepinStore` over a real postgres
    /// pool, so there is no seam to inject a failing row read through, and
    /// inventing one would be a mock where the rest of this crate uses the
    /// database. What is covered is the ordering that makes the failure
    /// unreachable. What is not covered is the store error itself: no test
    /// here observes a failed row read and its terminal write.
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

    /// A panicked decision task settles before it writes anything.
    ///
    /// `finish` is not a quick write: it retries a struggling postgres for
    /// seconds and can hand the terminal write to a detached loop. An entry
    /// left armed for that whole stretch answers every cancel request 202,
    /// for a task that has already died — an accepted cancel nothing will
    /// ever act on, which is the exact dishonesty the settlement lock
    /// exists to prevent. Settling first closes the slot, so those requests
    /// get the truthful "no job running".
    ///
    /// The outcome word does not move: a panic is an unobserved failure,
    /// so a pending cancel still ends the job `failed` (design decision 5),
    /// and the arm emits no `repin_cancelled` — that event asserts an
    /// unwind ran.
    ///
    /// Shape assertion over the source. The panic path needs a panicking
    /// `decide`, which no test can provoke without a fault seam through the
    /// engine's own ladder; what is covered is the ordering, not a
    /// live panic. `CancelRegistry`'s own settle-then-request behaviour is
    /// covered in `cancel.rs`.
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

    /// A panicked decision task records the pending cancel request itself,
    /// synchronously, before it writes the terminal status.
    ///
    /// The cancel path spawns a detached `record_cancel_request`, and this
    /// arm used to rely on it. That is a race with a wrong answer:
    /// `record_cancel_request` only matches a row still `running`, so if
    /// the `failed` terminal lands first the recorder updates zero rows and
    /// the job ends with NULL `cancel_requested_at`/`cancelled_by` even
    /// though the operator was told 202. Doing the write here makes one of
    /// the two synchronous, and since it is idempotent and keeps the first
    /// asker, the detached twin is then harmless in either order.
    ///
    /// Shape assertion over the source, for the same reason as its sibling
    /// above: reaching this arm needs a panicking `decide`, and there is no
    /// fault seam through the ladder to provoke one. What is covered is the
    /// ordering. The store behaviour the ordering depends on (a terminal
    /// row takes no later request) is covered against real postgres in
    /// `store_pg.rs`.
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

    /// A stopped pass keeps its two meanings apart all the way to the
    /// terminal write: a cancel is an operator's decision with a stage
    /// attached, a failure is prose. Collapsing them would let a repin that
    /// died on a `DuckDB` error report itself as cancelled, which reads as
    /// "somebody stopped this on purpose".
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

    /// The cancelled row's one prose slot names who, where, and the fact
    /// the operator most needs: nothing visible changed. Pinned because
    /// this sentence is what the CLI prints and what an integration test
    /// asserts on the row.
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

    /// The field side of admission: `start` gates on
    /// `schema::is_contract_typed`, so every contract slot (the sealed `_`
    /// namespace whole, present and future, plus the four sender-asserted
    /// bare names) is refused before a job is ever claimed, while ordinary
    /// sender vocabulary is repinnable.
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
