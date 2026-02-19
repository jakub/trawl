//! Prometheus metrics: metric name constants, descriptions, and gauge collection.

use std::path::Path;
use std::sync::Arc;

use metrics::{describe_counter, describe_gauge, describe_histogram};

use crate::hot_buffer::HotBuffer;

// -- metric name constants ---------------------------------------------------

pub const QUERIES_TOTAL: &str = "fleet_queries_total";
pub const QUERY_DURATION: &str = "fleet_query_duration_seconds";
pub const INGEST_EVENTS_TOTAL: &str = "fleet_ingest_events_total";
pub const HOT_BUFFER_EVENTS: &str = "fleet_hot_buffer_events";
pub const HOT_BUFFER_BYTES: &str = "fleet_hot_buffer_bytes";
pub const ACTIVE_CONNECTIONS: &str = "fleet_active_connections";
pub const PARQUET_FILES: &str = "fleet_parquet_files_total";
pub const PARQUET_BYTES: &str = "fleet_parquet_size_bytes";

// -- description registration ------------------------------------------------

/// Register metric descriptions (help text + units). Call once at startup.
pub fn describe_metrics() {
    describe_counter!(QUERIES_TOTAL, "Total number of queries executed");
    describe_histogram!(QUERY_DURATION, "Query execution duration in seconds");
    describe_counter!(INGEST_EVENTS_TOTAL, "Total number of ingested events");
    describe_gauge!(
        HOT_BUFFER_EVENTS,
        "Current number of events in the hot buffer"
    );
    describe_gauge!(HOT_BUFFER_BYTES, "Current byte size of the hot buffer");
    describe_gauge!(ACTIVE_CONNECTIONS, "Number of in-flight HTTP requests");
    describe_gauge!(PARQUET_FILES, "Total number of parquet data files");
    describe_gauge!(PARQUET_BYTES, "Total byte size of parquet data files");
}

// -- gauge collection --------------------------------------------------------

/// Update gauges that require periodic polling (hot buffer + parquet files).
///
/// Cheap enough to call on every prometheus scrape and in the stats emitter.
#[allow(clippy::cast_precision_loss)] // gauge values are f64; precision loss beyond 2^52 is fine
pub fn collect_gauges(hot_buffer: Option<&Arc<HotBuffer>>, fallback_glob: &str) {
    // Hot buffer gauges.
    if let Some(buf) = hot_buffer {
        metrics::gauge!(HOT_BUFFER_EVENTS).set(buf.event_count() as f64);
        metrics::gauge!(HOT_BUFFER_BYTES).set(buf.byte_count() as f64);
    }

    // Parquet file gauges — walk the glob pattern's parent directory.
    collect_parquet_gauges(fallback_glob);
}

/// Scan parquet files on disk and update the file count / byte size gauges.
#[allow(clippy::cast_precision_loss)]
fn collect_parquet_gauges(fallback_glob: &str) {
    // The fallback glob looks like "/path/to/data/**/*.parquet". Extract the
    // base directory (everything before the first glob wildcard).
    let base = fallback_glob
        .find('*')
        .map_or(fallback_glob, |pos| &fallback_glob[..pos]);
    let base = Path::new(base.trim_end_matches('/'));

    if !base.is_dir() {
        return;
    }

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    if let Ok(entries) = walk_parquet_files(base) {
        for (_, size) in entries {
            file_count += 1;
            total_bytes += size;
        }
    }

    metrics::gauge!(PARQUET_FILES).set(file_count as f64);
    metrics::gauge!(PARQUET_BYTES).set(total_bytes as f64);
}

/// Recursively walk a directory collecting `.parquet` file paths and sizes.
fn walk_parquet_files(dir: &Path) -> std::io::Result<Vec<(std::path::PathBuf, u64)>> {
    let mut results = Vec::new();
    walk_dir_recursive(dir, &mut results)?;
    Ok(results)
}

fn walk_dir_recursive(
    dir: &Path,
    results: &mut Vec<(std::path::PathBuf, u64)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_dir_recursive(&entry.path(), results)?;
        } else if ft.is_file() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "parquet") {
                let size = entry.metadata()?.len();
                results.push((path, size));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_metrics_does_not_panic() {
        // Install a test recorder so describe calls succeed.
        let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
        let _handle = builder.install_recorder().expect("install test recorder");
        describe_metrics();
    }

    #[test]
    fn collect_gauges_no_hot_buffer_no_panic() {
        // With no recorder installed and no hot buffer, should be a no-op.
        collect_gauges(None, "/nonexistent/path/**/*.parquet");
    }
}
