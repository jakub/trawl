// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin scan (ADR-0011): the dry-run report, and the mandatory first
//! phase of every executing job.
//!
//! Counts over the same files the rewrite rewrites
//! (`rewrite::affected_schema` — one affectedness gate, not two agreeing
//! predicates) with the same expressions the rewrite writes
//! (`ingest::compaction::repin_count_exprs` over `repin_target_expr`), so
//! the plan's `projected_nulls`/`resurrectable` are the rewrite's
//! `rows_nulled`/`rows_resurrected` over an unchanged corpus — a report
//! that can only drift by the events that arrive between scan and
//! rewrite, which the catch-up loop then counts for real.
//!
//! Because those numbers are equal by construction rather than by
//! approximation, the scan hands its per-file readings ([`ScanTallies`])
//! to the build instead of letting the first pass re-measure a corpus
//! nothing has touched: an unchanged `(ino, len, mtime)` reuses the
//! reading, anything else recounts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalog::conform::{Progress, open_bounded_connection};
use crate::ingest::compaction::RepinReading;
use crate::repin::rewrite::{
    FileSig, RepinEffect, affected_schema, count_repin_effect, count_repin_effect_sampled,
};
use crate::store::MAX_CONFLICT_SAMPLES;

/// The scan's per-file readings, keyed by data-root-relative path and
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
    /// Rows whose numeral reads as a different severity in each dialect —
    /// counted whatever the job asserted.
    pub ambiguous_numerals: u64,
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
///
/// The third return is up to [`MAX_CONFLICT_SAMPLES`] samples of the values
/// the new pin cannot read — the evidence that turns "42 rows would be
/// nulled" into a decision an operator can make. They ride the per-file
/// counting statement rather than a second query, so the numbers and the
/// evidence describe one read of one file, and the sampling stops being
/// asked for once `MAX_CONFLICT_SAMPLES` distinct samples are held: a
/// corpus-wide misfit pays for the sketch on the first files and nothing
/// after.
pub(crate) fn scan(
    data_dir: &Path,
    memory_limit: &str,
    field: &str,
    reading: RepinReading,
) -> Result<(ScanCounts, ScanTallies, Vec<String>), String> {
    let sources = crate::repin::rewrite::snapshot_env_files(data_dir)?;
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    let mut counts = ScanCounts::default();
    let mut tallies = ScanTallies::new();
    let mut samples: Vec<String> = Vec::new();
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
        let source = format!("read_parquet('{safe}')");
        // Sampling rides the counts until the sample budget is full;
        // after that the plain statement is the cheaper one.
        let effect = if samples.len() < MAX_CONFLICT_SAMPLES {
            let (effect, found) =
                count_repin_effect_sampled(&conn, &source, &schema, field, reading)?;
            for value in found {
                if samples.len() == MAX_CONFLICT_SAMPLES {
                    break;
                }
                if !samples.contains(&value) {
                    samples.push(value);
                }
            }
            effect
        } else {
            count_repin_effect(&conn, &source, &schema, field, reading)?
        };
        counts.files_total += 1;
        counts.rows_carrying += effect.carrying;
        counts.projected_nulls += effect.nulled;
        counts.resurrectable += effect.resurrected;
        counts.ambiguous_numerals += effect.ambiguous;
        counts.affected_bytes += std::fs::metadata(&path).map_or(0, |m| m.len());
        tallies.insert(rel, (sig, effect));
    }
    Ok((counts, tallies, samples))
}
