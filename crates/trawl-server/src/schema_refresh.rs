// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-column statistics are read from parquet footers in safe Rust rather
//! than via `DuckDB`'s `parquet_metadata()` table function, which can
//! `SIGSEGV` on some files and take the whole daemon down. A bad file is
//! skipped for that pass and retried on the next one.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::task::JoinHandle;
use trawl_api::value::sort_by_display_rank;
use trawl_api::{DailyCount, ServiceColumnStats, ServiceSchema};
use trawl_engine::parquet_stats::{self, StatsAccumulator};

use crate::catalog::FieldCatalog;
use crate::state::{AppState, CachedServiceSchema, DegradedSnapshot};

pub fn spawn_schema_refresh(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let ttl = Duration::from_secs(state.query.schema_cache_ttl_secs);
        let mut interval = tokio::time::interval(ttl);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let warned: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
        let warned_unpinned: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let catalog = Arc::clone(&state.query.field_catalog);

        loop {
            interval.tick().await;

            let data_root = PathBuf::from(state.query.pool.base_dir());
            let warned = Arc::clone(&warned);
            let warned_unpinned = Arc::clone(&warned_unpinned);
            let catalog = Arc::clone(&catalog);
            let result = tokio::task::spawn_blocking(move || {
                refresh_service_schema(&data_root, &warned, &warned_unpinned, &catalog)
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
                    tracing::debug!(
                        event_type = "schema_refresh_error",
                        error = %e,
                        "service schema refresh failed"
                    );
                    tracing::warn!(
                        event_type = "schema_refresh_error",
                        cause_kind = crate::error::CauseKind::of_io(&e).as_str(),
                        "service schema refresh failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        event_type = "schema_refresh_panic",
                        error = %crate::error::join_failure_text("service schema refresh", e),
                        "service schema refresh task panicked"
                    );
                }
            }

            refresh_degraded_fields(&state).await;
        }
    })
}

/// Runs on every node, ingesting or not: a query-only node serves both the
/// notice and the badge.
///
/// A store error keeps the entire previous snapshot rather than clearing it.
/// An empty set on a postgres blip would silently un-badge every degraded
/// field on the install for as long as the blip lasts. The gauge is left
/// alone for the same reason.
#[allow(clippy::cast_precision_loss)] // gauge values are f64; the pin cap is exact
pub async fn refresh_degraded_fields(state: &AppState) {
    // Taken before the await: the schema cache is a sync mutex, and it is
    // the attribution read's service axis.
    let visible: Vec<String> = state
        .query
        .service_schema_cache
        .lock()
        .as_ref()
        .map(|cached| cached.services.iter().map(|s| s.name.clone()).collect())
        .unwrap_or_default();

    let (degraded, pairs) = match state.storage.catalog.degraded_snapshot(&visible).await {
        Ok(generation) => generation,
        Err(e) => {
            tracing::debug!(
                event_type = "degraded_refresh_error",
                error = %e,
                "degraded-field refresh failed; keeping the previous snapshot"
            );
            let fault = crate::error::ServerError::from(e);
            tracing::warn!(
                event_type = "degraded_refresh_error",
                error_class = fault.error_class(),
                cause_kind = fault.cause_kind().as_str(),
                "degraded-field refresh failed; keeping the previous snapshot"
            );
            return;
        }
    };

    metrics::gauge!(crate::metrics::CATALOG_DEGRADED_FIELDS).set(degraded.len() as f64);
    *state.query.degraded_fields.lock() = Arc::new(DegradedSnapshot::new(degraded, pairs));
}

struct FileInfo {
    path: PathBuf,
    size: u64,
    date: Option<String>,
}

fn refresh_service_schema(
    base: &Path,
    warned: &Mutex<HashSet<PathBuf>>,
    warned_unpinned: &Mutex<HashSet<String>>,
    catalog: &FieldCatalog,
) -> std::io::Result<Vec<ServiceSchema>> {
    if !base.is_dir() {
        return Ok(Vec::new());
    }

    let prev_warned = warned.lock().clone();
    let mut skipped_now: HashSet<PathBuf> = HashSet::new();

    let entries = crate::metrics::walk_parquet_files(base)?;

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
    let mut unpinned_now: HashSet<String> = HashSet::new();

    for (service, files) in &by_service {
        result.push(build_service_schema(
            service,
            files,
            &prev_warned,
            &mut skipped_now,
            &mut unpinned_now,
            catalog,
        ));
    }

    {
        let prev_unpinned = warned_unpinned.lock().clone();
        let newly: Vec<&String> = unpinned_now.difference(&prev_unpinned).collect();
        if !newly.is_empty() {
            tracing::warn!(
                event_type = "schema_refresh_unpinned_columns",
                columns = ?newly,
                "columns present in parquet but absent from the field catalog; \
                 reported as UNPINNED (foreign or boot-skipped parquet?)"
            );
        }
        *warned_unpinned.lock() = unpinned_now;
    }

    *warned.lock() = skipped_now;

    Ok(result)
}

fn build_service_schema(
    service: &str,
    files: &[FileInfo],
    prev_warned: &HashSet<PathBuf>,
    skipped_now: &mut HashSet<PathBuf>,
    unpinned_now: &mut HashSet<String>,
    catalog: &FieldCatalog,
) -> ServiceSchema {
    // A parquet file with no YYYY-MM-DD ancestor contributes to `total_events`
    // but not to any daily bucket.
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
                // A footer error quotes the parquet crate's text, so the
                // text goes to DEBUG; only an open failure has a typed cause.
                if !prev_warned.contains(&f.path) {
                    tracing::debug!(
                        event_type = "schema_refresh_file_skip",
                        file = %f.path.display(),
                        error = %e,
                        "unreadable parquet file; skipping this pass, will retry next refresh"
                    );
                    let (error_class, cause_kind) = match &e {
                        parquet_stats::ParquetStatsError::Open { source, .. } => {
                            ("io", crate::error::CauseKind::of_io(source))
                        }
                        _ => ("internal", crate::error::CauseKind::Unknown),
                    };
                    tracing::warn!(
                        event_type = "schema_refresh_file_skip",
                        file = %f.path.display(),
                        error_class,
                        cause_kind = cause_kind.as_str(),
                        "unreadable parquet file; skipping this pass, will retry next refresh"
                    );
                }
                skipped_now.insert(f.path.clone());
            }
        }
    }

    let total_events = acc.total_rows();
    let column_stats = acc.finish();

    let mut columns: Vec<ServiceColumnStats> = column_stats
        .iter()
        .map(|cs| {
            let data_type = catalog.get(&cs.column_name).map_or_else(
                || {
                    unpinned_now.insert(cs.column_name.clone());
                    "UNPINNED".to_owned()
                },
                |ty| ty.as_catalog().to_owned(),
            );
            ServiceColumnStats {
                name: cs.column_name.clone(),
                data_type,
                null_count: cs.null_count,
                total_count: cs.total_count,
                min_value: cs.min_value.clone(),
                max_value: cs.max_value.clone(),
                compressed_bytes: cs.compressed_bytes,
            }
        })
        .collect();
    sort_by_display_rank(&mut columns, |c| &c.name);

    let daily_event_counts: Vec<DailyCount> = daily_counts
        .into_iter()
        .map(|(date, count)| DailyCount { date, count })
        .collect();

    ServiceSchema {
        name: service.to_owned(),
        columns,
        earliest_date,
        latest_date,
        file_count,
        total_bytes,
        total_events,
        daily_event_counts,
        // Stamped per request from the degraded snapshot, not cached here:
        // the two caches have independent refresh points, and a badge frozen
        // into the schema cache would outlive a repin by up to a full TTL.
        degraded_fields: Vec::new(),
    }
}

fn find_date_ancestor(path: &Path) -> Option<String> {
    for ancestor in path.ancestors().skip(1) {
        if let Some(name) = ancestor.file_name().and_then(|n| n.to_str())
            && crate::env_dirs::parse_date(name).is_some()
        {
            return Some(name.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn empty_catalog() -> crate::catalog::FieldCatalog {
        crate::catalog::FieldCatalog::new()
    }

    /// The refresh reads the configured root as a path. A root whose name
    /// holds `*` would be cut short by any glob-based recovery and read
    /// nothing.
    #[test]
    fn configured_data_root_is_passed_directly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data[1]*");
        write_service_parquet(
            &root.join("prod"),
            "2026-06-20/00",
            "nginx",
            "SELECT * FROM (VALUES (200)) t(status)",
        );

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        for root in [root.clone(), root.join("")] {
            let services =
                refresh_service_schema(&root, &warned, &warned_unpinned, &empty_catalog()).unwrap();
            let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, ["nginx"], "{}", root.display());
        }
    }

    #[test]
    fn catalog_pin_wins_over_footer_type_while_stats_stay_footer_true() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "nginx",
            "SELECT * FROM (VALUES (200), (404)) t(status)",
        );

        let catalog = empty_catalog();
        catalog.merge([(
            "status".to_owned(),
            trawl_core::schema::CanonicalType::BigInt,
        )]);

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services = refresh_service_schema(base, &warned, &warned_unpinned, &catalog).unwrap();

        let nginx = services.iter().find(|s| s.name == "nginx").unwrap();
        let status = nginx.columns.iter().find(|c| c.name == "status").unwrap();
        assert_eq!(
            status.data_type, "BIGINT",
            "the catalog pin is the type authority, not the footer/physical type"
        );
        assert_eq!(status.total_count, 2, "stats stay filesystem-true");
        assert_eq!(status.min_value.as_deref(), Some("200"));
        assert_eq!(status.max_value.as_deref(), Some("404"));
        assert!(status.compressed_bytes > 0);
    }

    #[test]
    fn severity_pin_reports_the_catalog_spelling_not_the_physical_one() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "nginx",
            "SELECT * FROM (VALUES (17::BIGINT), (9::BIGINT)) t(_severity)",
        );

        let catalog = empty_catalog();
        catalog.merge([(
            trawl_core::schema::SEVERITY.to_owned(),
            trawl_core::schema::CanonicalType::Severity,
        )]);

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services = refresh_service_schema(base, &warned, &warned_unpinned, &catalog).unwrap();

        let nginx = services.iter().find(|s| s.name == "nginx").unwrap();
        let severity = nginx
            .columns
            .iter()
            .find(|c| c.name == trawl_core::schema::SEVERITY)
            .unwrap();
        assert_eq!(
            severity.data_type, "SEVERITY",
            "the semantic catalog spelling is what the endpoint reports"
        );
        assert_eq!(severity.total_count, 2, "stats stay filesystem-true");
    }

    #[test]
    fn unpinned_column_reports_unpinned_with_stats() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "nginx",
            "SELECT * FROM (VALUES (200)) t(status)",
        );

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(base, &warned, &warned_unpinned, &empty_catalog()).unwrap();

        let nginx = services.iter().find(|s| s.name == "nginx").unwrap();
        let status = nginx.columns.iter().find(|c| c.name == "status").unwrap();
        assert_eq!(status.data_type, "UNPINNED");
        assert_eq!(status.total_count, 1, "stats still populated");
        assert!(
            warned_unpinned.lock().contains("status"),
            "the unpinned sighting is recorded for warn dedup across passes"
        );
    }

    #[test]
    fn columns_follow_field_display_rank() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "nginx",
            "SELECT * FROM (VALUES (200, TIMESTAMP '2026-06-20 10:00:00', 'x'))
             t(zz_custom, _time, alpha)",
        );

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(base, &warned, &warned_unpinned, &empty_catalog()).unwrap();

        let names: Vec<&str> = services[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["_time", "alpha", "zz_custom"]);
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

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(base, &warned, &warned_unpinned, &empty_catalog()).unwrap();

        let nginx = services.iter().find(|s| s.name == "nginx").unwrap();
        assert_eq!(nginx.file_count, 2);
        assert_eq!(nginx.total_events, 5);
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
        let bad = base.join("2026-06-20").join("bad.parquet");
        std::fs::write(&bad, b"not parquet at all").unwrap();

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(base, &warned, &warned_unpinned, &empty_catalog()).unwrap();
        assert!(services.iter().any(|s| s.name == "good"));

        let bad_svc = services.iter().find(|s| s.name == "bad").unwrap();
        assert_eq!(bad_svc.total_events, 0);

        assert!(warned.lock().contains(&bad));

        assert!(!base.join(".trawl-schema-quarantine").exists());
        assert!(!base.join(".trawl-schema-refresh.inflight").exists());
    }

    #[test]
    fn recovered_file_is_picked_up_next_pass() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let svc = base.join("2026-06-20").join("svc.parquet");
        std::fs::create_dir_all(svc.parent().unwrap()).unwrap();
        std::fs::write(&svc, b"garbage, not parquet yet").unwrap();

        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let catalog = empty_catalog();

        let pass1 = refresh_service_schema(base, &warned, &warned_unpinned, &catalog).unwrap();
        let svc1 = pass1.iter().find(|s| s.name == "svc").unwrap();
        assert_eq!(svc1.total_events, 0);
        assert!(warned.lock().contains(&svc));

        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM (VALUES (1), (2)) t(n)) TO '{}' (FORMAT PARQUET)",
            svc.display()
        ))
        .unwrap();

        let pass2 = refresh_service_schema(base, &warned, &warned_unpinned, &catalog).unwrap();
        let svc2 = pass2.iter().find(|s| s.name == "svc").unwrap();
        assert_eq!(svc2.total_events, 2);
        assert!(!warned.lock().contains(&svc));
    }
}
