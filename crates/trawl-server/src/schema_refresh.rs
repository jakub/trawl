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
//! The same tick also reloads the degraded-field snapshot the query path
//! stamps its incomplete-results notice from and `/schema/services` stamps
//! its per-service badge from (ADR-0011 slices C1/C2) — the ONE part of
//! this job that reads postgres, and the reason the "footer stats need
//! neither postgres nor `DuckDB`" claim above is about the SCHEMA half
//! only. That half is a single query on a healthy install; the attribution
//! read behind it runs only once something is actually degraded.
//!
//! Follows the same pattern as [`crate::monitor::spawn_snapshot_collector`].

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
        // refresh closure; lives for the process. `warned_unpinned` plays the
        // same role for columns with no catalog pin.
        let warned: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
        let warned_unpinned: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        // Types come from the in-process pin cache — the schema half needs
        // neither postgres nor DuckDB (ADR-0009 slice 3).
        let catalog = Arc::clone(&state.query.field_catalog);

        loop {
            interval.tick().await;

            let fallback_glob = state.query.pool.fallback_glob().to_owned();
            let warned = Arc::clone(&warned);
            let warned_unpinned = Arc::clone(&warned_unpinned);
            let catalog = Arc::clone(&catalog);
            let result = tokio::task::spawn_blocking(move || {
                refresh_service_schema(&fallback_glob, &warned, &warned_unpinned, &catalog)
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

            refresh_degraded_fields(&state).await;
        }
    })
}

/// Reload the degraded-field snapshot and publish the gauge (ADR-0011
/// slices C1/C2).
///
/// Runs on every node, ingesting or not: the notice is stamped by the query
/// path and the badge by `/schema/services`, and a query-only node serves
/// both.
///
/// Two reads, one generation. The aggregate read is unconditional and is
/// the whole cost on a healthy install — the degraded set is empty, so the
/// second read short-circuits to zero queries and the tick's postgres cost
/// is exactly the one query C1 shipped. Only when something IS degraded does
/// the pair read run, keyed on that (pin-capped) set AND on the service
/// names the schema cache can currently render, so neither axis can widen it
/// into a scan of a table whose service axis is client-chosen. A tick whose
/// service cache has not filled yet (boot) skips the pair read and publishes
/// an unattributed generation — one tick of missing badges, accepted.
///
/// A store error on EITHER read keeps the ENTIRE previous snapshot rather
/// than clearing it, and never publishes a half-built one. The alternative
/// — an empty set on a postgres blip — silently un-badges every degraded
/// field on the install for as long as the blip lasts, which is the one
/// failure mode a notice must not have; and a snapshot with fields but no
/// pairs is strictly worse than stale, since it would un-badge every
/// service while leaving the notice standing. The gauge is left alone for
/// the same reason: it would otherwise read as a fixed catalog.
#[allow(clippy::cast_precision_loss)] // gauge values are f64; the pin cap is exact
pub async fn refresh_degraded_fields(state: &AppState) {
    let aggregates = match state.storage.catalog.conflict_aggregates(None).await {
        Ok(aggregates) => aggregates,
        Err(e) => {
            tracing::warn!(
                event_type = "degraded_refresh_error",
                error = %e,
                "degraded-field refresh failed; keeping the previous snapshot"
            );
            return;
        }
    };

    let degraded: BTreeSet<String> = aggregates
        .iter()
        .filter(|a| crate::catalog::analyzer::is_degraded(a))
        .map(|a| a.field.clone())
        .collect();

    // Attribution: which senders' data actually indicted each pin. Keyed on
    // both axes, and skipped entirely when either is empty.
    let pairs = if degraded.is_empty() {
        Vec::new()
    } else {
        let names: Vec<String> = degraded.iter().cloned().collect();
        let visible: Vec<String> = state
            .query
            .service_schema_cache
            .lock()
            .as_ref()
            .map(|cached| cached.services.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        match state
            .storage
            .catalog
            .conflict_service_pairs(&names, &visible)
            .await
        {
            Ok(pairs) => pairs,
            Err(e) => {
                tracing::warn!(
                    event_type = "degraded_refresh_error",
                    error = %e,
                    "degraded-field service attribution failed; keeping the previous snapshot"
                );
                return;
            }
        }
    };

    metrics::gauge!(crate::metrics::CATALOG_DEGRADED_FIELDS).set(degraded.len() as f64);
    *state.query.degraded_fields.lock() = Arc::new(DegradedSnapshot::new(degraded, pairs));
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
/// footers for column stats and row counts; column TYPES come from `catalog`
/// (the in-process pin cache — no `DuckDB`, no postgres). `warned` carries the
/// set of files that failed to read on the previous pass so each break is
/// logged once; `warned_unpinned` does the same for columns with no pin.
fn refresh_service_schema(
    fallback_glob: &str,
    warned: &Mutex<HashSet<PathBuf>>,
    warned_unpinned: &Mutex<HashSet<String>>,
    catalog: &FieldCatalog,
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

    // Warn once per newly-unpinned column (deduped across passes, like the
    // unreadable-file skips): an UNPINNED column can only arise from
    // foreign/boot-skipped parquet — the condition `catalog_conform_skip` /
    // `catalog_conform_incomplete` already flags — or transiently during the
    // boot-conformance window, where it self-heals next tick.
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

    // Remember this pass's skips: files that recovered drop out of the set, so a
    // future re-break is logged again.
    *warned.lock() = skipped_now;

    Ok(result)
}

/// Build schema for a single service from its file list.
///
/// `prev_warned` is the read-only set of files already logged as broken;
/// `skipped_now` accumulates the files skipped this pass; `unpinned_now`
/// accumulates columns seen without a catalog pin.
fn build_service_schema(
    service: &str,
    files: &[FileInfo],
    prev_warned: &HashSet<PathBuf>,
    skipped_now: &mut HashSet<PathBuf>,
    unpinned_now: &mut HashSet<String>,
    catalog: &FieldCatalog,
) -> ServiceSchema {
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

    // Columns are driven by the footer stats accumulator (the union of every
    // column name across the service's files) and TYPED by the catalog pin —
    // the DuckDB DESCRIBE reconciler is gone (ADR-0009 slice 3). A
    // physically-present column with no pin reports the UNPINNED sentinel:
    // it can only arise from foreign or boot-skipped parquet.
    let mut columns: Vec<ServiceColumnStats> = column_stats
        .iter()
        .map(|cs| {
            let data_type = catalog.get(&cs.column_name).map_or_else(
                || {
                    unpinned_now.insert(cs.column_name.clone());
                    "UNPINNED".to_owned()
                },
                |ty| ty.as_duckdb().to_owned(),
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
        // Stamped per REQUEST from the degraded snapshot, not cached here:
        // the two caches have independent refresh points, and a badge frozen
        // into the schema cache would outlive a repin by up to a full TTL.
        degraded_fields: Vec::new(),
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

    fn empty_catalog() -> crate::catalog::FieldCatalog {
        crate::catalog::FieldCatalog::new()
    }

    #[test]
    fn catalog_pin_wins_over_footer_type_while_stats_stay_footer_true() {
        // The #51 acceptance criterion: seed a catalog pin that disagrees
        // with the file's physical type — DuckDB writes `VALUES (200)` as
        // INTEGER, the pin says BIGINT — and the endpoint must report the
        // catalog type while null/min/max/byte stats still match the
        // footers. No DuckDB DESCRIBE runs at all.
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

        let glob = format!("{}/**/*.parquet", base.display());
        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services = refresh_service_schema(&glob, &warned, &warned_unpinned, &catalog).unwrap();

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
    fn unpinned_column_reports_unpinned_with_stats() {
        // A physically-present column with no pin can only arise from
        // foreign/boot-skipped parquet (catalog_conform_incomplete already
        // flags that condition). It reports the sentinel type, keeps its
        // stats, and is warn-logged once — not silently dropped.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "nginx",
            "SELECT * FROM (VALUES (200)) t(status)",
        );

        let glob = format!("{}/**/*.parquet", base.display());
        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(&glob, &warned, &warned_unpinned, &empty_catalog()).unwrap();

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
        // Envelope columns lead, custom columns follow alphabetically —
        // mirrors query-result ordering (previously this was DESCRIBE's
        // physical order).
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        write_service_parquet(
            base,
            "2026-06-20",
            "nginx",
            "SELECT * FROM (VALUES (200, TIMESTAMP '2026-06-20 10:00:00', 'x'))
             t(zz_custom, _time, alpha)",
        );

        let glob = format!("{}/**/*.parquet", base.display());
        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(&glob, &warned, &warned_unpinned, &empty_catalog()).unwrap();

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

        let glob = format!("{}/**/*.parquet", base.display());
        let warned = Mutex::new(HashSet::new());
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(&glob, &warned, &warned_unpinned, &empty_catalog()).unwrap();

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
        let warned_unpinned = Mutex::new(HashSet::new());
        let services =
            refresh_service_schema(&glob, &warned, &warned_unpinned, &empty_catalog()).unwrap();
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
        let warned_unpinned = Mutex::new(HashSet::new());
        let catalog = empty_catalog();

        // Pass 1: file is garbage → skipped, zero events, recorded in warned.
        let pass1 = refresh_service_schema(&glob, &warned, &warned_unpinned, &catalog).unwrap();
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
        let pass2 = refresh_service_schema(&glob, &warned, &warned_unpinned, &catalog).unwrap();
        let svc2 = pass2.iter().find(|s| s.name == "svc").unwrap();
        assert_eq!(svc2.total_events, 2);
        assert!(!warned.lock().contains(&svc));
    }
}
