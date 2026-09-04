// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin garbage collection: reclaiming catalog slots no live field holds.
//!
//! A pin is spent permanently on the ingest path and the cap
//! ([`crate::store::MAX_PINNED_FIELDS`]) is install-wide, so a typo'd
//! field name or a decommissioned sender keeps a slot forever. This module
//! holds the pure half of the reclaim: how long "dead" is, and whether one
//! pin qualifies on the observation axis.
//!
//! Two independent proofs make a pin collectable. The observation axis is
//! the field catalog's own `field_services` history, which says nothing has
//! written the field recently. The metadata axis is the standing corpus
//! itself, where no parquet footer under a live env declares the column. A
//! candidate on the first axis is a candidate to be disproved by a footer,
//! never a decision to delete.
//!
//! The first half of the file is pure: no clock, no pool, no filesystem.
//! [`PinGc`] is the engine over it, and it samples one `decided_at` for the
//! whole run and hands the derived cutoff down, so the report, the SQL
//! comparisons and the audit events all agree on when "now" was.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use trawl_core::schema::{catalog_key, is_contract_typed};

use crate::catalog::FieldCatalog;
use crate::error::ServerError;
use crate::repin::RepinCoordinator;
use crate::store::{CatalogStore, GcPinRow, PurgedPin, RepinJobStatus, RepinStore};

/// How long a field must go unobserved before gc will consider it dead,
/// when the request names no window of its own.
///
/// A month covers the shapes that legitimately go quiet without being
/// gone: a monthly batch job, a service parked for a sprint, a host out
/// for a long repair. Shorter windows start collecting fields that were
/// only sleeping, and re-pinning is cheap but the conflict evidence and
/// per-service history that die with the pin are not.
pub const DEFAULT_DEAD_WINDOW: Duration = Duration::from_hours(24 * 30);

/// The window one gc run applies, and the numbers behind it.
///
/// The formula lives here and only here. The CLI and the SPA render
/// `requested`/`floor`/`effective` as the server reported them; if a
/// client recomputed the floor it would eventually disagree with the run
/// that actually happened, and an operator would be reading a window no
/// deletion ever used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadWindow {
    /// The requested window in seconds, after the server default applied.
    pub requested_secs: u64,
    /// The retention floor in seconds, when age retention is enabled.
    /// Reported whenever it exists, whether or not it bound the answer.
    pub floor_secs: Option<u64>,
    /// The window applied: the larger of the request and the floor.
    pub effective_secs: u64,
}

impl DeadWindow {
    /// The effective window as a [`Duration`].
    #[must_use]
    pub fn effective(&self) -> Duration {
        Duration::from_secs(self.effective_secs)
    }
}

/// Decide the window one gc run applies.
///
/// `older_than` is the operator's request, `None` meaning
/// [`DEFAULT_DEAD_WINDOW`]. `floor_secs` is the retention floor from
/// [`crate::retention::maximum_enabled_age_secs`]: `None` when age
/// retention is disabled, which imposes no floor at all.
///
/// The rule is one `max`. A window shorter than the corpus trawl still
/// keeps would call a field dead while its data sits on disk under a live
/// env, so retention raises the request; nothing ever lowers it. A
/// requested `0` is honoured literally, because the footer scan is the
/// proof that makes that safe, and an operator cleaning up after a bad shipper should
/// not have to wait out a window for a field the corpus has never carried.
#[must_use]
pub fn effective_dead_window(older_than: Option<Duration>, floor_secs: Option<u64>) -> DeadWindow {
    let requested_secs = older_than.unwrap_or(DEFAULT_DEAD_WINDOW).as_secs();
    let effective_secs = floor_secs.map_or(requested_secs, |floor| requested_secs.max(floor));
    DeadWindow {
        requested_secs,
        floor_secs,
        effective_secs,
    }
}

/// One pin's verdict on the observation axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidacy {
    /// Unobserved through the whole window: a candidate, pending the
    /// footer scan.
    Candidate,
    /// A field whose type is trawl's to declare, not an operator's to
    /// reclaim ([`is_contract_typed`]).
    ContractTyped,
    /// Observed at or after the cutoff, so something still writes it.
    ObservedInWindow,
}

/// Judge one pin on the observation axis.
///
/// `last_seen` is the newest `field_services` observation, `cutoff` the
/// run's `decided_at` minus the effective window.
///
/// Three rules, in this order:
///
/// 1. [`is_contract_typed`] wins over everything. The ten envelope slots
///    are trawl's own contract; an envelope field nobody has sent lately
///    is still the field the partition path, the peer fill and the
///    severity ladder are built on, and its seeded pin must survive a
///    corpus that never carried it.
/// 2. `last_seen >= cutoff` is alive. At-the-instant counts as observed:
///    the boundary belongs to the retained side, matching the `last_seen`
///    window the schema routes apply.
/// 3. Everything else, including a pin never observed at all, is a
///    candidate.
///
/// Rule 3 is a deliberate divergence from `/api/v1/schema`'s listing rule,
/// where a never-observed pin is always shown rather than windowed out
/// (a pin with no observation has no `last_seen` to compare, and hiding it
/// would make the seeded envelope invisible). The listing errs toward
/// showing; gc errs toward reclaiming, because a pin with no observation
/// and no carrier file is exactly the accidental slot this exists to
/// free: a `curl` typo pinned once, never written, never seen again. The
/// envelope stays safe through rule 1, not through the never-observed
/// case, and the metadata axis still has to agree before anything is
/// deleted.
#[must_use]
pub fn candidacy(
    field: &str,
    last_seen: Option<DateTime<Utc>>,
    cutoff: DateTime<Utc>,
) -> Candidacy {
    if is_contract_typed(field) {
        return Candidacy::ContractTyped;
    }
    match last_seen {
        Some(seen) if seen >= cutoff => Candidacy::ObservedInWindow,
        _ => Candidacy::Candidate,
    }
}

/// How long any READ-ONLY postgres operation performed under the corpus
/// gate may take before the run gives up on it.
///
/// The gate is held across every one of them, and while it is held no
/// compaction batch can publish, so a postgres that has stopped answering
/// must not translate into an ingest stall of unbounded length. One budget
/// per operation rather than one for the whole gated section: each is a
/// single round trip, and a per-call bound is the one an operator can read
/// off the refusal.
///
/// Read-only only. Cancelling a read costs the answer and nothing else;
/// cancelling the purge would abandon a transaction whose commit may still
/// land, which is why that one is bounded inside postgres instead
/// ([`crate::store::CatalogStore::delete_pins`]).
const IN_GATE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many offending paths a fail-closed refusal names. The rest are a
/// count: an operator needs a place to start, not a directory listing.
const MAX_REPORTED_PATHS: usize = 3;

/// Who asked for a run, for the audit record.
///
/// Name and key prefix only. The prefix is the stable, non-secret head of
/// the API key id that already identifies an actor elsewhere in trawl's
/// audit events, so two runs by the same key are recognisably the same
/// hand without the record carrying a credential.
///
/// Deliberately its own type rather than a borrow of the request's auth
/// context: the run outlives the request (see [`PinGc::run`]), so it needs
/// owned strings.
#[derive(Debug, Clone, Default)]
pub struct GcActor {
    /// The key's human-facing name, when it has one.
    pub name: Option<String>,
    /// Stable non-secret prefix of the key id.
    pub key_prefix: Option<String>,
}

/// The pin garbage collector — one per ingest-enabled daemon.
///
/// A query-only node owns nothing under the data root and so cannot prove
/// the metadata axis; it holds no engine at all
/// ([`crate::state::AppState::gc`] is `None` there).
#[derive(Debug, Clone)]
pub struct PinGc {
    store: CatalogStore,
    repin_store: RepinStore,
    cache: Arc<FieldCatalog>,
    coordinator: Arc<RepinCoordinator>,
    data_dir: PathBuf,
    /// The retention floor in seconds
    /// ([`crate::retention::maximum_enabled_age_secs`]), resolved once at
    /// construction because retention config is fixed for the process.
    retention_floor_secs: Option<u64>,
    /// Admission: one run at a time, per engine.
    ///
    /// Try-lock, never await. A queued second run would hold nothing while
    /// it waited and then take the corpus gate for another full footer
    /// scan, so two operators hammering the route would stall compaction
    /// for as long as they kept it up. It is also a run whose candidate set
    /// was read before the first run deleted anything, which has no useful
    /// answer to give. The honest reply is 409 now.
    admission: Arc<tokio::sync::Mutex<()>>,
    /// Test-only observation and hold points, held on the engine rather
    /// than in a static so one test's armed hold cannot leak into another
    /// test's run in the same binary (the repin coordinator's idiom).
    #[cfg(any(test, feature = "test-support"))]
    hooks: Arc<TestHooks>,
}

/// Test-only synchronisation points inside [`PinGc::run`].
///
/// Two orderings a gc test needs and cannot get from a sleep. Whether a run
/// currently holds admission, so a second caller's 409 is asserted against a
/// fact rather than a guess; and a hold right after the candidate read, so a
/// concurrent catalog write provably lands between that read and the purge.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
struct TestHooks {
    admitted: std::sync::atomic::AtomicBool,
    /// `true` while a test wants the run parked after its candidate read.
    post_candidate_hold: tokio::sync::watch::Sender<bool>,
    /// `true` while a run is actually parked there.
    post_candidate_parked: std::sync::atomic::AtomicBool,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for TestHooks {
    fn default() -> Self {
        Self {
            admitted: std::sync::atomic::AtomicBool::new(false),
            post_candidate_hold: tokio::sync::watch::channel(false).0,
            post_candidate_parked: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// Sets the admitted flag for as long as a run holds the admission lock,
/// including the early-return paths, because a flag left true would make the
/// next test's poll return immediately.
#[cfg(any(test, feature = "test-support"))]
struct AdmissionSignal(Arc<TestHooks>);

#[cfg(any(test, feature = "test-support"))]
impl Drop for AdmissionSignal {
    fn drop(&mut self) {
        self.0
            .admitted
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What the footer scan proved, and what it cost.
#[derive(Debug)]
struct Walk {
    /// Candidates still undisproved: no scanned footer named them.
    dead: BTreeSet<String>,
    /// Parquet footers actually read.
    files_scanned: u64,
}

/// Paths the walk could not make sense of, for one fail-closed message.
#[derive(Debug, Default)]
struct Problems {
    count: usize,
    shown: Vec<String>,
}

impl Problems {
    fn note(&mut self, path: &Path, why: &str) {
        self.count += 1;
        if self.shown.len() < MAX_REPORTED_PATHS {
            self.shown.push(format!("{} ({why})", path.display()));
        }
    }

    /// The refusal message, or `None` when the walk was clean.
    ///
    /// Data-root paths are operator-visible by design (the operator chose
    /// them and can read the directory), so naming them is help rather
    /// than disclosure.
    fn refusal(&self) -> Option<String> {
        (self.count > 0).then(|| {
            format!(
                "pin gc cannot read {} path(s) under the data root, so it \
                 cannot prove any pin dead and deleted nothing: {}. Repair \
                 or move them aside, then re-run.",
                self.count,
                self.shown.join("; ")
            )
        })
    }
}

impl PinGc {
    /// Build the engine. `retention_floor_secs` comes from
    /// [`crate::retention::maximum_enabled_age_secs`].
    #[must_use]
    pub fn new(
        store: CatalogStore,
        repin_store: RepinStore,
        cache: Arc<FieldCatalog>,
        coordinator: Arc<RepinCoordinator>,
        data_dir: PathBuf,
        retention_floor_secs: Option<u64>,
    ) -> Self {
        Self {
            store,
            repin_store,
            cache,
            coordinator,
            data_dir,
            retention_floor_secs,
            admission: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(any(test, feature = "test-support"))]
            hooks: Arc::new(TestHooks::default()),
        }
    }

    /// Test-only: whether a run holds admission right now. A second run's
    /// 409 is only meaningful once this is true.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn admission_held(&self) -> bool {
        self.hooks
            .admitted
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Test-only: park the next run between its candidate read and the
    /// corpus gate, until [`Self::release_post_candidate_hold`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn arm_post_candidate_hold(&self) {
        self.hooks.post_candidate_hold.send_replace(true);
    }

    /// Test-only: whether a run is parked in that hold right now — the
    /// rising edge a test waits for before it writes to the catalog.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn parked_after_candidate_read(&self) -> bool {
        self.hooks
            .post_candidate_parked
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Test-only: let the parked run continue.
    #[cfg(any(test, feature = "test-support"))]
    pub fn release_post_candidate_hold(&self) {
        self.hooks.post_candidate_hold.send_replace(false);
    }

    /// The run's side of the hold: a no-op unless a test armed it.
    #[cfg(any(test, feature = "test-support"))]
    async fn hold_after_candidate_read(&self) {
        let mut rx = self.hooks.post_candidate_hold.subscribe();
        if !*rx.borrow_and_update() {
            return;
        }
        self.hooks
            .post_candidate_parked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = rx.wait_for(|armed| !*armed).await;
        self.hooks
            .post_candidate_parked
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Run one collection, dry or real.
    ///
    /// The whole run happens in a spawned task the caller merely awaits.
    /// axum drops a handler future the moment the client disconnects, and
    /// this run has a window — between the postgres commit and the cache
    /// eviction — where being dropped would leave the pin gone from the
    /// store and present in the cache. Owned by its own task, the sequence
    /// always finishes; a caller that walked away only loses the response.
    ///
    /// # Errors
    /// [`ServerError::Conflict`] when another run is already in progress,
    /// when a repin owns the data root, when the corpus cannot be proved
    /// (unreadable path, foreign parquet), or when a gated store call times
    /// out; [`ServerError::Store`] for a postgres failure;
    /// [`ServerError::Internal`] if the scan task panics.
    pub async fn run(
        self: &Arc<Self>,
        older_than: Option<Duration>,
        dry_run: bool,
        actor: GcActor,
    ) -> Result<trawl_api::GcPinsResponse, ServerError> {
        let engine = Arc::clone(self);
        match tokio::spawn(async move { engine.run_owned(older_than, dry_run, actor).await }).await
        {
            Ok(outcome) => outcome,
            Err(e) => Err(ServerError::Internal(format!("pin gc task failed: {e}"))),
        }
    }

    /// The engine sequence, on its own task.
    async fn run_owned(
        self: Arc<Self>,
        older_than: Option<Duration>,
        dry_run: bool,
        actor: GcActor,
    ) -> Result<trawl_api::GcPinsResponse, ServerError> {
        // 0. Admission: this engine runs one collection at a time, and a
        //    second caller is told so immediately rather than queued behind
        //    a gate-holding scan.
        let Ok(_admitted) = self.admission.try_lock() else {
            return Err(ServerError::Conflict(
                "a pin gc run is already in progress; wait for it to finish \
                 and read its report rather than starting a second scan"
                    .to_owned(),
            ));
        };
        #[cfg(any(test, feature = "test-support"))]
        let _admission_signal = {
            self.hooks
                .admitted
                .store(true, std::sync::atomic::Ordering::SeqCst);
            AdmissionSignal(Arc::clone(&self.hooks))
        };

        // 1. Entry checks, before any work: a repin rearranging the corpus
        //    makes every footer proof provisional.
        self.refuse_if_repin_owns_the_corpus().await?;

        // 2. One clock for the whole run.
        let decided_at = Utc::now();
        let window = effective_dead_window(older_than, self.retention_floor_secs);
        let cutoff = cutoff_for(decided_at, window.effective_secs);

        // 3. The observation axis.
        let rows: Vec<GcPinRow> = self
            .store
            .pins_unobserved_since(cutoff)
            .await?
            .into_iter()
            .filter(|row| candidacy(&row.field, row.last_seen, cutoff) == Candidacy::Candidate)
            .collect();
        if rows.is_empty() {
            // Nothing to disprove: no gate, no walk. Compaction keeps
            // running through a gc that has nothing to do.
            let response = Report {
                window: &window,
                decided_at,
                dry_run,
                pins_examined: 0,
                pins: &[],
                files_scanned: 0,
                deleted: 0,
            }
            .build();
            log_complete(&response, &actor, 0);
            return Ok(response);
        }

        // 4. The metadata axis, under the corpus write gate: no compaction
        //    batch may publish a file between the scan and the delete.
        #[cfg(any(test, feature = "test-support"))]
        self.hold_after_candidate_read().await;
        let gate = self.coordinator.cutover_guard().await;
        let held_since = Instant::now();
        let outcome = self.prove_and_purge(&rows, dry_run).await;
        let gate_held_ms = u64::try_from(held_since.elapsed().as_millis()).unwrap_or(u64::MAX);
        drop(gate);

        let Purged {
            pins,
            files_scanned,
            deleted,
        } = outcome?;

        // The metric counts rows postgres actually deleted, so it is
        // incremented after the commit and never on a dry run.
        if deleted > 0 {
            metrics::counter!(crate::metrics::CATALOG_PINS_GC_TOTAL).increment(deleted);
        }
        let response = Report {
            window: &window,
            decided_at,
            dry_run,
            pins_examined: rows.len(),
            pins: &pins,
            files_scanned,
            deleted,
        }
        .build();
        if !dry_run {
            for pin in &pins {
                log_deleted(pin, &response, &actor);
            }
        }
        log_complete(&response, &actor, gate_held_ms);
        Ok(response)
    }

    /// Everything that happens under the corpus gate: the re-check, the
    /// walk, and (for a real run) the purge and the cache eviction.
    ///
    /// One function so the gate's scope is one `await` in the caller and
    /// the eviction provably precedes the release.
    async fn prove_and_purge(
        &self,
        rows: &[GcPinRow],
        dry_run: bool,
    ) -> Result<Purged, ServerError> {
        // The same two questions as the entry check, asked again with the
        // gate held. A courtesy fast-path only: a repin CLAIM takes no
        // corpus gate, so this look is still check-then-act. The authority
        // is the purge transaction, which asks the same question under the
        // catalog lifecycle lock a claim also takes
        // ([`crate::store::CATALOG_LIFECYCLE_LOCK_KEY`]) — asking here
        // merely saves a full footer scan in the common case.
        self.refuse_if_repin_owns_the_root()?;
        let latest = self
            .in_gate(
                "re-checking for a running repin job",
                self.repin_store.latest(),
            )
            .await?;
        Self::refuse_if_repin_job_running(latest.as_ref())?;

        let candidates: BTreeSet<String> = rows.iter().map(|row| row.field.clone()).collect();
        let data_dir = self.data_dir.clone();
        let walk = tokio::task::spawn_blocking(move || prove_carriers(&data_dir, candidates))
            .await
            .map_err(|e| ServerError::Internal(format!("pin gc scan task failed: {e}")))?
            .map_err(ServerError::Conflict)?;

        if dry_run || walk.dead.is_empty() {
            // A dry run mutates nothing at all: no delete, no eviction, no
            // generation bump. That is the whole safety story for a command
            // whose real form is irreversible. A real run with an empty set
            // takes the same exit for the simpler reason that there is
            // nothing to delete.
            let pins = rows
                .iter()
                .filter(|row| walk.dead.contains(&row.field))
                .map(projected)
                .collect();
            return Ok(Purged {
                pins,
                files_scanned: walk.files_scanned,
                deleted: 0,
            });
        }

        let fields: Vec<String> = walk.dead.iter().cloned().collect();
        // NOT bounded here. The purge bounds each of its own phases, and it
        // has to be the one to do it: the pre-commit phase is cancellable
        // and gets a wall-clock timeout inside the store, while the commit
        // gets a bound that DETACHES instead of cancelling. A timeout out
        // here would fire across both, drop a future postgres goes on to
        // commit, and this run would answer "deleted nothing" over a
        // catalog that lost the rows while every reader's cache kept them.
        let purged = match self.store.delete_pins(&fields).await {
            Ok(purged) => purged,
            // The run reached the commit and cannot say what postgres did
            // with it: either the commit outstayed its bound and is STILL
            // RUNNING on a task of its own, or it returned an error that a
            // backend which already made it durable can send. This is the
            // one error where the reconcile read is wrong: it races the
            // commit and could come back with either state, and a "still
            // pinned" answer read a microsecond early is a cache left
            // holding pins postgres has deleted. So skip the read entirely
            // and over-evict every candidate.
            Err(crate::store::StoreError::PurgeCommitUnknown) => {
                self.cache.evict_many(fields.iter().map(String::as_str));
                // The same over-correction the eviction makes: postgres may
                // have deleted the rows, so every cached derivation of the
                // pin set has to be rebuilt whether or not this map held
                // them.
                self.cache.touch_generation();
                return Err(ServerError::ServiceUnavailable(format!(
                    "pin gc's purge could not confirm its commit, so whether {} pin(s) \
                     were reclaimed is unknown: the commit either outstayed its {}s \
                     bound and is still in flight, or reported an error postgres may \
                     have applied anyway. Every candidate has been dropped from the \
                     pin cache, which is safe either way. Run `trawl schema gc-pins \
                     --dry-run` to see what the catalog actually holds before running \
                     it again.",
                    fields.len(),
                    crate::store::PURGE_COMMIT_BOUND.as_secs(),
                )));
            }
            Err(e) => {
                // Everything that failed BEFORE the commit, unclassified:
                // the DELETE may or may not have been issued, but nothing
                // was committed, so re-reading `field_types` settles it.
                // The cache is reconciled against postgres before the gate
                // opens, so cache-vs-store divergence cannot outlive the
                // gate.
                self.reconcile_cache(&fields).await;
                return Err(match e {
                    // The purge transaction's own running-row check is the
                    // authority on the claim race, and it refuses inside
                    // the transaction, so nothing was deleted.
                    crate::store::StoreError::RepinAlreadyRunning => ServerError::Conflict(
                        "a repin job claimed the catalog while pin gc was proving its \
                         candidates dead, so the purge refused and deleted nothing; \
                         re-run pin gc once the repin finishes"
                            .to_owned(),
                    ),
                    other => ServerError::from(other),
                });
            }
        };
        // Committed, so the cache may lose them — and must, before the gate
        // opens: infallible, one lock, one generation bump, nothing
        // fallible between it and the commit. The store hands back the
        // remaining-pin count from inside its own transaction precisely so
        // this sequence has no fallible step left; the gauges follow the
        // eviction, never precede it.
        self.cache
            .evict_many(purged.deleted.iter().map(|pin| pin.field.as_str()));
        if !purged.deleted.is_empty() {
            // The STORE decides whether this was a change, not the map. A
            // run retrying after an earlier over-eviction finds the map
            // already empty, so a bump conditioned on what `evict_many`
            // removed would be skipped and the schema cache would go on
            // listing a field postgres just deleted until its TTL ran out.
            self.cache.touch_generation();
        }
        self.store.publish_fill_gauges(purged.pinned_now);

        // The purge's own RETURNING set replaces the walk's projection from
        // here on, so the report and the audit records can only name pins
        // this run actually deleted, described as the transaction saw them.
        // The two differ when a candidate loses its row underneath the run
        // (a concurrent purge, an operator with psql): the walk still
        // believes in it, postgres does not.
        let deleted = u64::try_from(purged.deleted.len()).unwrap_or(u64::MAX);
        Ok(Purged {
            pins: purged.deleted,
            files_scanned: walk.files_scanned,
            deleted,
        })
    }

    /// Settle the pin cache against postgres after a purge that failed
    /// BEFORE its commit, while the corpus gate is still held.
    ///
    /// Every error this serves comes from the purge's pre-commit phase — a
    /// failed statement, a refused claim, the client-side prepare bound —
    /// and none of them can have committed anything. That is what makes
    /// re-reading `field_types` a settlement rather than a race: the answer
    /// is stable, because no commit is on its way.
    ///
    /// The asymmetry is the whole point. Evicting a pin postgres still
    /// holds costs a re-read: `pin_missing` goes to postgres, finds the row
    /// and puts it back, and in the meantime a column is treated as
    /// unpinned. NOT evicting a pin postgres deleted is corruption: readers
    /// keep typing comparisons by a pin no row backs, and compaction
    /// conforms new batches to it. So the failure of the reconcile READ is
    /// resolved by over-evicting every candidate, never by leaving the
    /// cache alone.
    ///
    /// The read is bounded by [`IN_GATE_TIMEOUT`] like the other gated
    /// reads: cancelling it is safe, and the timeout arm evicts everything
    /// anyway.
    ///
    /// One error never gets here at all.
    /// [`crate::store::StoreError::PurgeCommitUnknown`] means the commit
    /// was reached and its outcome is unknown — detached and still running,
    /// or failed by a backend that may have made it durable first — and a
    /// read racing that can answer with the state on either side of it.
    /// Reading "still pinned" a microsecond early would leave the cache
    /// holding pins postgres has deleted, which is the corruption this
    /// whole asymmetry exists to prevent, so that arm skips the read and
    /// over-evicts unconditionally.
    async fn reconcile_cache(&self, fields: &[String]) {
        match self
            .in_gate(
                "reconciling the pin cache after a failed purge",
                self.store.pins_present(fields),
            )
            .await
        {
            Ok(present) => {
                let evicted = self.cache.evict_many(
                    fields
                        .iter()
                        .map(String::as_str)
                        .filter(|field| !present.contains(*field)),
                );
                // Nothing committed on this path, so the map IS the change:
                // stamp only when the cache actually gave a pin up.
                if evicted > 0 {
                    self.cache.touch_generation();
                }
            }
            Err(e) => {
                tracing::error!(
                    event_type = "catalog_gc_reconcile_failed",
                    error_class = e.error_class(),
                    candidates = fields.len(),
                    "pin gc could not re-read the catalog after a failed purge; \
                     evicting every candidate from the pin cache"
                );
                if self.cache.evict_many(fields.iter().map(String::as_str)) > 0 {
                    self.cache.touch_generation();
                }
            }
        }
    }

    /// Refuse while a repin owns the data root: marker, shadow or aside
    /// root present.
    ///
    /// Unreadable evidence is evidence. `in_flight_evidence` is fallible
    /// exactly so that an I/O error cannot read as "go ahead", and gc is
    /// one of the two callers that must not proceed on a maybe.
    fn refuse_if_repin_owns_the_root(&self) -> Result<(), ServerError> {
        match crate::repin::in_flight_evidence(&self.data_dir) {
            Ok(None) => Ok(()),
            Ok(Some(what)) => Err(ServerError::Conflict(format!(
                "a repin owns the data root (its {what} is present), so no \
                 parquet footer proves anything right now; pin gc stands \
                 down until the repin finishes"
            ))),
            Err(e) => Err(ServerError::Conflict(format!(
                "pin gc cannot tell whether a repin owns the data root \
                 ({e}), so it refuses rather than delete on a guess"
            ))),
        }
    }

    /// The postgres half of the same question, over a job row already read.
    ///
    /// Split from the read so the gated caller can put the read behind
    /// [`Self::in_gate`] without a second copy of the verdict.
    fn refuse_if_repin_job_running(
        job: Option<&crate::store::RepinJob>,
    ) -> Result<(), ServerError> {
        match job {
            Some(job) if job.status == RepinJobStatus::Running => {
                Err(ServerError::Conflict(format!(
                    "repin job {} is running; pin gc stands down until it finishes",
                    job.id
                )))
            }
            _ => Ok(()),
        }
    }

    /// Both halves, off the gate: the entry check.
    async fn refuse_if_repin_owns_the_corpus(&self) -> Result<(), ServerError> {
        self.refuse_if_repin_owns_the_root()?;
        Self::refuse_if_repin_job_running(self.repin_store.latest().await?.as_ref())
    }

    /// Run one postgres operation under the corpus gate, bounded by
    /// [`IN_GATE_TIMEOUT`].
    ///
    /// Every gated store call goes through here, not just the purge. The
    /// gate excludes whole compaction batches, so a postgres that stops
    /// answering during the re-check stalls ingest exactly as one that
    /// stops answering during the delete would. A timeout is UNKNOWN, never
    /// a pass: `what` names the operation and the remedy is always the same
    /// one, a dry run that reports the catalog's real state.
    async fn in_gate<T>(
        &self,
        what: &str,
        op: impl Future<Output = Result<T, crate::store::StoreError>>,
    ) -> Result<T, ServerError> {
        match tokio::time::timeout(IN_GATE_TIMEOUT, op).await {
            Ok(result) => result.map_err(ServerError::from),
            Err(_elapsed) => Err(ServerError::Conflict(format!(
                "pin gc lost contact with the catalog store while {what} after \
                 {}s, with the corpus gate held; it deleted nothing it can \
                 account for. Re-run `trawl schema gc-pins --dry-run` to see \
                 the catalog's real state before running it again.",
                IN_GATE_TIMEOUT.as_secs()
            ))),
        }
    }
}

/// Everything the wire report is built from.
///
/// A struct rather than seven positional arguments, because six of them
/// are numbers and two of those are windows in seconds.
struct Report<'a> {
    window: &'a DeadWindow,
    decided_at: DateTime<Utc>,
    dry_run: bool,
    /// How many candidates the observation axis produced.
    pins_examined: usize,
    /// The pins the run reports: what the purge transaction returned on a
    /// real run, what the footer scan left undisproved on a dry one.
    pins: &'a [PurgedPin],
    files_scanned: u64,
    /// Rows postgres reported deleted; 0 on a dry run.
    deleted: u64,
}

impl Report<'_> {
    /// One shape for a dry run and a real one — `dry_run` and `deleted`
    /// are what tell them apart.
    fn build(self) -> trawl_api::GcPinsResponse {
        // Both producers are field-sorted already (`pins_unobserved_since`
        // orders by field, the purge sorts its RETURNING set), so the
        // candidate list needs no second sort.
        let candidates = self
            .pins
            .iter()
            .map(|pin| trawl_api::GcPinCandidate {
                field: pin.field.clone(),
                data_type: pin.duckdb_type.clone(),
                last_seen: pin.last_seen.map(iso8601),
                services: pin.services,
            })
            .collect::<Vec<_>>();
        trawl_api::GcPinsResponse {
            dry_run: self.dry_run,
            decided_at: iso8601(self.decided_at),
            requested_older_than_secs: self.window.requested_secs,
            retention_floor_secs: self.window.floor_secs,
            effective_older_than_secs: self.window.effective_secs,
            pins_examined: self.pins_examined as u64,
            files_scanned: self.files_scanned,
            candidates,
            deleted: self.deleted,
        }
    }
}

/// One candidate as the pre-gate snapshot described it, for the dry run.
///
/// A dry run deletes nothing, so there is no transaction to read the
/// current row from and nothing to audit; `pinned_from` is the one field
/// the candidate query does not carry and no report renders it.
fn projected(row: &GcPinRow) -> PurgedPin {
    PurgedPin {
        field: row.field.clone(),
        duckdb_type: row.duckdb_type.clone(),
        pinned_from: None,
        pinned_at: row.pinned_at,
        last_seen: row.last_seen,
        services: row.services,
    }
}

/// The run summary: one event per run, dry or real.
fn log_complete(r: &trawl_api::GcPinsResponse, actor: &GcActor, gate_held_ms: u64) {
    tracing::info!(
        event_type = "catalog_pin_gc_complete",
        dry_run = r.dry_run,
        decided_at = %r.decided_at,
        requested_older_than_secs = r.requested_older_than_secs,
        retention_floor_secs = r.retention_floor_secs,
        effective_older_than_secs = r.effective_older_than_secs,
        pins_examined = r.pins_examined,
        files_scanned = r.files_scanned,
        deleted = r.deleted,
        gate_held_ms,
        actor = actor.name.as_deref(),
        actor_key_prefix = actor.key_prefix.as_deref(),
        "pin gc finished"
    );
}

/// The purge's outcome, so the caller can release the gate before it
/// touches metrics or logs.
#[derive(Debug)]
struct Purged {
    pins: Vec<PurgedPin>,
    files_scanned: u64,
    deleted: u64,
}

/// One audit record per pin actually deleted (execution only).
///
/// The catalog row is gone after this, so everything the record needs is
/// in it: what the pin was, who set it, when it was taken, when it was last
/// observed, and the window that judged it. Every one of those comes from
/// the purge transaction, not from the candidate snapshot the run started
/// with, so a repin that finished in between cannot make the record name a
/// type the row no longer had.
fn log_deleted(pin: &PurgedPin, r: &trawl_api::GcPinsResponse, actor: &GcActor) {
    tracing::info!(
        event_type = "catalog_pin_gc",
        field = %pin.field,
        deleted_type = %pin.duckdb_type,
        pinned_from = pin.pinned_from.as_deref(),
        pinned_at = %iso8601(pin.pinned_at),
        last_seen = pin.last_seen.map(iso8601).as_deref(),
        services = pin.services,
        decided_at = %r.decided_at,
        requested_older_than_secs = r.requested_older_than_secs,
        retention_floor_secs = r.retention_floor_secs,
        effective_older_than_secs = r.effective_older_than_secs,
        files_scanned = r.files_scanned,
        actor = actor.name.as_deref(),
        actor_key_prefix = actor.key_prefix.as_deref(),
        "reclaimed a field-catalog pin"
    );
}

/// `decided_at` minus the window, clamped at the unix epoch.
///
/// `max_age_days` is an unvalidated operator number, so an "effectively
/// never" retention floor lands here as a window no subtraction can hold.
/// Going as far back as the arithmetic allows makes every OBSERVED pin
/// alive, which is the direction that deletes nothing.
///
/// The floor is the epoch rather than [`DateTime::<Utc>::MIN_UTC`] because
/// the cutoff is bound into a `timestamptz` comparison, and chrono's
/// minimum (year -262143) is outside what postgres will accept: binding it
/// turns a huge but perfectly valid `older_than_secs` into a 503 at query
/// time instead of a run that conservatively matches nothing. The two are
/// identical in effect. `field_services.last_seen` is written by trawl at
/// observation time, so no row predates the install, let alone 1970, and
/// every observed pin is on the alive side of either instant.
fn cutoff_for(decided_at: DateTime<Utc>, window_secs: u64) -> DateTime<Utc> {
    i64::try_from(window_secs)
        .ok()
        .and_then(chrono::TimeDelta::try_seconds)
        .and_then(|d| decided_at.checked_sub_signed(d))
        .unwrap_or(DateTime::UNIX_EPOCH)
        .max(DateTime::UNIX_EPOCH)
}

fn iso8601(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Walk the standing corpus and remove from `dead` every field some
/// parquet footer declares.
///
/// Blocking (one `read_dir` per directory, one footer read per file), so
/// callers run it on the blocking pool.
///
/// The scan domain is every `*.parquet` under each valid live `data/{env}/`
/// tree: hourly files, daily rollups, and anything else that happens to sit
/// there, because presence in the query glob is exactly what makes a file's
/// footer evidence. `data/scheduled/` and `wal/` are excluded by the env
/// name rule ([`trawl_config::RESERVED_ENV_NAMES`]); the repin staging
/// siblings live outside the data root and their existence has already been
/// refused by the time this runs.
///
/// It fails the whole run closed. A symlink under an env dir, a `.parquet`
/// that is not a regular file, a `.parquet` whose footer will not parse, a nested foreign schema, a directory that
/// cannot be listed, a file that vanished mid-walk: each is a file whose
/// columns are unknown, and an unknown file cannot be part of a proof that
/// nothing carries a field. Returning "no carrier found" from a corpus that
/// was only partly read is exactly the wrong answer, since the outcome is a
/// deletion. Non-parquet artefacts (`.corrupt` quarantines, `.tmp`
/// leftovers, marker files) are skipped, not refused: they are outside the
/// query glob and carry no columns.
///
/// # Errors
/// The refusal message, naming the count and up to
/// [`MAX_REPORTED_PATHS`] paths.
fn prove_carriers(data_dir: &Path, mut dead: BTreeSet<String>) -> Result<Walk, String> {
    let mut files_scanned = 0u64;
    let mut problems = Problems::default();

    let mut stack = env_dirs_strict(data_dir, &mut problems)?;
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                problems.note(&dir, &format!("unreadable directory: {e}"));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    problems.note(&dir, &format!("unreadable directory entry: {e}"));
                    continue;
                }
            };
            let path = entry.path();
            // `symlink_metadata`, never `metadata`: a symlink that resolves
            // is still a path whose target trawl does not own, and one that
            // dangles would otherwise report as a vanished file.
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) => {
                    problems.note(&path, &format!("unreadable: {e}"));
                    continue;
                }
            };
            if meta.is_symlink() {
                problems.note(&path, "symlink under a live env directory");
            } else if meta.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "parquet") {
                // Regular-file, decided from the SAME lstat that refused the
                // symlink. A FIFO, a device node or a unix socket wearing a
                // `.parquet` name is not a file trawl wrote, and opening one
                // under the corpus gate can block until somebody writes the
                // other end — an ingest stall with no timeout on it.
                if !meta.is_file() {
                    problems.note(&path, "not a regular file");
                    continue;
                }
                files_scanned += 1;
                match trawl_engine::parquet_stats::read_column_names(&path) {
                    Ok(names) => {
                        for name in names {
                            dead.remove(&catalog_key(&name));
                        }
                        if dead.is_empty() {
                            // Every candidate is carried: nothing left to
                            // disprove, so the rest of the corpus cannot
                            // change the answer.
                            return Ok(Walk {
                                dead,
                                files_scanned,
                            });
                        }
                    }
                    Err(e) => problems.note(&path, &format!("unreadable parquet: {e}")),
                }
            }
        }
    }

    problems.refusal().map_or(
        Ok(Walk {
            dead,
            files_scanned,
        }),
        Err,
    )
}

/// The env directories to walk, strictly.
///
/// Same env rule as the query planner and the compactor
/// ([`trawl_config::is_valid_env_name`] plus the reserved names), with one
/// addition: a symlink WEARING a valid env name is refused rather than
/// skipped, because the query glob would descend into it and read the
/// files it points at. Everything else at the top of the data root —
/// marker files, `scheduled/`, a directory whose name is not an env name —
/// is outside the glob and outside this walk.
///
/// A data root that is not there is UNKNOWN, not empty. An absent
/// directory reads identically to a corpus of zero files, and a corpus of
/// zero files disproves nothing, so every candidate would be "dead" and the
/// run would delete the whole catalog. An unmounted volume, a typo'd
/// `[storage] data_dir`, a root moved aside by hand: each is a reason to
/// refuse, and none is a reason to reclaim. A genuine cold start has an
/// empty data root (trawld creates it at boot), which walks to zero files
/// and is still refused only if a candidate exists — which, on a corpus
/// nothing has ever written, it does not.
fn env_dirs_strict(data_dir: &Path, problems: &mut Problems) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) => {
            return Err(format!(
                "pin gc cannot read the data root {} ({e}), so it cannot \
                 prove any pin dead and deleted nothing. Check that the \
                 volume is mounted and [storage] data_dir is right, then \
                 re-run.",
                data_dir.display()
            ));
        }
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                problems.note(data_dir, &format!("unreadable data-root entry: {e}"));
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !trawl_config::is_valid_env_name(name)
            || trawl_config::RESERVED_ENV_NAMES.contains(&name)
        {
            continue;
        }
        let path = entry.path();
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_symlink() => problems.note(&path, "symlinked env directory"),
            Ok(meta) if meta.is_dir() => dirs.push(path),
            Ok(_) => {}
            Err(e) => problems.note(&path, &format!("unreadable: {e}")),
        }
    }
    dirs.sort();
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;
    use trawl_core::schema::ENVELOPE_TYPES;

    const DAY: u64 = 86_400;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid instant")
    }

    #[test]
    fn dead_window_truth_table() {
        // No request, no retention: the packaged default.
        let w = effective_dead_window(None, None);
        assert_eq!(w.requested_secs, 30 * DAY);
        assert_eq!(w.floor_secs, None);
        assert_eq!(w.effective_secs, 30 * DAY);
        assert_eq!(w.effective(), DEFAULT_DEAD_WINDOW);

        // 7 days requested under 90 days of retention: the floor wins.
        let w = effective_dead_window(Some(Duration::from_secs(7 * DAY)), Some(90 * DAY));
        assert_eq!(w.requested_secs, 7 * DAY);
        assert_eq!(w.floor_secs, Some(90 * DAY));
        assert_eq!(w.effective_secs, 90 * DAY);

        // A request longer than the floor stands, and the floor is still
        // reported so the operator can see it did not bind.
        let w = effective_dead_window(Some(Duration::from_secs(120 * DAY)), Some(90 * DAY));
        assert_eq!(w.effective_secs, 120 * DAY);
        assert_eq!(w.floor_secs, Some(90 * DAY));

        // Retention disabled: the request is the answer, default included.
        let w = effective_dead_window(Some(Duration::from_secs(DAY)), None);
        assert_eq!(w.effective_secs, DAY);
        assert_eq!(effective_dead_window(None, None).effective_secs, 30 * DAY);

        // Zero is a window, not a missing value.
        let w = effective_dead_window(Some(Duration::ZERO), None);
        assert_eq!(w.requested_secs, 0);
        assert_eq!(w.effective_secs, 0);

        // ...and it is still floored when retention names one.
        let w = effective_dead_window(Some(Duration::ZERO), Some(90 * DAY));
        assert_eq!(w.effective_secs, 90 * DAY);
    }

    /// The envelope is trawl's contract, so no window and no absence of
    /// observations can reclaim its seeded pins. `ENVELOPE_TYPES` growing
    /// a slot must not need this test edited.
    #[test]
    fn every_envelope_seed_survives_gc() {
        let cutoff = at(4_000_000_000);
        let ancient = at(0);

        for (field, _) in ENVELOPE_TYPES {
            assert_eq!(
                candidacy(field, None, cutoff),
                Candidacy::ContractTyped,
                "{field} (never observed) escaped the envelope refusal"
            );
            assert_eq!(
                candidacy(field, Some(ancient), cutoff),
                Candidacy::ContractTyped,
                "{field} (last seen in 1970) escaped the envelope refusal"
            );
        }

        // The four sender-asserted bare names carry no `_` prefix, so
        // they are the half a prefix predicate alone would miss.
        for field in ["env", "service", "host", "message"] {
            assert_eq!(candidacy(field, None, cutoff), Candidacy::ContractTyped);
        }
    }

    #[test]
    fn observation_exactly_at_the_cutoff_is_alive() {
        let cutoff = at(1_000_000);
        assert_eq!(
            candidacy("duration", Some(cutoff), cutoff),
            Candidacy::ObservedInWindow
        );
        assert_eq!(
            candidacy("duration", Some(at(1_000_001)), cutoff),
            Candidacy::ObservedInWindow
        );
        assert_eq!(
            candidacy("duration", Some(at(999_999)), cutoff),
            Candidacy::Candidate
        );
    }

    /// The body of `prove_and_purge`, for the two source-shape tests below.
    fn gated_section() -> &'static str {
        let src = include_str!("gc.rs");
        let start = src
            .find("    async fn prove_and_purge(")
            .expect("the gated section is one function");
        let body = &src[start..];
        let end = body.find("\n    }\n").expect("the function closes");
        &body[..end]
    }

    /// Every READ made with the corpus gate held rides [`IN_GATE_TIMEOUT`],
    /// and the purge deliberately does not.
    ///
    /// Source-shape, and deliberately so: no fixture can make a live
    /// postgres stop answering in the middle of a gated transaction, so
    /// what is worth asserting is that a gated statement cannot be ADDED
    /// without a decision about its bound. The gate excludes whole
    /// compaction batches, and an unbounded read there is an ingest stall of
    /// unbounded length — while a purge cancelled from OUT here is worse
    /// than a slow one, because the commit it abandons may still land. The
    /// purge is bounded, just not by this caller: it splits its own phases
    /// and applies a cancelling bound to the one where cancelling is safe.
    #[test]
    fn every_gated_store_read_rides_the_timeout_and_the_purge_does_not() {
        let body = gated_section();
        assert!(
            body.contains(".in_gate("),
            "the gated section makes its store reads through in_gate"
        );
        for call in ["self.store.", "self.repin_store."] {
            for (idx, _) in body.match_indices(call) {
                // The statement the call sits in: from the previous `;` to
                // the next one. An AWAITED store call is a round trip that
                // can hang; `publish_fill_gauges` and friends are local.
                let head = body[..idx].rsplit(';').next().unwrap_or_default();
                let tail = body[idx..].split(';').next().unwrap_or_default();
                let statement = format!("{head}{tail}");
                if !statement.contains(".await") {
                    continue;
                }
                if statement.contains("delete_pins(") {
                    assert!(
                        !statement.contains(".in_gate("),
                        "the purge must not be wrapped in a cancellation \
                         timeout; postgres bounds it: {statement}"
                    );
                    continue;
                }
                assert!(
                    statement.contains(".in_gate("),
                    "an awaited `{call}` READ under the corpus gate must ride \
                     in_gate: {statement}"
                );
            }
        }
    }

    /// A purge that failed with an unknown outcome reconciles the pin cache
    /// against postgres BEFORE the caller drops the corpus gate.
    ///
    /// Source-shape too: injecting a store failure into a live purge means
    /// killing postgres mid-transaction, which no fixture here can do. What
    /// the shape guards is the ordering — reconcile, then return — because
    /// a `return Err` added above the reconcile would let a cache holding
    /// pins postgres deleted go on serving comparisons and conforming
    /// batches under them.
    #[test]
    fn a_failed_purge_reconciles_the_cache_before_it_returns() {
        let arm = ordinary_failure_arm();
        let reconcile = arm
            .find("self.reconcile_cache(&fields).await;")
            .expect("the error arm reconciles the cache");
        let returns = arm.find("return Err(").expect("the error arm returns");
        assert!(
            reconcile < returns,
            "the reconcile must precede the refusal, so no error path leaves \
             the gate with the cache and the store disagreeing"
        );
    }

    /// The purge's two failure arms, split off the one `match`.
    ///
    /// Returns (unknown-commit arm, every-other-error arm). The unknown arm
    /// is written first in the source, so the ordinary one is what follows
    /// `Err(e) => {`.
    fn purge_failure_arms() -> (&'static str, &'static str) {
        let body = gated_section();
        let tail = body
            .split("match self.store.delete_pins(&fields).await")
            .nth(1)
            .expect("the purge is one match on delete_pins");
        let unknown = tail
            .split("Err(crate::store::StoreError::PurgeCommitUnknown) => {")
            .nth(1)
            .expect("the purge matches the unknown-commit error by name");
        let (unknown, ordinary) = unknown
            .split_once("Err(e) => {")
            .expect("every other error shares one arm");
        (unknown, ordinary)
    }

    fn ordinary_failure_arm() -> &'static str {
        purge_failure_arms().1
    }

    /// The generation follows what the STORE deleted, never what the map
    /// happened to hold.
    ///
    /// Source-shape: the wiring is what can rot. "How many entries did the
    /// map lose" is the wrong question after an earlier run over-evicted —
    /// the map is empty, the store still has rows to delete, and a cached
    /// `/api/v1/schema` listing would outlive the deletion by a TTL. Both
    /// purge outcomes that may have changed postgres stamp the generation
    /// off the STORE's answer.
    #[test]
    fn the_generation_follows_the_store_not_the_map() {
        let body = gated_section();
        let committed = body
            .split("self.store.publish_fill_gauges(")
            .next()
            .expect("the success path publishes the gauges last");
        assert!(
            committed.contains("if !purged.deleted.is_empty() {")
                && committed.contains("self.cache.touch_generation();"),
            "a non-empty RETURNING set bumps the generation, whatever \
             evict_many found"
        );
        let (unknown, _) = purge_failure_arms();
        assert!(
            unknown.contains("self.cache.touch_generation();"),
            "an unknown outcome bumps too: postgres may have deleted the rows"
        );
    }

    /// A purge whose commit outcome is unknown must NOT re-read postgres.
    ///
    /// The commit was detached rather than cancelled, or it failed in a way
    /// that does not prove a rollback, so a reconcile read races it either
    /// way: "still pinned" answered a microsecond early would leave the
    /// cache serving pins postgres has deleted. The safe move is the
    /// over-evicting one, so this arm evicts every candidate and asks the
    /// store nothing.
    ///
    /// Source-shape, like its sibling above: no fixture can stall a live
    /// postgres commit past its bound, so what is guarded is that the arm
    /// keeps its shape.
    #[test]
    fn an_unknown_commit_over_evicts_and_never_reads_the_store() {
        let (unknown, _) = purge_failure_arms();
        assert!(
            unknown.contains("self.cache.evict_many(fields.iter().map(String::as_str));"),
            "the unknown-commit arm evicts every candidate, not a filtered subset"
        );
        assert!(
            !unknown.contains("reconcile_cache") && !unknown.contains("pins_present"),
            "the unknown-commit arm must not read postgres; a read races the commit"
        );
        assert!(
            unknown.contains("--dry-run"),
            "the refusal tells the operator how to observe the real state"
        );
    }

    /// A window nothing can subtract lands on the unix epoch, not on
    /// chrono's minimum.
    ///
    /// The cutoff is bound straight into a `timestamptz` comparison, and
    /// postgres refuses year -262143, so the old saturating floor turned an
    /// absurd-but-valid `--older-than` into a 503 at bind time. The epoch
    /// answers the same question (nothing was observed before 1970) and
    /// binds.
    #[test]
    fn an_unsubtractable_window_clamps_to_the_epoch() {
        let decided_at = at(1_800_000_000);
        for window in [u64::MAX, u64::try_from(i64::MAX).unwrap(), 1 << 62] {
            assert_eq!(
                cutoff_for(decided_at, window),
                DateTime::UNIX_EPOCH,
                "window {window} must clamp to the epoch"
            );
        }
        // A window that merely reaches back past 1970 clamps too, and an
        // ordinary one is untouched.
        assert_eq!(
            cutoff_for(decided_at, 1_800_000_001),
            DateTime::UNIX_EPOCH,
            "a cutoff before 1970 is the epoch"
        );
        assert_eq!(cutoff_for(decided_at, 86_400), at(1_799_913_600));
    }

    /// The divergence from the schema listing, asserted so a later "make
    /// gc match /schema" cleanup has to argue with a test.
    #[test]
    fn a_never_observed_pin_is_a_candidate() {
        assert_eq!(candidacy("typoed_feild", None, at(0)), Candidacy::Candidate);
    }
}
