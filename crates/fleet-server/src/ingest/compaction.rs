//! Background WAL → parquet compaction task.
//!
//! Periodically scans the WAL directory for `.ndjson` files, groups
//! them by service, and uses `DuckDB` to convert each batch to parquet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

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
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            event_type = "lifecycle",
            action = "compaction_start",
            wal_dir = %wal_dir.display(),
            data_dir = %data_dir.display(),
            interval_secs = interval.as_secs(),
            "compaction task started"
        );

        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    if let Err(e) = compact_once(&wal_dir, &data_dir, interval).await {
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
async fn compact_once(wal_dir: &Path, data_dir: &Path, min_age: Duration) -> Result<(), String> {
    // Remove orphaned .parquet.tmp files from interrupted compaction runs.
    cleanup_stale_tmp_files(data_dir, min_age * 2);

    let files = scan_wal_files(wal_dir, min_age).map_err(|e| format!("scan failed: {e}"))?;

    if files.is_empty() {
        return Ok(());
    }

    // Group WAL files by service prefix.
    let groups = group_by_service(files);

    for (service, wal_files) in &groups {
        tracing::debug!(
            event_type = "compaction_start",
            compact_service = %service,
            wal_files = wal_files.len(),
            "compacting service batch"
        );

        match compact_service_batch(wal_files, data_dir, service).await {
            Ok(()) => {
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

    if merged {
        // Merge: union existing parquet rows with new WAL batch.
        conn.execute_batch(&format!(
            "CREATE TABLE merged AS \
             SELECT * FROM read_parquet('{}') \
             UNION ALL \
             SELECT * FROM wal_batch",
            canonical_path.display(),
        ))
        .map_err(|e| format!("merge read_parquet failed: {e}"))?;

        conn.execute_batch(&format!(
            "COPY merged TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY)",
            tmp_path.display(),
        ))
        .map_err(|e| format!("COPY TO parquet failed: {e}"))?;

        conn.execute_batch("DROP TABLE IF EXISTS merged")
            .map_err(|e| format!("DROP TABLE failed: {e}"))?;
    } else {
        // Fresh write: no existing file to merge with.
        conn.execute_batch(&format!(
            "COPY wal_batch TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY)",
            tmp_path.display(),
        ))
        .map_err(|e| format!("COPY TO parquet failed: {e}"))?;
    }

    conn.execute_batch("DROP TABLE IF EXISTS wal_batch")
        .map_err(|e| format!("DROP TABLE failed: {e}"))?;

    // Atomic rename: crash-safe swap. POSIX rename() is atomic, so
    // concurrent readers on the old inode finish normally while new
    // readers get the merged file.
    std::fs::rename(&tmp_path, &canonical_path)
        .map_err(|e| format!("atomic rename failed: {e}"))?;

    tracing::info!(
        event_type = "compaction_complete",
        compact_service = %service,
        output = %canonical_path.display(),
        wal_files = wal_files.len(),
        merged,
        "compaction complete"
    );

    Ok(())
}

/// Remove stale `.parquet.tmp` files left by interrupted compaction runs.
///
/// Walks `data_dir/{date}/{hour}/` looking for `.tmp` files older than
/// `max_age`. These are inert (don't match `*.parquet` globs) but should
/// be cleaned up to avoid disk waste.
fn cleanup_stale_tmp_files(data_dir: &Path, max_age: Duration) {
    let Ok(days) = std::fs::read_dir(data_dir) else {
        return;
    };
    for day_entry in days.flatten() {
        let Ok(hours) = std::fs::read_dir(day_entry.path()) else {
            continue;
        };
        for hour_entry in hours.flatten() {
            let Ok(files) = std::fs::read_dir(hour_entry.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().is_some_and(|ext| ext == "tmp") {
                    if let Ok(meta) = file.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            if SystemTime::now().duration_since(mtime).unwrap_or_default() > max_age
                            {
                                let _ = std::fs::remove_file(&path);
                                tracing::debug!(path = %path.display(), "removed stale tmp file");
                            }
                        }
                    }
                }
            }
        }
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

        if path.extension().is_some_and(|ext| ext == "ndjson") {
            let metadata = entry.metadata()?;
            if let Ok(mtime) = metadata.modified() {
                if now.duration_since(mtime).unwrap_or_default() > min_age {
                    files.push(path);
                }
            }
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
}
