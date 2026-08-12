// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-file processing for the shadow build (ADR-0011 slice B): source
//! enumeration/signatures, the affectedness decision, and the two ways a
//! file lands in the shadow generation — a hardlink (unaffected, foreign,
//! or non-parquet) or a `ConformPlan` rewrite (affected).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use trawl_core::schema::CanonicalType;

use crate::catalog::conform::layout_path;
use crate::ingest::compaction::{
    ColInfo, ConformPlan, ConformPolicy, describe_source, is_valid_parquet, quote_ident,
    repin_count_exprs,
};

/// Identity of one source file, for the additive catch-up diff. A
/// compaction merge replaces a file via rename (new inode), so any change
/// to `(ino, len, mtime)` — or a new path — marks the file for
/// reprocessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileSig {
    ino: u64,
    len: u64,
    mtime: Option<std::time::SystemTime>,
}

/// Enumerate every FILE under the env directories of `data_dir` (all
/// files, not just parquet — `.corrupt` quarantines and other evidence
/// must ride the swap too), keyed by data-root-relative path.
///
/// Only VALID env directories are covered: the per-env swap moves exactly
/// these, so `wal/`, `scheduled/`, the markers, and any non-env top-level
/// entry stay in the live root untouched.
///
/// `*.tmp` is the ONE exclusion: a staging file belongs to the writer
/// holding it open (compaction's `{service}.parquet.tmp`, this module's
/// own), is never evidence, and hardlinking one into the shadow would
/// make a second name for an inode a live writer is about to rewrite.
/// Anything left behind is an orphan compaction's own stale-tmp sweep
/// reclaims.
pub(crate) fn snapshot_env_files(data_dir: &Path) -> Result<BTreeMap<PathBuf, FileSig>, String> {
    let mut out = BTreeMap::new();
    for (_env, env_dir) in crate::env_dirs::try_list_env_dirs(data_dir)
        .map_err(|e| format!("failed to list env dirs under {}: {e}", data_dir.display()))?
    {
        walk_files(&env_dir, data_dir, &mut out)?;
    }
    Ok(out)
}

fn walk_files(
    dir: &Path,
    data_dir: &Path,
    out: &mut BTreeMap<PathBuf, FileSig>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
        let path = entry.path();
        let meta = entry
            .metadata()
            .map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            walk_files(&path, data_dir, out)?;
        } else if meta.is_file() {
            if path.extension().is_some_and(|ext| ext == "tmp") {
                continue;
            }
            let rel = path
                .strip_prefix(data_dir)
                .map_err(|e| format!("path outside data root: {e}"))?
                .to_path_buf();
            out.insert(rel, file_sig(&meta));
        }
    }
    Ok(())
}

fn file_sig(meta: &std::fs::Metadata) -> FileSig {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let ino = 0;
    FileSig {
        ino,
        len: meta.len(),
        mtime: meta.modified().ok(),
    }
}

/// What processing one file into the shadow did.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessTally {
    /// Whether the file was rewritten (vs hardlinked verbatim).
    pub(crate) rewritten: bool,
    /// Rows the rewrite wrote.
    pub(crate) rows: u64,
    /// Stored values the new pin (with resurrection) could not keep.
    pub(crate) nulled: u64,
    /// Values recovered from `_raw` into the column.
    pub(crate) resurrected: u64,
    /// The service the file belongs to (layout files only) — conflict
    /// evidence for a forced lossy repin is attributed per service.
    pub(crate) service: Option<String>,
}

/// Process ONE source file into the shadow root: an affected layout
/// parquet is rewritten through [`ConformPolicy::Repin`]; everything else
/// — unaffected parquet, foreign parquet, non-parquet evidence — is
/// hardlinked verbatim (same filesystem by construction: the shadow is a
/// sibling of the data root).
///
/// Idempotent per file: a pre-existing shadow entry (an earlier catch-up
/// pass's output for a since-replaced source) is removed first.
pub(crate) fn process_file(
    conn: &duckdb::Connection,
    data_dir: &Path,
    shadow: &Path,
    rel: &Path,
    field: &str,
    to: CanonicalType,
    flipped_pins: &HashMap<String, CanonicalType>,
) -> Result<ProcessTally, String> {
    let src = data_dir.join(rel);
    let dst = shadow.join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    match std::fs::remove_file(&dst) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("failed to replace {}: {e}", dst.display())),
    }

    let affected = affected_schema(conn, data_dir, &src, field)?;
    let Some((schema, layout)) = affected else {
        std::fs::hard_link(&src, &dst).map_err(|e| {
            format!(
                "failed to hardlink {} -> {}: {e}",
                src.display(),
                dst.display()
            )
        })?;
        return Ok(ProcessTally::default());
    };

    rewrite_affected(conn, &src, &dst, &schema, layout, field, to, flipped_pins)
}

/// Decide affectedness: a file is rewritten only when it is (a) at a path
/// trawl itself wrote ([`layout_path`] — the same ownership gate as the
/// boot pass; a foreign file is NEVER rewritten however it parses) and
/// (b) a readable parquet whose schema carries the target column under
/// its folded name.
fn affected_schema(
    conn: &duckdb::Connection,
    data_dir: &Path,
    src: &Path,
    field: &str,
) -> Result<Option<(Vec<ColInfo>, crate::catalog::conform::LayoutPath)>, String> {
    if src.extension().is_none_or(|e| e != "parquet") {
        return Ok(None);
    }
    let Some(layout) = layout_path(data_dir, src) else {
        return Ok(None);
    };
    if !is_valid_parquet(src) {
        // A layout-path file that is not parquet inside: not ours to
        // rewrite; it rides verbatim like any other evidence.
        return Ok(None);
    }
    let safe = src.to_string_lossy().replace('\'', "''");
    let schema = describe_source(conn, &format!("SELECT * FROM read_parquet('{safe}')"))?;
    let carries = schema.iter().any(|c| c.name.eq_ignore_ascii_case(field));
    Ok(carries.then_some((schema, layout)))
}

/// Rewrite one affected file into the shadow: tally with the SAME
/// expressions the write uses, then `COPY` through the
/// [`ConformPolicy::Repin`] plan, staged `.tmp` → fsync → rename.
#[allow(clippy::too_many_arguments)]
fn rewrite_affected(
    conn: &duckdb::Connection,
    src: &Path,
    dst: &Path,
    schema: &[ColInfo],
    layout: crate::catalog::conform::LayoutPath,
    field: &str,
    to: CanonicalType,
    flipped_pins: &HashMap<String, CanonicalType>,
) -> Result<ProcessTally, String> {
    let safe = src.to_string_lossy().replace('\'', "''");
    let source = format!("read_parquet('{safe}')");
    let counts = count_repin_effect(conn, &source, schema, field, to)?;

    let plan = ConformPlan::build(
        schema,
        flipped_pins,
        &ConformPolicy::Repin {
            resurrect_field: field.to_owned(),
            time_fallback: layout.instant,
        },
    );
    let has_time = schema
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(trawl_core::schema::TIME));
    let order = if has_time { " ORDER BY \"_time\"" } else { "" };
    let tmp = staging_path(dst);
    conn.execute_batch(&format!(
        "COPY (SELECT {} FROM {source}{order}) TO '{}' \
         (FORMAT PARQUET, COMPRESSION SNAPPY, \
          BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
        plan.select_list.join(", "),
        tmp.to_string_lossy().replace('\'', "''"),
    ))
    .map_err(|e| format!("repin rewrite failed for {}: {e}", src.display()))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("repin fsync failed for {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dst)
        .map_err(|e| format!("repin rename failed for {}: {e}", dst.display()))?;

    Ok(ProcessTally {
        rewritten: true,
        rows: counts.rows,
        nulled: counts.nulled,
        resurrected: counts.resurrected,
        service: Some(layout.service),
    })
}

/// Where a rewrite stages its output before the rename onto `dst`.
///
/// Deliberately NOT `{service}.parquet.tmp` — that is byte-for-byte the
/// path hourly compaction stages at, and `DuckDB`'s `COPY` opens its target
/// `O_CREAT|O_TRUNC` without unlinking first, so a shadow entry that is a
/// hardlink to a live staging file would be truncated and rewritten IN
/// the live data root. The pid keeps two processes sharing a shadow root
/// (a would-be operator mistake) off each other's staging file, and the
/// `.tmp` extension keeps a crash leftover inert and inside compaction's
/// stale-tmp sweep once the shadow is published.
fn staging_path(dst: &Path) -> PathBuf {
    dst.with_extension(format!("parquet.repin-{}.tmp", std::process::id()))
}

/// The three per-file numbers the plan reports and the rewrite achieves,
/// computed with the SAME expressions the rewrite writes
/// (`repin_count_exprs` — see `ingest::compaction::repin_target_expr`).
pub(crate) struct RepinEffect {
    /// Total rows in the file.
    pub(crate) rows: u64,
    /// Stored values carried by the column.
    pub(crate) carrying: u64,
    /// Stored values lost under the new pin (resurrection arm included).
    pub(crate) nulled: u64,
    /// Shelved values (`NULL` stored) recovered from `_raw`.
    pub(crate) resurrected: u64,
}

/// Count what the repin would do to one affected source.
pub(crate) fn count_repin_effect(
    conn: &duckdb::Connection,
    source: &str,
    schema: &[ColInfo],
    field: &str,
    to: CanonicalType,
) -> Result<RepinEffect, String> {
    let stored = schema
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(field))
        .ok_or_else(|| format!("column {field} absent from an affected file"))?;
    let has_raw = schema
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(trawl_core::schema::RAW));
    let (carrying, kept, resurrectable) =
        repin_count_exprs(&quote_ident(&stored.name), has_raw, field, to);
    let sql = format!(
        "SELECT count(*)::BIGINT, {carrying}::BIGINT, {kept}::BIGINT, \
         {resurrectable}::BIGINT FROM {source}"
    );
    let (rows, carrying, kept, resurrected): (i64, i64, i64, i64) = conn
        .query_row(&sql, [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(|e| format!("repin count failed: {e}"))?;
    Ok(RepinEffect {
        rows: u64::try_from(rows).unwrap_or(0),
        carrying: u64::try_from(carrying).unwrap_or(0),
        nulled: u64::try_from(carrying - kept).unwrap_or(0),
        resurrected: u64::try_from(resurrected).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live staging file must never enter the shadow: hardlinking one
    /// gives the shadow a second name for an inode compaction is about to
    /// rewrite (or has already renamed into place), and the next pass's
    /// `COPY` would truncate it through the link. Everything else — parquet
    /// and non-parquet evidence alike — still rides.
    #[test]
    fn snapshot_skips_staging_files_and_keeps_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path();
        let hour = data.join("prod/2026-08-12/10");
        std::fs::create_dir_all(&hour).unwrap();
        std::fs::write(hour.join("nginx.parquet"), b"p").unwrap();
        std::fs::write(hour.join("nginx.parquet.tmp"), b"staging").unwrap();
        std::fs::write(hour.join("nginx.parquet.corrupt"), b"evidence").unwrap();

        let seen: Vec<String> = snapshot_env_files(data)
            .unwrap()
            .keys()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(seen, vec!["nginx.parquet", "nginx.parquet.corrupt"]);
    }

    /// The rewrite must not stage where compaction stages.
    #[test]
    fn staging_path_cannot_collide_with_compactions() {
        let dst = Path::new("/data/prod/2026-08-12/10/nginx.parquet");
        let staged = staging_path(dst);
        assert_ne!(staged, dst.with_extension("parquet.tmp"));
        assert_eq!(staged.parent(), dst.parent());
        assert_eq!(staged.extension().unwrap(), "tmp");
        // And it is itself excluded from the source enumeration, so a
        // crash leftover can never be hardlinked forward.
        assert!(staged.extension().is_some_and(|e| e == "tmp"));
    }
}
