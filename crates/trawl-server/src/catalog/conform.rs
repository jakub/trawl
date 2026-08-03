// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boot conformance pass (ADR-0009 slice 2, §2b): make the write-time
//! invariant true over the STANDING corpus, not just files written after
//! the catalog shipped.
//!
//! Runs inline at boot — after the storage epoch gate, before the ingest
//! pipeline and HTTP serving — and only on ingest-enabled nodes (a
//! query-only node does not own the data root). Bounded by a post-cutover
//! corpus days old (#52 moved the legacy root aside), and usually zero
//! rewrites.
//!
//! Identity is dual-sided: `catalog_state.catalog_id` in postgres is
//! mirrored into a `data/CATALOG` marker file. The pass is skipped only
//! when BOTH sides agree — a repointed `DATABASE_URL` or a data root
//! restored from backup shows up as a mismatch and forces a re-run. A
//! crash mid-pass leaves unrewritten files to be redetected on the next
//! boot (every rewrite is staged + atomically renamed).
//!
//! This machinery is deliberately the embryo of the repin rewriter (#53).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use trawl_core::schema::{CanonicalType, LADDER, TypeResolution, normalize_duckdb_type};

use super::FieldCatalog;
use crate::ingest::compaction::{ColInfo, conform_expr, describe_source, quote_ident};
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
}

/// One scanned parquet file.
struct FileScan {
    path: PathBuf,
    service: String,
    rows: u64,
    schema: Vec<ColInfo>,
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
        let pins = store
            .load_pins()
            .await
            .map_err(|e| format!("failed to load pins: {e}"))?;
        cache.replace(pins);
        return Ok(ConformSummary {
            ran: false,
            scanned: 0,
            rewritten: 0,
        });
    }

    tracing::info!(
        event_type = "catalog_conform_start",
        data_dir = %data_dir.display(),
        "boot conformance pass starting (first boot with this catalog, or \
         catalog/data-root identity mismatch)"
    );

    // Phase A (blocking): enumerate + describe the corpus.
    let scan = {
        let data_dir = data_dir.to_path_buf();
        let memory_limit = memory_limit.to_owned();
        tokio::task::spawn_blocking(move || scan_corpus(&data_dir, &memory_limit))
            .await
            .map_err(|e| format!("conformance scan task panicked: {e}"))??
    };

    // Seed pins: declared fields came with the migration; custom fields by
    // most-rows-wins across files, ties by ladder order.
    let existing: HashMap<String, CanonicalType> = store
        .load_pins()
        .await
        .map_err(|e| format!("failed to load pins: {e}"))?
        .into_iter()
        .collect();
    let proposals = most_rows_wins(&scan, &existing);
    store
        .pin_missing(&proposals)
        .await
        .map_err(|e| format!("failed to seed pins: {e}"))?;
    let pins: Vec<(String, CanonicalType)> = store
        .load_pins()
        .await
        .map_err(|e| format!("failed to load pins: {e}"))?;
    cache.replace(pins.clone());
    let pins: HashMap<String, CanonicalType> = pins.into_iter().collect();

    // Phase B (blocking): rewrite the nonconforming files.
    let scanned = scan.len();
    let (rewritten, conflicts) = {
        let data_dir = data_dir.to_path_buf();
        let memory_limit = memory_limit.to_owned();
        tokio::task::spawn_blocking(move || {
            rewrite_nonconforming(&scan, &pins, &data_dir, &memory_limit)
        })
        .await
        .map_err(|e| format!("conformance rewrite task panicked: {e}"))??
    };

    record_boot_conflicts(store, &conflicts).await;
    if rewritten > 0 {
        metrics::counter!(crate::metrics::CATALOG_CONFORM_REWRITES_TOTAL)
            .increment(rewritten as u64);
    }

    // Publish completion LAST: postgres side, then the marker file.
    store
        .mark_conformed()
        .await
        .map_err(|e| format!("failed to record conformance completion: {e}"))?;
    publish_marker(data_dir, &catalog_id)?;

    tracing::info!(
        event_type = "catalog_conform_complete",
        scanned,
        rewritten,
        conflicts = conflicts.len(),
        "boot conformance pass complete"
    );

    Ok(ConformSummary {
        ran: true,
        scanned,
        rewritten,
    })
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
/// rollups, skipping `scheduled/`) and describe each.
fn scan_corpus(data_dir: &Path, memory_limit: &str) -> Result<Vec<FileScan>, String> {
    let files = crate::metrics::walk_parquet_files(data_dir)
        .map_err(|e| format!("failed to walk data root: {e}"))?;
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    let mut out = Vec::with_capacity(files.len());
    for (path, _) in files {
        let safe = path.to_string_lossy().replace('\'', "''");
        let schema = describe_source(&conn, &format!("SELECT * FROM read_parquet('{safe}')"))?;
        let rows: i64 = conn
            .query_row(
                &format!("SELECT count(*)::BIGINT FROM read_parquet('{safe}')"),
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("row count failed for {}: {e}", path.display()))?;
        let service = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned();
        out.push(FileScan {
            path,
            service,
            rows: u64::try_from(rows).unwrap_or(0),
            schema,
        });
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
fn most_rows_wins(
    scan: &[FileScan],
    existing: &HashMap<String, CanonicalType>,
) -> Vec<PinProposal> {
    let mut votes: HashMap<&str, HashMap<CanonicalType, u64>> = HashMap::new();
    for file in scan {
        for col in &file.schema {
            if existing.contains_key(&col.name) {
                continue;
            }
            *votes
                .entry(col.name.as_str())
                .or_default()
                .entry(boot_candidate(&col.dtype))
                .or_default() += file.rows;
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
/// the pin, staged `.tmp` write, atomic rename. Returns the rewrite count
/// and the conflict evidence.
fn rewrite_nonconforming(
    scan: &[FileScan],
    pins: &HashMap<String, CanonicalType>,
    data_dir: &Path,
    memory_limit: &str,
) -> Result<(usize, Vec<FieldConflict>), String> {
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    let mut rewritten = 0usize;
    let mut conflicts: Vec<FieldConflict> = Vec::new();

    for file in scan {
        struct CastEntry {
            name: String,
            dtype: String,
            pin: CanonicalType,
            expr: String,
        }
        let mut select_list: Vec<String> = Vec::with_capacity(file.schema.len());
        let mut casts: Vec<CastEntry> = Vec::new();
        let mut has_time = false;
        for col in &file.schema {
            if col.name == trawl_core::schema::TIME {
                has_time = true;
            }
            let quoted = quote_ident(&col.name);
            match pins.get(&col.name).copied() {
                None => select_list.push(quoted), // unpinnable? pins cover all scanned fields
                Some(pin) => match conform_expr(&quoted, &col.dtype, pin) {
                    None => select_list.push(quoted),
                    Some(expr) => {
                        select_list.push(format!("{expr} AS {quoted}"));
                        casts.push(CastEntry {
                            name: col.name.clone(),
                            dtype: col.dtype.clone(),
                            pin,
                            expr,
                        });
                    }
                },
            }
        }
        if casts.is_empty() {
            continue;
        }

        let safe = file.path.to_string_lossy().replace('\'', "''");
        // Tally the nulled rows before rewriting.
        let stats_sql = format!(
            "SELECT {} FROM read_parquet('{safe}')",
            casts
                .iter()
                .map(|c| {
                    let q = quote_ident(&c.name);
                    format!("count({q})::BIGINT, count({})::BIGINT", c.expr)
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let stats: Vec<(i64, i64)> = conn
            .query_row(&stats_sql, [], |row| {
                let mut out = Vec::with_capacity(casts.len());
                for i in 0..casts.len() {
                    out.push((row.get::<_, i64>(2 * i)?, row.get::<_, i64>(2 * i + 1)?));
                }
                Ok(out)
            })
            .map_err(|e| format!("conform stats failed for {}: {e}", file.path.display()))?;

        let order = if has_time { " ORDER BY \"_time\"" } else { "" };
        let tmp = file.path.with_extension("parquet.tmp");
        conn.execute_batch(&format!(
            "COPY (SELECT {} FROM read_parquet('{safe}'){order}) TO '{}' \
             (FORMAT PARQUET, COMPRESSION SNAPPY, \
              BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
            select_list.join(", "),
            tmp.to_string_lossy().replace('\'', "''"),
        ))
        .map_err(|e| format!("conform rewrite failed for {}: {e}", file.path.display()))?;
        std::fs::rename(&tmp, &file.path)
            .map_err(|e| format!("conform rename failed for {}: {e}", file.path.display()))?;
        rewritten += 1;
        tracing::info!(
            event_type = "catalog_conform_rewrite",
            file = %file.path.display(),
            columns = casts.len(),
            "rewrote nonconforming parquet file to match the catalog"
        );

        for (cast, (non_null, ok)) in casts.iter().zip(&stats) {
            let rows_nulled = u64::try_from(non_null - ok).unwrap_or(0);
            if *non_null > 0 && (cast.dtype != "JSON" || rows_nulled > 0) {
                conflicts.push(FieldConflict {
                    field: cast.name.clone(),
                    service: file.service.clone(),
                    observed_type: cast.dtype.clone(),
                    expected_type: cast.pin,
                    rows_nulled,
                });
            }
        }
    }

    Ok((rewritten, conflicts))
}
