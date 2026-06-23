// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background schema refresh job.
//!
//! Pre-computes per-service schema metadata from parquet file footers on a
//! periodic interval. The `/api/v1/schema/services` endpoint is a pure cache
//! read — no query execution or file I/O on the request path.
//!
//! Per-column statistics are read from parquet footers in safe Rust (see
//! [`trawl_engine::parquet_stats`]) rather than via `DuckDB`'s
//! `parquet_metadata()` table function, which can `SIGSEGV` on some files and
//! take the whole daemon down. Because the footer reader turns a poisoned file
//! into a catchable [`Err`] rather than an uncatchable crash, a bad file is
//! simply skipped for that pass and retried on the next one — no daemon death,
//! no persistent quarantine to drift out of sync. The skip is logged loudly so
//! the offending file can be identified, deduplicated across passes (via the
//! `warned` set) so a persistently-broken file doesn't spam the log every tick.
//!
//! Follows the same pattern as [`crate::monitor::spawn_snapshot_collector`].

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use trawl_api::{DailyCount, ServiceColumnStats, ServiceSchema};
use trawl_engine::executor::Executor;
use trawl_engine::parquet_stats::{self, StatsAccumulator};

use crate::state::{AppState, CachedServiceSchema};

/// Spawn the background schema refresh task.
///
/// Runs immediately on startup, then repeats on `schema_cache_ttl_secs`
/// interval. Results are stored in `state.query.service_schema_cache`.
pub fn spawn_schema_refresh(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let ttl = Duration::from_secs(state.query.schema_cache_ttl_secs);
        let mut interval = tokio::time::interval(ttl);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Files skipped (unreadable) on the previous pass, so we log each newly
        // broken file once rather than every tick. Shared with the blocking
        // refresh closure; lives for the process.
        let warned: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

        loop {
            interval.tick().await;

            let fallback_glob = state.query.pool.fallback_glob().to_owned();
            let warned = Arc::clone(&warned);
            let result = tokio::task::spawn_blocking(move || {
                refresh_service_schema(&fallback_glob, &warned)
            })
            .await;

            match result {
                Ok(Ok(services)) => {
                    tracing::info!(
                        event_type = "schema_refresh_complete",
                        services = services.len(),
                        "service schema refresh complete"
                    );
                    *state.query.service_schema_cache.lock() = Some(CachedServiceSchema {
                        services,
                        cached_at: Instant::now(),
                    });
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        event_type = "schema_refresh_error",
                        error = %e,
                        "service schema refresh failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        event_type = "schema_refresh_panic",
                        error = %e,
                        "service schema refresh task panicked"
                    );
                }
            }
        }
    })
}

/// Collected metadata for a single parquet file.
struct FileInfo {
    path: PathBuf,
    size: u64,
    date: Option<String>,
}

/// Perform the full service schema refresh.
///
/// Walks parquet files, groups by service, then for each service reads parquet
/// footers for column stats and row counts. `warned` carries the set of files
/// that failed to read on the previous pass so each break is logged once.
fn refresh_service_schema(
    fallback_glob: &str,
    warned: &Mutex<HashSet<PathBuf>>,
) -> Result<Vec<ServiceSchema>, Box<dyn std::error::Error + Send + Sync>> {
    let base = fallback_glob
        .find('*')
        .map_or(fallback_glob, |pos| &fallback_glob[..pos]);
    let base = Path::new(base.trim_end_matches('/'));

    if !base.is_dir() {
        return Ok(Vec::new());
    }

    // Files we'd already warned about (snapshot of last pass), and the files we
    // skip this pass — used to log each newly-broken file exactly once.
    let prev_warned = warned.lock().clone();
    let mut skipped_now: HashSet<PathBuf> = HashSet::new();

    let entries = crate::metrics::walk_parquet_files(base)?;

    // Group files by service, collecting per-file metadata.
    let mut by_service: BTreeMap<String, Vec<FileInfo>> = BTreeMap::new();

    for (path, size) in entries {
        let service = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned();

        let date = find_date_ancestor(&path);

        by_service
            .entry(service)
            .or_default()
            .push(FileInfo { path, size, date });
    }

    let mut result = Vec::with_capacity(by_service.len());

    for (service, files) in &by_service {
        let schema = build_service_schema(service, files, base, &prev_warned, &mut skipped_now)?;
        result.push(schema);
    }

    // Remember this pass's skips: files that recovered drop out of the set, so a
    // future re-break is logged again.
    *warned.lock() = skipped_now;

    Ok(result)
}

/// Build schema for a single service from its file list.
///
/// `prev_warned` is the read-only set of files already logged as broken;
/// `skipped_now` accumulates the files skipped this pass.
fn build_service_schema(
    service: &str,
    files: &[FileInfo],
    base: &Path,
    prev_warned: &HashSet<PathBuf>,
    skipped_now: &mut HashSet<PathBuf>,
) -> Result<ServiceSchema, Box<dyn std::error::Error + Send + Sync>> {
    // Aggregate file-level metadata. `daily_counts` is seeded with every date
    // seen in the directory tree (so a date whose only files are skipped this
    // pass still appears, with a count of 0) and filled with exact per-file row
    // counts from the footers below.
    //
    // Note: a parquet file directly under `base` with no YYYY-MM-DD ancestor
    // contributes to `total_events` but not to any daily bucket — the two
    // totals can legitimately diverge for date-less files.
    let mut dates: BTreeSet<String> = BTreeSet::new();
    let mut total_bytes: u64 = 0;
    let mut daily_counts: BTreeMap<String, u64> = BTreeMap::new();

    for f in files {
        total_bytes += f.size;
        if let Some(ref d) = f.date {
            dates.insert(d.clone());
            daily_counts.entry(d.clone()).or_insert(0);
        }
    }

    let file_count = u64::try_from(files.len()).unwrap_or(0);
    let earliest_date = dates.iter().next().cloned();
    let latest_date = dates.iter().next_back().cloned();

    // Read per-column stats + row counts from each file's footer (pure Rust, no
    // DuckDB). An unreadable file (corrupt, truncated, or caught mid-write) is a
    // catchable `Err`, never a crash: skip it this pass and retry next time.
    let mut acc = StatsAccumulator::default();
    for f in files {
        match parquet_stats::read_file_stats(&f.path) {
            Ok(stats) => {
                if let Some(ref d) = f.date {
                    *daily_counts.entry(d.clone()).or_insert(0) += stats.num_rows;
                }
                acc.add_file(stats);
            }
            Err(e) => {
                // Log once per break (the file wasn't already on the warned
                // list), so a persistently-broken file doesn't spam every tick.
                if !prev_warned.contains(&f.path) {
                    tracing::warn!(
                        event_type = "schema_refresh_file_skip",
                        file = %f.path.display(),
                        error = %e,
                        "unreadable parquet file; skipping this pass, will retry next refresh"
                    );
                }
                skipped_now.insert(f.path.clone());
            }
        }
    }

    let total_events = acc.total_rows();
    let column_stats = acc.finish();

    // Build a service-scoped glob for the DuckDB schema describe.
    let service_glob = format!("{}/**/{}.parquet", base.to_string_lossy(), service);

    // Column names + types via DuckDB DESCRIBE — the reconciler for cross-file
    // schema drift. DESCRIBE reads only the footer schema (names/types), not the
    // row-group stat values that crash `parquet_metadata()`, so it stays off the
    // crash path the footer reader was added to avoid.
    let executor = Executor::new()?;
    let schema_result = executor.describe_schema(&service_glob);
    let schema_columns = schema_result.as_ref().map_or(&[][..], |r| &r.columns);

    // Merge column stats with schema column types.
    let columns: Vec<ServiceColumnStats> = schema_columns
        .iter()
        .map(|sc| {
            let stats = column_stats.iter().find(|cs| cs.column_name == sc.name);
            ServiceColumnStats {
                name: sc.name.clone(),
                data_type: sc.data_type.clone(),
                null_count: stats.map_or(0, |s| s.null_count),
                total_count: stats.map_or(0, |s| s.total_count),
                min_value: stats.and_then(|s| s.min_value.clone()),
                max_value: stats.and_then(|s| s.max_value.clone()),
                compressed_bytes: stats.map_or(0, |s| s.compressed_bytes),
            }
        })
        .collect();

    let daily_event_counts: Vec<DailyCount> = daily_counts
        .into_iter()
        .map(|(date, count)| DailyCount { date, count })
        .collect();

    Ok(ServiceSchema {
        name: service.to_owned(),
        columns,
        earliest_date,
        latest_date,
        file_count,
        total_bytes,
        total_events,
        daily_event_counts,
    })
}

/// Walk ancestors of a path looking for a YYYY-MM-DD directory component.
fn find_date_ancestor(path: &Path) -> Option<String> {
    for ancestor in path.ancestors().skip(1) {
        if let Some(name) = ancestor.file_name().and_then(|n| n.to_str())
            && is_date_dir(name)
        {
            return Some(name.to_owned());
        }
    }
    None
}

/// Check if a directory name looks like YYYY-MM-DD.
fn is_date_dir(name: &str) -> bool {
    name.len() == 10
        && name.as_bytes()[4] == b'-'
        && name.as_bytes()[7] == b'-'
        && name[..4].bytes().all(|b| b.is_ascii_digit())
        && name[5..7].bytes().all(|b| b.is_ascii_digit())
        && name[8..10].bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DuckDB-written parquet under `<base>/<date>/<service>.parquet`.
    fn write_service_parquet(base: &Path, date: &str, service: &str, select: &str) -> PathBuf {
        let dir = base.join(date);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{service}.parquet"));
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY ({select}) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
        path
    }

    #[test]
    fn refresh_reads_footer_stats_and_exact_daily_counts() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "nginx",
            "SELECT * FROM (VALUES (200), (404)) t(status)",
        );
        write_service_parquet(
            base,
            "2026-06-21",
            "nginx",
            "SELECT * FROM (VALUES (500), (502), (200)) t(status)",
        );

        let glob = format!("{}/**/*.parquet", base.display());
        let warned = Mutex::new(HashSet::new());
        let services = refresh_service_schema(&glob, &warned).unwrap();

        let nginx = services.iter().find(|s| s.name == "nginx").unwrap();
        assert_eq!(nginx.file_count, 2);
        assert_eq!(nginx.total_events, 5);
        // Exact per-day counts from footer row counts, not a size estimate.
        let day0 = nginx
            .daily_event_counts
            .iter()
            .find(|d| d.date == "2026-06-20")
            .unwrap();
        let day1 = nginx
            .daily_event_counts
            .iter()
            .find(|d| d.date == "2026-06-21")
            .unwrap();
        assert_eq!(day0.count, 2);
        assert_eq!(day1.count, 3);

        let status = nginx.columns.iter().find(|c| c.name == "status").unwrap();
        assert_eq!(status.total_count, 5);
        assert_eq!(status.min_value.as_deref(), Some("200"));
        assert_eq!(status.max_value.as_deref(), Some("502"));
    }

    #[test]
    fn corrupt_file_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "good",
            "SELECT * FROM (VALUES (1)) t(n)",
        );
        // A bogus ".parquet" that the footer reader will reject.
        let bad = base.join("2026-06-20").join("bad.parquet");
        std::fs::write(&bad, b"not parquet at all").unwrap();

        // Refresh succeeds despite the bad file...
        let glob = format!("{}/**/*.parquet", base.display());
        let warned = Mutex::new(HashSet::new());
        let services = refresh_service_schema(&glob, &warned).unwrap();
        assert!(services.iter().any(|s| s.name == "good"));

        // ...the bad file's service has zero events (it was skipped)...
        let bad_svc = services.iter().find(|s| s.name == "bad").unwrap();
        assert_eq!(bad_svc.total_events, 0);

        // ...the skip is recorded for log-dedup across passes...
        assert!(warned.lock().contains(&bad));

        // ...and NO persistent quarantine/marker artifact is written to disk.
        assert!(!base.join(".trawl-schema-quarantine").exists());
        assert!(!base.join(".trawl-schema-refresh.inflight").exists());
    }

    #[test]
    fn recovered_file_is_picked_up_next_pass() {
        // A file that fails one pass but reads on the next must NOT be lost
        // permanently (the regression the persistent quarantine introduced).
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let svc = base.join("2026-06-20").join("svc.parquet");
        std::fs::create_dir_all(svc.parent().unwrap()).unwrap();
        std::fs::write(&svc, b"garbage, not parquet yet").unwrap();

        let glob = format!("{}/**/*.parquet", base.display());
        let warned = Mutex::new(HashSet::new());

        // Pass 1: file is garbage → skipped, zero events, recorded in warned.
        let pass1 = refresh_service_schema(&glob, &warned).unwrap();
        let svc1 = pass1.iter().find(|s| s.name == "svc").unwrap();
        assert_eq!(svc1.total_events, 0);
        assert!(warned.lock().contains(&svc));

        // The file becomes valid (e.g. compaction finished writing it).
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM (VALUES (1), (2)) t(n)) TO '{}' (FORMAT PARQUET)",
            svc.display()
        ))
        .unwrap();

        // Pass 2: it reads cleanly and its rows are counted; warned set clears.
        let pass2 = refresh_service_schema(&glob, &warned).unwrap();
        let svc2 = pass2.iter().find(|s| s.name == "svc").unwrap();
        assert_eq!(svc2.total_events, 2);
        assert!(!warned.lock().contains(&svc));
    }
}
