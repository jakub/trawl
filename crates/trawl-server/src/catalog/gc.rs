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
use crate::store::{CatalogStore, GcPinRow, RepinJobStatus, RepinStore};

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

/// How long the purge transaction may take before the run gives up on it.
///
/// The corpus gate is held across it, and while it is held no compaction
/// batch can publish, so a postgres that has stopped answering must not
/// translate into an ingest stall of unbounded length.
const PURGE_TIMEOUT: Duration = Duration::from_secs(5);

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
        }
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
    /// [`ServerError::Conflict`] when a repin owns the data root, when the
    /// corpus cannot be proved (unreadable path, foreign parquet), or when
    /// the purge times out; [`ServerError::Store`] for a postgres failure;
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
                rows: &rows,
                dead: &BTreeSet::new(),
                files_scanned: 0,
                deleted: 0,
            }
            .build();
            log_complete(&response, &actor, 0);
            return Ok(response);
        }

        // 4. The metadata axis, under the corpus write gate: no compaction
        //    batch may publish a file between the scan and the delete.
        let gate = self.coordinator.cutover_guard().await;
        let held_since = Instant::now();
        let outcome = self.prove_and_purge(&rows, dry_run).await;
        let gate_held_ms = u64::try_from(held_since.elapsed().as_millis()).unwrap_or(u64::MAX);
        drop(gate);

        let Purged {
            dead,
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
            rows: &rows,
            dead: &dead,
            files_scanned,
            deleted,
        }
        .build();
        if !dry_run {
            for row in rows.iter().filter(|row| dead.contains(&row.field)) {
                log_deleted(row, &response, &actor);
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
        self.refuse_if_repin_owns_the_corpus().await?;

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
            return Ok(Purged {
                dead: walk.dead,
                files_scanned: walk.files_scanned,
                deleted: 0,
            });
        }

        let fields: Vec<String> = walk.dead.iter().cloned().collect();
        let purged =
            match tokio::time::timeout(PURGE_TIMEOUT, self.store.delete_pins(&fields)).await {
                // The purge transaction's own running-row check is the
                // authority on the claim race, and it refuses inside the
                // transaction, so nothing was deleted.
                Ok(Err(crate::store::StoreError::RepinAlreadyRunning)) => {
                    return Err(ServerError::Conflict(
                        "a repin job claimed the catalog while pin gc was proving its \
                         candidates dead, so the purge refused and deleted nothing; \
                         re-run pin gc once the repin finishes"
                            .to_owned(),
                    ));
                }
                Ok(result) => result?,
                Err(_elapsed) => {
                    // The statement was already sent, so postgres may have
                    // committed it after we stopped waiting. The honest answer
                    // is UNKNOWN: refuse loudly, evict nothing (an eviction
                    // over rows that survived would hide a pin the store still
                    // has), and let the operator observe the real state.
                    // Retrying blind would delete a second time or report a
                    // deletion that never happened.
                    return Err(ServerError::Conflict(format!(
                        "pin gc lost contact with the catalog store while \
                         deleting {} pin(s) after {}s; whether the purge \
                         committed is unknown. Re-run `trawl schema gc-pins \
                         --dry-run` to see which pins are still there before \
                         running it again.",
                        fields.len(),
                        PURGE_TIMEOUT.as_secs()
                    )));
                }
            };
        // Committed, so the cache may lose them — and must, before the gate
        // opens: infallible, one lock, one generation bump, nothing
        // fallible between it and the commit. The store hands back the
        // remaining-pin count from inside its own transaction precisely so
        // this sequence has no fallible step left; the gauges follow the
        // eviction, never precede it.
        self.cache
            .evict_many(purged.deleted.iter().map(String::as_str));
        self.store.publish_fill_gauges(purged.pinned_now);

        let deleted = u64::try_from(purged.deleted.len()).unwrap_or(u64::MAX);
        Ok(Purged {
            dead: walk.dead,
            files_scanned: walk.files_scanned,
            deleted,
        })
    }

    /// Refuse while a repin owns the data root: marker, shadow or aside
    /// root present, or a `running` job row.
    ///
    /// Unreadable evidence is evidence. `in_flight_evidence` is fallible
    /// exactly so that an I/O error cannot read as "go ahead", and gc is
    /// one of the two callers that must not proceed on a maybe.
    async fn refuse_if_repin_owns_the_corpus(&self) -> Result<(), ServerError> {
        match crate::repin::in_flight_evidence(&self.data_dir) {
            Ok(None) => {}
            Ok(Some(what)) => {
                return Err(ServerError::Conflict(format!(
                    "a repin owns the data root (its {what} is present), so no \
                     parquet footer proves anything right now; pin gc stands \
                     down until the repin finishes"
                )));
            }
            Err(e) => {
                return Err(ServerError::Conflict(format!(
                    "pin gc cannot tell whether a repin owns the data root \
                     ({e}), so it refuses rather than delete on a guess"
                )));
            }
        }
        if let Some(job) = self.repin_store.latest().await?
            && job.status == RepinJobStatus::Running
        {
            return Err(ServerError::Conflict(format!(
                "repin job {} is running; pin gc stands down until it finishes",
                job.id
            )));
        }
        Ok(())
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
    /// Every candidate the observation axis produced.
    rows: &'a [GcPinRow],
    /// The candidates the footer scan left undisproved.
    dead: &'a BTreeSet<String>,
    files_scanned: u64,
    /// Rows postgres reported deleted; 0 on a dry run.
    deleted: u64,
}

impl Report<'_> {
    /// One shape for a dry run and a real one — `dry_run` and `deleted`
    /// are what tell them apart.
    fn build(self) -> trawl_api::GcPinsResponse {
        // `pins_unobserved_since` orders by field, so the candidate list is
        // field-sorted without a second sort.
        let candidates = self
            .rows
            .iter()
            .filter(|row| self.dead.contains(&row.field))
            .map(|row| trawl_api::GcPinCandidate {
                field: row.field.clone(),
                data_type: row.duckdb_type.clone(),
                last_seen: row.last_seen.map(iso8601),
                services: row.services,
            })
            .collect::<Vec<_>>();
        trawl_api::GcPinsResponse {
            dry_run: self.dry_run,
            decided_at: iso8601(self.decided_at),
            requested_older_than_secs: self.window.requested_secs,
            retention_floor_secs: self.window.floor_secs,
            effective_older_than_secs: self.window.effective_secs,
            pins_examined: self.rows.len() as u64,
            files_scanned: self.files_scanned,
            candidates,
            deleted: self.deleted,
        }
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
    dead: BTreeSet<String>,
    files_scanned: u64,
    deleted: u64,
}

/// One audit record per pin actually deleted (execution only).
///
/// The catalog row is gone after this, so everything the record needs is
/// in it: what the pin was, when it was taken, when it was last observed,
/// and the window that judged it.
fn log_deleted(row: &GcPinRow, r: &trawl_api::GcPinsResponse, actor: &GcActor) {
    tracing::info!(
        event_type = "catalog_pin_gc",
        field = %row.field,
        deleted_type = %row.duckdb_type,
        pinned_at = %iso8601(row.pinned_at),
        last_seen = row.last_seen.map(iso8601).as_deref(),
        services = row.services,
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

/// `decided_at` minus the window, saturating at the earliest representable
/// instant.
///
/// `max_age_days` is an unvalidated operator number, so an "effectively
/// never" retention floor lands here as a window no subtraction can hold.
/// Saturating to the beginning of time makes every OBSERVED pin alive,
/// which is the direction that deletes nothing.
fn cutoff_for(decided_at: DateTime<Utc>, window_secs: u64) -> DateTime<Utc> {
    i64::try_from(window_secs)
        .ok()
        .and_then(chrono::TimeDelta::try_seconds)
        .and_then(|d| decided_at.checked_sub_signed(d))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
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
/// whose footer will not parse, a nested foreign schema, a directory that
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
/// A missing data root is a cold start, not a failure: no file exists, so
/// no file carries anything.
fn env_dirs_strict(data_dir: &Path, problems: &mut Problems) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "pin gc cannot list the data root {} ({e}), so it cannot \
                 prove any pin dead and deleted nothing",
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

    /// The divergence from the schema listing, asserted so a later "make
    /// gc match /schema" cleanup has to argue with a test.
    #[test]
    fn a_never_observed_pin_is_a_candidate() {
        assert_eq!(candidacy("typoed_feild", None, at(0)), Candidacy::Candidate);
    }
}
