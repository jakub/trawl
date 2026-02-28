//! Background WAL → parquet compaction task.
//!
//! Periodically scans the WAL directory for `.ndjson` files, groups
//! them by service, and uses `DuckDB` to convert each batch to parquet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

use crate::hot_buffer::HotBuffer;

/// Spawn the compaction background loop.
///
/// Runs every `interval` seconds, scanning `wal_dir` for `.ndjson` files
/// whose mtime is older than `interval`. Groups by service and writes
/// parquet to `data_dir/{date}/{hour}/{service}.parquet`.
///
/// Stops when `shutdown_rx` receives a signal.
pub fn spawn_compaction(
    wal_dir: PathBuf,
    data_dir: PathBuf,
    interval: Duration,
    daily_rollup: bool,
    hot_buffer: Option<Arc<HotBuffer>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            event_type = "lifecycle",
            action = "compaction_start",
            wal_dir = %wal_dir.display(),
            data_dir = %data_dir.display(),
            interval_secs = interval.as_secs(),
            daily_rollup,
            "compaction task started"
        );

        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    if let Err(e) = compact_once(&wal_dir, &data_dir, interval, daily_rollup, hot_buffer.as_ref()).await {
                        tracing::error!(event_type = "compaction_error", error = %e, "compaction tick failed");
                    }
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!(event_type = "lifecycle", action = "compaction_stop", "compaction task shutting down");
                    break;
                }
            }
        }
    })
}

/// Run one compaction cycle.
///
/// Public for integration tests only — not part of the external API.
/// Called internally by [`spawn_compaction`].
pub async fn compact_once(
    wal_dir: &Path,
    data_dir: &Path,
    min_age: Duration,
    daily_rollup: bool,
    hot_buffer: Option<&Arc<HotBuffer>>,
) -> Result<(), String> {
    // Remove orphaned .parquet.tmp files from interrupted compaction runs.
    cleanup_stale_tmp_files(data_dir, min_age * 2);

    let files = scan_wal_files(wal_dir, min_age).map_err(|e| format!("scan failed: {e}"))?;

    if !files.is_empty() {
        // Group WAL files by service prefix.
        let groups = group_by_service(files);

        for (service, wal_files) in &groups {
            tracing::debug!(
                event_type = "compaction_start",
                compact_service = %service,
                wal_files = wal_files.len(),
                "compacting service batch"
            );

            // Mark hot buffer batches as draining BEFORE writing parquet,
            // so concurrent snapshots skip them (prevents TOCTOU duplicates).
            let batch_ids: Vec<&str> = wal_files
                .iter()
                .filter_map(|f| f.file_stem()?.to_str())
                .collect();
            if let Some(buf) = &hot_buffer {
                buf.mark_draining(&batch_ids);
            }

            match compact_service_batch(wal_files, data_dir, service).await {
                Ok(()) => {
                    // Remove fully compacted batches from the hot buffer.
                    if let Some(buf) = &hot_buffer {
                        buf.drain(&batch_ids);
                    }

                    // Clean up consumed WAL files.
                    for f in wal_files {
                        if let Err(e) = std::fs::remove_file(f) {
                            tracing::warn!(
                                event_type = "compaction_error",
                                file = %f.display(),
                                error = %e,
                                "failed to delete consumed WAL file"
                            );
                        }
                    }
                }
                Err(e) => {
                    // Leave WAL files for retry on next tick.
                    tracing::error!(
                        event_type = "compaction_error",
                        compact_service = %service,
                        error = %e,
                        "compaction failed, will retry next tick"
                    );
                }
            }
        }
    }

    // After WAL compaction, consolidate older days' hourly files into
    // per-service daily files. This dramatically reduces file count for
    // long lookback queries.
    if daily_rollup && let Err(e) = rollup_once(data_dir).await {
        tracing::error!(event_type = "rollup_error", error = %e, "daily rollup failed");
    }

    Ok(())
}

/// Consolidate hourly per-service parquet files into daily files.
///
/// For each date-directory older than today, collects all
/// `{hour}/{service}.parquet` files, merges them (sorted by timestamp)
/// into `{date}/{service}.parquet`, then removes the hourly sources.
async fn rollup_once(data_dir: &Path) -> Result<(), String> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let date_dirs =
        std::fs::read_dir(data_dir).map_err(|e| format!("failed to read data_dir: {e}"))?;

    for entry in date_dirs.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };

        // Skip today — it stays hourly for fast writes.
        if dir_name == today {
            continue;
        }

        // Skip directories that aren't date-formatted (e.g. "wal").
        if !looks_like_date(&dir_name) {
            continue;
        }

        // Recover any interrupted rollups from previous runs before
        // starting new ones. This ensures crash-orphaned hourly files
        // are cleaned up without re-merging already-consolidated data.
        if let Err(e) = recover_rollup_markers(&path) {
            tracing::error!(
                event_type = "rollup_error",
                date = %dir_name,
                error = %e,
                "rollup recovery failed"
            );
        }

        // Collect hourly subdirs. If none exist, this day is already consolidated.
        let hour_dirs = collect_hour_dirs(&path);
        if hour_dirs.is_empty() {
            continue;
        }

        // Group all parquet files across hour-dirs by service name.
        let service_files = collect_service_files(&hour_dirs);
        if service_files.is_empty() {
            continue;
        }

        for (service, files) in &service_files {
            let day_dir = path.clone();
            let svc = service.clone();
            let files = files.clone();

            match tokio::task::spawn_blocking(move || rollup_day_blocking(&day_dir, &svc, &files))
                .await
                .map_err(|e| format!("rollup task panicked: {e}"))?
            {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(
                        event_type = "rollup_error",
                        compact_service = %service,
                        error = %e,
                        "rollup failed for service, will retry next tick"
                    );
                }
            }
        }

        // Remove empty hour-directories after all services are rolled up.
        for hour_dir in &hour_dirs {
            if is_dir_empty(hour_dir) {
                let _ = std::fs::remove_dir(hour_dir);
            }
        }
    }

    Ok(())
}

/// Check if a directory name looks like a date (YYYY-MM-DD).
fn looks_like_date(name: &str) -> bool {
    name.len() == 10
        && name.as_bytes().get(4) == Some(&b'-')
        && name.as_bytes().get(7) == Some(&b'-')
        && name[..4].bytes().all(|b| b.is_ascii_digit())
}

/// Collect subdirectories that look like hour directories (00-23).
fn collect_hour_dirs(day_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(day_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name()?.to_str()?;
                if name.len() == 2 && name.bytes().all(|b| b.is_ascii_digit()) {
                    return Some(p);
                }
            }
            None
        })
        .collect()
}

/// Collect `{service}.parquet` files across all hour-dirs, grouped by service.
fn collect_service_files(hour_dirs: &[PathBuf]) -> HashMap<String, Vec<PathBuf>> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for hour_dir in hour_dirs {
        let Ok(entries) = std::fs::read_dir(hour_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "parquet")
                && let Some(service) = path.file_stem().and_then(|s| s.to_str())
            {
                groups.entry(service.to_owned()).or_default().push(path);
            }
        }
    }

    groups
}

/// Check if a directory is empty.
fn is_dir_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// Path for the rollup marker file that tracks in-progress merges.
///
/// The marker lists hourly file paths being merged, enabling crash
/// recovery without re-reading already-consolidated data.
fn rollup_marker_path(day_dir: &Path, service: &str) -> PathBuf {
    day_dir.join(format!(".rollup-{service}"))
}

/// Write a rollup marker listing the hourly files being merged.
fn write_rollup_marker(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
) -> Result<(), String> {
    let marker = rollup_marker_path(day_dir, service);
    let content = hourly_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&marker, content).map_err(|e| format!("failed to write rollup marker: {e}"))
}

/// Delete the rollup marker after successful cleanup.
fn delete_rollup_marker(day_dir: &Path, service: &str) {
    let marker = rollup_marker_path(day_dir, service);
    let _ = std::fs::remove_file(marker);
}

/// Recover from interrupted rollup operations in a date directory.
///
/// Checks for `.rollup-{service}` marker files and completes the
/// interrupted operation:
/// - If canonical `.parquet` exists: crash after rename — delete
///   hourly source files listed in marker.
/// - If `.parquet.tmp` exists: crash after write but before rename —
///   rename `.tmp` to canonical, then delete hourlies.
/// - If neither exists: stale marker, just remove it.
fn recover_rollup_markers(day_dir: &Path) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(day_dir) else {
        return Ok(());
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let Some(service) = name.strip_prefix(".rollup-") else {
            continue;
        };

        let canonical = day_dir.join(format!("{service}.parquet"));
        let tmp = day_dir.join(format!("{service}.parquet.tmp"));

        // Read hourly file paths from marker.
        let marker_content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read rollup marker: {e}"))?;
        let hourly_files: Vec<PathBuf> = marker_content
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();

        if canonical.exists() {
            // Crash after rename — just clean up hourlies.
            tracing::info!(
                event_type = "rollup_recovery",
                compact_service = %service,
                "recovering rollup: canonical exists, deleting hourly files"
            );
            for f in &hourly_files {
                let _ = std::fs::remove_file(f);
            }
        } else if tmp.exists() {
            // Crash after write but before rename — complete the rename.
            tracing::info!(
                event_type = "rollup_recovery",
                compact_service = %service,
                "recovering rollup: renaming tmp to canonical"
            );
            std::fs::rename(&tmp, &canonical)
                .map_err(|e| format!("rollup recovery rename failed: {e}"))?;
            for f in &hourly_files {
                let _ = std::fs::remove_file(f);
            }
        } else {
            // Neither exists — stale marker.
            tracing::warn!(
                event_type = "rollup_recovery",
                compact_service = %service,
                "removing stale rollup marker (no tmp or canonical file)"
            );
        }

        // Remove the marker.
        let _ = std::fs::remove_file(&path);
    }

    Ok(())
}

/// Blocking: merge hourly parquet files for one service into a daily file.
///
/// Opens an in-memory `DuckDB` connection, reads all hourly files via
/// `read_parquet([list], union_by_name=true)`, sorts by timestamp for
/// optimal row group statistics, writes to `.tmp`, then atomic-renames.
///
/// Uses a `.rollup-{service}` marker file for crash safety: the marker
/// lists which hourly files are being merged, enabling recovery without
/// re-reading already-consolidated data.
fn rollup_day_blocking(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
) -> Result<(), String> {
    let rollup_start = std::time::Instant::now();
    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;

    // Build file list for read_parquet.
    let mut file_list_parts = Vec::with_capacity(hourly_files.len() + 1);
    for f in hourly_files {
        file_list_parts.push(format!("'{}'", f.to_string_lossy()));
    }

    // If a day-level file already exists (e.g. late-arriving data after a
    // previous rollup), include it in the merge.
    let canonical_path = day_dir.join(format!("{service}.parquet"));
    if canonical_path.exists() {
        file_list_parts.push(format!("'{}'", canonical_path.to_string_lossy()));
    }

    let file_list_sql = file_list_parts.join(", ");
    let tmp_path = day_dir.join(format!("{service}.parquet.tmp"));

    // Read, merge, sort by timestamp, and write to tmp file.
    conn.execute_batch(&format!(
        "COPY (\
             SELECT * FROM read_parquet([{file_list_sql}], union_by_name=true) \
             ORDER BY \"timestamp\"\
         ) TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY)",
        tmp_path.to_string_lossy(),
    ))
    .map_err(|e| format!("rollup COPY failed: {e}"))?;

    // Write marker BEFORE rename so recovery knows which hourlies to clean up.
    write_rollup_marker(day_dir, service, hourly_files)?;

    // Atomic rename.
    std::fs::rename(&tmp_path, &canonical_path)
        .map_err(|e| format!("rollup rename failed: {e}"))?;

    // Delete hourly source files.
    for f in hourly_files {
        if let Err(e) = std::fs::remove_file(f) {
            tracing::warn!(
                event_type = "rollup_error",
                file = %f.display(),
                error = %e,
                "failed to delete hourly file after rollup"
            );
        }
    }

    // Remove marker — rollup fully complete.
    delete_rollup_marker(day_dir, service);

    let output_bytes = std::fs::metadata(&canonical_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let duration_ms = rollup_start.elapsed().as_millis();
    tracing::info!(
        event_type = "rollup_complete",
        compact_service = %service,
        output = %canonical_path.display(),
        hourly_files = hourly_files.len(),
        output_bytes,
        duration_ms,
        "daily rollup complete"
    );

    Ok(())
}

/// Compact a batch of WAL files for a single service into parquet.
async fn compact_service_batch(
    wal_files: &[PathBuf],
    data_dir: &Path,
    service: &str,
) -> Result<(), String> {
    let wal_files = wal_files.to_vec();
    let data_dir = data_dir.to_path_buf();
    let service = service.to_owned();

    tokio::task::spawn_blocking(move || compact_service_blocking(&wal_files, &data_dir, &service))
        .await
        .map_err(|e| format!("compaction task panicked: {e}"))?
}

/// Count rows in a `DuckDB` table. Used to capture row counts before
/// dropping temporary tables.
fn count_rows(conn: &duckdb::Connection, table: &str) -> Result<u64, String> {
    conn.query_row(
        &format!("SELECT count(*)::BIGINT FROM \"{table}\""),
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| u64::try_from(n).unwrap_or(0))
    .map_err(|e| format!("count_rows failed: {e}"))
}

/// Blocking compaction: open `DuckDB`, read ndjson, write parquet.
///
/// Uses a canonical filename (`{service}.parquet`) per service per
/// hour-directory. When a canonical file already exists, merges the
/// new WAL data with it via `UNION ALL`. Writes to a `.tmp` file
/// first, then does an atomic `rename()` for crash safety.
fn compact_service_blocking(
    wal_files: &[PathBuf],
    data_dir: &Path,
    service: &str,
) -> Result<(), String> {
    let compact_start = std::time::Instant::now();
    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;

    // Build file list for read_json_auto.
    let file_list_sql = wal_files
        .iter()
        .map(|p| format!("'{}'", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");

    // Read all WAL files into a temp table, casting timestamp to native TIMESTAMP
    // so parquet row group statistics enable predicate pushdown for time filters.
    conn.execute_batch(&format!(
        "CREATE TABLE wal_batch AS \
         SELECT * REPLACE (CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") \
         FROM read_json_auto([{file_list_sql}])"
    ))
    .map_err(|e| format!("read_json_auto failed: {e}"))?;

    // Determine output directory from current time.
    let now = chrono::Utc::now();
    let day_part = now.format("%Y-%m-%d").to_string();
    let hour_part = now.format("%H").to_string();
    let output_dir = data_dir.join(&day_part).join(&hour_part);

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("failed to create output dir: {e}"))?;

    // Canonical filename: one parquet file per service per hour-directory.
    // Merge with existing data if present, then atomic-rename into place.
    let canonical_path = output_dir.join(format!("{service}.parquet"));
    let tmp_path = output_dir.join(format!("{service}.parquet.tmp"));

    let merged = canonical_path.exists();

    let rows: u64 = if merged {
        // Merge: union existing parquet rows with new WAL batch.
        // BY NAME handles heterogeneous schemas (different events have
        // different fields) — missing columns become NULL in parquet.
        conn.execute_batch(&format!(
            "CREATE TABLE merged AS \
             SELECT * FROM read_parquet('{}') \
             UNION ALL BY NAME \
             SELECT * FROM wal_batch",
            canonical_path.display(),
        ))
        .map_err(|e| format!("merge read_parquet failed: {e}"))?;

        conn.execute_batch(&format!(
            "COPY merged TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY)",
            tmp_path.display(),
        ))
        .map_err(|e| format!("COPY TO parquet failed: {e}"))?;

        let count = count_rows(&conn, "merged")?;
        conn.execute_batch("DROP TABLE IF EXISTS merged")
            .map_err(|e| format!("DROP TABLE failed: {e}"))?;
        count
    } else {
        // Fresh write: no existing file to merge with.
        conn.execute_batch(&format!(
            "COPY wal_batch TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY)",
            tmp_path.display(),
        ))
        .map_err(|e| format!("COPY TO parquet failed: {e}"))?;

        count_rows(&conn, "wal_batch")?
    };

    conn.execute_batch("DROP TABLE IF EXISTS wal_batch")
        .map_err(|e| format!("DROP TABLE failed: {e}"))?;

    // Atomic rename: crash-safe swap. POSIX rename() is atomic, so
    // concurrent readers on the old inode finish normally while new
    // readers get the merged file.
    std::fs::rename(&tmp_path, &canonical_path)
        .map_err(|e| format!("atomic rename failed: {e}"))?;

    let output_bytes = std::fs::metadata(&canonical_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let duration_ms = compact_start.elapsed().as_millis();
    tracing::info!(
        event_type = "compaction_complete",
        compact_service = %service,
        output = %canonical_path.display(),
        wal_files = wal_files.len(),
        merged,
        rows,
        output_bytes,
        duration_ms,
        "compaction complete"
    );

    Ok(())
}

/// Remove stale `.parquet.tmp` files left by interrupted compaction or
/// rollup runs.
///
/// Checks both `data_dir/{date}/{hour}/` (hourly compaction) and
/// `data_dir/{date}/` (daily rollup) for `.tmp` files older than
/// `max_age`. These are inert (don't match `*.parquet` globs) but
/// should be cleaned up to avoid disk waste.
fn cleanup_stale_tmp_files(data_dir: &Path, max_age: Duration) {
    let Ok(days) = std::fs::read_dir(data_dir) else {
        return;
    };
    for day_entry in days.flatten() {
        let day_path = day_entry.path();
        if !day_path.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&day_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Hour subdirectory — check its contents.
                cleanup_tmp_in_dir(&path, max_age);
            } else {
                // Day-level file (from rollup).
                remove_stale_tmp(&path, max_age);
            }
        }
    }
}

/// Remove `.tmp` files in a single directory that are older than `max_age`.
fn cleanup_tmp_in_dir(dir: &Path, max_age: Duration) {
    let Ok(files) = std::fs::read_dir(dir) else {
        return;
    };
    for file in files.flatten() {
        remove_stale_tmp(&file.path(), max_age);
    }
}

/// Remove a single `.tmp` file if older than `max_age`.
fn remove_stale_tmp(path: &Path, max_age: Duration) {
    if path.extension().is_some_and(|ext| ext == "tmp")
        && let Ok(meta) = std::fs::metadata(path)
        && let Ok(mtime) = meta.modified()
        && SystemTime::now().duration_since(mtime).unwrap_or_default() > max_age
    {
        let _ = std::fs::remove_file(path);
        tracing::debug!(event_type = "tmp_cleanup", path = %path.display(), "removed stale tmp file");
    }
}

/// Scan the WAL directory for `.ndjson` files older than `min_age`.
fn scan_wal_files(wal_dir: &Path, min_age: Duration) -> std::io::Result<Vec<PathBuf>> {
    let now = SystemTime::now();
    let mut files = Vec::new();

    let entries = match std::fs::read_dir(wal_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "ndjson")
            && let Ok(mtime) = entry.metadata()?.modified()
            && now.duration_since(mtime).unwrap_or_default() > min_age
        {
            files.push(path);
        }
    }

    Ok(files)
}

/// Group WAL file paths by service name (extracted from filename prefix).
fn group_by_service(files: Vec<PathBuf>) -> HashMap<String, Vec<PathBuf>> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for path in files {
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        // Filename format: {service}_{unix_millis}_{hex_random}
        // Find the service by taking everything before the last two `_` segments.
        let service = extract_service_from_filename(filename);
        groups.entry(service).or_default().push(path);
    }

    groups
}

/// Extract the service name from a WAL filename.
///
/// Filename format: `{service}_{unix_millis}_{hex_random}`
/// The millis and hex segments are always the last two `_`-delimited parts.
fn extract_service_from_filename(filename: &str) -> String {
    let parts: Vec<&str> = filename.rsplitn(3, '_').collect();
    if parts.len() == 3 {
        parts[2].to_owned()
    } else {
        filename.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_service_simple() {
        assert_eq!(
            extract_service_from_filename("nginx_1234567890_abcd"),
            "nginx"
        );
    }

    #[test]
    fn extract_service_with_underscores() {
        assert_eq!(
            extract_service_from_filename("my_cool_service_1234567890_abcd"),
            "my_cool_service"
        );
    }

    #[test]
    fn group_by_service_groups_correctly() {
        let files = vec![
            PathBuf::from("/wal/nginx_100_aaaa.ndjson"),
            PathBuf::from("/wal/nginx_200_bbbb.ndjson"),
            PathBuf::from("/wal/postgres_100_cccc.ndjson"),
        ];
        let groups = group_by_service(files);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["nginx"].len(), 2);
        assert_eq!(groups["postgres"].len(), 1);
    }

    /// Helper: write an ndjson WAL file with the given records.
    fn write_wal_file(dir: &Path, service: &str, records: &[&str]) -> PathBuf {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let path = dir.join(format!("{service}_{millis}_test.ndjson"));
        std::fs::write(&path, records.join("\n")).unwrap();
        path
    }

    /// Recursively find files matching an extension under a directory.
    fn find_files_by_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
        let mut result = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    result.extend(find_files_by_ext(&path, ext));
                } else if path.extension().is_some_and(|e| e == ext) {
                    result.push(path);
                }
            }
        }
        result
    }

    #[test]
    fn compact_creates_canonical_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let record = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"nginx","message":"hello"}"#;
        let wal_files = vec![write_wal_file(&wal_dir, "nginx", &[record])];

        compact_service_blocking(&wal_files, &data_dir, "nginx").unwrap();

        // Should create {date}/{hour}/nginx.parquet (canonical name).
        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);
        assert_eq!(
            parquet_files[0].file_name().unwrap().to_str().unwrap(),
            "nginx.parquet",
            "expected canonical filename"
        );

        // No .tmp files left behind.
        let tmp_files = find_files_by_ext(&data_dir, "tmp");
        assert!(tmp_files.is_empty(), "stale .tmp files found");
    }

    #[test]
    fn compact_merges_with_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // First compaction: write initial data.
        let r1 = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"nginx","message":"first"}"#;
        let files1 = vec![write_wal_file(&wal_dir, "nginx", &[r1])];
        compact_service_blocking(&files1, &data_dir, "nginx").unwrap();

        // Second compaction: merge new data into existing file.
        let r2 = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"nginx","message":"second"}"#;
        let files2 = vec![write_wal_file(&wal_dir, "nginx", &[r2])];
        compact_service_blocking(&files2, &data_dir, "nginx").unwrap();

        // Still only one parquet file.
        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        // Verify merged file contains both rows.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "merged file should contain 2 rows");
    }

    #[test]
    fn cleanup_removes_stale_tmp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let hour_dir = data_dir.join("2026-01-01").join("00");
        std::fs::create_dir_all(&hour_dir).unwrap();

        // Create the "stale" file first, then sleep so it ages past the threshold.
        let stale = hour_dir.join("nginx.parquet.tmp");
        std::fs::write(&stale, b"stale").unwrap();

        std::thread::sleep(Duration::from_millis(50));

        // Create a fresh .tmp file that should NOT be removed.
        let fresh = hour_dir.join("postgres.parquet.tmp");
        std::fs::write(&fresh, b"fresh").unwrap();

        // Threshold between stale (50ms+ old) and fresh (~0ms old).
        cleanup_stale_tmp_files(&data_dir, Duration::from_millis(25));

        assert!(!stale.exists(), "stale .tmp should be removed");
        assert!(fresh.exists(), "fresh .tmp should be kept");
    }

    #[test]
    fn cleanup_removes_day_level_stale_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let day_dir = data_dir.join("2026-01-01");
        std::fs::create_dir_all(&day_dir).unwrap();

        // Day-level stale .tmp from interrupted rollup.
        let stale = day_dir.join("nginx.parquet.tmp");
        std::fs::write(&stale, b"stale").unwrap();

        std::thread::sleep(Duration::from_millis(50));

        cleanup_stale_tmp_files(&data_dir, Duration::from_millis(25));
        assert!(!stale.exists(), "day-level stale .tmp should be removed");
    }

    #[test]
    fn looks_like_date_valid() {
        assert!(looks_like_date("2026-02-13"));
        assert!(looks_like_date("2025-01-01"));
    }

    #[test]
    fn looks_like_date_invalid() {
        assert!(!looks_like_date("wal"));
        assert!(!looks_like_date("00"));
        assert!(!looks_like_date("2026-1-01"));
        assert!(!looks_like_date(""));
    }

    /// Helper: create a parquet file with the given rows in a specific hour-dir.
    fn write_hourly_parquet(
        data_dir: &Path,
        date: &str,
        hour: &str,
        service: &str,
        records: &[&str],
    ) -> PathBuf {
        let wal_dir = data_dir.join("_wal_tmp");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let wal_files: Vec<PathBuf> = records
            .iter()
            .map(|r| write_wal_file(&wal_dir, service, &[r]))
            .collect();

        // Use DuckDB directly to write parquet (simpler than going through compact).
        let hour_dir = data_dir.join(date).join(hour);
        std::fs::create_dir_all(&hour_dir).unwrap();
        let out = hour_dir.join(format!("{service}.parquet"));

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let file_list = wal_files
            .iter()
            .map(|p| format!("'{}'", p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM read_json_auto([{file_list}])) \
             TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY)",
            out.to_string_lossy(),
        ))
        .unwrap();

        // Clean up temp wal files.
        let _ = std::fs::remove_dir_all(&wal_dir);

        out
    }

    #[test]
    fn rollup_day_blocking_merges_hourly_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let r2 = r#"{"timestamp":"2026-01-15T02:30:00Z","service":"nginx","msg":"b"}"#;
        let r3 = r#"{"timestamp":"2026-01-15T02:45:00Z","service":"nginx","msg":"c"}"#;

        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let f2 = write_hourly_parquet(&data_dir, date, "02", "nginx", &[r2, r3]);

        let day_dir = data_dir.join(date);
        rollup_day_blocking(&day_dir, "nginx", &[f1.clone(), f2.clone()]).unwrap();

        // Day-level file should exist.
        let daily = day_dir.join("nginx.parquet");
        assert!(daily.exists(), "daily parquet should exist");

        // Hourly files should be deleted.
        assert!(!f1.exists(), "hourly file 1 should be deleted");
        assert!(!f2.exists(), "hourly file 2 should be deleted");

        // Verify row count.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3, "daily file should contain all 3 rows");
    }

    #[test]
    fn rollup_day_blocking_merges_with_existing_daily() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // First rollup: create initial daily file.
        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let day_dir = data_dir.join(date);
        rollup_day_blocking(&day_dir, "nginx", &[f1]).unwrap();

        // Late-arriving data creates a new hourly file.
        let r2 = r#"{"timestamp":"2026-01-15T03:00:00Z","service":"nginx","msg":"late"}"#;
        let f2 = write_hourly_parquet(&data_dir, date, "03", "nginx", &[r2]);

        // Second rollup: should merge existing daily + new hourly.
        rollup_day_blocking(&day_dir, "nginx", &[f2]).unwrap();

        let daily = day_dir.join("nginx.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "daily file should contain original + late row");
    }

    #[test]
    fn rollup_day_blocking_sorts_by_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // Write records in reverse order across hours.
        let r1 = r#"{"timestamp":"2026-01-15T23:00:00Z","service":"nginx","msg":"late"}"#;
        let r2 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"early"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "23", "nginx", &[r1]);
        let f2 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r2]);

        let day_dir = data_dir.join(date);
        rollup_day_blocking(&day_dir, "nginx", &[f1, f2]).unwrap();

        // Verify rows are sorted by timestamp (ascending).
        let daily = day_dir.join("nginx.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let first_msg: String = conn
            .query_row(
                &format!(
                    "SELECT msg FROM read_parquet('{}') ORDER BY \"timestamp\" LIMIT 1",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_msg, "early", "rows should be sorted by timestamp");
    }

    #[test]
    fn collect_hour_dirs_finds_valid_hours() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(day.join("00")).unwrap();
        std::fs::create_dir_all(day.join("14")).unwrap();
        std::fs::create_dir_all(day.join("23")).unwrap();
        // Not an hour directory.
        std::fs::write(day.join("nginx.parquet"), b"data").unwrap();

        let dirs = collect_hour_dirs(&day);
        assert_eq!(dirs.len(), 3, "should find 3 hour directories");
    }

    #[test]
    fn collect_service_files_groups_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let h00 = tmp.path().join("00");
        let h01 = tmp.path().join("01");
        std::fs::create_dir_all(&h00).unwrap();
        std::fs::create_dir_all(&h01).unwrap();
        std::fs::write(h00.join("nginx.parquet"), b"data").unwrap();
        std::fs::write(h00.join("postgres.parquet"), b"data").unwrap();
        std::fs::write(h01.join("nginx.parquet"), b"data").unwrap();

        let groups = collect_service_files(&[h00, h01]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["nginx"].len(), 2);
        assert_eq!(groups["postgres"].len(), 1);
    }

    #[tokio::test]
    async fn rollup_runs_without_wal_files() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Create hourly parquet files for a historical date (not today).
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let r1 = format!(r#"{{"timestamp":"{yesterday}T01:00:00Z","service":"nginx","msg":"a"}}"#);
        write_hourly_parquet(&data_dir, &yesterday, "01", "nginx", &[&r1]);

        // WAL dir is empty — compact_once should still run rollup.
        compact_once(&wal_dir, &data_dir, Duration::from_secs(1), true, None)
            .await
            .unwrap();

        // Day-level file should exist from rollup.
        let daily = data_dir.join(&yesterday).join("nginx.parquet");
        assert!(
            daily.exists(),
            "rollup should run even when WAL dir is empty"
        );
    }

    #[test]
    fn rollup_marker_written_and_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let marker = day_dir.join(".rollup-nginx");

        rollup_day_blocking(&day_dir, "nginx", &[f1]).unwrap();

        // Marker should be cleaned up after successful rollup.
        assert!(!marker.exists(), "marker should be removed after rollup");
        // Canonical file should exist.
        assert!(day_dir.join("nginx.parquet").exists());
    }

    #[test]
    fn rollup_recovery_after_rename_crash() {
        // Simulate crash after rename but before hourly deletion:
        // canonical exists, marker exists, hourly files still present.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        let marker = day_dir.join(".rollup-nginx");

        // Simulate: canonical was written (via rename), marker exists, hourly survives.
        std::fs::write(&canonical, b"consolidated data").unwrap();
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&f1)).unwrap();
        assert!(marker.exists());
        assert!(f1.exists());

        // Recovery should delete hourlies and marker without touching canonical.
        recover_rollup_markers(&day_dir).unwrap();

        assert!(canonical.exists(), "canonical should survive recovery");
        assert!(!f1.exists(), "hourly file should be deleted by recovery");
        assert!(!marker.exists(), "marker should be removed after recovery");
    }

    #[test]
    fn rollup_recovery_after_tmp_crash() {
        // Simulate crash after .tmp write but before rename:
        // .tmp exists, marker exists, no canonical yet.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        let tmp_file = day_dir.join("nginx.parquet.tmp");
        let marker = day_dir.join(".rollup-nginx");

        // Simulate: .tmp was written, marker exists, no canonical yet.
        std::fs::write(&tmp_file, b"merged data").unwrap();
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&f1)).unwrap();
        assert!(!canonical.exists());
        assert!(tmp_file.exists());

        // Recovery should rename .tmp to canonical, delete hourlies and marker.
        recover_rollup_markers(&day_dir).unwrap();

        assert!(canonical.exists(), "canonical should exist after recovery");
        assert!(!tmp_file.exists(), ".tmp should be gone after recovery");
        assert!(!f1.exists(), "hourly file should be deleted by recovery");
        assert!(!marker.exists(), "marker should be removed after recovery");
    }

    #[test]
    fn rollup_recovery_stale_marker() {
        // Stale marker: neither canonical nor .tmp exists.
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();

        let marker = day_dir.join(".rollup-nginx");
        std::fs::write(&marker, "/nonexistent/path.parquet").unwrap();

        recover_rollup_markers(&day_dir).unwrap();
        assert!(!marker.exists(), "stale marker should be removed");
    }
}
