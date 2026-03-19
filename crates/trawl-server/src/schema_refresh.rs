// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background schema refresh job.
//!
//! Pre-computes per-service schema metadata from parquet file footers on a
//! periodic interval. The `/api/v1/schema/services` endpoint is a pure cache
//! read — no query execution or file I/O on the request path.
//!
//! Follows the same pattern as [`crate::monitor::spawn_snapshot_collector`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use trawl_api::{DailyCount, ServiceColumnStats, ServiceSchema};
use trawl_engine::executor::Executor;

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
    size: u64,
    date: Option<String>,
}

/// Perform the full service schema refresh.
///
/// Walks parquet files, groups by service, then for each service queries
/// `DuckDB`'s parquet metadata functions for column stats and row counts.
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
            .push(FileInfo { size, date });
    }

    let mut result = Vec::with_capacity(by_service.len());

    for (service, files) in &by_service {
        let schema = build_service_schema(service, files, base)?;
        result.push(schema);
    }

    Ok(result)
}

/// Build schema for a single service from its file list.
fn build_service_schema(
    service: &str,
    files: &[FileInfo],
    base: &Path,
) -> Result<ServiceSchema, Box<dyn std::error::Error + Send + Sync>> {
    // Aggregate file-level metadata.
    let mut dates: BTreeSet<String> = BTreeSet::new();
    let mut total_bytes: u64 = 0;
    let mut daily_counts: BTreeMap<String, u64> = BTreeMap::new();

    for f in files {
        total_bytes += f.size;
        if let Some(ref d) = f.date {
            dates.insert(d.clone());
            // We'll fill daily_counts from parquet metadata below if possible,
            // but track dates from the directory structure.
            daily_counts.entry(d.clone()).or_insert(0);
        }
    }

    let file_count = u64::try_from(files.len()).unwrap_or(0);
    let earliest_date = dates.iter().next().cloned();
    let latest_date = dates.iter().next_back().cloned();

    // Build a service-scoped glob for DuckDB queries.
    let service_glob = format!("{}/**/{}.parquet", base.to_string_lossy(), service);

    // Query parquet metadata for column stats + row counts.
    let executor = Executor::new()?;

    let column_stats = executor
        .parquet_column_stats(&service_glob)
        .unwrap_or_default();

    // Get schema columns (names + types) via DESCRIBE.
    let schema_result = executor.describe_schema(&service_glob);
    let schema_columns = schema_result.as_ref().map_or(&[][..], |r| &r.columns);

    // Row counts per file for daily aggregation — use total from column stats
    // (cheaper than a separate call if we already have it).
    let total_events = executor.parquet_row_counts(&service_glob).unwrap_or(0);

    // Build per-day event counts from parquet_file_metadata if we have dates.
    // For simplicity, distribute total_events proportionally by file size per date.
    if total_events > 0 && !daily_counts.is_empty() {
        let mut bytes_by_date: BTreeMap<String, u64> = BTreeMap::new();
        for f in files {
            if let Some(ref d) = f.date {
                *bytes_by_date.entry(d.clone()).or_insert(0) += f.size;
            }
        }
        let total = total_bytes.max(1);
        for (date, bytes) in &bytes_by_date {
            let estimated =
                u64::try_from(u128::from(total_events) * u128::from(*bytes) / u128::from(total))
                    .unwrap_or(u64::MAX);
            daily_counts.insert(date.clone(), estimated);
        }
    }

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
