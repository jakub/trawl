// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin scan (ADR-0011 slice B): the dry-run report, and the
//! mandatory first phase of every executing job.
//!
//! Counts with the SAME expressions the rewrite writes
//! (`ingest::compaction::repin_count_exprs` over `repin_target_expr`), so
//! the plan's `projected_nulls`/`resurrectable` are the rewrite's
//! `rows_nulled`/`rows_resurrected` over an unchanged corpus — a report
//! that can only drift by the events that arrive between scan and
//! rewrite, which the catch-up loop then counts for real.

use std::path::Path;

use trawl_core::schema::CanonicalType;

use crate::catalog::conform::{Progress, layout_path, open_bounded_connection};
use crate::ingest::compaction::describe_source;
use crate::repin::rewrite::count_repin_effect;

/// What the scan found — the dry-run report's numbers.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanCounts {
    /// Affected files (carry the target column at an owned layout path).
    pub files_total: u64,
    /// Rows carrying a stored value for the field.
    pub rows_carrying: u64,
    /// Stored values the new pin cannot keep (nulled unless forced away).
    pub projected_nulls: u64,
    /// Shelved values `_raw` gives back under the new pin.
    pub resurrectable: u64,
    /// Bytes across the affected files — the double-hold peak the
    /// free-space pre-flight budgets for.
    pub affected_bytes: u64,
}

/// Scan the corpus for `field` repinned to `to`.
///
/// Blocking (`DuckDB` + filesystem) — callers run it on the blocking
/// pool. Unreadable or foreign paths are simply not affected files: the
/// rewrite never touches them either (they ride the swap verbatim), so
/// skipping them here keeps the plan honest rather than optimistic.
pub(crate) fn scan(
    data_dir: &Path,
    memory_limit: &str,
    field: &str,
    to: CanonicalType,
) -> Result<ScanCounts, String> {
    let sources = crate::repin::rewrite::snapshot_env_files(data_dir)?;
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    let mut counts = ScanCounts::default();
    let mut progress = Progress::new("repin-scan", sources.len());
    for (rel, _sig) in sources {
        progress.tick();
        if rel.extension().is_none_or(|e| e != "parquet") {
            continue;
        }
        let path = data_dir.join(&rel);
        if layout_path(data_dir, &path).is_none() {
            continue;
        }
        let safe = path.to_string_lossy().replace('\'', "''");
        let Ok(schema) = describe_source(&conn, &format!("SELECT * FROM read_parquet('{safe}')"))
        else {
            // Unreadable: not an affected file (and not rewritable).
            continue;
        };
        if !schema.iter().any(|c| c.name.eq_ignore_ascii_case(field)) {
            continue;
        }
        let effect = count_repin_effect(
            &conn,
            &format!("read_parquet('{safe}')"),
            &schema,
            field,
            to,
        )?;
        counts.files_total += 1;
        counts.rows_carrying += effect.carrying;
        counts.projected_nulls += effect.nulled;
        counts.resurrectable += effect.resurrected;
        counts.affected_bytes += std::fs::metadata(&path).map_or(0, |m| m.len());
    }
    Ok(counts)
}
