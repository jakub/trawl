// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin scan (ADR-0011): the dry-run report, and the mandatory first
//! phase of every executing job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalog::conform::{Progress, open_bounded_connection};
use crate::ingest::compaction::RepinReading;
use crate::repin::cancel::{CancelHandle, PassStop, STAGE_SCAN};
use crate::repin::rewrite::{
    FileSig, RepinEffect, affected_schema, count_repin_effect, count_repin_effect_sampled,
};
use crate::store::MAX_CONFLICT_SAMPLES;

/// The scan's per-file readings, keyed by data-root-relative path and
/// stamped with the signature they were measured at.
///
/// A file compaction replaced between scan and build gets a new
/// `(ino, len, mtime)` and is recounted, exactly as a catch-up pass
/// recounts it.
pub(crate) type ScanTallies = BTreeMap<PathBuf, (FileSig, RepinEffect)>;

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanCounts {
    /// Affected files (carry the target column at an owned layout path).
    pub files_total: u64,
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

/// Blocking (`DuckDB` + filesystem) — callers run it on the blocking
/// pool.
///
/// The third return is up to [`MAX_CONFLICT_SAMPLES`] samples of the values
/// the new pin cannot read.
pub(crate) fn scan(
    data_dir: &Path,
    memory_limit: &str,
    field: &str,
    reading: RepinReading,
    cancel: &CancelHandle,
) -> Result<(ScanCounts, ScanTallies, Vec<String>), PassStop> {
    // Test-only panic (see `crate::repin::engine::TEST_PANIC_IN_SCAN`); the
    // message is the payload the request must never see.
    #[cfg(any(test, feature = "test-support"))]
    assert!(
        !crate::repin::engine::TEST_PANIC_IN_SCAN.swap(false, std::sync::atomic::Ordering::SeqCst),
        "zz_repin_scan_payload_sentinel"
    );
    let sources = crate::repin::rewrite::snapshot_env_files(data_dir)?;
    let conn = open_bounded_connection(data_dir, memory_limit)?;

    let mut counts = ScanCounts::default();
    let mut tallies = ScanTallies::new();
    let mut samples: Vec<String> = Vec::new();
    let mut progress = Progress::new("repin-scan", sources.len());
    for (rel, sig) in sources {
        cancel.check(STAGE_SCAN)?;
        progress.tick();
        #[cfg(any(test, feature = "test-support"))]
        {
            let delay =
                crate::repin::engine::TEST_SCAN_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
            if delay > 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            // Test-only hold at this file's boundary, so a cancel can be
            // requested of a scan that provably has files left to read
            // (see `crate::repin::engine::TEST_HOLD_IN_SCAN`).
            if crate::repin::engine::TEST_HOLD_IN_SCAN
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                crate::repin::engine::TEST_SCAN_HELD
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                while !crate::repin::engine::TEST_RELEASE_SCAN
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        let path = data_dir.join(&rel);
        if let Some((schema, _layout)) = affected_schema(&conn, data_dir, &path, field)? {
            let safe = path.to_string_lossy().replace('\'', "''");
            let source = format!("read_parquet('{safe}')");
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
        cancel.check(STAGE_SCAN)?;
    }
    Ok((counts, tallies, samples))
}

#[cfg(test)]
mod tests {
    /// The check is a real read of the corpus (`DuckDB` plus a parquet
    /// tree), so what is guarded here is the control flow rather than the
    /// latency.
    #[test]
    fn no_scanned_file_skips_the_post_file_cancel_check() {
        const SOURCE: &str = include_str!("plan.rs");
        let start = SOURCE
            .find("    for (rel, sig) in sources {")
            .expect("the scan still walks the snapshot file by file");
        let end = SOURCE[start..]
            .find("    Ok((counts, tallies, samples))")
            .expect("the scan still returns its readings")
            + start;
        let body = &SOURCE[start..end];
        assert!(
            !body.contains("continue"),
            "an early exit from the scan loop skips its post-file cancel \
             check; keep the whole body inside the conditional instead"
        );
        assert!(
            body.trim_end().ends_with('}'),
            "the loop body is the region asserted on"
        );
        assert_eq!(
            body.matches("cancel.check(STAGE_SCAN)?").count(),
            2,
            "one check before each file and one after it"
        );
    }
}
