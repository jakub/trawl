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
            wal_dir = %wal_dir.display(),
            data_dir = %data_dir.display(),
            interval_secs = interval.as_secs(),
            "compaction task started"
        );

        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    if let Err(e) = compact_once(&wal_dir, &data_dir, interval).await {
                        tracing::error!(error = %e, "compaction tick failed");
                    }
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!("compaction task shutting down");
                    break;
                }
            }
        }
    })
}

/// Run one compaction cycle.
async fn compact_once(wal_dir: &Path, data_dir: &Path, min_age: Duration) -> Result<(), String> {
    let files = scan_wal_files(wal_dir, min_age).map_err(|e| format!("scan failed: {e}"))?;

    if files.is_empty() {
        return Ok(());
    }

    // Group WAL files by service prefix.
    let groups = group_by_service(files);

    for (service, wal_files) in &groups {
        tracing::debug!(
            service = %service,
            files = wal_files.len(),
            "compacting service batch"
        );

        match compact_service_batch(wal_files, data_dir, service).await {
            Ok(()) => {
                // Clean up consumed WAL files.
                for f in wal_files {
                    if let Err(e) = std::fs::remove_file(f) {
                        tracing::warn!(
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
                    service = %service,
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
fn compact_service_blocking(
    wal_files: &[PathBuf],
    data_dir: &Path,
    service: &str,
) -> Result<(), String> {
    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;

    // Build file list for read_json_auto.
    let file_list: Vec<String> = wal_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let file_list_sql = file_list
        .iter()
        .map(|f| format!("'{f}'"))
        .collect::<Vec<_>>()
        .join(", ");

    // Read all WAL files into a temp table.
    conn.execute_batch(&format!(
        "CREATE TABLE wal_batch AS SELECT * FROM read_json_auto([{file_list_sql}])"
    ))
    .map_err(|e| format!("read_json_auto failed: {e}"))?;

    // Determine output path from current time.
    let now = chrono::Utc::now();
    let day_part = now.format("%Y-%m-%d").to_string();
    let hour_part = now.format("%H").to_string();
    let output_dir = data_dir.join(&day_part).join(&hour_part);

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("failed to create output dir: {e}"))?;

    let output_path = output_dir.join(format!("{service}.parquet"));

    // Write parquet. If the file already exists, append to it.
    let mode = if output_path.exists() {
        ", FILE_SIZE_BYTES '500MB', OVERWRITE true"
    } else {
        ""
    };

    conn.execute_batch(&format!(
        "COPY wal_batch TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY{})",
        output_path.display(),
        mode
    ))
    .map_err(|e| format!("COPY TO parquet failed: {e}"))?;

    conn.execute_batch("DROP TABLE IF EXISTS wal_batch")
        .map_err(|e| format!("DROP TABLE failed: {e}"))?;

    tracing::info!(
        service = %service,
        output = %output_path.display(),
        wal_files = wal_files.len(),
        "compaction complete"
    );

    Ok(())
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
}
