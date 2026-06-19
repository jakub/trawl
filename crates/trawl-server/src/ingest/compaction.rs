// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background WAL → parquet compaction task.
//!
//! Periodically scans the WAL directory for `.ndjson` files, groups
//! them by service, and uses `DuckDB` to convert each batch to parquet.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use tokio::sync::watch;
use trawl_engine::{is_complex_type, is_union_type_conflict};

use crate::hot_buffer::HotBuffer;
use crate::state::CompactionStats;

/// Default compaction chunk size, used when config is not threaded
/// through (e.g. in direct `compact_once` calls from tests).
#[cfg(test)]
const DEFAULT_CHUNK_SIZE: usize = 500;

/// Spawn the compaction background loop.
///
/// Runs every `interval` seconds, scanning `wal_dir` for `.ndjson` files
/// whose mtime is older than `interval`. Groups by service and writes
/// parquet to `data_dir/{date}/{hour}/{service}.parquet`.
///
/// Stops when `shutdown_rx` receives a signal.
#[allow(clippy::too_many_arguments)] // internal API, config struct is overkill here
pub fn spawn_compaction(
    wal_dir: PathBuf,
    data_dir: PathBuf,
    interval: Duration,
    daily_rollup: bool,
    chunk_size: usize,
    memory_limit: String,
    hot_buffer: Option<Arc<HotBuffer>>,
    compaction_stats: Option<Arc<CompactionStats>>,
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
            chunk_size,
            memory_limit = %memory_limit,
            "compaction task started"
        );

        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    match compact_once(&wal_dir, &data_dir, interval, daily_rollup, hot_buffer.as_ref(), chunk_size, &memory_limit).await {
                        Ok(rollup_failures) => {
                            if let Some(ref stats) = compaction_stats {
                                stats.total_runs.fetch_add(1, Ordering::Relaxed);
                                // WAL compaction ran; surface any best-effort
                                // rollup failures so they're visible on the
                                // dashboard instead of only in logs.
                                if rollup_failures > 0 {
                                    stats.total_errors.fetch_add(rollup_failures, Ordering::Relaxed);
                                }
                                let epoch_secs = SystemTime::now()
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .map_or(0, |d| d.as_secs());
                                stats.last_run_epoch_secs.store(epoch_secs, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            if let Some(ref stats) = compaction_stats {
                                stats.total_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            tracing::error!(event_type = "compaction_error", error = %e, "compaction tick failed");
                        }
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
/// Returns the number of per-service daily rollups that failed this cycle
/// (0 on a clean run). WAL compaction errors are surfaced as `Err`; rollup
/// failures are best-effort and reported via the count so the caller can
/// track them without failing the whole cycle.
///
/// Public for integration tests only — not part of the external API.
/// Called internally by [`spawn_compaction`].
pub async fn compact_once(
    wal_dir: &Path,
    data_dir: &Path,
    min_age: Duration,
    daily_rollup: bool,
    hot_buffer: Option<&Arc<HotBuffer>>,
    chunk_size: usize,
    memory_limit: &str,
) -> Result<u64, String> {
    // Remove orphaned .parquet.tmp files from interrupted compaction runs.
    cleanup_stale_tmp_files(data_dir, min_age * 2);

    let files = scan_wal_files(wal_dir, min_age).map_err(|e| format!("scan failed: {e}"))?;

    // Tally of corrupt WAL files quarantined this cycle. Folded into the
    // return value so it lands on `CompactionStats.total_errors` as a
    // data-loss signal, mirroring the rollup quarantine count.
    let mut wal_quarantined: u64 = 0;

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

            // Process WAL files in chunks to avoid OOM on large backlogs.
            // Each chunk independently compacts to parquet (merging with
            // the canonical file if it exists), drains the hot buffer, and
            // cleans up consumed WAL files. If a chunk fails, remaining
            // chunks are skipped and retried on the next tick.
            let safe_chunk_size = chunk_size.max(1);
            let chunks: Vec<&[PathBuf]> = wal_files.chunks(safe_chunk_size).collect();
            let total_chunks = chunks.len();

            for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
                if total_chunks > 1 {
                    tracing::debug!(
                        event_type = "compaction_chunk",
                        compact_service = %service,
                        chunk = chunk_idx + 1,
                        total_chunks,
                        chunk_files = chunk.len(),
                        "processing compaction chunk"
                    );
                }

                // Events remain visible in the hot buffer until drain. Brief
                // duplicates (events in both parquet and hot snapshot) are
                // acceptable — invisible events are not.
                let batch_ids: Vec<&str> = chunk
                    .iter()
                    .filter_map(|f| f.file_stem()?.to_str())
                    .collect();

                match compact_service_batch(chunk, data_dir, service, memory_limit).await {
                    Ok(quarantined) => {
                        wal_quarantined += quarantined;

                        // Remove fully compacted batches from the hot buffer.
                        if let Some(buf) = &hot_buffer {
                            buf.drain(&batch_ids);
                        }

                        // Clean up consumed WAL files. A file that was
                        // quarantined (renamed to `.corrupt`) is already gone
                        // from its original path — NotFound means the goal
                        // (no longer a re-compactable WAL file) is satisfied.
                        for f in chunk {
                            match std::fs::remove_file(f) {
                                Ok(()) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                Err(e) => {
                                    tracing::warn!(
                                        event_type = "compaction_error",
                                        file = %f.display(),
                                        error = %e,
                                        "failed to delete consumed WAL file"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Leave remaining WAL files for retry on next tick.
                        tracing::error!(
                            event_type = "compaction_error",
                            compact_service = %service,
                            chunk = chunk_idx + 1,
                            total_chunks,
                            error = %e,
                            "compaction chunk failed, will retry next tick"
                        );
                        break;
                    }
                }
            }
        }
    }

    // After WAL compaction, consolidate older days' hourly files into
    // per-service daily files. This dramatically reduces file count for
    // long lookback queries. Rollup is best-effort: a failure for one
    // day/service is counted and retried next tick, not propagated.
    let rollup_failures = if daily_rollup {
        match rollup_once(data_dir, memory_limit).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(event_type = "rollup_error", error = %e, "daily rollup failed");
                1
            }
        }
    } else {
        0
    };

    Ok(rollup_failures + wal_quarantined)
}

/// Consolidate hourly per-service parquet files into daily files.
///
/// For each date-directory older than today, collects all
/// `{hour}/{service}.parquet` files, merges them (sorted by timestamp)
/// into `{date}/{service}.parquet`, then removes the hourly sources.
async fn rollup_once(data_dir: &Path, memory_limit: &str) -> Result<u64, String> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut failures: u64 = 0;
    let mut quarantined_total: u64 = 0;

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
            // A wedged recovery is data-loss-adjacent (an interrupted rollup
            // left orphaned hourlies/tmp that couldn't be cleaned up), so count
            // it on the error tally like the quarantine path does — otherwise it
            // is visible only in logs, never on the dashboard counter. Counting
            // (not `continue`) is deliberate: the day's fresh rollup below can
            // still make progress on other services.
            failures += 1;
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

            let mem_limit = memory_limit.to_owned();
            let outcome = tokio::task::spawn_blocking(move || {
                rollup_day_blocking(&day_dir, &svc, &files, &mem_limit)
            })
            .await
            .map_err(|e| format!("rollup task panicked: {e}"))?;

            // Quarantined inputs are data-loss whether or not the merge then
            // succeeded — fold the count in unconditionally so it lands on the
            // dashboard counter even when the merge errored and dropped its
            // valid output (the quarantined files are already renamed aside, so
            // a retry can't re-count them).
            quarantined_total += outcome.quarantined;
            if let Err(e) = outcome.result {
                failures += 1;
                tracing::error!(
                    event_type = "rollup_error",
                    compact_service = %service,
                    error = %e,
                    "rollup failed for service, will retry next tick"
                );
            }
        }

        // Remove empty hour-directories after all services are rolled up.
        for hour_dir in &hour_dirs {
            if is_dir_empty(hour_dir) {
                let _ = std::fs::remove_dir(hour_dir);
            }
        }
    }

    Ok(failures + quarantined_total)
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
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
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
            // Crash after rename — just clean up hourlies. A failed delete
            // renames the hourly aside (`.merged`) so it can never be
            // re-merged; a failed rename-aside propagates as `Err` and leaves
            // the marker in place for the next recovery pass to retry. The
            // marker delete below is only reached if every hourly is gone.
            tracing::info!(
                event_type = "rollup_recovery",
                compact_service = %service,
                "recovering rollup: canonical exists, deleting hourly files"
            );
            for f in &hourly_files {
                retire_merged_hourly(f)?;
            }
        } else if tmp.exists() {
            if is_valid_parquet(&tmp) {
                // Crash after a complete .tmp write but before rename —
                // promote it and clean up the merged hourly sources.
                tracing::info!(
                    event_type = "rollup_recovery",
                    compact_service = %service,
                    "recovering rollup: renaming tmp to canonical"
                );
                std::fs::rename(&tmp, &canonical)
                    .map_err(|e| format!("rollup recovery rename failed: {e}"))?;
                for f in &hourly_files {
                    retire_merged_hourly(f)?;
                }
            } else {
                // Crash MID-COPY left a truncated .tmp. Promoting it would
                // persist an unreadable parquet under the canonical name
                // (the origin of "too small to be a Parquet file" errors).
                // Quarantine it and KEEP the hourly files so the next tick
                // re-rolls them from scratch. A failed quarantine propagates
                // as `Err` BEFORE the marker delete, so recovery retries it.
                tracing::warn!(
                    event_type = "rollup_recovery",
                    compact_service = %service,
                    "discarding truncated rollup tmp; retaining hourly files for retry"
                );
                quarantine_file(&tmp, service, "rollup_quarantine")?;
            }
        } else {
            // Neither exists — stale marker. Nothing to strand, so the marker
            // delete below is unconditional for this branch.
            tracing::warn!(
                event_type = "rollup_recovery",
                compact_service = %service,
                "removing stale rollup marker (no tmp or canonical file)"
            );
        }

        // Remove the marker — only reached once the hourlies are verifiably
        // gone-or-retired (any failure above short-circuited via `?`, leaving
        // the marker for the next recovery pass).
        let _ = std::fs::remove_file(&path);
    }

    Ok(())
}

/// Parquet magic bytes, written at both the start and end of every valid
/// parquet file.
const PARQUET_MAGIC: &[u8; 4] = b"PAR1";

/// Cheap structural validity check for a parquet file.
///
/// A valid parquet file is at least large enough to hold its header and
/// footer magic and starts and ends with the `PAR1` magic bytes. This
/// catches zero-byte and truncated files (e.g. a crash mid-`COPY`) before
/// they reach `read_parquet`, where they otherwise surface as
/// "File ... too small to be a Parquet file" and wedge the rollup forever
/// — no amount of retrying repairs a truncated file.
fn is_valid_parquet(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    // Header magic (4) + a minimal footer + footer length (4) + trailer
    // magic (4). Anything below this cannot be a parquet file.
    if meta.len() < 12 {
        return false;
    }
    let mut head = [0u8; 4];
    if file.read_exact(&mut head).is_err() || &head != PARQUET_MAGIC {
        return false;
    }
    let mut tail = [0u8; 4];
    if file.seek(SeekFrom::End(-4)).is_err() || file.read_exact(&mut tail).is_err() {
        return false;
    }
    &tail == PARQUET_MAGIC
}

/// Cheap content sniff for a WAL ndjson file, catching the torn-write
/// corruption signature before it reaches `read_json` — where it otherwise
/// fails with "Malformed JSON ... unexpected character" and wedges the whole
/// batch. Like a truncated parquet, no amount of retrying repairs a file of
/// zeros, so the only escape is to set it aside.
///
/// A WAL file is text: newline-delimited JSON objects. We reject it as
/// corrupt if it is empty (`read_json(records=true)` errors on empty input),
/// contains a NUL byte (illegal in JSON text and the dead giveaway of a torn
/// write whose data blocks never flushed), or is not valid UTF-8. An
/// unreadable file is likewise rejected — `read_json` can't read it either,
/// so quarantining beats wedging.
///
/// This deliberately does NOT fully JSON-parse each line: that would
/// re-implement `read_json` and reject merely-malformed-but-textual data, a
/// different (and rarer) class than the crash debris this guards against.
fn is_valid_ndjson(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.is_empty() || bytes.contains(&0u8) {
        return false;
    }
    std::str::from_utf8(&bytes).is_ok()
}

/// Move a corrupt/unreadable file aside so it stops wedging compaction,
/// preserving the bytes for forensics. Appends `.corrupt` to the filename,
/// which makes it inert: it no longer matches the `*.parquet`/`*.tmp` globs
/// the rollup scans, nor the `*.ndjson` glob WAL compaction scans.
///
/// `event_type` tags the structured log so operators can distinguish a
/// rollup quarantine (`rollup_quarantine`) from a WAL-compaction one
/// (`compaction_quarantine`).
///
/// A failed rename is a HARD error: the bad file still matches the scan glob
/// and would re-wedge on every tick forever, so callers must surface (and
/// count) the failure rather than swallow it.
fn quarantine_file(path: &Path, service: &str, event_type: &str) -> Result<(), String> {
    let mut quarantined = path.as_os_str().to_owned();
    quarantined.push(".corrupt");
    let quarantined = PathBuf::from(quarantined);
    match std::fs::rename(path, &quarantined) {
        Ok(()) => {
            tracing::warn!(
                event_type,
                compact_service = %service,
                from = %path.display(),
                to = %quarantined.display(),
                "quarantined corrupt file"
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                event_type,
                compact_service = %service,
                file = %path.display(),
                error = %e,
                "failed to quarantine corrupt file"
            );
            Err(format!("failed to quarantine {}: {e}", path.display()))
        }
    }
}

/// Make a merged hourly file inert when it can't be deleted.
///
/// Renames it aside with a `.merged` suffix so it no longer matches the
/// `*.parquet`/`*.tmp` compaction globs and can never be re-merged into a
/// later rollup (which would duplicate its rows — the merge path has no
/// dedup by design, since two identical log lines are distinct events).
///
/// A MISSING source is success: the goal — "this file is no longer present as
/// a re-mergeable hourly" — is already satisfied if it's gone. This keeps the
/// op idempotent so a recovery pass that replays a marker after a partial
/// cleanup loop (some hourlies already deleted) doesn't wedge on a phantom.
/// A failed rename whose source vanished from under us (lost a delete race) is
/// likewise fine; only a genuine non-`NotFound` rename failure is a hard error,
/// because then the file still matches `*.parquet` and WOULD be re-merged.
fn retire_merged_hourly(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    let mut aside = path.as_os_str().to_owned();
    aside.push(".merged");
    match std::fs::rename(path, PathBuf::from(&aside)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "failed to retire merged hourly {}: {e}",
            path.display()
        )),
    }
}

/// Outcome of one per-service daily rollup.
///
/// Carries `quarantined` alongside `result` so the data-loss count survives a
/// hard merge error: quarantining renames the bad input to `.corrupt`, so a
/// later retry can't re-see (and re-count) it — if a `result: Err` dropped the
/// tally, those quarantines would never reach the dashboard counter.
#[derive(Debug, Clone)]
struct RollupOutcome {
    /// Inputs quarantined this call (corrupt/truncated → `.corrupt`).
    /// Counts as data-loss against the compaction error counter, whether or not
    /// the merge itself then succeeded.
    quarantined: u64,
    /// Whether the merge wrote a daily file (`Ok`) or failed and must retry
    /// next tick (`Err`).
    result: Result<(), String>,
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
///
/// Thin wrapper over [`rollup_day_inner`] that pairs the accumulated quarantine
/// count with the merge result, so the count is reported even when the merge
/// then errors — every `?` bail-out in the inner body would otherwise drop it.
fn rollup_day_blocking(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
    memory_limit: &str,
) -> RollupOutcome {
    let mut quarantined: u64 = 0;
    let result = rollup_day_inner(
        day_dir,
        service,
        hourly_files,
        memory_limit,
        &mut quarantined,
    );
    RollupOutcome {
        quarantined,
        result,
    }
}

/// The fallible body of one per-service rollup. Increments `*quarantined` as
/// corrupt inputs are renamed aside; [`rollup_day_blocking`] pairs that running
/// count with this `Result` so a mid-merge `Err` can't lose it.
fn rollup_day_inner(
    day_dir: &Path,
    service: &str,
    hourly_files: &[PathBuf],
    memory_limit: &str,
    quarantined: &mut u64,
) -> Result<(), String> {
    let rollup_start = std::time::Instant::now();
    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;

    // Point DuckDB temp directory at the PVC so spill-to-disk works on
    // read-only container overlay filesystems.
    conn.execute_batch(&format!(
        "SET temp_directory='{}'",
        day_dir.to_string_lossy().replace('\'', "''")
    ))
    .map_err(|e| format!("SET temp_directory failed: {e}"))?;

    // Cap memory and threads — same rationale as compact_service_blocking.
    conn.execute_batch(&format!(
        "SET memory_limit='{}'; SET threads=2",
        memory_limit.replace('\'', "''")
    ))
    .map_err(|e| format!("SET memory_limit/threads failed: {e}"))?;

    // Build the merge input list: all hourly files, plus the existing
    // day-level file (late-arriving data merges into it after a prior
    // rollup). Validate each input and quarantine truncated/corrupt files
    // so one bad file (e.g. a crash mid-COPY) doesn't wedge the rollup
    // forever. `merged_hourly` tracks the valid hourlies we will delete on
    // success — quarantined files are already renamed away.
    let canonical_path = day_dir.join(format!("{service}.parquet"));
    let mut all_files: Vec<PathBuf> = Vec::with_capacity(hourly_files.len() + 1);
    let mut merged_hourly: Vec<PathBuf> = Vec::with_capacity(hourly_files.len());
    for f in hourly_files {
        if is_valid_parquet(f) {
            all_files.push(f.clone());
            merged_hourly.push(f.clone());
        } else {
            // A failed quarantine is a HARD error — the bad file still
            // matches `*.parquet` and would wedge the rollup forever.
            quarantine_file(f, service, "rollup_quarantine")?;
            *quarantined += 1;
        }
    }
    if canonical_path.exists() {
        if is_valid_parquet(&canonical_path) {
            all_files.push(canonical_path.clone());
        } else {
            quarantine_file(&canonical_path, service, "rollup_quarantine")?;
            *quarantined += 1;
        }
    }

    // Every input was corrupt and has been quarantined — nothing readable
    // to merge, and nothing left to retry. This is DATA LOSS: a whole
    // service/day produced no daily file. Surface it distinctly at error
    // level (the partial-corrupt path only logs a cheerful rollup_complete)
    // and report the quarantine count so it lands on the dashboard counter.
    if all_files.is_empty() {
        if *quarantined > 0 {
            tracing::error!(
                event_type = "rollup_data_loss",
                compact_service = %service,
                quarantined = *quarantined,
                "all rollup inputs corrupt; quarantined, no daily file produced — DATA LOSS"
            );
        }
        return Ok(());
    }

    // Path provenance: service is sanitized to [A-Za-z0-9_-] at ingest
    // (wal.rs sanitize_service_for_filename) and data_dir is operator-trusted,
    // so direct interpolation cannot inject. No quote-escaping needed.
    let file_list_sql = all_files
        .iter()
        .map(|f| format!("'{}'", f.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");
    let tmp_path = day_dir.join(format!("{service}.parquet.tmp"));

    // Read, merge, sort by timestamp, and write to tmp file. The fast path
    // leans on `union_by_name` to reconcile heterogeneous schemas, but that
    // only unifies by column NAME — it cannot bridge a column that is JSON
    // or STRUCT in one hourly file and VARCHAR in another (independent
    // per-batch type inference at write time produces exactly this drift).
    // On that bind-time type/remap error, fall back to describing each file
    // and casting the conflicting columns to VARCHAR before unioning.
    let fast = conn.execute_batch(&format!(
        "COPY (\
             SELECT * FROM read_parquet([{file_list_sql}], union_by_name=true) \
             ORDER BY \"timestamp\"\
         ) TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY, \
             BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
        tmp_path.to_string_lossy(),
    ));
    match fast {
        Ok(()) => {}
        Err(e) if is_union_type_conflict(&e) => {
            tracing::warn!(
                event_type = "rollup_fallback",
                compact_service = %service,
                error = %e,
                "rollup type conflict across hourly files, falling back to VARCHAR casts"
            );
            rollup_with_casts(&conn, &all_files, &tmp_path, service)?;
        }
        Err(e) => {
            // S2: this file passed `is_valid_parquet`'s magic-byte sniff but
            // is unreadable by `read_parquet` for a non-type, non-corruption
            // reason (e.g. a valid header/trailer but a corrupt MIDDLE). The
            // sniff can't catch that, and no amount of retrying repairs it —
            // surface it distinctly at error level so the rare forever-retry
            // is visible on the dashboard rather than buried in the count.
            tracing::error!(
                event_type = "rollup_unreadable",
                compact_service = %service,
                error = %e,
                "rollup input passed magic-byte validation but is unreadable; will retry indefinitely"
            );
            return Err(format!("rollup COPY failed: {e}"));
        }
    }

    // Write marker BEFORE rename so recovery knows which hourlies to clean up.
    write_rollup_marker(day_dir, service, &merged_hourly)?;

    // Atomic rename.
    std::fs::rename(&tmp_path, &canonical_path)
        .map_err(|e| format!("rollup rename failed: {e}"))?;

    // Delete the hourly source files that were merged. If a delete fails,
    // rename the file aside (`.merged`) so it can never be re-merged into a
    // later rollup — the merge path has no dedup, so a surviving hourly would
    // silently duplicate every one of its rows on the next tick. A failed
    // rename-aside is a hard error: the file still matches `*.parquet`.
    for f in &merged_hourly {
        retire_merged_hourly(f)?;
    }

    // Remove marker — rollup fully complete.
    delete_rollup_marker(day_dir, service);

    let output_bytes = std::fs::metadata(&canonical_path).map_or(0, |m| m.len());

    let duration_ms = rollup_start.elapsed().as_millis();
    tracing::info!(
        event_type = "rollup_complete",
        compact_service = %service,
        output = %canonical_path.display(),
        hourly_files = merged_hourly.len(),
        output_bytes,
        duration_ms,
        "daily rollup complete"
    );

    Ok(())
}

/// Rollup fallback: union hourly parquet files when their schemas conflict.
///
/// The fast-path `read_parquet([...], union_by_name=true)` fails at bind
/// time when the same column name has incompatible physical types across
/// files (e.g. JSON/STRUCT in one hour, VARCHAR in another). This rebuilds
/// the merge explicitly: `DESCRIBE` every file, find columns whose type
/// differs across files, cast those to `VARCHAR` in each branch, and
/// `UNION ALL BY NAME` so heterogeneous column sets still line up (missing
/// columns become NULL). Mirrors [`merge_with_existing`] but generalised
/// to N files for the daily rollup.
fn rollup_with_casts(
    conn: &duckdb::Connection,
    files: &[PathBuf],
    tmp_path: &Path,
    service: &str,
) -> Result<(), String> {
    // Describe every input file and accumulate the set of types seen per
    // column name across all files.
    let mut col_types: HashMap<String, HashSet<String>> = HashMap::new();
    let mut schemas: Vec<Vec<ColInfo>> = Vec::with_capacity(files.len());
    for f in files {
        let schema = describe_source(
            conn,
            &format!("SELECT * FROM read_parquet('{}')", f.display()),
        )?;
        for col in &schema {
            col_types
                .entry(col.name.clone())
                .or_default()
                .insert(col.dtype.clone());
        }
        schemas.push(schema);
    }

    // A column conflicts when it appears with more than one distinct type.
    let conflicts: Vec<String> = col_types
        .into_iter()
        .filter(|(_, types)| types.len() > 1)
        .map(|(name, _)| name)
        .collect();

    tracing::info!(
        event_type = "rollup_fallback",
        compact_service = %service,
        conflicting_columns = ?conflicts,
        "casting conflicting columns to VARCHAR for rollup"
    );

    // Build one casting SELECT per file and union them by name.
    // Path provenance: service is sanitized to [A-Za-z0-9_-] at ingest
    // (wal.rs sanitize_service_for_filename) and data_dir is operator-trusted,
    // so direct interpolation cannot inject. No quote-escaping needed.
    let union_sql = files
        .iter()
        .zip(&schemas)
        .map(|(f, schema)| {
            format!(
                "SELECT {} FROM read_parquet('{}')",
                build_cast_select(schema, &conflicts),
                f.display()
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL BY NAME ");

    conn.execute_batch(&format!(
        "COPY (SELECT * FROM ({union_sql}) ORDER BY \"timestamp\") TO '{}' \
         (FORMAT PARQUET, COMPRESSION SNAPPY, BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
        tmp_path.to_string_lossy(),
    ))
    .map_err(|e| format!("rollup (type fallback) failed: {e}"))
}

/// Compact a batch of WAL files for a single service into parquet.
async fn compact_service_batch(
    wal_files: &[PathBuf],
    data_dir: &Path,
    service: &str,
    memory_limit: &str,
) -> Result<u64, String> {
    let wal_files = wal_files.to_vec();
    let data_dir = data_dir.to_path_buf();
    let service = service.to_owned();
    let memory_limit = memory_limit.to_owned();

    tokio::task::spawn_blocking(move || {
        compact_service_blocking(&wal_files, &data_dir, &service, &memory_limit)
    })
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

/// Read WAL ndjson files into a `DuckDB` temp table called `wal_batch`.
///
/// Tries auto-detection first (`maximum_depth=2`). If `DuckDB` hits a
/// "Duplicate name" error (nested JSON keys that collide when flattened),
/// falls back to an explicit column list with `json` typed as opaque JSON.
///
/// After the table is built, any column `DuckDB` inferred as a complex type
/// (STRUCT/MAP/JSON/LIST) is coerced to VARCHAR — see
/// [`coerce_complex_columns_to_varchar`] for why.
fn read_wal_to_table(
    conn: &duckdb::Connection,
    wal_files: &[PathBuf],
    service: &str,
) -> Result<(), String> {
    let file_list_sql = wal_files
        .iter()
        .map(|p| format!("'{}'", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ");

    // Primary path: auto-detect with union_by_name to handle heterogeneous schemas.
    let result = conn.execute_batch(&format!(
        "CREATE TABLE wal_batch AS \
         SELECT * REPLACE (CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") \
         FROM read_json([{file_list_sql}], format='newline_delimited', \
         records=true, auto_detect=true, union_by_name=true, \
         field_appearance_threshold=0, maximum_depth=2)"
    ));

    match result {
        Ok(()) => {}
        Err(e) if e.to_string().contains("Duplicate name") => {
            tracing::warn!(
                event_type = "compaction_fallback",
                compact_service = %service,
                error = %e,
                "falling back to explicit columns to avoid duplicate key collision"
            );
            // Explicit columns: the stable vector envelope, with `json` as
            // opaque JSON to prevent struct flattening that causes collisions.
            conn.execute_batch(&format!(
                "CREATE TABLE wal_batch AS \
                 SELECT * REPLACE (CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") \
                 FROM read_json([{file_list_sql}], format='newline_delimited', \
                 records=true, union_by_name=true, columns={{\
                 host: 'VARCHAR', json: 'JSON', \
                 k8s_container: 'VARCHAR', k8s_namespace: 'VARCHAR', \
                 k8s_node: 'VARCHAR', k8s_pod: 'VARCHAR', \
                 level: 'VARCHAR', message: 'VARCHAR', \
                 service: 'VARCHAR', timestamp: 'VARCHAR'}})"
            ))
            .map_err(|e| format!("read_json (explicit columns) failed: {e}"))?;
        }
        Err(e) => return Err(format!("read_json failed: {e}")),
    }

    coerce_complex_columns_to_varchar(conn, service)
}

/// Coerce every complex-typed column in `wal_batch` to VARCHAR.
///
/// This is the root-cause fix for cross-file schema drift. `DuckDB` infers
/// each column's type independently per compaction batch, so a field that
/// is object-valued in one batch (→ STRUCT/JSON) but only ever a string in
/// another (→ VARCHAR) lands with different physical types across hourly
/// parquet files. Those later collide under `read_parquet(...,
/// union_by_name=true)` at both rollup AND query time (which has no
/// fallback and degrades to dropping cold rows).
///
/// Forcing complex columns to VARCHAR converges both cases: an object
/// becomes its JSON text and a plain string stays a string — both VARCHAR —
/// so every hourly file shares a stable schema for that column. Scalars
/// (numbers, booleans, timestamps) keep their types. The DSL never relies
/// on a column being JSON/STRUCT-typed (dotted fields are flat identifiers;
/// `json()`/`json_extract()` accept VARCHAR), so this is transparent to
/// queries. The hot-buffer snapshot is coerced symmetrically so the
/// query-time union of hot + cold sources stays type-aligned.
fn coerce_complex_columns_to_varchar(
    conn: &duckdb::Connection,
    service: &str,
) -> Result<(), String> {
    let schema = describe_source(conn, "SELECT * FROM wal_batch")?;
    let complex: Vec<String> = schema
        .iter()
        .filter(|c| is_complex_type(&c.dtype))
        .map(|c| c.name.clone())
        .collect();

    if complex.is_empty() {
        return Ok(());
    }

    tracing::debug!(
        event_type = "compaction_coerce",
        compact_service = %service,
        columns = ?complex,
        "coercing complex columns to VARCHAR for a stable on-disk schema"
    );

    let select = build_cast_select(&schema, &complex);
    conn.execute_batch(&format!(
        "CREATE OR REPLACE TABLE wal_batch AS SELECT {select} FROM wal_batch"
    ))
    .map_err(|e| format!("complex column coercion failed: {e}"))
}

/// Column name and type from `DuckDB` `DESCRIBE`.
struct ColInfo {
    name: String,
    dtype: String,
}

/// Run `DESCRIBE <query>` and return the column names and types.
fn describe_source(conn: &duckdb::Connection, query: &str) -> Result<Vec<ColInfo>, String> {
    let mut stmt = conn
        .prepare(&format!("DESCRIBE {query}"))
        .map_err(|e| format!("DESCRIBE failed: {e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(ColInfo {
                name: row.get::<_, String>(0)?,
                dtype: row.get::<_, String>(1)?,
            })
        })
        .map_err(|e| format!("DESCRIBE query failed: {e}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("DESCRIBE row read failed: {e}"))
}

/// Escape a `DuckDB` identifier: wrap in double-quotes, doubling any
/// embedded double-quotes. Column names come from ingested JSON keys
/// (user-controlled), so this prevents SQL injection in the fallback path.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Build a SELECT list that casts conflicting columns to `VARCHAR`.
///
/// Non-conflicting columns pass through quoted (`"col"`); conflicting
/// ones become `CAST("col" AS VARCHAR) AS "col"`.
fn build_cast_select(schema: &[ColInfo], conflicts: &[String]) -> String {
    schema
        .iter()
        .map(|col| {
            let quoted = quote_ident(&col.name);
            if conflicts.contains(&col.name) {
                format!("CAST({quoted} AS VARCHAR) AS {quoted}")
            } else {
                quoted
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Merge `wal_batch` with an existing parquet file via `UNION ALL BY NAME`.
///
/// Fast path: direct union. If that fails with a type mismatch (e.g.
/// JSON vs VARCHAR for the same column), falls back to `DESCRIBE`-ing
/// both sides, finding the conflicting columns, and casting them to
/// `VARCHAR` before retrying.
fn merge_with_existing(
    conn: &duckdb::Connection,
    canonical_path: &Path,
    service: &str,
) -> Result<(), String> {
    let pq_path = canonical_path.display();

    // Fast path: direct union.
    let result = conn.execute_batch(&format!(
        "CREATE TABLE merged AS \
         SELECT * FROM read_parquet('{pq_path}') \
         UNION ALL BY NAME \
         SELECT * FROM wal_batch",
    ));

    match result {
        Ok(()) => Ok(()),
        Err(e) if is_union_type_conflict(&e) => {
            tracing::warn!(
                event_type = "compaction_fallback",
                compact_service = %service,
                error = %e,
                "type mismatch during merge, falling back to explicit casts"
            );

            // Get schemas for both sides.
            let pq_schema =
                describe_source(conn, &format!("SELECT * FROM read_parquet('{pq_path}')"))?;
            let wb_schema = describe_source(conn, "SELECT * FROM wal_batch")?;

            // Build a lookup of wal_batch column types.
            let wb_types: HashMap<&str, &str> = wb_schema
                .iter()
                .map(|c| (c.name.as_str(), c.dtype.as_str()))
                .collect();

            // Find columns present in both with different types.
            let conflicts: Vec<String> = pq_schema
                .iter()
                .filter(|col| {
                    wb_types
                        .get(col.name.as_str())
                        .is_some_and(|wb_type| *wb_type != col.dtype)
                })
                .map(|col| col.name.clone())
                .collect();

            if conflicts.is_empty() {
                // Not actually a type conflict — re-raise original error.
                return Err(format!("merge read_parquet failed: {e}"));
            }

            tracing::info!(
                event_type = "compaction_fallback",
                compact_service = %service,
                conflicting_columns = ?conflicts,
                "casting conflicting columns to VARCHAR"
            );

            let pq_select = build_cast_select(&pq_schema, &conflicts);
            let wb_select = build_cast_select(&wb_schema, &conflicts);

            conn.execute_batch(&format!(
                "CREATE TABLE merged AS \
                 SELECT {pq_select} FROM read_parquet('{pq_path}') \
                 UNION ALL BY NAME \
                 SELECT {wb_select} FROM wal_batch",
            ))
            .map_err(|e| format!("merge (type fallback) failed: {e}"))
        }
        Err(e) => Err(format!("merge read_parquet failed: {e}")),
    }
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
    memory_limit: &str,
) -> Result<u64, String> {
    let compact_start = std::time::Instant::now();

    // Validate WAL inputs before they reach `read_json`. A single corrupt
    // file (e.g. a pure-NUL torn write from a hard kill) fails the whole
    // multi-file `read_json` call and, with no quarantine, head-of-line
    // blocks this service's compaction forever — re-scanned and re-failed
    // every tick. Sniff each file and move the corrupt ones aside (bytes
    // preserved), mirroring the rollup parquet-quarantine path.
    let mut valid_files: Vec<PathBuf> = Vec::with_capacity(wal_files.len());
    let mut quarantined: u64 = 0;
    for f in wal_files {
        if is_valid_ndjson(f) {
            valid_files.push(f.clone());
        } else {
            quarantine_file(f, service, "compaction_quarantine")?;
            quarantined += 1;
        }
    }

    if valid_files.is_empty() {
        // Every file in the batch was corrupt — nothing readable to compact.
        // This is data loss (torn writes are unrecoverable), not an error:
        // there is nothing to retry, so surface it via the quarantine count
        // rather than wedging. Mirrors the rollup all-corrupt branch.
        tracing::error!(
            event_type = "compaction_data_loss",
            compact_service = %service,
            quarantined,
            "all WAL files in batch were corrupt — no parquet produced, DATA LOSS"
        );
        return Ok(quarantined);
    }

    let conn =
        duckdb::Connection::open_in_memory().map_err(|e| format!("DuckDB open failed: {e}"))?;

    // Point DuckDB temp directory at the PVC so spill-to-disk works on
    // read-only container overlay filesystems.
    conn.execute_batch(&format!(
        "SET temp_directory='{}'",
        data_dir.to_string_lossy().replace('\'', "''")
    ))
    .map_err(|e| format!("SET temp_directory failed: {e}"))?;

    // Cap memory usage so DuckDB spills to disk earlier rather than
    // consuming 80% of container RAM. Limit threads to reduce peak
    // memory — compaction is a background task where latency is fine.
    conn.execute_batch(&format!(
        "SET memory_limit='{}'; SET threads=2",
        memory_limit.replace('\'', "''")
    ))
    .map_err(|e| format!("SET memory_limit/threads failed: {e}"))?;

    read_wal_to_table(&conn, &valid_files, service)?;

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
        // Falls back to explicit casts if column types conflict.
        merge_with_existing(&conn, &canonical_path, service)?;

        // ORDER BY timestamp so row-group min/max stats enable range
        // pruning for `last=Xh` queries — the dominant query shape.
        // BLOOM_FILTER_FALSE_POSITIVE_RATIO pins the bloom filter FP
        // target (DuckDB auto-writes bloom filters on any column it
        // dictionary-encodes; this locks in a known FP rate).
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM merged ORDER BY \"timestamp\") TO '{}' \
             (FORMAT PARQUET, COMPRESSION SNAPPY, \
              BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
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
            "COPY (SELECT * FROM wal_batch ORDER BY \"timestamp\") TO '{}' \
             (FORMAT PARQUET, COMPRESSION SNAPPY, \
              BLOOM_FILTER_FALSE_POSITIVE_RATIO 0.01)",
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

    let output_bytes = std::fs::metadata(&canonical_path).map_or(0, |m| m.len());

    let duration_ms = compact_start.elapsed().as_millis();
    tracing::info!(
        event_type = "compaction_complete",
        compact_service = %service,
        output = %canonical_path.display(),
        wal_files = valid_files.len(),
        merged,
        rows,
        output_bytes,
        duration_ms,
        "compaction complete"
    );

    Ok(quarantined)
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

        compact_service_blocking(&wal_files, &data_dir, "nginx", "2GB").unwrap();

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
        compact_service_blocking(&files1, &data_dir, "nginx", "2GB").unwrap();

        // Second compaction: merge new data into existing file.
        let r2 = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"nginx","message":"second"}"#;
        let files2 = vec![write_wal_file(&wal_dir, "nginx", &[r2])];
        compact_service_blocking(&files2, &data_dir, "nginx", "2GB").unwrap();

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
    fn compact_sorts_rows_by_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Ingest out-of-order timestamps in both the fresh-write and
        // merge paths to verify both sort.
        let late = r#"{"timestamp":"2026-01-01T00:00:10Z","service":"nginx","msg":"late"}"#;
        let early = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"nginx","msg":"early"}"#;
        compact_service_blocking(
            &[write_wal_file(&wal_dir, "nginx", &[late, early])],
            &data_dir,
            "nginx",
            "2GB",
        )
        .unwrap();

        // Second batch merges into the existing file; include a timestamp
        // that should sort between the two above.
        let middle = r#"{"timestamp":"2026-01-01T00:00:05Z","service":"nginx","msg":"middle"}"#;
        compact_service_blocking(
            &[write_wal_file(&wal_dir, "nginx", &[middle])],
            &data_dir,
            "nginx",
            "2GB",
        )
        .unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        // Read rows in file order (no ORDER BY in the query) — they should
        // already be timestamp-ascending because of sort-on-compaction.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT msg FROM read_parquet('{}')",
                parquet_files[0].display()
            ))
            .unwrap();
        let msgs: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            msgs,
            vec!["early", "middle", "late"],
            "hourly parquet should be timestamp-sorted"
        );
    }

    #[test]
    fn compact_handles_heterogeneous_schemas() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Two records with different key sets — simulates the real scenario
        // where services emit log lines with varying JSON shapes.
        let r1 = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"test","message":"base record"}"#;
        let r2 = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"test","message":"extra","extra_field":"surprise","error":"oh no"}"#;

        let f1 = write_wal_file(&wal_dir, "test", &[r1]);
        let f2 = write_wal_file(&wal_dir, "test", &[r2]);

        compact_service_blocking(&[f1, f2], &data_dir, "test", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

        // Verify both rows are present in the output.
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
        assert_eq!(
            count, 2,
            "both records should be present despite different schemas"
        );
    }

    #[test]
    fn compact_handles_nested_json() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Simulates the real data shape: outer envelope + nested json object
        // with varying subkeys. The `json` object has different keys across
        // records, and `k8s_namespace` at the top level coexists with
        // `namespace` inside the nested object.
        let r1 = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"test","message":"startup","k8s_namespace":"default","json":{"level":"info","msg":"starting","namespace":"kube-system","build":{"version":"1.0","commit":"abc123"}}}"#;
        let r2 = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"test","message":"runtime","k8s_namespace":"default","json":{"level":"warn","msg":"something happened"}}"#;

        let f1 = write_wal_file(&wal_dir, "test", &[r1]);
        let f2 = write_wal_file(&wal_dir, "test", &[r2]);

        compact_service_blocking(&[f1, f2], &data_dir, "test", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

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
        assert_eq!(
            count, 2,
            "nested JSON records should compact without duplicate key errors"
        );
    }

    #[test]
    fn compact_handles_duplicate_key_across_nesting_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Simulates flux-operator: `k8s_namespace` at top level, `namespace`
        // inside the `json` sub-object. With unlimited depth, DuckDB would
        // flatten `json.namespace` and collide with the top-level key.
        let r1 = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"flux-op","message":"reconcile","k8s_namespace":"flux-system","json":{"controller":"fluxinstance","level":"info","msg":"Reconciliation finished","name":"flux","namespace":"flux-system"}}"#;
        let r2 = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"flux-op","message":"sync","k8s_namespace":"flux-system","json":{"controller":"kustomization","level":"info","msg":"Applied revision","namespace":"default"}}"#;

        let f1 = write_wal_file(&wal_dir, "flux-op", &[r1]);
        let f2 = write_wal_file(&wal_dir, "flux-op", &[r2]);

        compact_service_blocking(&[f1, f2], &data_dir, "flux-op", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

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
        assert_eq!(
            count, 2,
            "duplicate nested key names should not cause compaction failure"
        );
    }

    #[test]
    fn compact_chunked_merges_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Create 3 WAL files. Compact them in two calls to simulate chunking:
        // first call processes 2 files, second call processes 1 file and
        // merges with the existing parquet from the first call.
        let r1 = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"test","message":"one"}"#;
        let r2 = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"test","message":"two"}"#;
        let r3 = r#"{"timestamp":"2026-01-01T00:00:02Z","service":"test","message":"three"}"#;

        let f1 = write_wal_file(&wal_dir, "test", &[r1]);
        let f2 = write_wal_file(&wal_dir, "test", &[r2]);
        let f3 = write_wal_file(&wal_dir, "test", &[r3]);

        // Chunk 1: first two files.
        compact_service_blocking(&[f1, f2], &data_dir, "test", "2GB").unwrap();

        // Chunk 2: third file merges into existing parquet.
        compact_service_blocking(&[f3], &data_dir, "test", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1, "should still be one canonical file");

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
        assert_eq!(
            count, 3,
            "all three rows should be present after incremental merge"
        );
    }

    #[test]
    fn compact_merges_despite_type_conflict() {
        // Simulates the k8s containerID scenario: first compaction writes
        // a parquet file where `container_id` is a JSON object, second
        // compaction has WAL data where `container_id` is a plain string.
        // The merge should succeed by falling back to VARCHAR casts.
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // First batch: container_id is a JSON object.
        let r1 = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"kubelet","message":"start","container_id":{"id":"abc123","runtime":"containerd"}}"#;
        let f1 = write_wal_file(&wal_dir, "kubelet", &[r1]);
        compact_service_blocking(&[f1], &data_dir, "kubelet", "2GB").unwrap();

        // Second batch: container_id is a plain string.
        let r2 = r#"{"timestamp":"2026-01-01T00:00:01Z","service":"kubelet","message":"running","container_id":"def456"}"#;
        let f2 = write_wal_file(&wal_dir, "kubelet", &[r2]);

        // This would previously fail with a type mismatch error.
        compact_service_blocking(&[f2], &data_dir, "kubelet", "2GB").unwrap();

        let parquet_files = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet_files.len(), 1);

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
        assert_eq!(
            count, 2,
            "both rows should be present despite type conflict on container_id"
        );

        // Verify the conflicting column was cast to VARCHAR.
        let col_type: String = conn
            .query_row(
                &format!(
                    "SELECT typeof(container_id) FROM read_parquet('{}') LIMIT 1",
                    parquet_files[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            col_type, "VARCHAR",
            "conflicting column should be cast to VARCHAR"
        );
    }

    #[test]
    fn compaction_coerces_complex_fields_to_varchar() {
        // Root-cause fix: an object-valued field must be written as VARCHAR
        // so hourly files never disagree on its physical type. Scalars keep
        // their inferred types.
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let r = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"k","containerID":{"id":"abc"},"count":5}"#;
        let f = write_wal_file(&wal_dir, "k", &[r]);
        compact_service_blocking(std::slice::from_ref(&f), &data_dir, "k", "2GB").unwrap();

        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1);
        let conn = duckdb::Connection::open_in_memory().unwrap();

        // The object field is coerced to VARCHAR, preserving the JSON text.
        let (ty, val): (String, String) = conn
            .query_row(
                &format!(
                    "SELECT typeof(\"containerID\"), \"containerID\" \
                     FROM read_parquet('{}') LIMIT 1",
                    parquet[0].display()
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ty, "VARCHAR", "object field must be coerced to VARCHAR");
        assert!(val.contains("abc"), "JSON text should be preserved: {val}");

        // A scalar field keeps its inferred numeric type.
        let count_ty: String = conn
            .query_row(
                &format!(
                    "SELECT typeof(\"count\") FROM read_parquet('{}') LIMIT 1",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(count_ty, "VARCHAR", "scalar field must keep its type");
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
            "COPY (SELECT * FROM read_json([{file_list}], format='newline_delimited', \
             records=true, auto_detect=true, union_by_name=true, \
             field_appearance_threshold=0, maximum_depth=2)) \
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
        rollup_day_blocking(&day_dir, "nginx", &[f1.clone(), f2.clone()], "2GB")
            .result
            .unwrap();

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
        rollup_day_blocking(&day_dir, "nginx", &[f1], "2GB")
            .result
            .unwrap();

        // Late-arriving data creates a new hourly file.
        let r2 = r#"{"timestamp":"2026-01-15T03:00:00Z","service":"nginx","msg":"late"}"#;
        let f2 = write_hourly_parquet(&data_dir, date, "03", "nginx", &[r2]);

        // Second rollup: should merge existing daily + new hourly.
        rollup_day_blocking(&day_dir, "nginx", &[f2], "2GB")
            .result
            .unwrap();

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
        rollup_day_blocking(&day_dir, "nginx", &[f1, f2], "2GB")
            .result
            .unwrap();

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
    fn rollup_merges_despite_type_conflict_across_hours() {
        // The prod failure: hour 01 wrote `offset` as a JSON/STRUCT object,
        // hour 02 wrote it as a plain string ("540.203µs"). The bare
        // read_parquet(union_by_name=true) raises a bind-time type/remap
        // error; the rollup must fall back to VARCHAR casts and still merge.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 =
            r#"{"timestamp":"2026-01-15T01:00:00Z","service":"ctrl","offset":{"v":1,"u":"x"}}"#;
        let r2 = r#"{"timestamp":"2026-01-15T02:00:00Z","service":"ctrl","offset":"540.203µs"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "ctrl", &[r1]);
        let f2 = write_hourly_parquet(&data_dir, date, "02", "ctrl", &[r2]);

        let day_dir = data_dir.join(date);
        rollup_day_blocking(&day_dir, "ctrl", &[f1.clone(), f2.clone()], "2GB")
            .result
            .unwrap();

        let daily = day_dir.join("ctrl.parquet");
        assert!(daily.exists(), "daily parquet should exist after fallback");
        assert!(!f1.exists(), "hourly file 1 should be deleted");
        assert!(!f2.exists(), "hourly file 2 should be deleted");

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
        assert_eq!(count, 2, "both rows present despite offset type conflict");

        // The conflicting column must be unified to VARCHAR.
        let col_type: String = conn
            .query_row(
                &format!(
                    "SELECT typeof(\"offset\") FROM read_parquet('{}') LIMIT 1",
                    daily.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(col_type, "VARCHAR", "offset should be cast to VARCHAR");
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
        compact_once(
            &wal_dir,
            &data_dir,
            Duration::from_secs(1),
            true,
            None,
            DEFAULT_CHUNK_SIZE,
            "2GB",
        )
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

        rollup_day_blocking(&day_dir, "nginx", &[f1], "2GB")
            .result
            .unwrap();

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

        // Simulate: a COMPLETE .tmp was written, marker exists, no canonical
        // yet. A real interrupted-after-write tmp is a valid parquet, so use
        // f1's bytes (recovery validates before promoting — see
        // `recovery_discards_truncated_tmp` for the truncated case).
        std::fs::copy(&f1, &tmp_file).unwrap();
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

    #[test]
    fn is_valid_parquet_detects_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // A real parquet file is valid.
        let data_dir = dir.join("data");
        let r = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"x","m":"y"}"#;
        let good = write_hourly_parquet(&data_dir, "2026-01-15", "01", "x", &[r]);
        assert!(is_valid_parquet(&good), "real parquet should be valid");

        let empty = dir.join("empty.parquet");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_valid_parquet(&empty), "zero-byte file is invalid");

        let trunc = dir.join("trunc.parquet");
        std::fs::write(&trunc, b"PAR1\x00\x00").unwrap();
        assert!(!is_valid_parquet(&trunc), "truncated file is invalid");

        let nomagic = dir.join("nomagic.parquet");
        std::fs::write(&nomagic, b"definitely not a parquet file, but long enough").unwrap();
        assert!(!is_valid_parquet(&nomagic), "missing PAR1 magic is invalid");

        assert!(
            !is_valid_parquet(&dir.join("does-not-exist.parquet")),
            "missing file is invalid"
        );
    }

    #[test]
    fn is_valid_ndjson_detects_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let good = dir.join("good.ndjson");
        std::fs::write(&good, b"{\"service\":\"x\",\"message\":\"hi\"}\n").unwrap();
        assert!(is_valid_ndjson(&good), "real ndjson should be valid");

        // Pure-NUL file: the torn-write crash signature (unflushed blocks read
        // back as 0x00). 662 bytes mirrors the live homelab debris.
        let nul = dir.join("nul.ndjson");
        std::fs::write(&nul, [0u8; 662]).unwrap();
        assert!(!is_valid_ndjson(&nul), "all-NUL file is corrupt");

        // A valid record followed by an embedded NUL (partial torn write).
        let partial = dir.join("partial.ndjson");
        std::fs::write(&partial, b"{\"a\":1}\n\x00\x00\x00").unwrap();
        assert!(!is_valid_ndjson(&partial), "embedded NUL is corrupt");

        // Zero-byte: read_json(records=true) errors on empty input, so an
        // empty WAL file would wedge compaction just like a NUL one.
        let empty = dir.join("empty.ndjson");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_valid_ndjson(&empty), "zero-byte file is invalid");

        // Non-UTF-8 garbage (no NUL byte) exercises the UTF-8 branch.
        let binary = dir.join("binary.ndjson");
        std::fs::write(&binary, [0xff, 0xfe, 0xfd, 0x01]).unwrap();
        assert!(!is_valid_ndjson(&binary), "non-UTF-8 is invalid");

        assert!(
            !is_valid_ndjson(&dir.join("does-not-exist.ndjson")),
            "missing file is invalid"
        );
    }

    #[test]
    fn compact_quarantines_nul_ndjson_keeps_good() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let good = r#"{"timestamp":"2026-01-01T00:00:00Z","service":"nginx","message":"hello"}"#;
        let good_file = write_wal_file(&wal_dir, "nginx", &[good]);

        // A pure-NUL WAL file (the torn-write crash signature: unflushed
        // blocks read back as 0x00). write_wal_file only writes text, so write
        // it directly; keep the `nginx_` prefix so it groups with the good
        // file's service. 662 bytes mirrors the live homelab debris.
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let bad_file = wal_dir.join(format!("nginx_{millis}_bad.ndjson"));
        std::fs::write(&bad_file, [0u8; 662]).unwrap();

        let quarantined =
            compact_service_blocking(&[good_file, bad_file.clone()], &data_dir, "nginx", "2GB")
                .unwrap();
        assert_eq!(quarantined, 1, "the NUL file should be quarantined");

        // Good data still compacts to exactly one parquet.
        let parquet = find_files_by_ext(&data_dir, "parquet");
        assert_eq!(parquet.len(), 1, "good WAL must still produce a parquet");

        // Corrupt file renamed aside; bytes preserved for forensics.
        assert!(!bad_file.exists(), "corrupt WAL renamed away");
        let mut corrupt = bad_file.into_os_string();
        corrupt.push(".corrupt");
        assert!(
            PathBuf::from(corrupt).exists(),
            "corrupt WAL quarantined to .corrupt"
        );

        // Only the good row landed.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    parquet[0].display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only the valid row should be present");
    }

    #[test]
    fn compact_all_nul_is_data_loss_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let millis = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let a = wal_dir.join(format!("svc_{millis}_a.ndjson"));
        let b = wal_dir.join(format!("svc_{millis}_b.ndjson"));
        std::fs::write(&a, [0u8; 128]).unwrap();
        std::fs::write(&b, [0u8; 128]).unwrap();

        // All inputs corrupt: no parquet, no error (nothing readable to retry),
        // but both are counted as quarantined data-loss.
        let quarantined =
            compact_service_blocking(&[a.clone(), b.clone()], &data_dir, "svc", "2GB").unwrap();
        assert_eq!(quarantined, 2, "both corrupt inputs counted");

        assert!(
            find_files_by_ext(&data_dir, "parquet").is_empty(),
            "no parquet from all-corrupt batch"
        );
        assert!(!a.exists() && !b.exists(), "both corrupt files quarantined");
    }

    #[test]
    fn rollup_quarantines_truncated_hourly_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"ok"}"#;
        let good = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        // A truncated parquet in another hour (crash mid-COPY).
        let bad_dir = data_dir.join(date).join("02");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let bad = bad_dir.join("nginx.parquet");
        std::fs::write(&bad, b"PAR1\x00\x00").unwrap();

        let day_dir = data_dir.join(date);
        let outcome = rollup_day_blocking(&day_dir, "nginx", &[good.clone(), bad.clone()], "2GB");
        assert!(
            outcome.result.is_ok(),
            "partial-corrupt rollup should still produce a daily file"
        );
        assert_eq!(outcome.quarantined, 1, "one corrupt input quarantined");

        // Good data rolled up; corrupt file quarantined, not read.
        let daily = day_dir.join("nginx.parquet");
        assert!(daily.exists(), "daily file built from the valid hourly");
        assert!(!good.exists(), "consumed valid hourly should be deleted");
        assert!(!bad.exists(), "corrupt file should be renamed away");

        let mut corrupt = bad.into_os_string();
        corrupt.push(".corrupt");
        assert!(
            PathBuf::from(corrupt).exists(),
            "corrupt file should be quarantined to .corrupt"
        );

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
        assert_eq!(count, 1, "only the valid row should be present");
    }

    #[test]
    fn rollup_skips_when_all_inputs_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let bad_dir = data_dir.join(date).join("00");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let bad = bad_dir.join("svc.parquet");
        std::fs::write(&bad, b"").unwrap(); // zero-byte

        let day_dir = data_dir.join(date);
        // No error (nothing recoverable to retry), no daily file produced,
        // but the quarantine count surfaces the data-loss.
        let outcome = rollup_day_blocking(&day_dir, "svc", std::slice::from_ref(&bad), "2GB");
        assert!(
            outcome.result.is_ok(),
            "all-corrupt rollup returns Ok — nothing readable to retry"
        );
        assert_eq!(outcome.quarantined, 1, "the all-corrupt input is counted");

        assert!(
            !day_dir.join("svc.parquet").exists(),
            "no daily file from all-corrupt input"
        );
        assert!(!bad.exists(), "corrupt file should be quarantined");
    }

    #[test]
    fn rollup_keeps_quarantine_count_when_merge_errors() {
        // One input is corrupt (quarantined, count=1); a SECOND input passes the
        // magic-byte sniff but is unreadable by read_parquet (valid PAR1
        // bookends, bogus footer length), so the merge COPY hard-errors. The
        // quarantine count must still ride out on the outcome: a naive
        // `return Err` would drop it, and since the quarantined file is already
        // renamed `.corrupt`, a retry tick could never re-count it.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // Corrupt (zero-byte) input → quarantined.
        let corrupt_dir = data_dir.join(date).join("00");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        let corrupt = corrupt_dir.join("nginx.parquet");
        std::fs::write(&corrupt, b"").unwrap();

        // Sniff-valid but unreadable input → hard COPY error. 12 bytes: PAR1
        // header + a 0xFFFFFFFF "footer length" (points way past the file) +
        // PAR1 trailer. Passes is_valid_parquet; read_parquet cannot parse it.
        let unreadable_dir = data_dir.join(date).join("01");
        std::fs::create_dir_all(&unreadable_dir).unwrap();
        let unreadable = unreadable_dir.join("nginx.parquet");
        std::fs::write(&unreadable, b"PAR1\xff\xff\xff\xffPAR1").unwrap();

        let day_dir = data_dir.join(date);
        let outcome = rollup_day_blocking(
            &day_dir,
            "nginx",
            &[corrupt.clone(), unreadable.clone()],
            "2GB",
        );
        assert!(
            outcome.result.is_err(),
            "an unreadable surviving input must fail the merge"
        );
        assert_eq!(
            outcome.quarantined, 1,
            "the quarantine count must survive the hard merge error, not be dropped"
        );
    }

    #[test]
    fn recovery_discards_truncated_tmp() {
        // Crash mid-COPY: a truncated .tmp must NOT be promoted to canonical,
        // and the hourly files must be retained for a fresh rollup.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        let tmp_file = day_dir.join("nginx.parquet.tmp");

        std::fs::write(&tmp_file, b"PAR1trunc").unwrap();
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&f1)).unwrap();

        recover_rollup_markers(&day_dir).unwrap();

        assert!(
            !canonical.exists(),
            "must NOT promote a truncated tmp to canonical"
        );
        assert!(!tmp_file.exists(), "truncated tmp should be moved aside");
        assert!(f1.exists(), "hourly file retained for re-rollup");
        assert!(
            !day_dir.join(".rollup-nginx").exists(),
            "marker should be removed"
        );
    }

    #[test]
    fn compact_once_counts_quarantine_as_error() {
        // I3 counter wiring: an all-corrupt service/day must surface its
        // quarantine count through rollup_once → compact_once's returned u64
        // (which spawn_compaction folds into stats.total_errors).
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Historical (non-today) day with one all-corrupt service.
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let bad_dir = data_dir.join(&yesterday).join("00");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("svc.parquet"), b"").unwrap(); // zero-byte

        let rt = tokio::runtime::Runtime::new().unwrap();
        let errors = rt
            .block_on(compact_once(
                &wal_dir,
                &data_dir,
                Duration::from_secs(1),
                true,
                None,
                DEFAULT_CHUNK_SIZE,
                "2GB",
            ))
            .unwrap();

        assert!(
            errors > 0,
            "quarantine data-loss should flow out as a rollup failure/data-loss tally, got {errors}"
        );
        // No daily file from the all-corrupt input.
        assert!(!data_dir.join(&yesterday).join("svc.parquet").exists());
    }

    #[test]
    fn retire_merged_hourly_deletes_when_possible() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("nginx.parquet");
        std::fs::write(&f, b"data").unwrap();

        retire_merged_hourly(&f).unwrap();

        assert!(!f.exists(), "file should be deleted on the happy path");
        let mut aside = f.into_os_string();
        aside.push(".merged");
        assert!(
            !PathBuf::from(aside).exists(),
            "no .merged sibling when delete succeeds"
        );
    }

    #[test]
    fn retire_merged_hourly_renames_aside_when_delete_fails() {
        // remove_file on a directory fails → fall through to rename-aside.
        let tmp = tempfile::tempdir().unwrap();
        let dir_path = tmp.path().join("nginx.parquet");
        std::fs::create_dir(&dir_path).unwrap();

        retire_merged_hourly(&dir_path).unwrap();

        assert!(!dir_path.exists(), "original should be renamed away");
        let mut aside = dir_path.into_os_string();
        aside.push(".merged");
        assert!(
            PathBuf::from(aside).exists(),
            "should be renamed aside with .merged suffix"
        );
    }

    #[test]
    fn merged_suffix_is_not_picked_up_by_collect_service_files() {
        // A retired hourly (`.merged`) must never be re-merged: the glob keys
        // off the `parquet` extension, and `nginx.parquet.merged` has the
        // `merged` extension.
        let tmp = tempfile::tempdir().unwrap();
        let hour = tmp.path().join("00");
        std::fs::create_dir_all(&hour).unwrap();
        std::fs::write(hour.join("nginx.parquet.merged"), b"retired").unwrap();
        std::fs::write(hour.join("postgres.parquet"), b"live").unwrap();

        let groups = collect_service_files(&[hour]);
        assert!(
            !groups.contains_key("nginx.parquet"),
            "retired .merged file must not be collected"
        );
        assert!(
            !groups.contains_key("nginx"),
            "retired .merged file must not be collected"
        );
        assert_eq!(groups.len(), 1, "only the live parquet should be collected");
        assert!(groups.contains_key("postgres"));
    }

    #[test]
    fn rollup_does_not_duplicate_rows_when_hourly_survives() {
        // I4: simulate a delete that fails on the first rollup (the hourly is
        // retired aside as `.merged`), then run a second rollup over the same
        // day. The retired `.merged` file must not be re-merged, so the daily
        // row count stays put rather than doubling.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let f1 = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let day_dir = data_dir.join(date);

        // First rollup builds the daily file and retires the hourly.
        rollup_day_blocking(&day_dir, "nginx", std::slice::from_ref(&f1), "2GB")
            .result
            .unwrap();

        let daily = day_dir.join("nginx.parquet");
        assert!(daily.exists());

        // Simulate a "delete failed → retired aside" hourly that survived as
        // `.merged` (what retire_merged_hourly leaves behind on a bad delete).
        // It must NOT be re-collected nor re-merged.
        let stranded = day_dir.join("01").join("nginx.parquet.merged");
        std::fs::create_dir_all(stranded.parent().unwrap()).unwrap();
        std::fs::copy(&daily, &stranded).unwrap();

        // Re-discover service files for the day exactly as rollup_once does.
        let hour_dirs = collect_hour_dirs(&day_dir);
        let service_files = collect_service_files(&hour_dirs);

        // The stranded `.merged` file must not appear as an input.
        assert!(
            service_files.get("nginx").is_none_or(Vec::is_empty),
            "retired .merged hourly must not be re-collected for rollup"
        );

        // Second rollup over the canonical only (no live hourlies) — must not
        // double the count even though `.merged` bytes sit alongside.
        rollup_day_blocking(&day_dir, "nginx", &[], "2GB")
            .result
            .unwrap();

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
        assert_eq!(count, 1, "row count must not double from a stranded hourly");
    }

    #[test]
    fn recovery_keeps_marker_when_hourly_retire_fails() {
        // I4b: canonical present, marker lists an hourly that can be neither
        // deleted nor renamed aside. recover_rollup_markers must return Err
        // and leave the marker in place so the next pass retries.
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();

        let canonical = day_dir.join("nginx.parquet");
        std::fs::write(&canonical, b"consolidated").unwrap();

        // The "hourly" is a non-empty directory: remove_file fails (EISDIR),
        // and rename-aside fails too because a non-empty `.merged` directory
        // already occupies the target (ENOTEMPTY).
        let hourly = day_dir.join("01").join("nginx.parquet");
        std::fs::create_dir_all(&hourly).unwrap();
        std::fs::write(hourly.join("blocker"), b"x").unwrap();
        let aside = day_dir.join("01").join("nginx.parquet.merged");
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("blocker"), b"x").unwrap();

        let marker = day_dir.join(".rollup-nginx");
        write_rollup_marker(&day_dir, "nginx", std::slice::from_ref(&hourly)).unwrap();
        assert!(marker.exists());

        let result = recover_rollup_markers(&day_dir);
        assert!(
            result.is_err(),
            "recovery should fail when an hourly can be neither removed nor retired"
        );
        assert!(
            marker.exists(),
            "marker must survive so the next recovery pass retries"
        );
    }

    #[test]
    fn retire_merged_hourly_is_idempotent_on_missing_file() {
        // I4c: retiring an already-gone hourly is a no-op success. The goal
        // ("this file is no longer a re-mergeable hourly") is already met, so a
        // missing source must NOT propagate as Err (which would wedge recovery).
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("never-existed.parquet");
        assert!(!gone.exists());

        retire_merged_hourly(&gone).expect("missing hourly should retire as Ok");

        let mut aside = gone.into_os_string();
        aside.push(".merged");
        assert!(
            !PathBuf::from(aside).exists(),
            "must not conjure a .merged sibling for a phantom-missing source"
        );
    }

    #[test]
    fn recovery_completes_after_partial_hourly_cleanup() {
        // I4c regression: a crash MID-cleanup-loop deleted SOME hourlies but
        // left the marker (it lists ALL of them). On the next recovery pass the
        // marker is replayed verbatim — retiring an already-gone hourly must be
        // a no-op success so recovery still retires the surviving hourly AND
        // deletes the marker. The old code returned Err on the first phantom,
        // short-circuiting before the marker delete → permanently wedged.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let date = "2026-01-15";

        // Two hourlies were originally merged; the canonical already exists.
        let r1 = r#"{"timestamp":"2026-01-15T01:00:00Z","service":"nginx","msg":"a"}"#;
        let r2 = r#"{"timestamp":"2026-01-15T02:00:00Z","service":"nginx","msg":"b"}"#;
        let already_deleted = write_hourly_parquet(&data_dir, date, "01", "nginx", &[r1]);
        let still_present = write_hourly_parquet(&data_dir, date, "02", "nginx", &[r2]);

        let day_dir = data_dir.join(date);
        let canonical = day_dir.join("nginx.parquet");
        std::fs::write(&canonical, b"consolidated").unwrap();

        // Marker lists BOTH (written before the cleanup loop ran).
        write_rollup_marker(
            &day_dir,
            "nginx",
            &[already_deleted.clone(), still_present.clone()],
        )
        .unwrap();
        let marker = day_dir.join(".rollup-nginx");
        assert!(marker.exists());

        // Simulate the partial cleanup: the first hourly was deleted before the
        // crash, the second was not.
        std::fs::remove_file(&already_deleted).unwrap();
        assert!(!already_deleted.exists());
        assert!(still_present.exists());

        recover_rollup_markers(&day_dir).expect("recovery must tolerate a phantom-missing hourly");

        // The surviving hourly is retired (deleted, or renamed aside as .merged).
        let mut aside = still_present.clone().into_os_string();
        aside.push(".merged");
        assert!(
            !still_present.exists() || PathBuf::from(aside).exists(),
            "the present hourly must be retired (gone or .merged)"
        );
        // The marker MUST be removed so later compaction ticks don't replay it.
        assert!(
            !marker.exists(),
            "marker must be removed after recovery completes"
        );
    }

    #[test]
    fn quarantine_file_signals_rename_failure() {
        // E3: a failed quarantine-rename is a hard error, not a swallowed log.
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("svc.parquet");
        std::fs::write(&bad, b"corrupt").unwrap();

        // Pre-create the `.corrupt` target as a non-empty directory so the
        // rename fails (cannot rename a file onto a non-empty directory).
        let target = tmp.path().join("svc.parquet.corrupt");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("blocker"), b"x").unwrap();

        let result = quarantine_file(&bad, "svc", "rollup_quarantine");
        assert!(
            result.is_err(),
            "quarantine must surface a rename failure as Err"
        );
        assert!(bad.exists(), "original stays put when quarantine fails");
    }
}
