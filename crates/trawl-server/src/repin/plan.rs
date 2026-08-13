// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin scan (ADR-0011 slice B): the dry-run report, and the
//! mandatory first phase of every executing job.
//!
//! Counts over the SAME files the rewrite rewrites
//! (`rewrite::affected_schema` — one affectedness gate, not two agreeing
//! predicates) with the SAME expressions the rewrite writes
//! (`ingest::compaction::repin_count_exprs` over `repin_target_expr`), so
//! the plan's `projected_nulls`/`resurrectable` are the rewrite's
//! `rows_nulled`/`rows_resurrected` over an unchanged corpus — a report
//! that can only drift by the events that arrive between scan and
//! rewrite, which the catch-up loop then counts for real.
//!
//! Because those numbers are equal by construction rather than by
//! approximation, the scan hands its PER-FILE readings ([`ScanTallies`])
//! to the build instead of letting the first pass re-measure a corpus
//! nothing has touched: an unchanged `(ino, len, mtime)` reuses the
//! reading, anything else recounts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use trawl_core::schema::CanonicalType;

use crate::catalog::conform::{Progress, open_bounded_connection};
use crate::repin::rewrite::{FileSig, RepinEffect, affected_schema, count_repin_effect};

/// The scan's PER-FILE readings, keyed by data-root-relative path and
/// stamped with the signature they were measured at.
///
/// The build begins synchronously after the scan returns, so in the common
/// case every affected file is still byte-identical and its tally — the
/// same aggregate SQL the rewrite would run again — is reusable as-is.
/// Only the signature licenses that: a file compaction replaced between
/// scan and build gets a new `(ino, len, mtime)` and is recounted, exactly
/// as a catch-up pass recounts it.
pub(crate) type ScanTallies = BTreeMap<PathBuf, (FileSig, RepinEffect)>;

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
/// pool. Which files count is [`affected_schema`]'s decision, the very
/// one the rewrite makes: non-parquet, foreign and unreadable paths are
/// not affected files there either (they ride the swap verbatim), so the
/// plan is honest rather than optimistic, and a file `read_parquet`
/// cannot open at all fails the scan exactly as it would fail the
/// rewrite — before anything has been staged.
pub(crate) fn scan(
    data_dir: &Path,
    memory_limit: &str,
    field: &str,
    to: CanonicalType,
) -> Result<(ScanCounts, ScanTallies), String> {
    let sources = crate::repin::rewrite::snapshot_env_files(data_dir)?;
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    let mut counts = ScanCounts::default();
    let mut tallies = ScanTallies::new();
    let mut progress = Progress::new("repin-scan", sources.len());
    for (rel, sig) in sources {
        progress.tick();
        #[cfg(any(test, feature = "test-support"))]
        {
            let delay =
                crate::repin::engine::TEST_SCAN_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
            if delay > 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
        }
        let path = data_dir.join(&rel);
        let Some((schema, _layout)) = affected_schema(&conn, data_dir, &path, field)? else {
            continue;
        };
        let safe = path.to_string_lossy().replace('\'', "''");
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
        tallies.insert(rel, (sig, effect));
    }
    Ok((counts, tallies))
}
