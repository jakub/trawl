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
//! take the whole daemon down. As defence-in-depth against any *other* native
//! crash on a poisoned file (e.g. the `DuckDB` `DESCRIBE` still used for column
//! types), each file is write-ahead-logged to an in-flight marker before it is
//! touched: if a refresh dies mid-file, the next start reads the marker, logs
//! the suspect loudly, and quarantines it so the daemon stops crash-looping.
//!
//! Follows the same pattern as [`crate::monitor::spawn_snapshot_collector`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use trawl_api::{DailyCount, ServiceColumnStats, ServiceSchema};
use trawl_engine::executor::Executor;
use trawl_engine::parquet_stats::{self, StatsAccumulator};

use crate::state::{AppState, CachedServiceSchema};

/// File (in the data dir) listing parquet paths known to crash or fail the
/// refresh; these are skipped on every subsequent pass. One path per line.
const QUARANTINE_FILE: &str = ".trawl-schema-quarantine";
/// Write-ahead marker (in the data dir) naming the unit currently being read.
/// Its survival across a process death is the signal that that unit crashed us.
const MARKER_FILE: &str = ".trawl-schema-refresh.inflight";
/// Marker prefix for the `DuckDB` `DESCRIBE` step (which works per-service glob,
/// not per-file, so a crash there can't be pinned to a single file).
const DESCRIBE_MARK: &str = "describe:";

/// Spawn the background schema refresh task.
///
/// Runs immediately on startup, then repeats on `schema_cache_ttl_secs`
/// interval. Results are stored in `state.query.service_schema_cache`.
pub fn spawn_schema_refresh(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let ttl = Duration::from_secs(state.query.schema_cache_ttl_secs);
        let mut interval = tokio::time::interval(ttl);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let fallback_glob = state.query.pool.fallback_glob().to_owned();
            let result =
                tokio::task::spawn_blocking(move || refresh_service_schema(&fallback_glob)).await;

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
/// footers for column stats and row counts.
fn refresh_service_schema(
    fallback_glob: &str,
) -> Result<Vec<ServiceSchema>, Box<dyn std::error::Error + Send + Sync>> {
    let base = fallback_glob
        .find('*')
        .map_or(fallback_glob, |pos| &fallback_glob[..pos]);
    let base = Path::new(base.trim_end_matches('/'));

    if !base.is_dir() {
        return Ok(Vec::new());
    }

    // Load the persistent quarantine, then recover from any marker a previous
    // refresh left behind when it crashed mid-file.
    let mut quarantine = load_quarantine(base);
    recover_stale_marker(base, &mut quarantine);

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
        let schema = build_service_schema(service, files, base, &mut quarantine)?;
        result.push(schema);
    }

    Ok(result)
}

/// Build schema for a single service from its file list.
fn build_service_schema(
    service: &str,
    files: &[FileInfo],
    base: &Path,
    quarantine: &mut BTreeSet<PathBuf>,
) -> Result<ServiceSchema, Box<dyn std::error::Error + Send + Sync>> {
    // Aggregate file-level metadata. `daily_counts` is seeded with every date
    // seen in the directory tree (so a date whose only files are quarantined
    // still appears, with a count of 0) and filled with exact per-file row
    // counts from the footers below.
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
    // DuckDB). Each file is marker-guarded: a crash leaves the marker naming it,
    // and an `Err` (corrupt/unreadable) quarantines it inline — neither can take
    // the daemon down.
    let mut acc = StatsAccumulator::default();
    for f in files {
        if quarantine.contains(&f.path) {
            continue;
        }
        write_marker(base, &f.path.to_string_lossy());
        match parquet_stats::read_file_stats(&f.path) {
            Ok(stats) => {
                if let Some(ref d) = f.date {
                    *daily_counts.entry(d.clone()).or_insert(0) += stats.num_rows;
                }
                acc.add_file(stats);
            }
            Err(e) => {
                tracing::warn!(
                    event_type = "schema_refresh_file_skip",
                    file = %f.path.display(),
                    error = %e,
                    "unreadable parquet file; quarantining and skipping"
                );
                add_to_quarantine(base, &f.path);
                quarantine.insert(f.path.clone());
            }
        }
        clear_marker(base);
    }

    let total_events = acc.total_rows();
    let column_stats = acc.finish();

    // Build a service-scoped glob for the DuckDB schema describe.
    let service_glob = format!("{}/**/{}.parquet", base.to_string_lossy(), service);

    // Column names + types via DuckDB DESCRIBE (the reconciler for cross-file
    // schema drift). This still touches libduckdb, so guard it with a marker too.
    write_marker(base, &format!("{DESCRIBE_MARK}{service_glob}"));
    let executor = Executor::new()?;
    let schema_result = executor.describe_schema(&service_glob);
    clear_marker(base);
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

/// Load the quarantine list (parquet paths to skip) from the data dir.
fn load_quarantine(base: &Path) -> BTreeSet<PathBuf> {
    match std::fs::read_to_string(base.join(QUARANTINE_FILE)) {
        Ok(contents) => contents
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// Append a parquet path to the persistent quarantine (best-effort).
fn add_to_quarantine(base: &Path, file: &Path) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join(QUARANTINE_FILE))
    {
        let _ = writeln!(f, "{}", file.display());
        let _ = f.sync_all();
    }
}

/// Write the in-flight marker naming the unit about to be read, flushing it to
/// disk so it survives a process death (the marker's survival is the crash
/// signal recovered on the next start).
fn write_marker(base: &Path, unit: &str) {
    if let Ok(mut f) = File::create(base.join(MARKER_FILE)) {
        let _ = f.write_all(unit.as_bytes());
        let _ = f.sync_all();
    }
}

/// Clear the in-flight marker after a unit was read without crashing.
fn clear_marker(base: &Path) {
    let _ = std::fs::remove_file(base.join(MARKER_FILE));
}

/// If a marker survived from a previous refresh, that unit crashed the daemon.
/// Log it loudly and — when it names a specific file — quarantine it.
fn recover_stale_marker(base: &Path, quarantine: &mut BTreeSet<PathBuf>) {
    let marker = base.join(MARKER_FILE);
    let Ok(contents) = std::fs::read_to_string(&marker) else {
        return;
    };
    let _ = std::fs::remove_file(&marker);
    let unit = contents.trim();
    if unit.is_empty() {
        return;
    }

    if let Some(glob) = unit.strip_prefix(DESCRIBE_MARK) {
        tracing::error!(
            event_type = "schema_refresh_crash_recovered",
            suspect = %glob,
            "schema refresh previously crashed while DESCRIBE-ing this service's parquet; \
             continuing (a glob describe can't be pinned to one file)"
        );
    } else {
        let path = PathBuf::from(unit);
        tracing::error!(
            event_type = "schema_refresh_crash_recovered",
            suspect_file = %unit,
            "schema refresh previously crashed reading this parquet file; quarantining it. \
             Inspect the file and file it upstream (likely a DuckDB parquet-metadata bug)"
        );
        add_to_quarantine(base, &path);
        quarantine.insert(path);
    }
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
        let services = refresh_service_schema(&glob).unwrap();

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
    fn corrupt_file_is_quarantined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "good",
            "SELECT * FROM (VALUES (1)) t(n)",
        );
        // A bogus ".parquet" that the footer reader will reject.
        let bad_dir = base.join("2026-06-20");
        let bad = bad_dir.join("bad.parquet");
        std::fs::write(&bad, b"not parquet at all").unwrap();

        // Refresh succeeds despite the bad file...
        let glob = format!("{}/**/*.parquet", base.display());
        let services = refresh_service_schema(&glob).unwrap();
        assert!(services.iter().any(|s| s.name == "good"));

        // ...and the bad file is now persisted in the quarantine.
        let quarantined = load_quarantine(base);
        assert!(quarantined.contains(&bad), "bad file should be quarantined");
        // No marker is left behind after a clean (if degraded) pass.
        assert!(!base.join(MARKER_FILE).exists());
    }

    #[test]
    fn stale_file_marker_quarantines_suspect_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let victim = base.join("2026-06-20").join("victim.parquet");

        // Simulate a previous refresh that died while reading `victim`.
        std::fs::write(base.join(MARKER_FILE), victim.to_string_lossy().as_bytes()).unwrap();

        let mut quarantine = load_quarantine(base);
        recover_stale_marker(base, &mut quarantine);

        assert!(quarantine.contains(&victim));
        assert!(load_quarantine(base).contains(&victim));
        // Marker is consumed.
        assert!(!base.join(MARKER_FILE).exists());
    }
}
