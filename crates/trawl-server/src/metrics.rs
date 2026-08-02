// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Prometheus metrics: metric name constants, descriptions, and gauge collection.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use metrics::{describe_counter, describe_gauge, describe_histogram};

use crate::hot_buffer::HotBuffer;

// -- metric name constants ---------------------------------------------------

pub const QUERIES_TOTAL: &str = "trawl_queries_total";
pub const QUERY_DURATION: &str = "trawl_query_duration_seconds";
pub const INGEST_EVENTS_TOTAL: &str = "trawl_ingest_events_total";
pub const INGEST_EVENTS_REJECTED_TOTAL: &str = "trawl_ingest_events_rejected_total";
pub const INGEST_REPAIRS_TOTAL: &str = "trawl_ingest_repairs_total";
pub const HOT_BUFFER_EVENTS: &str = "trawl_hot_buffer_events";
pub const HOT_BUFFER_BYTES: &str = "trawl_hot_buffer_bytes";
pub const ACTIVE_CONNECTIONS: &str = "trawl_active_connections";
pub const PARQUET_FILES: &str = "trawl_parquet_files_total";
pub const PARQUET_BYTES: &str = "trawl_parquet_size_bytes";
pub const HEALTH_CHECK: &str = "trawl_health_check";
pub const SYSLOG_EVENTS_TOTAL: &str = "trawl_syslog_events_total";
pub const SYSLOG_PARSE_ERRORS_TOTAL: &str = "trawl_syslog_parse_errors_total";
pub const SYSLOG_EVENTS_DROPPED_TOTAL: &str = "trawl_syslog_events_dropped_total";
pub const SYSLOG_TCP_CONNECTIONS: &str = "trawl_syslog_tcp_connections";
pub const WAL_FILES: &str = "trawl_wal_files";
pub const WAL_BYTES: &str = "trawl_wal_bytes";

// -- description registration ------------------------------------------------

/// Register metric descriptions (help text + units). Call once at startup.
pub fn describe_metrics() {
    describe_counter!(QUERIES_TOTAL, "Total number of queries executed");
    describe_histogram!(QUERY_DURATION, "Query execution duration in seconds");
    describe_counter!(INGEST_EVENTS_TOTAL, "Total number of ingested events");
    describe_counter!(
        INGEST_EVENTS_REJECTED_TOTAL,
        "Total number of rejected ingest events"
    );
    describe_counter!(
        INGEST_REPAIRS_TOTAL,
        "Repairs applied to accepted ingest events, labelled by repair code \
         and service (codes also recorded per-event in _repairs)"
    );
    describe_gauge!(
        HOT_BUFFER_EVENTS,
        "Current number of events in the hot buffer"
    );
    describe_gauge!(HOT_BUFFER_BYTES, "Current byte size of the hot buffer");
    describe_gauge!(ACTIVE_CONNECTIONS, "Number of in-flight HTTP requests");
    describe_gauge!(PARQUET_FILES, "Total number of parquet data files");
    describe_gauge!(PARQUET_BYTES, "Total byte size of parquet data files");
    describe_gauge!(
        HEALTH_CHECK,
        "Subsystem health (1 = ok, 0 = failed), labeled by subsystem"
    );
    describe_counter!(
        SYSLOG_EVENTS_TOTAL,
        "Total events ingested via syslog listener"
    );
    describe_counter!(
        SYSLOG_PARSE_ERRORS_TOTAL,
        "Total unparseable syslog messages"
    );
    describe_counter!(
        SYSLOG_EVENTS_DROPPED_TOTAL,
        "Syslog events dropped due to backpressure"
    );
    describe_gauge!(
        SYSLOG_TCP_CONNECTIONS,
        "Current active syslog TCP connections"
    );
    describe_gauge!(WAL_FILES, "Number of pending WAL (ndjson) files");
    describe_gauge!(WAL_BYTES, "Total byte size of pending WAL files");
}

// -- gauge collection --------------------------------------------------------

/// TTL for the parquet stats cache. At most one filesystem walk per this interval,
/// regardless of scrape frequency or stats emitter cadence.
const PARQUET_CACHE_TTL_SECS: u64 = 30;

/// Cached parquet file statistics to avoid repeated filesystem walks.
struct CachedParquetStats {
    file_count: u64,
    total_bytes: u64,
    /// `None` means never cached — first call always triggers a walk.
    last_updated: Option<Instant>,
}

/// Module-level cache for parquet gauge values.
fn parquet_cache() -> &'static Mutex<CachedParquetStats> {
    static CACHE: OnceLock<Mutex<CachedParquetStats>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(CachedParquetStats {
            file_count: 0,
            total_bytes: 0,
            last_updated: None,
        })
    })
}

/// Update gauges that require periodic polling (hot buffer + parquet + WAL files).
///
/// Cheap enough to call on every prometheus scrape and in the stats emitter.
#[allow(clippy::cast_precision_loss)] // gauge values are f64; precision loss beyond 2^52 is fine
pub fn collect_gauges(
    hot_buffer: Option<&Arc<HotBuffer>>,
    fallback_glob: &str,
    wal_dir: Option<&Path>,
) {
    // Hot buffer gauges.
    if let Some(buf) = hot_buffer {
        metrics::gauge!(HOT_BUFFER_EVENTS).set(buf.event_count() as f64);
        metrics::gauge!(HOT_BUFFER_BYTES).set(buf.byte_count() as f64);
    }

    // Parquet file gauges — walk the glob pattern's parent directory.
    collect_parquet_gauges(fallback_glob);

    // WAL file gauges.
    if let Some(dir) = wal_dir {
        collect_wal_gauges(dir);
    }
}

/// Scan parquet files on disk and update the file count / byte size gauges.
///
/// Uses a 30s TTL cache to avoid repeated filesystem walks. If the cache is
/// fresh, sets gauges from cached values and returns immediately.
#[allow(clippy::cast_precision_loss)]
fn collect_parquet_gauges(fallback_glob: &str) {
    let cache = parquet_cache();

    // Fast path: serve from cache if fresh.
    {
        let cached = cache.lock().expect("parquet cache poisoned");
        let is_fresh = cached
            .last_updated
            .is_some_and(|t| t.elapsed().as_secs() < PARQUET_CACHE_TTL_SECS);
        if is_fresh {
            metrics::gauge!(PARQUET_FILES).set(cached.file_count as f64);
            metrics::gauge!(PARQUET_BYTES).set(cached.total_bytes as f64);
            return;
        }
    }

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

    // Update cache after setting gauges.
    let mut cached = cache.lock().expect("parquet cache poisoned");
    cached.file_count = file_count;
    cached.total_bytes = total_bytes;
    cached.last_updated = Some(Instant::now());
}

/// Recursively walk a directory collecting `.parquet` file paths and sizes.
pub(crate) fn walk_parquet_files(dir: &Path) -> std::io::Result<Vec<(std::path::PathBuf, u64)>> {
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
            // Skip `scheduled/` — contains saved query result parquet, not ingested logs.
            if entry.file_name() == "scheduled" {
                continue;
            }
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

// -- WAL gauge collection ----------------------------------------------------

/// TTL for the WAL stats cache (same cadence as parquet).
const WAL_CACHE_TTL_SECS: u64 = 30;

/// Cached WAL file statistics.
struct CachedWalStats {
    file_count: u64,
    total_bytes: u64,
    last_updated: Option<Instant>,
}

/// Module-level cache for WAL gauge values.
fn wal_cache() -> &'static Mutex<CachedWalStats> {
    static CACHE: OnceLock<Mutex<CachedWalStats>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(CachedWalStats {
            file_count: 0,
            total_bytes: 0,
            last_updated: None,
        })
    })
}

/// Scan WAL directory for `.ndjson` files and update gauge metrics.
///
/// Uses a 30s TTL cache to avoid repeated directory scans.
#[allow(clippy::cast_precision_loss)]
fn collect_wal_gauges(wal_dir: &Path) {
    let cache = wal_cache();

    // Fast path: serve from cache if fresh.
    {
        let cached = cache.lock().expect("wal cache poisoned");
        let is_fresh = cached
            .last_updated
            .is_some_and(|t| t.elapsed().as_secs() < WAL_CACHE_TTL_SECS);
        if is_fresh {
            metrics::gauge!(WAL_FILES).set(cached.file_count as f64);
            metrics::gauge!(WAL_BYTES).set(cached.total_bytes as f64);
            return;
        }
    }

    if !wal_dir.is_dir() {
        return;
    }

    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    if let Ok(entries) = std::fs::read_dir(wal_dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "ndjson") {
                file_count += 1;
                if let Ok(meta) = entry.metadata() {
                    total_bytes += meta.len();
                }
            }
        }
    }

    metrics::gauge!(WAL_FILES).set(file_count as f64);
    metrics::gauge!(WAL_BYTES).set(total_bytes as f64);

    let mut cached = cache.lock().expect("wal cache poisoned");
    cached.file_count = file_count;
    cached.total_bytes = total_bytes;
    cached.last_updated = Some(Instant::now());
}

// -- public cache accessors (for dashboard monitor) --------------------------

/// Read the cached WAL file stats. Returns `(file_count, total_bytes)`.
///
/// Returns `(0, 0)` if the cache has never been populated (no prometheus
/// scrape or stats-emit has run yet).
pub fn cached_wal_stats() -> (u64, u64) {
    let cached = wal_cache().lock().expect("wal cache poisoned");
    (cached.file_count, cached.total_bytes)
}

/// Read the cached parquet file stats. Returns `(file_count, total_bytes)`.
pub fn cached_parquet_stats() -> (u64, u64) {
    let cached = parquet_cache().lock().expect("parquet cache poisoned");
    (cached.file_count, cached.total_bytes)
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
        collect_gauges(None, "/nonexistent/path/**/*.parquet", None);
    }
}
