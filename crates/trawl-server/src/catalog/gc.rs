// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin garbage collection: reclaiming catalog slots no live field holds.

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

pub const DEFAULT_DEAD_WINDOW: Duration = Duration::from_hours(24 * 30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadWindow {
    pub requested_secs: u64,
    /// The retention floor in seconds, when a finite retention horizon
    /// exists. `None` when the global `max_age_days` or any
    /// `[retention.env.*]` entry is 0: no finite age then bounds what the
    /// corpus still holds, so nothing floors the request and it applies as
    /// given (the standing-parquet footer axis is what still guards a pin).
    pub floor_secs: Option<u64>,
    pub effective_secs: u64,
}

impl DeadWindow {
    #[must_use]
    pub fn effective(&self) -> Duration {
        Duration::from_secs(self.effective_secs)
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidacy {
    Candidate,
    ContractTyped,
    ObservedInWindow,
}

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

/// Read-only only. Cancelling a read costs the answer and nothing else;
/// cancelling the purge would abandon a transaction whose commit may still
/// land, which is why that one is bounded inside postgres instead
/// ([`crate::store::CatalogStore::delete_pins`]).
const IN_GATE_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_REPORTED_PATHS: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct GcActor {
    pub name: Option<String>,
    pub key_prefix: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PinGc {
    store: CatalogStore,
    repin_store: RepinStore,
    cache: Arc<FieldCatalog>,
    coordinator: Arc<RepinCoordinator>,
    data_dir: PathBuf,
    retention_floor_secs: Option<u64>,
    admission: Arc<tokio::sync::Mutex<()>>,
    /// Test-only observation and hold points, held on the engine rather
    /// than in a static so one test's armed hold cannot leak into another
    /// test's run in the same binary (the repin coordinator's idiom).
    #[cfg(any(test, feature = "test-support"))]
    hooks: Arc<TestHooks>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
struct TestHooks {
    admitted: std::sync::atomic::AtomicBool,
    post_candidate_hold: tokio::sync::watch::Sender<bool>,
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

#[derive(Debug)]
struct Walk {
    dead: BTreeSet<String>,
    files_scanned: u64,
}

#[derive(Debug)]
pub(crate) struct Problems {
    root: PathBuf,
    count: usize,
    shown: Vec<String>,
}

impl Problems {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            count: 0,
            shown: Vec::new(),
        }
    }

    pub(crate) fn note(&mut self, path: &Path, why: &str) {
        self.count += 1;
        if self.shown.len() < MAX_REPORTED_PATHS {
            self.shown
                .push(format!("{} ({why})", root_relative(&self.root, path)));
        }
    }

    pub(crate) fn note_io(&mut self, path: &Path, what: &str, e: &std::io::Error) {
        tracing::debug!(
            event_type = "catalog_gc_unreadable_path",
            path = %path.display(),
            error = %e,
            "pin gc could not read a path under the data root"
        );
        self.note(path, &format!("{what}: {}", e.kind()));
    }

    pub(crate) fn note_parquet(
        &mut self,
        path: &Path,
        e: &trawl_engine::parquet_stats::ParquetStatsError,
    ) {
        tracing::debug!(
            event_type = "catalog_gc_unreadable_path",
            path = %path.display(),
            error = %e,
            "pin gc could not read a parquet footer under the data root"
        );
        self.note(path, &parquet_problem(e));
    }

    pub(crate) fn refusal(&self) -> Option<String> {
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

pub(crate) fn root_relative(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => "the data root".to_owned(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => "a path outside the data root".to_owned(),
    }
}

pub(crate) fn parquet_problem(e: &trawl_engine::parquet_stats::ParquetStatsError) -> String {
    use trawl_engine::parquet_stats::ParquetStatsError;
    match e {
        ParquetStatsError::Open { source, .. } => {
            format!("unreadable parquet: {}", source.kind())
        }
        ParquetStatsError::Footer { .. } => "unreadable parquet: the footer does not parse".into(),
        ParquetStatsError::NonScalarColumn { .. } => {
            "unreadable parquet: a top-level column is not a scalar".into()
        }
    }
}

pub(crate) fn unreadable_evidence_refusal(kind: std::io::ErrorKind) -> String {
    format!(
        "pin gc cannot tell whether a repin owns the data root (checking \
         for its marker, shadow and aside roots failed: {kind}), so it \
         refuses rather than delete on a guess"
    )
}

pub(crate) fn data_root_unreadable_refusal(kind: std::io::ErrorKind) -> String {
    format!(
        "pin gc cannot read the data root ({kind}), so it cannot prove any \
         pin dead and deleted nothing. Check that the volume is mounted and \
         [data] path is right, then re-run."
    )
}

impl PinGc {
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

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn admission_held(&self) -> bool {
        self.hooks
            .admitted
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn arm_post_candidate_hold(&self) {
        self.hooks.post_candidate_hold.send_replace(true);
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn parked_after_candidate_read(&self) -> bool {
        self.hooks
            .post_candidate_parked
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn release_post_candidate_hold(&self) {
        self.hooks.post_candidate_hold.send_replace(false);
    }

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

    /// The whole run happens in a spawned task the caller merely awaits.
    /// axum drops a handler future the moment the client disconnects, and
    /// this run has a window — between the postgres commit and the cache
    /// eviction — where being dropped would leave the pin gone from the
    /// store and present in the cache. Owned by its own task, the sequence
    /// always finishes; a caller that walked away only loses the response.
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
            Err(e) => Err(ServerError::from_join("pin gc", e)),
        }
    }

    async fn run_owned(
        self: Arc<Self>,
        older_than: Option<Duration>,
        dry_run: bool,
        actor: GcActor,
    ) -> Result<trawl_api::GcPinsResponse, ServerError> {
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

        self.refuse_if_repin_owns_the_corpus().await?;

        // One clock for the whole run.
        let decided_at = Utc::now();
        let window = effective_dead_window(older_than, self.retention_floor_secs);
        let cutoff = cutoff_for(decided_at, window.effective_secs);

        let rows: Vec<GcPinRow> = self
            .store
            .pins_unobserved_since(cutoff)
            .await?
            .into_iter()
            .filter(|row| candidacy(&row.field, row.last_seen, cutoff) == Candidacy::Candidate)
            .collect();
        if rows.is_empty() {
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
            .map_err(|e| ServerError::from_join("pin gc scan", e))?
            .map_err(ServerError::Conflict)?;

        if dry_run || walk.dead.is_empty() {
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

    fn refuse_if_repin_owns_the_root(&self) -> Result<(), ServerError> {
        match crate::repin::in_flight_evidence(&self.data_dir) {
            Ok(None) => Ok(()),
            Ok(Some(what)) => Err(ServerError::Conflict(format!(
                "a repin owns the data root (its {what} is present), so no \
                 parquet footer proves anything right now; pin gc stands \
                 down until the repin finishes"
            ))),
            Err(e) => {
                tracing::debug!(
                    event_type = "catalog_gc_unreadable_path",
                    error = %e,
                    "pin gc could not check for repin evidence"
                );
                Err(ServerError::Conflict(unreadable_evidence_refusal(e.kind())))
            }
        }
    }

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

    async fn refuse_if_repin_owns_the_corpus(&self) -> Result<(), ServerError> {
        self.refuse_if_repin_owns_the_root()?;
        Self::refuse_if_repin_job_running(self.repin_store.latest().await?.as_ref())
    }

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

struct Report<'a> {
    window: &'a DeadWindow,
    decided_at: DateTime<Utc>,
    dry_run: bool,
    pins_examined: usize,
    pins: &'a [PurgedPin],
    files_scanned: u64,
    deleted: u64,
}

impl Report<'_> {
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

#[derive(Debug)]
struct Purged {
    pins: Vec<PurgedPin>,
    files_scanned: u64,
    deleted: u64,
}

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

/// `max_age_days` is an unvalidated operator number, in the global and in
/// every `[retention.env.*]` override, so an "effectively never" retention
/// floor lands here as a window no subtraction can hold.
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

fn prove_carriers(data_dir: &Path, mut dead: BTreeSet<String>) -> Result<Walk, String> {
    let mut files_scanned = 0u64;
    let mut problems = Problems::new(data_dir);

    let mut stack = env_dirs_strict(data_dir, &mut problems)?;
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                problems.note_io(&dir, "unreadable directory", &e);
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    problems.note_io(&dir, "unreadable directory entry", &e);
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
                    problems.note_io(&path, "unreadable", &e);
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
                            return Ok(Walk {
                                dead,
                                files_scanned,
                            });
                        }
                    }
                    Err(e) => problems.note_parquet(&path, &e),
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
/// `[data] path`, a root moved aside by hand: each is a reason to
/// refuse, and none is a reason to reclaim.
fn env_dirs_strict(data_dir: &Path, problems: &mut Problems) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(
                event_type = "catalog_gc_unreadable_path",
                path = %data_dir.display(),
                error = %e,
                "pin gc could not list the data root"
            );
            return Err(data_root_unreadable_refusal(e.kind()));
        }
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                problems.note_io(data_dir, "unreadable data-root entry", &e);
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
            Err(e) => problems.note_io(&path, "unreadable", &e),
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
        let w = effective_dead_window(None, None);
        assert_eq!(w.requested_secs, 30 * DAY);
        assert_eq!(w.floor_secs, None);
        assert_eq!(w.effective_secs, 30 * DAY);
        assert_eq!(w.effective(), DEFAULT_DEAD_WINDOW);

        let w = effective_dead_window(Some(Duration::from_secs(7 * DAY)), Some(90 * DAY));
        assert_eq!(w.requested_secs, 7 * DAY);
        assert_eq!(w.floor_secs, Some(90 * DAY));
        assert_eq!(w.effective_secs, 90 * DAY);

        let w = effective_dead_window(Some(Duration::from_secs(120 * DAY)), Some(90 * DAY));
        assert_eq!(w.effective_secs, 120 * DAY);
        assert_eq!(w.floor_secs, Some(90 * DAY));

        let w = effective_dead_window(Some(Duration::from_secs(DAY)), None);
        assert_eq!(w.effective_secs, DAY);
        assert_eq!(effective_dead_window(None, None).effective_secs, 30 * DAY);

        let w = effective_dead_window(Some(Duration::ZERO), None);
        assert_eq!(w.requested_secs, 0);
        assert_eq!(w.effective_secs, 0);

        let w = effective_dead_window(Some(Duration::ZERO), Some(90 * DAY));
        assert_eq!(w.effective_secs, 90 * DAY);
    }

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

    fn gated_section() -> &'static str {
        let src = include_str!("gc.rs");
        let start = src
            .find("    async fn prove_and_purge(")
            .expect("the gated section is one function");
        let body = &src[start..];
        let end = body.find("\n    }\n").expect("the function closes");
        &body[..end]
    }

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
        assert_eq!(
            cutoff_for(decided_at, 1_800_000_001),
            DateTime::UNIX_EPOCH,
            "a cutoff before 1970 is the epoch"
        );
        assert_eq!(cutoff_for(decided_at, 86_400), at(1_799_913_600));
    }

    #[test]
    fn a_never_observed_pin_is_a_candidate() {
        assert_eq!(candidacy("typoed_feild", None, at(0)), Candidacy::Candidate);
    }

    #[test]
    fn refusals_do_not_carry_io_text_or_the_absolute_root() {
        const MARKER: &str = "zz_io_marker_/srv/secret";
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir_all(root.join("prod")).unwrap();
        let abs = root.display().to_string();
        assert!(root.is_absolute());
        let clean = |what: &str, msg: &str| {
            assert!(!msg.contains(MARKER), "{what} carries the io text: {msg}");
            assert!(!msg.contains(&abs), "{what} carries the root: {msg}");
            let tmp_abs = tmp.path().display().to_string();
            assert!(!msg.contains(&tmp_abs), "{what} carries a host path: {msg}");
        };

        let injected = std::io::Error::other(MARKER);
        let msg = unreadable_evidence_refusal(injected.kind());
        clean("evidence refusal", &msg);
        assert!(msg.contains("other error"), "names the kind: {msg}");
        assert!(msg.contains("refuses rather than delete"), "{msg}");

        let mut problems = Problems::new(&root);
        problems.note_io(&root.join("prod"), "unreadable directory", &injected);
        problems.note_io(&root, "unreadable data-root entry", &injected);
        problems.note_io(
            &std::path::PathBuf::from("/elsewhere/prod/x.parquet"),
            "unreadable",
            &injected,
        );
        let msg = problems.refusal().expect("a walk with problems refuses");
        clean("walk refusal", &msg);
        assert!(
            msg.contains("prod (unreadable directory: other error)"),
            "{msg}"
        );
        assert!(
            msg.contains("the data root (unreadable data-root entry"),
            "{msg}"
        );
        assert!(msg.contains("a path outside the data root"), "{msg}");
        assert!(
            msg.contains("Repair or move them aside, then re-run"),
            "{msg}"
        );

        let junk = root.join("prod").join("junk.parquet");
        std::fs::write(&junk, b"PAR1 and then some lies").unwrap();
        let e = trawl_engine::parquet_stats::read_column_names(&junk)
            .expect_err("a junk footer does not parse");
        assert!(e.to_string().contains(&abs), "premise: Display carries it");
        let mut problems = Problems::new(&root);
        problems.note_parquet(&junk, &e);
        let opened =
            trawl_engine::parquet_stats::read_column_names(&root.join("prod/gone.parquet"))
                .expect_err("a missing file does not open");
        problems.note_parquet(&root.join("prod/gone.parquet"), &opened);
        let msg = problems.refusal().unwrap();
        clean("parquet refusal", &msg);
        assert!(
            msg.contains("prod/junk.parquet (unreadable parquet: the footer does not parse)"),
            "{msg}"
        );
        assert!(
            msg.contains("prod/gone.parquet (unreadable parquet: entity not found)"),
            "{msg}"
        );

        let vanished = tmp.path().join("not-mounted");
        let msg = env_dirs_strict(&vanished, &mut Problems::new(&vanished))
            .expect_err("a missing data root refuses");
        clean("data-root refusal", &msg);
        assert!(!msg.contains("not-mounted"), "{msg}");
        assert!(!msg.contains("os error"), "{msg}");
        assert!(msg.contains("entity not found"), "{msg}");
        assert!(msg.contains("[data] path is right, then re-run"), "{msg}");
    }
}
