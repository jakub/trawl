// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-file processing for the shadow build (ADR-0011).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use trawl_core::schema::CanonicalType;

use crate::env_dirs::{LayoutPath, layout_path};
use crate::ingest::compaction::{
    ColInfo, ConformPlan, ConformPolicy, RepinReading, StatementFailure,
    bounded_misfit_samples_sql, decode_misfit_samples, describe_source, is_valid_parquet,
    quote_ident, repin_count_exprs, statement_failed,
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

/// Enumerate every file under the env directories of `data_dir` (all
/// files, not just parquet — `.corrupt` quarantines and other evidence
/// must ride the swap too), keyed by data-root-relative path.
///
/// `*.tmp` is the one exclusion: hardlinking a staging file into the shadow
/// would make a second name for an inode a live writer is about to rewrite.
///
/// A symlink anywhere under an env directory aborts the walk: the shadow
/// cannot carry it, and silently skipping it would leave the only surviving
/// copy in `data.repin-aside/` for the job's own sweep to delete.
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
        // `DirEntry::metadata` is an `lstat`: it does not follow the link,
        // so a symlinked entry answers neither `is_dir` nor `is_file` and
        // would fall through both arms unrecorded.
        let meta = entry
            .metadata()
            .map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
        if meta.is_symlink() {
            return Err(format!(
                "{} is a symlink: the repin shadow cannot carry symlinked \
                 entries under the data root — replace it with the real file \
                 or directory (or move it out of the env directory) and retry",
                path.display()
            ));
        }
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

#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessTally {
    pub(crate) rewritten: bool,
    pub(crate) rows: u64,
    /// Stored values the new pin (with resurrection) could not keep.
    pub(crate) nulled: u64,
    /// Values recovered from `_raw` into the column.
    pub(crate) resurrected: u64,
    /// Rows the new pin reads differently in each dialect — the values only
    /// the operator's assertion can settle.
    pub(crate) ambiguous: u64,
    /// The service the file belongs to (layout files only) — conflict
    /// evidence for a forced lossy repin is attributed per service.
    pub(crate) service: Option<String>,
}

/// `precounted` is the scan's own tally for this byte-identical file (see
/// [`crate::repin::plan::ScanTallies`]); supplying it skips the aggregate
/// recount, and supplying a stale one would misreport the rewrite, so the
/// caller owns the signature check.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_file(
    conn: &duckdb::Connection,
    data_dir: &Path,
    shadow: &Path,
    rel: &Path,
    field: &str,
    reading: RepinReading,
    flipped_pins: &HashMap<String, CanonicalType>,
    precounted: Option<RepinEffect>,
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

    rewrite_affected(
        conn,
        &src,
        &dst,
        &schema,
        layout,
        field,
        reading,
        flipped_pins,
        precounted,
    )
}

/// The scan (`repin::plan`) asks this same function, so the dry run's file
/// set is the rewrite's file set by construction rather than by two
/// predicates agreeing.
pub(crate) fn affected_schema(
    conn: &duckdb::Connection,
    data_dir: &Path,
    src: &Path,
    field: &str,
) -> Result<Option<(Vec<ColInfo>, LayoutPath)>, String> {
    if src.extension().is_none_or(|e| e != "parquet") {
        return Ok(None);
    }
    let Some(layout) = layout_path(data_dir, src) else {
        return Ok(None);
    };
    if !is_valid_parquet(src) {
        return Ok(None);
    }
    let safe = src.to_string_lossy().replace('\'', "''");
    let schema = describe_source(conn, &format!("SELECT * FROM read_parquet('{safe}')"))?;
    let carries = schema.iter().any(|c| c.name.eq_ignore_ascii_case(field));
    Ok(carries.then_some((schema, layout)))
}

#[allow(clippy::too_many_arguments)]
fn rewrite_affected(
    conn: &duckdb::Connection,
    src: &Path,
    dst: &Path,
    schema: &[ColInfo],
    layout: LayoutPath,
    field: &str,
    reading: RepinReading,
    flipped_pins: &HashMap<String, CanonicalType>,
    precounted: Option<RepinEffect>,
) -> Result<ProcessTally, String> {
    let safe = src.to_string_lossy().replace('\'', "''");
    let source = format!("read_parquet('{safe}')");
    let counts = match precounted {
        Some(counts) => counts,
        None => count_repin_effect(conn, &source, schema, field, reading)?,
    };

    let plan = ConformPlan::build(
        schema,
        flipped_pins,
        &ConformPolicy::Repin {
            resurrect_field: field.to_owned(),
            time_fallback: layout.instant,
            target: reading.written,
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
    .map_err(|e| statement_failed(&format!("repin rewrite failed for {}", src.display()), &e))?;
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
        ambiguous: counts.ambiguous,
        service: Some(layout.service),
    })
}

/// Deliberately not `{service}.parquet.tmp` — that is byte-for-byte the
/// path hourly compaction stages at, and `DuckDB`'s `COPY` opens its target
/// `O_CREAT|O_TRUNC` without unlinking first, so a shadow entry that is a
/// hardlink to a live staging file would be truncated and rewritten inside
/// the live data root.
fn staging_path(dst: &Path) -> PathBuf {
    dst.with_extension(format!("parquet.repin-{}.tmp", std::process::id()))
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RepinEffect {
    pub(crate) rows: u64,
    pub(crate) carrying: u64,
    /// Stored values lost under the new pin (resurrection arm included).
    pub(crate) nulled: u64,
    /// Shelved values (`NULL` stored) recovered from `_raw`.
    pub(crate) resurrected: u64,
    /// Rows whose reading differs between the two dialects. Counted on
    /// every scan whatever the job asserted — the count is evidence; what
    /// the assertion governs is the force gate.
    pub(crate) ambiguous: u64,
}

pub(crate) fn count_repin_effect(
    conn: &duckdb::Connection,
    source: &str,
    schema: &[ColInfo],
    field: &str,
    reading: RepinReading,
) -> Result<RepinEffect, String> {
    let (effect, _) = measure(conn, source, schema, field, reading, false)?;
    Ok(effect)
}

/// One statement because the numbers and the evidence must describe the
/// same read of the same file: compaction can replace a file between two
/// statements, and a report whose counts came from one generation and whose
/// samples came from another describes a corpus that never existed.
pub(crate) fn count_repin_effect_sampled(
    conn: &duckdb::Connection,
    source: &str,
    schema: &[ColInfo],
    field: &str,
    reading: RepinReading,
) -> Result<(RepinEffect, Vec<String>), String> {
    match measure(conn, source, schema, field, reading, true) {
        Ok(measured) => Ok(measured),
        Err(e) => {
            // DuckDB's or serde's wording can quote the file's values, so
            // the text goes to DEBUG only.
            tracing::debug!(
                event_type = "repin_sample_failed",
                error = %e,
                "could not sample the values this repin cannot read; \
                 continuing with counts only"
            );
            tracing::warn!(
                event_type = "repin_sample_failed",
                error_class = e.error_class(),
                cause_kind = e.cause_kind().as_str(),
                "could not sample the values this repin cannot read; \
                 continuing with counts only"
            );
            Ok((
                count_repin_effect(conn, source, schema, field, reading)?,
                Vec::new(),
            ))
        }
    }
}

fn measure(
    conn: &duckdb::Connection,
    source: &str,
    schema: &[ColInfo],
    field: &str,
    reading: RepinReading,
    sampled: bool,
) -> Result<(RepinEffect, Vec<String>), StatementFailure> {
    let stored = schema
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(field))
        .ok_or_else(|| format!("column {field} absent from an affected file"))?;
    let quoted = quote_ident(&stored.name);
    let has_raw = schema
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(trawl_core::schema::RAW));
    let exprs = repin_count_exprs(&quoted, has_raw, field, reading);
    let samples_column = if sampled {
        let target =
            crate::ingest::compaction::repin_target_expr(&quoted, has_raw, field, reading.written);
        let text = trawl_core::conform::untyped_text(&quoted);
        format!(", {}", bounded_misfit_samples_sql(&quoted, &text, &target))
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT count(*)::BIGINT, {}::BIGINT, {}::BIGINT, {}::BIGINT, {}::BIGINT{samples_column} \
         FROM {source}",
        exprs.carrying, exprs.kept, exprs.resurrectable, exprs.ambiguous
    );
    let (rows, carrying, kept, resurrected, ambiguous, misfits): (
        i64,
        i64,
        i64,
        i64,
        i64,
        Option<String>,
    ) = conn
        .query_row(&sql, [], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                if sampled { row.get(5)? } else { None },
            ))
        })
        .map_err(|e| StatementFailure::duckdb("repin count failed", &e))?;
    Ok((
        RepinEffect {
            rows: u64::try_from(rows).unwrap_or(0),
            carrying: u64::try_from(carrying).unwrap_or(0),
            nulled: u64::try_from(carrying - kept).unwrap_or(0),
            resurrected: u64::try_from(resurrected).unwrap_or(0),
            ambiguous: u64::try_from(ambiguous).unwrap_or(0),
        },
        decode_misfit_samples(misfits.as_deref())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_out_of_memory_is_a_resource_failure_in_repin() {
        use crate::ingest::compaction::oom_support;
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let shadow = tmp.path().join("data.repin-next");
        let rel = Path::new("prod/2026-01-14/00/svc.parquet");
        let src = data_dir.join(rel);
        oom_support::big_parquet(&src);
        let before = std::fs::read(&src).unwrap();
        let conn =
            crate::catalog::conform::open_bounded_connection(&data_dir, oom_support::STARVED)
                .unwrap();
        let reading = RepinReading::new(
            CanonicalType::Varchar,
            CanonicalType::BigInt,
            trawl_core::severity::Dialect::Otel,
        );
        let flipped = HashMap::from([("message".to_owned(), CanonicalType::BigInt)]);
        let counted = RepinEffect {
            rows: 131_072,
            carrying: 131_072,
            nulled: 0,
            resurrected: 0,
            ambiguous: 0,
        };

        let (result, logs, exhausted) = oom_support::capture(|| {
            process_file(
                &conn,
                &data_dir,
                &shadow,
                rel,
                "message",
                reading,
                &flipped,
                Some(counted),
            )
        });

        let err = result.unwrap_err();
        assert!(err.contains("Out of Memory Error: "), "{err}");
        assert_eq!(exhausted, 1, "{logs}");
        assert!(logs.contains("maintenance_resource_exhausted"), "{logs}");
        assert_eq!(std::fs::read(&src).unwrap(), before, "the source is kept");
        assert!(!shadow.join(rel).exists(), "nothing reaches the shadow");
    }

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

    #[cfg(unix)]
    #[test]
    fn snapshot_refuses_symlinked_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path();
        let hour = data.join("prod/2026-08-12/10");
        std::fs::create_dir_all(&hour).unwrap();
        let elsewhere = tmp.path().join("elsewhere.parquet");
        std::fs::write(&elsewhere, b"p").unwrap();
        std::os::unix::fs::symlink(&elsewhere, hour.join("nginx.parquet")).unwrap();

        let err = snapshot_env_files(data).unwrap_err();
        assert!(err.contains("nginx.parquet"), "{err}");
        assert!(err.contains("symlink"), "{err}");

        // A symlinked directory is refused on the same terms: recursing
        // through it would hardlink its contents onto the data root's own
        // filesystem and the cutover would then unlink the link.
        std::fs::remove_file(hour.join("nginx.parquet")).unwrap();
        let other_hour = tmp.path().join("other-hour");
        std::fs::create_dir_all(&other_hour).unwrap();
        std::os::unix::fs::symlink(&other_hour, hour.join("11")).unwrap();
        let err = snapshot_env_files(data).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn staging_path_cannot_collide_with_compactions() {
        let dst = Path::new("/data/prod/2026-08-12/10/nginx.parquet");
        let staged = staging_path(dst);
        assert_ne!(staged, dst.with_extension("parquet.tmp"));
        assert_eq!(staged.parent(), dst.parent());
        assert_eq!(staged.extension().unwrap(), "tmp");
        assert!(staged.extension().is_some_and(|e| e == "tmp"));
    }
}
