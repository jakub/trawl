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
//! boot (every rewrite is staged + atomically renamed).
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
//! Per-file failures are isolated, never boot-fatal: a truncated, bit-rotted
//! or foreign `.parquet` under the data root is skipped with a warning and a
//! `trawl_catalog_conform_skipped_total` bump, exactly like the rollup path
//! sniffs and sets aside unreadable inputs rather than wedging. Nothing is
//! moved or deleted (an operator's stray file is theirs), and because the
//! corpus was then NOT proven conformant, completion is deliberately not
//! published — the next boot re-runs the pass, so a transient read failure
//! self-heals and a permanent one keeps warning.
//!
//! This machinery is deliberately the embryo of the repin rewriter (#53).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use trawl_core::schema::{CanonicalType, LADDER, TypeResolution, normalize_duckdb_type};

use super::FieldCatalog;
use crate::ingest::compaction::{
    ColInfo, ConformPlan, ConformPolicy, describe_source, is_valid_parquet, quote_ident,
};
use crate::store::{CatalogStore, FieldConflict, PinProposal};

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
}

/// One scanned parquet file.
struct FileScan {
    path: PathBuf,
    service: String,
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
struct Progress {
    phase: &'static str,
    total: usize,
    done: usize,
    started: std::time::Instant,
    last: std::time::Instant,
}

impl Progress {
    fn new(phase: &'static str, total: usize) -> Self {
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
    fn tick(&mut self) {
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
    if conformed && read_marker(data_dir).as_deref() == Some(catalog_id.as_str()) {
        // Still hydrate the cache — skipping the pass must not skip pins.
        hydrate(store, cache).await?;
        return Ok(ConformSummary {
            ran: false,
            scanned: 0,
            rewritten: 0,
            skipped: 0,
        });
    }

    tracing::info!(
        event_type = "catalog_conform_start",
        data_dir = %data_dir.display(),
        "boot conformance pass starting (first boot with this catalog, or \
         catalog/data-root identity mismatch)"
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
    let proposals = most_rows_wins(&scan, &existing);
    store
        .pin_missing(&proposals)
        .await
        .map_err(|e| format!("failed to seed pins: {e}"))?;
    let pins: HashMap<String, CanonicalType> = hydrate(store, cache).await?.into_iter().collect();

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
    if rewritten > 0 {
        metrics::counter!(crate::metrics::CATALOG_CONFORM_REWRITES_TOTAL)
            .increment(rewritten as u64);
    }

    // Publish completion LAST: postgres side, then the marker file — and
    // only when every file was accounted for. Skipped files mean the corpus
    // is not proven conformant, so the identity stays unpublished and the
    // next boot re-runs the pass rather than declaring victory forever.
    if skipped == 0 {
        store
            .mark_conformed()
            .await
            .map_err(|e| format!("failed to record conformance completion: {e}"))?;
        publish_marker(data_dir, &catalog_id)?;
    } else {
        tracing::warn!(
            event_type = "catalog_conform_incomplete",
            scanned,
            rewritten,
            skipped,
            "boot conformance pass skipped unreadable or foreign parquet files; \
             the corpus is not proven conformant and the pass will re-run on the \
             next boot — inspect the skipped files (queries touching them error)"
        );
    }

    tracing::info!(
        event_type = "catalog_conform_complete",
        scanned,
        rewritten,
        skipped,
        conflicts = conflicts.len(),
        "boot conformance pass complete"
    );

    Ok(ConformSummary {
        ran: true,
        scanned,
        rewritten,
        skipped,
    })
}

/// Isolate one bad file: warn, count, leave it exactly where it is.
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
        "unreadable or foreign parquet file skipped by the boot conformance \
         pass; it is left untouched and remains outside the catalog invariant"
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
/// overlays), capped memory, two threads.
fn open_bounded_connection(
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
    Ok(conn)
}

/// Enumerate every parquet file under the data root (hourly + daily
/// rollups, skipping `scheduled/`) and describe each. Returns the readable
/// files plus the count of files skipped as unreadable or foreign — one bad
/// file must never take the daemon's boot down with it.
fn scan_corpus(
    data_dir: &Path,
    memory_limit: &str,
    pinned: &HashMap<String, CanonicalType>,
) -> Result<(Vec<FileScan>, usize), String> {
    let files = crate::metrics::walk_parquet_files(data_dir)
        .map_err(|e| format!("failed to walk data root: {e}"))?;
    if files.is_empty() {
        return Ok((Vec::new(), 0));
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
    let mut skipped = 0usize;
    for (path, _) in files {
        match scan_file(&conn, path.clone(), pinned) {
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
    pinned: &HashMap<String, CanonicalType>,
) -> Result<FileScan, String> {
    if !is_valid_parquet(&path) {
        return Err("not a parquet file (magic-byte sniff failed)".to_owned());
    }
    let safe = path.to_string_lossy().replace('\'', "''");
    let schema = describe_source(conn, &format!("SELECT * FROM read_parquet('{safe}')"))?;
    let non_null = count_non_null(conn, &safe, &schema, &voting_columns(&schema, pinned))?;
    let service = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_owned();
    Ok(FileScan {
        path,
        service,
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
fn voting_columns(schema: &[ColInfo], pinned: &HashMap<String, CanonicalType>) -> Vec<usize> {
    schema
        .iter()
        .enumerate()
        .filter(|(_, c)| !pinned.contains_key(&c.name))
        .map(|(i, _)| i)
        .collect()
}

/// Non-null row count per column, positionally parallel to `schema`.
///
/// Only `voting` indices are counted; every other position is 0 and must
/// not be read as evidence (see [`FileScan::non_null`]).
fn count_non_null(
    conn: &duckdb::Connection,
    safe_path: &str,
    schema: &[ColInfo],
    voting: &[usize],
) -> Result<Vec<u64>, String> {
    let mut out = vec![0u64; schema.len()];
    if voting.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT {} FROM read_parquet('{safe_path}')",
        voting
            .iter()
            .map(|i| format!("count({})::BIGINT", quote_ident(&schema[*i].name)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let counts: Vec<u64> = conn
        .query_row(&sql, [], |row| {
            let mut counts = Vec::with_capacity(voting.len());
            for i in 0..voting.len() {
                counts.push(u64::try_from(row.get::<_, i64>(i)?).unwrap_or(0));
            }
            Ok(counts)
        })
        .map_err(|e| format!("non-null counts failed: {e}"))?;
    for (idx, count) in voting.iter().zip(counts) {
        out[*idx] = count;
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
fn most_rows_wins(
    scan: &[FileScan],
    existing: &HashMap<String, CanonicalType>,
) -> Vec<PinProposal> {
    let mut votes: HashMap<&str, HashMap<CanonicalType, u64>> = HashMap::new();
    for file in scan {
        for (col, rows) in file.schema.iter().zip(&file.non_null) {
            if existing.contains_key(&col.name) {
                continue;
            }
            *votes
                .entry(col.name.as_str())
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
                field: field.to_owned(),
                ty,
                pinned_from: BOOT_PIN_SOURCE.to_owned(),
            }
        })
        .collect();
    proposals.sort_by(|a, b| a.field.cmp(&b.field));
    proposals
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
    let plan = ConformPlan::build(&file.schema, pins, ConformPolicy::StandingFile);
    if plan.cast_count() == 0 {
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
    std::fs::rename(&tmp, &file.path).map_err(|e| format!("conform rename failed: {e}"))?;
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
    use super::{FileScan, most_rows_wins, voting_columns};
    use crate::ingest::compaction::ColInfo;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use trawl_core::schema::CanonicalType;

    fn scan(name: &str, cols: &[(&str, &str, u64)]) -> FileScan {
        FileScan {
            path: PathBuf::from(format!("/data/prod/2026-08-01/10/{name}.parquet")),
            service: name.to_owned(),
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
}
