// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boot conformance pass (ADR-0009 slice 2, §2b): make the write-time
//! invariant true over the STANDING corpus, not just files written after
//! the catalog shipped.
//!
//! Runs inline at boot — after the storage epoch gate, before the ingest
//! pipeline and HTTP serving — and only on ingest-enabled nodes (a
//! query-only node does not own the data root).
//!
//! Identity is dual-sided: `catalog_state.catalog_id` in postgres is
//! mirrored into a `data/CATALOG` marker file. The pass is skipped only
//! when BOTH sides agree — a repointed `DATABASE_URL` or a data root
//! restored from backup shows up as a mismatch and forces a re-run. A
//! crash mid-pass leaves unrewritten files to be redetected on the next
//! boot (every rewrite is staged, fsynced + atomically renamed).
//!
//! Cost, since the pass sits in front of HTTP serving and a re-arm can hit
//! a corpus of any age (the first boot after upgrade is small — #52 moved
//! the legacy root aside — but a lost marker, a restored data root or a
//! recreated `trawl` database re-arms it over the whole standing corpus):
//! the scan is deliberately **metadata-only unless a column still needs a
//! pin vote**. Describing a file reads its footer; counting a column reads
//! the column. Only UNPINNED fields vote (`most_rows_wins` ignores the
//! pinned ones), so pins are loaded BEFORE the scan and the count query is
//! narrowed to the voting columns — and skipped entirely for a file whose
//! every field is already pinned. A re-arm against a catalog that already
//! pins the corpus (the common one: the data and the catalog were always a
//! pair) therefore costs one footer read per file, not one corpus read.
//! Both phases emit a `catalog_conform_progress` heartbeat so a long pass
//! is visibly working rather than indistinguishable from a hang, and every
//! rewrite is durable on its own, so a boot killed by a supervisor's start
//! timeout leaves the corpus strictly closer to conformant than it found it.
//!
//! Per-path failures are isolated, never boot-fatal: a truncated, bit-rotted
//! or foreign `.parquet` under the data root — or a whole subdirectory the
//! walk cannot enumerate — is skipped with a warning and a
//! `trawl_catalog_conform_skipped_total` bump, exactly like the rollup path
//! sniffs and sets aside unreadable inputs rather than wedging. Nothing is
//! moved or deleted (an operator's stray file is theirs), and because the
//! corpus was then NOT proven conformant, completion is deliberately not
//! published — the next boot re-runs the pass, so a transient read failure
//! self-heals and a permanent one keeps warning.
//!
//! "Foreign" is decided by the PATH, before the file is ever read
//! ([`layout_path`]): the pass rewrites a standing file IN PLACE, lossily
//! (every value the `TRY_CAST` cannot read becomes NULL) and irreversibly
//! (the source is the destination — there is no backup, no dry-run, and no
//! operator opt-in), so it may only ever touch files trawl itself wrote.
//! That means a path that reads back as `{env}/{date}[/{HH}]/{service}.parquet`
//! with every component passing the injective ingest predicates. A parquet an
//! operator dropped anywhere else under the data root is skipped exactly like
//! an unreadable one — same warning, same counter, same withheld completion —
//! because "readable" is not "mine", and silently rewriting someone else's
//! file would be the destructive surprise [`skip_file`] exists to refuse.
//!
//! The pass also BACKFILLS `field_services` from the files it adopted. Those
//! rows are the authority behind `?service=` and the `last_seen` window on
//! the schema surfaces, and until this they were written only by live
//! compaction — so on an upgrade the migration created the table empty and a
//! service whose data all predates the catalog answered `?service=` with
//! nothing while its pins sat outside every window forever. The backfill is
//! idempotent (the pass re-runs until the corpus is proven conformant) and
//! stamped from each file's partition directory, never `now()`.
//!
//! This machinery is deliberately the embryo of the repin rewriter (#53).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use trawl_core::schema::{CanonicalType, LADDER, TypeResolution, normalize_duckdb_type};

use super::FieldCatalog;
use crate::ingest::compaction::{
    AGG_CHUNK_COLS, ColInfo, ConformPlan, ConformPolicy, describe_source, is_valid_parquet,
    quote_ident,
};
use crate::store::{CatalogStore, FieldConflict, PinProposal, ServiceObservation};

/// Marker file mirroring `catalog_state.catalog_id` into the data root.
const CATALOG_MARKER: &str = "CATALOG";

/// `pinned_from` value for pins seeded by the boot pass.
const BOOT_PIN_SOURCE: &str = "_boot";

/// What one `ensure_conformance` call did.
#[derive(Debug, Clone, Copy)]
pub struct ConformSummary {
    /// Whether the pass ran (false = marker and catalog agreed; skipped).
    pub ran: bool,
    /// Parquet files scanned.
    pub scanned: usize,
    /// Files rewritten to conform.
    pub rewritten: usize,
    /// Files skipped as unreadable or foreign (nonzero = the corpus was not
    /// proven conformant, so completion is withheld and the next boot re-runs).
    pub skipped: usize,
    /// `(field, service)` observations backfilled from the standing corpus.
    pub observed: usize,
}

/// One scanned parquet file. Only ever built for a path that read back as
/// trawl's own layout ([`layout_path`]) — a `FileScan` is a licence to
/// rewrite the file in place, so a foreign path never becomes one.
struct FileScan {
    path: PathBuf,
    service: String,
    /// The instant this file's partition directory claims — the rewrite's
    /// never-NULL `_time`/`_ingested` fallback (see [`layout_path`]), and
    /// the `first_seen`/`last_seen` the observation backfill attributes to
    /// every field the file carries.
    time_fallback: chrono::DateTime<chrono::Utc>,
    /// Rows in the file, straight from its footer — the weight the
    /// observation backfill gives this file (see [`corpus_observations`]).
    rows: u64,
    schema: Vec<ColInfo>,
    /// Non-null row count per column, positionally parallel to `schema`.
    /// This — not the file's total row count — is a column's voting weight:
    /// a column that is 99.9% NULL in a huge file describes almost nothing
    /// and must not outvote the same field fully populated elsewhere.
    ///
    /// Zero for an already-pinned column: it holds no vote (`most_rows_wins`
    /// skips it), so the count is never read and deliberately never taken —
    /// counting it would read the column off disk for a number nothing uses.
    non_null: Vec<u64>,
}

/// Heartbeat interval for the per-phase progress log.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Per-phase progress heartbeat.
///
/// The pass blocks HTTP serving, so a long one must be visibly making
/// progress: without this, an operator watching a multi-thousand-file
/// corpus cannot tell a working boot from a wedged one (and a supervisor
/// start timeout looks identical to both).
pub(crate) struct Progress {
    phase: &'static str,
    total: usize,
    done: usize,
    started: std::time::Instant,
    last: std::time::Instant,
}

impl Progress {
    pub(crate) fn new(phase: &'static str, total: usize) -> Self {
        let now = std::time::Instant::now();
        Self {
            phase,
            total,
            done: 0,
            started: now,
            last: now,
        }
    }

    /// Count one file, logging at most once per [`PROGRESS_INTERVAL`].
    pub(crate) fn tick(&mut self) {
        self.done += 1;
        if self.last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last = std::time::Instant::now();
        tracing::info!(
            event_type = "catalog_conform_progress",
            phase = self.phase,
            done = self.done,
            total = self.total,
            elapsed_secs = self.started.elapsed().as_secs(),
            "boot conformance pass in progress (HTTP serving starts when it completes)"
        );
    }
}

/// Load the catalog's pins and mirror them into the in-process cache the
/// query path reads.
async fn hydrate(
    store: &CatalogStore,
    cache: &FieldCatalog,
) -> Result<Vec<(String, CanonicalType)>, String> {
    let pins = store
        .load_pins()
        .await
        .map_err(|e| format!("failed to load pins: {e}"))?;
    cache.replace(pins.clone());
    Ok(pins)
}

/// What the dual-sided marker proved about the archive a query-only node is
/// about to serve. Every variant boots; the refusal is an `Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveIdentity {
    /// `data/{CATALOG_MARKER}` names the catalog this node is connected to.
    Proven,
    /// The archive positively holds no parquet — a cold start, with no
    /// schema to get wrong.
    Empty,
    /// The archive holds parquet and carries no marker at all: nothing
    /// proves it is this catalog's, and nothing proves it is not.
    Unproven,
}

/// Query-only boot gate: prove the standing archive was written by the
/// catalog this node is connected to.
///
/// An ingest-enabled node earns that proof by running the pass above. A
/// query-only node deliberately does not (it owns nothing under the data
/// root), yet it still serves `/api/v1/schema` — and since ADR-0009 slice 3
/// that answer is the catalog's pins, not a `DESCRIBE`. Pins from an
/// unrelated catalog describe unrelated columns: point a query-only trawld
/// at a shared or read-only archive with a fresh `trawl` database and
/// `/schema` advertises the seeded envelope while queries read entirely
/// different physical columns. So the same dual-sided marker that lets the
/// pass skip itself is checked here as a gate.
///
/// The refusal is deliberately narrow: only a marker naming ANOTHER catalog
/// is fatal, because only that is positive proof of the wrong pairing. A
/// MISSING marker is not proof of anything — and it is an ordinary state,
/// since [`publish_completion`] withholds the marker whenever the pass
/// skipped a path (`catalog_conform_incomplete`; an operator's export
/// subtree under the data root is enough), warns, and serves. Refusing the
/// boot for that same corpus would make "disable ingest and restart to
/// investigate" a startup failure curable only by re-enabling ingest, and
/// would answer identical state with warn-and-serve on one node and
/// fail-closed on the other. So an unmarked archive returns
/// [`ArchiveIdentity::Unproven`] and the caller warns, exactly as the ingest
/// node does.
///
/// Only the unproven path walks the tree, so a matching marker costs one
/// `read_to_string`.
pub async fn verify_archive_identity(
    store: &CatalogStore,
    data_dir: &Path,
) -> Result<ArchiveIdentity, String> {
    let catalog_id = store
        .catalog_id()
        .await
        .map_err(|e| format!("failed to read catalog identity: {e}"))?;
    let marker = read_marker(data_dir);
    if marker.as_deref() == Some(catalog_id.as_str()) {
        return Ok(ArchiveIdentity::Proven);
    }
    if archive_is_empty(data_dir) {
        return Ok(ArchiveIdentity::Empty);
    }
    match marker {
        Some(other) => Err(format!(
            "the parquet archive at {} was written by a different catalog than \
             the one this node is connected to (data/{CATALOG_MARKER} = \
             {other}, catalog_state.catalog_id = {catalog_id}), so its columns \
             are not the pins /api/v1/schema would advertise. Point the app \
             database at the catalog that owns this archive, or boot once with \
             [ingest] enabled = true to run the conformance pass and adopt it",
            data_dir.display(),
        )),
        None => Ok(ArchiveIdentity::Unproven),
    }
}

/// Whether the data root positively holds no parquet — the only state in
/// which an unprovable identity is provably harmless.
///
/// A walk failure is NOT emptiness: the subtree it could not enumerate may
/// hold the whole corpus, so it reads as standing data. That is conservative
/// where it matters (a foreign marker still refuses) and costs nothing where
/// it does not (an unmarked archive warns either way).
fn archive_is_empty(data_dir: &Path) -> bool {
    if !data_dir.is_dir() {
        return true;
    }
    let (files, errors) = crate::metrics::walk_parquet_files_lossy(data_dir);
    if let Some((path, e)) = errors.into_iter().next() {
        tracing::warn!(
            event_type = "catalog_identity_walk_failed",
            file = %path.display(),
            error = %e,
            "cannot enumerate the data root, so the archive cannot be proven \
             empty; treating it as holding standing data"
        );
        return false;
    }
    files.is_empty()
}

/// Run the boot conformance pass unless the dual-sided identity says it
/// already ran for exactly this (catalog, data root) pair.
pub async fn ensure_conformance(
    store: &CatalogStore,
    cache: &FieldCatalog,
    data_dir: &Path,
    memory_limit: &str,
) -> Result<ConformSummary, String> {
    let catalog_id = store
        .catalog_id()
        .await
        .map_err(|e| format!("failed to read catalog identity: {e}"))?;
    let conformed = store
        .is_conformed()
        .await
        .map_err(|e| format!("failed to read conformance state: {e}"))?;
    // The observation backfill has its OWN flag, and both must be set to
    // skip the pass. `conformed_at` alone would make the backfill inert on
    // exactly the installs it was written for: a node that conformed under
    // the previous slice carries the marker and `conformed_at`, so the pass
    // would return here — leaving `field_services` empty forever for every
    // service whose data is standing parquet no live batch re-sends.
    let backfilled = store
        .services_backfilled()
        .await
        .map_err(|e| format!("failed to read observation backfill state: {e}"))?;
    if conformed && backfilled && read_marker(data_dir).as_deref() == Some(catalog_id.as_str()) {
        // Still hydrate the cache — skipping the pass must not skip pins.
        hydrate(store, cache).await?;
        return Ok(ConformSummary {
            ran: false,
            scanned: 0,
            rewritten: 0,
            skipped: 0,
            observed: 0,
        });
    }

    tracing::info!(
        event_type = "catalog_conform_start",
        data_dir = %data_dir.display(),
        conformed,
        backfilled,
        "boot conformance pass starting (first boot with this catalog, a \
         catalog/data-root identity mismatch, or a corpus conformed before \
         the observation backfill existed)"
    );

    // Pins BEFORE the scan, not after: they are what makes the scan cheap.
    // An already-pinned field never votes, so its column is never counted —
    // and a file with no unpinned field is described from its footer alone.
    let existing: HashMap<String, CanonicalType> = store
        .load_pins()
        .await
        .map_err(|e| format!("failed to load pins: {e}"))?
        .into_iter()
        .collect();

    // Phase A (blocking): enumerate + describe the corpus.
    let (scan, scan_skipped) = {
        let data_dir = data_dir.to_path_buf();
        let memory_limit = memory_limit.to_owned();
        let pinned = existing.clone();
        tokio::task::spawn_blocking(move || scan_corpus(&data_dir, &memory_limit, &pinned))
            .await
            .map_err(|e| format!("conformance scan task panicked: {e}"))??
    };

    // Seed pins: declared fields came with the migration; custom fields by
    // most-rows-wins across files — rows CARRYING the field, not the files'
    // row counts — ties by ladder order.
    // Unrationed: these proposals describe columns that are ALREADY on
    // disk, and phase B rewrites whatever stays unpinned out of the files
    // carrying it. The ingest path's half-the-free-slots ration exists to
    // stop a burst from claiming the catalog; applying it here would only
    // delete standing data to slow a sender who has already spent the slots.
    let proposals = most_rows_wins(&scan, &existing);
    store
        .pin_missing_unrationed(&proposals)
        .await
        .map_err(|e| format!("failed to seed pins: {e}"))?;
    let pins: HashMap<String, CanonicalType> = hydrate(store, cache).await?.into_iter().collect();

    // What the standing corpus attests to, read off the scan before it is
    // consumed by the rewrite. Written after phase B, so a crash mid-rewrite
    // cannot leave observations claiming files that were never conformed.
    let observations = corpus_observations(&scan, &pins);

    // Phase B (blocking): rewrite the nonconforming files.
    let scanned = scan.len();
    let (rewritten, rewrite_skipped, conflicts) = {
        let data_dir = data_dir.to_path_buf();
        let memory_limit = memory_limit.to_owned();
        tokio::task::spawn_blocking(move || {
            rewrite_nonconforming(&scan, &pins, &data_dir, &memory_limit)
        })
        .await
        .map_err(|e| format!("conformance rewrite task panicked: {e}"))??
    };
    let skipped = scan_skipped + rewrite_skipped;

    record_boot_conflicts(store, &conflicts).await;

    // NOT best-effort, unlike the conflict evidence: `field_services` is the
    // authority behind `?service=` and the `last_seen` window, and this pass
    // is the ONLY thing that will ever observe a corpus no live batch
    // re-sends. Dropping it with a warning would publish the marker over a
    // permanent gap; failing the boot leaves the pass armed for the retry.
    let observed = observations.len();
    store
        .backfill_services(&observations)
        .await
        .map_err(|e| format!("failed to backfill field_services observations: {e}"))?;

    if rewritten > 0 {
        metrics::counter!(crate::metrics::CATALOG_CONFORM_REWRITES_TOTAL)
            .increment(rewritten as u64);
    }

    publish_completion(store, data_dir, &catalog_id, scanned, rewritten, skipped).await?;

    tracing::info!(
        event_type = "catalog_conform_complete",
        scanned,
        rewritten,
        skipped,
        observed,
        conflicts = conflicts.len(),
        "boot conformance pass complete"
    );

    Ok(ConformSummary {
        ran: true,
        scanned,
        rewritten,
        skipped,
        observed,
    })
}

/// Publish completion LAST: postgres side, then the marker file — and only
/// when every file was accounted for. Skipped files mean the corpus is not
/// proven conformant, so the identity stays unpublished and the next boot
/// re-runs the pass rather than declaring victory forever.
///
/// Both completion flags are stamped here, together: a skipped path means
/// the observations are as incomplete as the rewrites, so neither is
/// declared done.
async fn publish_completion(
    store: &CatalogStore,
    data_dir: &Path,
    catalog_id: &str,
    scanned: usize,
    rewritten: usize,
    skipped: usize,
) -> Result<(), String> {
    if skipped > 0 {
        tracing::warn!(
            event_type = "catalog_conform_incomplete",
            scanned,
            rewritten,
            skipped,
            "boot conformance pass skipped unreadable or foreign paths; the \
             corpus is not proven conformant and the pass will re-run on the \
             next boot — inspect the skipped paths (queries touching them error)"
        );
        return Ok(());
    }
    store
        .mark_conformed()
        .await
        .map_err(|e| format!("failed to record conformance completion: {e}"))?;
    store
        .mark_services_backfilled()
        .await
        .map_err(|e| format!("failed to record observation backfill completion: {e}"))?;
    publish_marker(data_dir, catalog_id)
}

/// Isolate one bad path: warn, count, leave it exactly where it is.
///
/// Deliberately not the rollup's quarantine-rename — the rollup MUST move a
/// corrupt input aside or it re-reads it forever, whereas the boot pass just
/// declines to touch what it cannot read. Renaming an operator's file at boot
/// would be a destructive surprise on a path (foreign parquet dropped into
/// the tree) where the honest answer is "not mine".
fn skip_file(path: &Path, phase: &str, error: &str) {
    metrics::counter!(crate::metrics::CATALOG_CONFORM_SKIPPED_TOTAL).increment(1);
    tracing::warn!(
        event_type = "catalog_conform_skip",
        file = %path.display(),
        phase,
        error,
        "unreadable or foreign path skipped by the boot conformance pass; it \
         is left untouched and remains outside the catalog invariant"
    );
}

/// Record boot-pass conflict evidence: `field_conflicts` rows (best
/// effort) and the same conflict/rows-nulled counters compaction bumps —
/// one metric surface for "the catalog nulled data", regardless of which
/// pass did it.
async fn record_boot_conflicts(store: &CatalogStore, conflicts: &[FieldConflict]) {
    if conflicts.is_empty() {
        return;
    }
    if let Err(e) = store.record_conflicts(conflicts).await {
        tracing::warn!(
            event_type = "catalog_bookkeeping_error",
            error = %e,
            "boot pass failed to record field_conflicts rows"
        );
    }
    let mut by_service: HashMap<&str, Vec<FieldConflict>> = HashMap::new();
    for c in conflicts {
        by_service
            .entry(c.service.as_str())
            .or_default()
            .push(c.clone());
    }
    for (service, group) in by_service {
        crate::ingest::compaction::record_conflict_metrics(service, &group);
    }
}

/// Read the data-root marker, if present.
fn read_marker(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join(CATALOG_MARKER))
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Publish the marker via the epoch idiom: staged write → fsync → atomic
/// rename, so a crash can never leave a half-written identity.
fn publish_marker(data_dir: &Path, catalog_id: &str) -> Result<(), String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("failed to create data root for marker: {e}"))?;
    // PID-unique staged name: concurrent publishers (test harnesses share
    // a fixture corpus) must not clobber each other's staged file between
    // write and rename.
    let staged = data_dir.join(format!("{CATALOG_MARKER}.next.{}", std::process::id()));
    std::fs::write(&staged, format!("{catalog_id}\n"))
        .map_err(|e| format!("failed to write {}: {e}", staged.display()))?;
    std::fs::File::open(&staged)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("failed to fsync {}: {e}", staged.display()))?;
    let marker = data_dir.join(CATALOG_MARKER);
    std::fs::rename(&staged, &marker)
        .map_err(|e| format!("failed to publish {}: {e}", marker.display()))?;
    crate::epoch::fsync_dir_best_effort(data_dir);
    Ok(())
}

/// Open an in-memory `DuckDB` connection bounded like compaction's: temp
/// directory on the data root (spill-to-disk works on read-only container
/// overlays), capped memory, two threads — and the UTC session zone every
/// conform depends on
/// ([`trawl_core::conform::SESSION_TIME_ZONE_SQL`]).
pub(crate) fn open_bounded_connection(
    data_dir: &Path,
    memory_limit: &str,
) -> Result<duckdb::Connection, String> {
    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;
    conn.execute_batch(&format!(
        "SET temp_directory='{}'",
        data_dir.to_string_lossy().replace('\'', "''")
    ))
    .map_err(|e| format!("SET temp_directory failed: {e}"))?;
    conn.execute_batch(&format!(
        "SET memory_limit='{}'; SET threads=2",
        memory_limit.replace('\'', "''")
    ))
    .map_err(|e| format!("SET memory_limit/threads failed: {e}"))?;
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .map_err(|e| format!("SET TimeZone failed: {e}"))?;
    Ok(conn)
}

/// Enumerate every parquet file under the data root (hourly + daily
/// rollups, skipping `scheduled/`) and describe each. Returns the readable
/// files plus the count of paths skipped as unreadable or foreign — one bad
/// file, or one unreadable directory, must never take the daemon's boot down
/// with it.
fn scan_corpus(
    data_dir: &Path,
    memory_limit: &str,
    pinned: &HashMap<String, CanonicalType>,
) -> Result<(Vec<FileScan>, usize), String> {
    // A root that was never created is a cold start, not a failure: there is
    // no corpus to prove anything about.
    if !data_dir.is_dir() {
        return Ok((Vec::new(), 0));
    }
    // A directory that cannot be enumerated (permissions, a transient fault,
    // a stale handle) is isolated exactly like an unreadable file rather than
    // aborting the walk — this tree is wider than the corpus (the WAL lives
    // under the data root by default), and one unreadable corner of it must
    // not keep the daemon down. It counts as a skip, so the corpus is not
    // proven conformant and the next boot re-runs.
    let (files, walk_errors) = crate::metrics::walk_parquet_files_lossy(data_dir);
    let mut skipped = walk_errors.len();
    for (path, error) in walk_errors {
        skip_file(&path, "walk", &error.to_string());
    }
    if files.is_empty() {
        return Ok((Vec::new(), skipped));
    }
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    // Announce the corpus size up front: the one number that tells an
    // operator (and a supervisor start-timeout budget) what this boot is in for.
    tracing::info!(
        event_type = "catalog_conform_scan_start",
        files = files.len(),
        pinned_fields = pinned.len(),
        "boot conformance pass scanning the corpus"
    );
    let mut progress = Progress::new("scan", files.len());

    let mut out = Vec::with_capacity(files.len());
    for (path, _) in files {
        // The layout gate comes FIRST, before the file is opened: everything
        // downstream of the scan may rewrite the file in place, so a path
        // trawl did not write is set aside here, unread and untouched.
        let Some(layout) = layout_path(data_dir, &path) else {
            skipped += 1;
            skip_file(
                &path,
                "layout",
                "not in trawl's {env}/{date}[/{HH}]/{service}.parquet layout — \
                 the boot pass only rewrites files it wrote itself",
            );
            progress.tick();
            continue;
        };
        match scan_file(&conn, path.clone(), &layout, pinned) {
            Ok(scan) => out.push(scan),
            Err(e) => {
                skipped += 1;
                skip_file(&path, "scan", &e);
            }
        }
        progress.tick();
    }
    Ok((out, skipped))
}

/// Where one standing file sits in trawl's own storage layout.
pub(crate) struct LayoutPath {
    /// The service the path names — read off the layout, not guessed from a
    /// file stem, so it is the same string ingest wrote verbatim.
    pub(crate) service: String,
    /// The instant this file's partition directory claims (see
    /// [`ConformPolicy::StandingFile`]).
    pub(crate) instant: chrono::DateTime<chrono::Utc>,
}

/// Read `path` back as trawl's own storage layout — `{env}/{date}/{HH}/
/// {service}.parquet`, or `{env}/{date}/{service}.parquet` for a daily
/// rollup — relative to the data root. `None` = foreign, i.e. NOT ours.
///
/// This is the whole safety gate on an in-place, lossy, irreversible rewrite,
/// so it is deliberately the strict inverse of the write path rather than a
/// loose shape match: every component is checked against the same injective
/// predicates ingest funnels names through (`is_valid_env_name` minus the
/// reserved names, `is_valid_service_name`), and the date/hour must be a real
/// instant. A file that ingest could not have produced this path for is not
/// trawl's file, whatever it contains.
///
/// The boundary it can draw is "a path the writer could have produced", not
/// provenance: a file planted at an exactly-valid layout path is ours as far
/// as anything here can tell. Deliberately NOT tightened with the configured
/// `[ingest] envs` allowlist — an env retired from the config still has a
/// standing corpus that queries read and the invariant must therefore cover.
///
/// It doubles as the rewrite's never-NULL `_time` fallback: a standing file
/// has no WAL filename to recover an ingest instant from, but it does sit in
/// a directory that already claims an hour — the closest honest answer
/// available, and by construction inside the window the row was pruned to.
pub(crate) fn layout_path(data_dir: &Path, path: &Path) -> Option<LayoutPath> {
    let rel = path.strip_prefix(data_dir).ok()?;
    let mut parts: Vec<&str> = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?),
            // `..`, a root, a prefix: not a path the writer can produce.
            _ => return None,
        }
    }
    let (env, day, hour, file) = match parts.as_slice() {
        [env, day, file] => (*env, *day, None, *file),
        [env, day, hour, file] => (*env, *day, Some(*hour), *file),
        _ => return None,
    };
    if !trawl_config::is_valid_env_name(env) || trawl_config::RESERVED_ENV_NAMES.contains(&env) {
        return None;
    }
    let service = file.strip_suffix(".parquet")?;
    if !trawl_config::is_valid_service_name(service) {
        return None;
    }
    let day = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let instant = match hour {
        // Two digits, and an hour that exists: `and_hms_opt` rejects 24+.
        Some(hh) if hh.len() == 2 => day.and_hms_opt(hh.parse::<u32>().ok()?, 0, 0)?.and_utc(),
        Some(_) => return None,
        None => day.and_hms_opt(0, 0, 0)?.and_utc(),
    };
    Some(LayoutPath {
        service: service.to_owned(),
        instant,
    })
}

/// Describe one parquet file: magic-byte sniff first (the cheap catch for a
/// truncated or non-parquet file), then the schema, then — only if some
/// column still needs a pin vote — that column's non-null count.
///
/// One `count(<col>)` per voting column rather than a single `count(*)`: the
/// counts weight the pin vote, and a column's weight must be the rows that
/// actually carry a value for it.
fn scan_file(
    conn: &duckdb::Connection,
    path: PathBuf,
    layout: &LayoutPath,
    pinned: &HashMap<String, CanonicalType>,
) -> Result<FileScan, String> {
    if !is_valid_parquet(&path) {
        return Err("not a parquet file (magic-byte sniff failed)".to_owned());
    }
    let safe = path.to_string_lossy().replace('\'', "''");
    let schema = describe_source(conn, &format!("SELECT * FROM read_parquet('{safe}')"))?;
    // Footer-only (never a `count(*)` scan), and via the pure-Rust reader
    // for the same reason `schema_refresh` uses it: a poisoned footer must
    // be a catchable error, never a `SIGSEGV` inside `DuckDB`'s
    // `parquet_metadata()`. A file `DuckDB` just described but this reader
    // cannot is NOT a skip — the pass proves type conformance, and a missing
    // row count costs only an observation's weight.
    let rows = trawl_engine::parquet_stats::read_file_stats(&path).map_or_else(
        |e| {
            tracing::warn!(
                event_type = "catalog_conform_row_count_unreadable",
                file = %path.display(),
                error = %e,
                "parquet footer row count unreadable; the file still conforms, \
                 its field_services observation carries no row weight"
            );
            0
        },
        |stats| stats.num_rows,
    );
    let non_null = count_non_null(conn, &safe, &schema, &voting_columns(&schema, pinned))?;
    Ok(FileScan {
        path,
        service: layout.service.clone(),
        time_fallback: layout.instant,
        rows,
        schema,
        non_null,
    })
}

/// Schema indices whose non-null count is worth reading: the columns whose
/// field is not pinned yet, i.e. exactly the ones that get a vote in
/// [`most_rows_wins`].
///
/// A pinned field's count is dead weight — `most_rows_wins` skips it, and
/// paying for it means reading the column off disk for every file in the
/// corpus. When this comes back empty (a catalog that already pins every
/// field the file carries), the file is described from its footer and never
/// read at all.
///
/// The lookup ASCII-case-folds the stored spelling: the catalog holds
/// folded names only, and a standing `Dur` column is the pinned `dur`
/// field as far as any vote is concerned.
fn voting_columns(schema: &[ColInfo], pinned: &HashMap<String, CanonicalType>) -> Vec<usize> {
    schema
        .iter()
        .enumerate()
        .filter(|(_, c)| !pinned.contains_key(&c.name.to_ascii_lowercase()))
        .map(|(i, _)| i)
        .collect()
}

/// Non-null row count per column, positionally parallel to `schema`.
///
/// Only `voting` indices are counted; every other position is 0 and must
/// not be read as evidence (see [`FileScan::non_null`]).
///
/// Counted in passes of at most [`AGG_CHUNK_COLS`] columns, for the same
/// reason the pin ladder is chunked: the width is client-chosen, one
/// aggregate per column over the full width degrades quadratically, and on a
/// first boot (or a re-arm against an empty catalog) EVERY column votes —
/// per file, in front of HTTP serving, where a slow pass is indistinguishable
/// from a hang to a supervisor start timeout.
fn count_non_null(
    conn: &duckdb::Connection,
    safe_path: &str,
    schema: &[ColInfo],
    voting: &[usize],
) -> Result<Vec<u64>, String> {
    let mut out = vec![0u64; schema.len()];
    for chunk in voting.chunks(AGG_CHUNK_COLS) {
        let sql = format!(
            "SELECT {} FROM read_parquet('{safe_path}')",
            chunk
                .iter()
                .map(|i| format!("count({})::BIGINT", quote_ident(&schema[*i].name)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let counts: Vec<u64> = conn
            .query_row(&sql, [], |row| {
                let mut counts = Vec::with_capacity(chunk.len());
                for i in 0..chunk.len() {
                    counts.push(u64::try_from(row.get::<_, i64>(i)?).unwrap_or(0));
                }
                Ok(counts)
            })
            .map_err(|e| format!("non-null counts failed: {e}"))?;
        for (idx, count) in chunk.iter().zip(counts) {
            out[*idx] = count;
        }
    }
    Ok(out)
}

/// The canonical type a standing parquet column proposes at boot.
///
/// The value-level candidate ladder needs the actual values; the boot pass
/// works from schemas + row counts, so the two data-dependent lattice rows
/// resolve conservatively: an out-of-range integer column proposes DOUBLE
/// (always representable, never nulls) and an opaque JSON column (the old
/// ten-column fallback wrote these) proposes VARCHAR (honest text).
fn boot_candidate(dtype: &str) -> CanonicalType {
    match normalize_duckdb_type(dtype) {
        TypeResolution::Pin(t) => t,
        TypeResolution::Ladder => CanonicalType::Double,
        TypeResolution::Json => CanonicalType::Varchar,
    }
}

/// Rank used for most-rows ties: ladder order, VARCHAR last.
fn ladder_rank(ty: CanonicalType) -> usize {
    LADDER.iter().position(|t| *t == ty).unwrap_or(LADDER.len())
}

/// Seed proposals for unpinned fields: per field, the candidate backed by
/// the most rows across files wins; ties resolve by ladder order.
///
/// A candidate's weight is the number of rows that actually carry a value
/// for that column (`count(<col>)`), never the file's total row count — an
/// all-but-empty column in a large file describes no values and so gets no
/// say over a fully populated column in a smaller one. Since the losers get
/// `TRY_CAST` to the winner's pin, a misweighted vote is a data loss.
///
/// Votes are grouped by the ASCII-case-FOLDED name and the proposal carries
/// the folded spelling: the catalog holds folded names only (ingest folds
/// at canonicalization), while parquet written before the fold shipped can
/// carry mixed-case column names — `Dur` in one file and `dur` in another
/// are one `DuckDB` column and must be one pin, decided by most-rows-wins
/// within the folded group. The rewrite then renames such columns to the
/// folded form ([`crate::ingest::compaction::ConformPlan`]).
fn most_rows_wins(
    scan: &[FileScan],
    existing: &HashMap<String, CanonicalType>,
) -> Vec<PinProposal> {
    let mut votes: HashMap<String, HashMap<CanonicalType, u64>> = HashMap::new();
    for file in scan {
        for (col, rows) in file.schema.iter().zip(&file.non_null) {
            let folded = col.name.to_ascii_lowercase();
            if existing.contains_key(&folded) {
                continue;
            }
            *votes
                .entry(folded)
                .or_default()
                .entry(boot_candidate(&col.dtype))
                .or_default() += *rows;
        }
    }
    let mut proposals: Vec<PinProposal> = votes
        .into_iter()
        .map(|(field, candidates)| {
            let ty = candidates
                .into_iter()
                .min_by(|(ta, ra), (tb, rb)| {
                    rb.cmp(ra).then(ladder_rank(*ta).cmp(&ladder_rank(*tb)))
                })
                .map_or(CanonicalType::Varchar, |(t, _)| t);
            PinProposal {
                field,
                ty,
                pinned_from: BOOT_PIN_SOURCE.to_owned(),
            }
        })
        .collect();
    proposals.sort_by(|a, b| a.field.cmp(&b.field));
    proposals
}

/// The `field_services` observations the standing corpus attests to, one
/// row per `(field, service)` pair — the input to
/// [`CatalogStore::backfill_services`].
///
/// Only PINNED fields are observed, which is exactly the post-rewrite column
/// set: phase B drops every unpinned column from the files carrying it, and
/// `field_services`'s field axis is bounded by the pin cap precisely because
/// an unpinned field is never observed.
///
/// The timestamps come from the partition directory rather than from
/// `now()`: an observation stamped "now" for a file written two years ago
/// would place a dead field inside every `last_seen` window, which is the
/// opposite of what the windowing exists to do. The row weight is the file's
/// own row count — the same quantity compaction accumulates per batch, since
/// a file is the sum of the batches that built it.
///
/// Names are ASCII-folded and de-duplicated per file: a pre-fold corpus can
/// carry `Dur` and `dur` as separate physical columns of ONE catalog field,
/// and counting the file twice for it would inflate the weight.
fn corpus_observations(
    scan: &[FileScan],
    pins: &HashMap<String, CanonicalType>,
) -> Vec<ServiceObservation> {
    type Agg = (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
        u64,
    );
    let mut agg: HashMap<(String, String), Agg> = HashMap::new();
    for file in scan {
        let mut folded: Vec<String> = file
            .schema
            .iter()
            .map(|c| c.name.to_ascii_lowercase())
            .filter(|n| pins.contains_key(n))
            .collect();
        folded.sort_unstable();
        folded.dedup();
        for field in folded {
            let entry = agg.entry((field, file.service.clone())).or_insert((
                file.time_fallback,
                file.time_fallback,
                0,
            ));
            entry.0 = entry.0.min(file.time_fallback);
            entry.1 = entry.1.max(file.time_fallback);
            entry.2 = entry.2.saturating_add(file.rows);
        }
    }
    let mut out: Vec<ServiceObservation> = agg
        .into_iter()
        .map(
            |((field, service), (first_seen, last_seen, rows))| ServiceObservation {
                field,
                service,
                first_seen,
                last_seen,
                row_count: i64::try_from(rows).unwrap_or(i64::MAX),
            },
        )
        .collect();
    out.sort_by(|a, b| (&a.field, &a.service).cmp(&(&b.field, &b.service)));
    out
}

/// Rewrite every file whose columns disagree with the pins: `TRY_CAST` to
/// the pin, staged `.tmp` write, atomic rename. Returns the rewrite count,
/// the count of files skipped because their rewrite failed, and the conflict
/// evidence. A file whose footer described cleanly can still be unreadable
/// further in (corrupt page, bit rot) — that is one skipped file, not a
/// refusal to boot.
fn rewrite_nonconforming(
    scan: &[FileScan],
    pins: &HashMap<String, CanonicalType>,
    data_dir: &Path,
    memory_limit: &str,
) -> Result<(usize, usize, Vec<FieldConflict>), String> {
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    let mut rewritten = 0usize;
    let mut skipped = 0usize;
    let mut conflicts: Vec<FieldConflict> = Vec::new();
    let mut progress = Progress::new("rewrite", scan.len());

    for file in scan {
        match rewrite_file(&conn, file, pins) {
            Ok(None) => {}
            Ok(Some(found)) => {
                rewritten += 1;
                conflicts.extend(found);
            }
            Err(e) => {
                skipped += 1;
                // Best effort: drop the staged rewrite so a retry starts clean.
                let _ = std::fs::remove_file(file.path.with_extension("parquet.tmp"));
                skip_file(&file.path, "rewrite", &e);
            }
        }
        progress.tick();
    }

    Ok((rewritten, skipped, conflicts))
}

/// Conform one file. `Ok(None)` = it already agreed with every pin.
fn rewrite_file(
    conn: &duckdb::Connection,
    file: &FileScan,
    pins: &HashMap<String, CanonicalType>,
) -> Result<Option<Vec<FieldConflict>>, String> {
    let plan = ConformPlan::build(
        &file.schema,
        pins,
        ConformPolicy::StandingFile {
            time_fallback: file.time_fallback,
        },
    );
    if plan.is_noop() {
        return Ok(None);
    }

    let safe = file.path.to_string_lossy().replace('\'', "''");
    let source = format!("read_parquet('{safe}')");
    // Tally the nulled rows before rewriting: the rewrite destroys the
    // pre-cast values this evidence is drawn from.
    let conflicts = plan.tally_conflicts(conn, &source, &file.service)?;

    let has_time = file
        .schema
        .iter()
        .any(|c| c.name == trawl_core::schema::TIME);
    let order = if has_time { " ORDER BY \"_time\"" } else { "" };
    let tmp = file.path.with_extension("parquet.tmp");
    conn.execute_batch(&format!(
        "COPY (SELECT {} FROM {source}{order}) TO '{}' \
         (FORMAT PARQUET, COMPRESSION SNAPPY, \
          BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
        plan.select_list.join(", "),
        tmp.to_string_lossy().replace('\'', "''"),
    ))
    .map_err(|e| format!("conform rewrite failed: {e}"))?;
    // Unlike every other staged rename in the tree, this one has no backing
    // copy: the source IS the destination, and once the rename lands the
    // pre-conform file is gone. Compaction's `.tmp` is covered by the retained
    // WAL and the rollup keeps its hourlies until after the rename; here a
    // crash between the rename and writeback would leave a truncated parquet
    // where an hour (or a day) of logs used to be — and the next boot would
    // not retry, because a successful pass publishes the marker. So fsync the
    // staged file BEFORE the rename, and unlike `publish_marker`'s best-effort
    // directory sync this one is fatal: failing the file is a skip (the
    // original stays, the pass withholds completion, the next boot re-runs),
    // which is strictly better than publishing data that may not be there.
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("conform fsync failed: {e}"))?;
    std::fs::rename(&tmp, &file.path).map_err(|e| format!("conform rename failed: {e}"))?;
    // And make the rename entry itself durable, so a crash cannot resurrect
    // the directory entry for a file the data of which is already replaced.
    if let Some(parent) = file.path.parent() {
        crate::epoch::fsync_dir_best_effort(parent);
    }
    tracing::info!(
        event_type = "catalog_conform_rewrite",
        file = %file.path.display(),
        columns = plan.cast_count(),
        "rewrote nonconforming parquet file to match the catalog"
    );

    Ok(Some(conflicts))
}

#[cfg(test)]
mod tests {
    use super::{FileScan, corpus_observations, count_non_null, most_rows_wins, voting_columns};
    use crate::ingest::compaction::{AGG_CHUNK_COLS, ColInfo, describe_source};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use trawl_core::schema::CanonicalType;

    /// The vote weights are read in batched aggregate passes, so a file wider
    /// than one chunk must still attribute each count to the column it came
    /// from — a slot-mapping slip would hand a field its neighbour's weight
    /// and decide the pin on someone else's evidence.
    #[test]
    fn non_null_counts_span_chunk_boundary_without_crossing_columns() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("svc.parquet");
        let cols = AGG_CHUNK_COLS + 5;
        // Column i carries a value in exactly `(i % 3) + 1` of the three rows.
        let projection = (0..cols)
            .map(|i| format!("CASE WHEN r <= {} THEN 1 END AS f{i}", i % 3))
            .collect::<Vec<_>>()
            .join(", ");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT {projection} FROM (VALUES (0), (1), (2)) t(r)) \
             TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();

        let safe = path.to_string_lossy().replace('\'', "''");
        let schema =
            describe_source(&conn, &format!("SELECT * FROM read_parquet('{safe}')")).unwrap();
        assert_eq!(schema.len(), cols);
        let voting: Vec<usize> = (0..cols).collect();

        let counts = count_non_null(&conn, &safe, &schema, &voting).unwrap();
        for (i, count) in counts.iter().enumerate() {
            let expected = u64::try_from(i % 3).unwrap() + 1;
            assert_eq!(*count, expected, "column f{i} got the wrong non-null count");
        }
    }

    fn scan(name: &str, cols: &[(&str, &str, u64)]) -> FileScan {
        FileScan {
            path: PathBuf::from(format!("/data/prod/2026-08-01/10/{name}.parquet")),
            service: name.to_owned(),
            time_fallback: chrono::Utc::now(),
            rows: cols.iter().map(|(_, _, rows)| *rows).max().unwrap_or(0),
            schema: cols
                .iter()
                .map(|(n, t, _)| ColInfo {
                    name: (*n).to_owned(),
                    dtype: (*t).to_owned(),
                })
                .collect(),
            non_null: cols.iter().map(|(_, _, rows)| *rows).collect(),
        }
    }

    /// A near-empty column in a huge file must not outvote a fully populated
    /// one in a small file — the loser's values are `TRY_CAST` away.
    #[test]
    fn vote_weight_is_rows_carrying_the_field_not_file_rows() {
        // 1M-row file, `duration` non-null in 1000 of them; 100k-row file with
        // `duration` populated throughout.
        let corpus = vec![
            scan("svc-a", &[("duration", "BIGINT", 1_000)]),
            scan("svc-b", &[("duration", "VARCHAR", 100_000)]),
        ];
        let proposals = most_rows_wins(&corpus, &HashMap::new());
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].field, "duration");
        assert_eq!(
            proposals[0].ty,
            CanonicalType::Varchar,
            "the populated column wins the vote"
        );
    }

    /// An all-NULL column carries no evidence at all: it must not decide the
    /// pin against the only column that holds values.
    #[test]
    fn all_null_column_does_not_vote() {
        let corpus = vec![
            scan("svc-a", &[("duration", "VARCHAR", 0)]),
            scan("svc-b", &[("duration", "BIGINT", 5)]),
        ];
        let proposals = most_rows_wins(&corpus, &HashMap::new());
        assert_eq!(proposals[0].ty, CanonicalType::BigInt);
    }

    /// Already-pinned fields never re-vote.
    #[test]
    fn existing_pins_are_left_alone() {
        let corpus = vec![scan("svc-a", &[("duration", "VARCHAR", 10)])];
        let existing = HashMap::from([("duration".to_owned(), CanonicalType::BigInt)]);
        assert!(most_rows_wins(&corpus, &existing).is_empty());
    }

    /// A pinned column has no vote, so its count is never taken — counting it
    /// would read the column off disk for a number `most_rows_wins` discards.
    #[test]
    fn only_unpinned_columns_are_counted() {
        let file = scan(
            "svc-a",
            &[("_time", "TIMESTAMP", 0), ("duration", "BIGINT", 0)],
        );
        let pinned = HashMap::from([("_time".to_owned(), CanonicalType::Timestamp)]);
        assert_eq!(voting_columns(&file.schema, &pinned), vec![1]);
    }

    /// The identity-mismatch re-arm over a fully-pinned catalog: no column
    /// needs a vote, so the scan is footer-only and never reads the corpus.
    #[test]
    fn a_fully_pinned_file_needs_no_counts_at_all() {
        let file = scan(
            "svc-a",
            &[("_time", "TIMESTAMP", 0), ("duration", "BIGINT", 0)],
        );
        let pinned = HashMap::from([
            ("_time".to_owned(), CanonicalType::Timestamp),
            ("duration".to_owned(), CanonicalType::BigInt),
        ]);
        assert!(
            voting_columns(&file.schema, &pinned).is_empty(),
            "an all-pinned schema must skip the count query entirely"
        );
    }

    /// One scanned file at a chosen partition instant, carrying `rows` rows.
    fn scan_at(service: &str, hour: u32, rows: u64, cols: &[&str]) -> FileScan {
        FileScan {
            path: PathBuf::from(format!("/data/prod/2026-08-01/{hour:02}/{service}.parquet")),
            service: service.to_owned(),
            time_fallback: chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
                .unwrap()
                .and_hms_opt(hour, 0, 0)
                .unwrap()
                .and_utc(),
            rows,
            schema: cols
                .iter()
                .map(|n| ColInfo {
                    name: (*n).to_owned(),
                    dtype: "VARCHAR".to_owned(),
                })
                .collect(),
            non_null: vec![0; cols.len()],
        }
    }

    /// The backfill's shape: one row per (field, service), spanning the
    /// partition instants the corpus actually sits at and weighted by the
    /// rows the files hold. An unpinned column is never observed — phase B
    /// drops it, and `field_services`' field axis is bounded by the pin cap
    /// precisely because only pinned fields land here.
    #[test]
    fn observations_span_the_corpus_and_cover_only_pinned_fields() {
        let corpus = vec![
            scan_at("svc-a", 10, 3, &["duration", "unpinned"]),
            scan_at("svc-a", 12, 4, &["duration"]),
            scan_at("svc-b", 11, 5, &["duration"]),
        ];
        let pins = HashMap::from([("duration".to_owned(), CanonicalType::BigInt)]);
        let obs = corpus_observations(&corpus, &pins);

        assert_eq!(obs.len(), 2, "one row per (field, service): {obs:?}");
        let a = &obs[0];
        assert_eq!(
            (a.field.as_str(), a.service.as_str()),
            ("duration", "svc-a")
        );
        assert_eq!(a.row_count, 7, "both of svc-a's files weigh in");
        assert_eq!(a.first_seen.to_rfc3339(), "2026-08-01T10:00:00+00:00");
        assert_eq!(
            a.last_seen.to_rfc3339(),
            "2026-08-01T12:00:00+00:00",
            "stamped from the partition directory, never now()"
        );
        assert_eq!(obs[1].service, "svc-b");
        assert!(
            obs.iter().all(|o| o.field != "unpinned"),
            "an unpinned column is not an observation"
        );
    }

    /// A pre-fold corpus can carry `Dur` and `dur` as separate physical
    /// columns of ONE catalog field: that is one observation counted once,
    /// not the file's rows charged twice.
    #[test]
    fn case_variant_columns_are_one_observation() {
        let corpus = vec![scan_at("svc-a", 10, 6, &["Dur", "dur"])];
        let pins = HashMap::from([("dur".to_owned(), CanonicalType::Varchar)]);
        let obs = corpus_observations(&corpus, &pins);
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].field, "dur");
        assert_eq!(
            obs[0].row_count, 6,
            "the file is weighed once, not per column"
        );
    }

    /// Both of trawl's own layouts read back, service and instant intact —
    /// the service off the path (verbatim, as ingest wrote it) and the
    /// rewrite's never-NULL `_time` fallback off the partition directory.
    #[test]
    fn layout_path_reads_the_hour_and_the_daily_rollup() {
        let root = PathBuf::from("/var/lib/trawl/data");
        let hourly =
            super::layout_path(&root, &root.join("prod/2026-08-01/10/api.v2.parquet")).unwrap();
        assert_eq!(hourly.service, "api.v2");
        assert_eq!(hourly.instant.to_rfc3339(), "2026-08-01T10:00:00+00:00");

        let daily = super::layout_path(&root, &root.join("prod/2026-08-01/svc-a.parquet")).unwrap();
        assert_eq!(daily.service, "svc-a");
        assert_eq!(daily.instant.to_rfc3339(), "2026-08-01T00:00:00+00:00");
    }

    /// The gate on an in-place, lossy, irreversible rewrite: anything the
    /// write path could not have produced is foreign, and a foreign file is
    /// never scanned, never votes, and above all is never rewritten.
    #[test]
    fn layout_path_declines_everything_trawl_did_not_write() {
        let root = PathBuf::from("/var/lib/trawl/data");
        let foreign = [
            // Depth: loose at the root, or nested past the layout.
            "svc-a.parquet",
            "prod/svc-a.parquet",
            "prod/2026-08-01/10/extra/svc-a.parquet",
            // An operator's own tree that happens to sit under the data root.
            "backups/exports/10/svc-a.parquet",
            "prod/backup-2026-08-01/svc-a.parquet",
            // Reserved env names are trawl's, but not the corpus.
            "wal/2026-08-01/10/svc-a.parquet",
            "scheduled/2026-08-01/10/svc-a.parquet",
            // Names ingest's injective encoding cannot emit.
            "PROD/2026-08-01/10/svc-a.parquet",
            "prod/2026-08-01/10/Living Room AP.parquet",
            // Not an instant.
            "prod/2026-08-01/99/svc-a.parquet",
            "prod/2026-08-01/1/svc-a.parquet",
            "prod/2026-13-01/10/svc-a.parquet",
        ];
        for rel in foreign {
            assert!(
                super::layout_path(&root, &root.join(rel)).is_none(),
                "{rel} is not a path trawl wrote and must not be adopted"
            );
        }
        // Outside the data root entirely.
        assert!(
            super::layout_path(
                &root,
                &PathBuf::from("/srv/other/prod/2026-08-01/10/s.parquet")
            )
            .is_none()
        );
    }
}
