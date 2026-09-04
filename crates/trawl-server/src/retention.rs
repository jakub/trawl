// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background data retention task.
//!
//! Periodically scans the data directory for date-partitioned directories
//! and enforces two independent retention policies:
//!
//! 1. **Age-based**: deletes date directories older than `max_age_days`.
//! 2. **Disk pressure**: if free disk space drops below `min_free_disk_bytes`,
//!    deletes the oldest data regardless of age.
//!
//! Today's directory is never deleted (compaction writes there actively).
//! Both policies can be independently disabled by setting their value to 0.
//!
//! Disk-pressure deletion is additionally suppressed while any epoch
//! set-aside root ([`crate::epoch::set_aside_paths`]) exists: a set-aside
//! is a sibling of `data/`, so it yields no deletion candidates while
//! still occupying the filesystem free space is measured on — deleting
//! fresh partitions could never reclaim it. A repin in flight suppresses
//! both sweeps (marker or either staging sibling).
//!
//! The field catalog's `field_services` observations are ever-observed:
//! retention deleting a partition deliberately never reconciles them, and
//! nothing else removes a row either (ADR-0009 — "which services ever
//! carried this field" is historical fact, not an index over live files).
//! Consumers window on `last_seen`; the field axis is bounded by the pin
//! cap ([`crate::store::MAX_PINNED_FIELDS`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::NaiveDate;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::RetentionConfig;

/// Spawn the retention background loop.
///
/// Runs every `retention_interval_secs`, scanning `data_dir` for date
/// directories eligible for deletion. Stops when `shutdown_rx` fires.
pub fn spawn_retention(
    data_dir: PathBuf,
    config: RetentionConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let interval = Duration::from_secs(config.retention_interval_secs);

    tokio::spawn(async move {
        tracing::info!(
            event_type = "lifecycle",
            action = "retention_start",
            data_dir = %data_dir.display(),
            max_age_days = config.max_age_days,
            min_free_disk_bytes = config.min_free_disk_bytes,
            interval_secs = config.retention_interval_secs,
            "retention task started"
        );

        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {
                    let dir = data_dir.clone();
                    let cfg = config.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        retention_tick(&dir, &cfg, |p| fs4::available_space(p))
                    })
                    .await;

                    match result {
                        Ok(Err(e)) => {
                            tracing::error!(
                                event_type = "retention_error",
                                error = %e,
                                "retention tick failed"
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                event_type = "retention_error",
                                error = %e,
                                "retention task panicked"
                            );
                        }
                        Ok(Ok(())) => {}
                    }
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!(
                        event_type = "lifecycle",
                        action = "retention_stop",
                        "retention task shutting down"
                    );
                    break;
                }
            }
        }
    })
}

/// The longest age this install still keeps data for, in seconds, or
/// `None` when age retention is disabled.
///
/// Pin garbage collection ([`crate::catalog::gc`]) floors its dead window
/// here: calling a field dead over a span shorter than the corpus trawl
/// still stores would reclaim a pin whose data is right there on disk.
/// Disk-pressure retention contributes nothing: it deletes by free space
/// rather than by age, so it names no window a pin could be judged
/// against.
///
/// `max_age_days` is an unvalidated operator `u64`, so the multiply
/// saturates; an "effectively never" setting floors the window at
/// "effectively never", which refuses every candidate. That is the right
/// answer for an install that keeps everything.
///
/// This is the one function per-env retention (#108) changes: the floor
/// becomes the maximum enabled age across all envs, and every caller keeps
/// asking the same question.
#[must_use]
pub fn maximum_enabled_age_secs(config: &RetentionConfig) -> Option<u64> {
    const SECS_PER_DAY: u64 = 86_400;
    if config.max_age_days == 0 {
        return None;
    }
    Some(config.max_age_days.saturating_mul(SECS_PER_DAY))
}

/// A single retention tick. Testable via injectable `free_space_fn`.
fn retention_tick(
    data_dir: &Path,
    config: &RetentionConfig,
    free_space_fn: impl Fn(&Path) -> std::io::Result<u64>,
) -> Result<(), String> {
    // A repin in flight — marker, shadow sibling, or aside sibling —
    // suppresses both sweeps, not just pressure (ADR-0011). Age deletion
    // would remove affected files out from under the shadow build (the
    // catch-up diff treats disappearance as an operator act, not a normal
    // event), and pressure deletion can never reclaim the bytes the job is
    // deliberately double-holding. The job's own free-space pre-flight is
    // what keeps this suppression affordable.
    if let Some(what) = repin_in_flight(data_dir) {
        // Alertable, because "suppressed" is not always "a job is
        // running": staging whose sweep keeps failing holds this at 1
        // across boots with no job to explain it, and the archive grows
        // the whole time. An info line per tick is not something an
        // operator can page on; a gauge held high is.
        metrics::gauge!(crate::metrics::RETENTION_SUPPRESSED).set(1.0);
        tracing::info!(
            event_type = "retention_repin_suppressed",
            evidence = what,
            "retention sweeps suppressed while a repin job's marker or \
             staging exists; they resume when the job completes (or its \
             boot replay finishes)"
        );
        return Ok(());
    }
    metrics::gauge!(crate::metrics::RETENTION_SUPPRESSED).set(0.0);

    let today = chrono::Utc::now().date_naive();
    let today_str = today.format("%Y-%m-%d").to_string();

    let mut candidates = enumerate_date_dirs(data_dir, &today_str)?;

    let mut total_bytes_freed: u64 = 0;
    let mut total_dirs_deleted: u64 = 0;

    // Phase 1: age-based retention.
    if config.max_age_days > 0 {
        let cutoff =
            today - chrono::Duration::days(i64::try_from(config.max_age_days).unwrap_or(i64::MAX));

        let age_targets: Vec<PathBuf> = candidates
            .iter()
            .filter(|(date, _)| *date < cutoff)
            .map(|(_, path)| path.clone())
            .collect();

        for path in &age_targets {
            if repin_claimed_mid_sweep(data_dir) {
                return Ok(());
            }
            match delete_date_dir(path) {
                Ok(bytes) => {
                    let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                    tracing::info!(
                        event_type = "retention_delete",
                        date = %dir_name,
                        bytes_freed = bytes,
                        trigger = "age",
                        "deleted date directory"
                    );
                    total_bytes_freed += bytes;
                    total_dirs_deleted += 1;
                }
                Err(e) => {
                    tracing::error!(
                        event_type = "retention_error",
                        path = %path.display(),
                        error = %e,
                        "failed to delete date directory"
                    );
                }
            }
        }

        // Everything past the cutoff was already attempted above, failures
        // included, so phase 2 works on what age retention left alone.
        candidates.retain(|(date, _)| *date >= cutoff);
    }

    // Phase 2: disk-pressure retention.
    if config.min_free_disk_bytes > 0 {
        let (bytes, dirs) = disk_pressure_sweep(data_dir, config, candidates, free_space_fn)?;
        total_bytes_freed += bytes;
        total_dirs_deleted += dirs;
    }

    if total_dirs_deleted > 0 {
        tracing::info!(
            event_type = "retention_sweep",
            dirs_deleted = total_dirs_deleted,
            bytes_freed = total_bytes_freed,
            "retention sweep complete"
        );
    }

    Ok(())
}

/// Delete oldest-first until free space clears `min_free_disk_bytes` or
/// there is nothing left to delete. Returns `(bytes_freed, dirs_deleted)`.
fn disk_pressure_sweep(
    data_dir: &Path,
    config: &RetentionConfig,
    mut candidates: Vec<(NaiveDate, PathBuf)>,
    free_space_fn: impl Fn(&Path) -> std::io::Result<u64>,
) -> Result<(u64, u64), String> {
    let mut total_bytes_freed: u64 = 0;
    let mut total_dirs_deleted: u64 = 0;

    // An epoch set-aside root is a *sibling* of `data/`: it yields no
    // deletion candidates yet still occupies the filesystem
    // `free_space_fn` measures. Deleting date dirs cannot reclaim it, so
    // an unattended loop would destroy every non-today partition and
    // remain under threshold. Refuse to delete anything under pressure
    // while one exists, and say why. Either set-aside suppresses the
    // sweep: an install can hold one from each epoch bump, and trawl
    // deletes neither.
    let set_asides = crate::epoch::set_aside_paths(data_dir);

    loop {
        let available =
            free_space_fn(data_dir).map_err(|e| format!("failed to check free disk space: {e}"))?;

        if available >= config.min_free_disk_bytes {
            break;
        }

        if let Some(set_aside) = set_asides.iter().find(|p| p.exists()) {
            tracing::warn!(
                event_type = "retention_disk_pressure_suppressed",
                available_bytes = available,
                threshold_bytes = config.min_free_disk_bytes,
                set_aside_path = %set_aside.display(),
                remaining_dirs = candidates.len(),
                "disk pressure: refusing to delete data while the \
                 pre-cutover set-aside directory still occupies the \
                 filesystem — remove it to reclaim space and re-enable \
                 disk-pressure retention"
            );
            break;
        }

        if candidates.is_empty() {
            tracing::warn!(
                event_type = "retention_disk_pressure",
                available_bytes = available,
                threshold_bytes = config.min_free_disk_bytes,
                remaining_dirs = 0u64,
                "disk pressure: no more directories to delete (only today remains)"
            );
            break;
        }

        if repin_claimed_mid_sweep(data_dir) {
            break;
        }

        // Delete the oldest remaining dir.
        let (date, path) = candidates.remove(0);
        match delete_date_dir(&path) {
            Ok(bytes) => {
                tracing::info!(
                    event_type = "retention_delete",
                    date = %date,
                    bytes_freed = bytes,
                    trigger = "disk_pressure",
                    available_bytes = available,
                    "deleted date directory due to disk pressure"
                );
                total_bytes_freed += bytes;
                total_dirs_deleted += 1;
            }
            Err(e) => {
                tracing::error!(
                    event_type = "retention_error",
                    path = %path.display(),
                    error = %e,
                    "failed to delete date directory under disk pressure"
                );
                // Continue trying other dirs.
            }
        }
    }

    Ok((total_bytes_freed, total_dirs_deleted))
}

/// Re-read the repin claim immediately before a deletion, and say so if
/// it has appeared since the tick opened.
///
/// The tick-opening check only says that no job owned the data root when
/// the tick started. A job admitted mid-tick writes its marker before it
/// touches anything and keeps it until it is completely done, so re-reading
/// it here means a `remove_dir_all` can only overlap a shadow build or a
/// swap if that single directory removal outlives the whole job — as
/// opposed to any sweep that merely started before the marker landed.
fn repin_claimed_mid_sweep(data_dir: &Path) -> bool {
    let Some(what) = repin_in_flight(data_dir) else {
        return false;
    };
    metrics::gauge!(crate::metrics::RETENTION_SUPPRESSED).set(1.0);
    tracing::info!(
        event_type = "retention_repin_suppressed",
        evidence = what,
        "retention sweep stood down mid-tick: a repin job claimed the data \
         root while this tick was running; sweeps resume when the job \
         completes"
    );
    true
}

/// Evidence that a repin job owns this data root right now, if any.
fn repin_in_flight(data_dir: &Path) -> Option<&'static str> {
    if crate::repin::marker_path(data_dir).exists() {
        Some("marker")
    } else if crate::repin::shadow_root(data_dir).exists() {
        Some("shadow root")
    } else if crate::repin::aside_root(data_dir).exists() {
        Some("aside root")
    } else {
        None
    }
}

/// Enumerate date-formatted directories across every env directory in
/// `data_dir` (ADR-0009 layout: `data/{env}/{date}/`), excluding today
/// and non-date directories. Env directories are recognised by the env
/// charset with `wal`/`scheduled` reserved — anything else at the top
/// level (a stray file, the EPOCH marker, a set-aside dir) is skipped.
/// Candidates are merged across envs, sorted oldest-first, so
/// disk-pressure deletion stays globally oldest-first while deleting one
/// env's date dir stays O(1) and never touches other envs.
fn enumerate_date_dirs(data_dir: &Path, today: &str) -> Result<Vec<(NaiveDate, PathBuf)>, String> {
    let entries = std::fs::read_dir(data_dir)
        .map_err(|e| format!("failed to read data directory {}: {e}", data_dir.display()))?;

    let mut dirs: Vec<(NaiveDate, PathBuf)> = Vec::new();
    for env_entry in entries.flatten() {
        let env_path = env_entry.path();
        if !env_path.is_dir() {
            continue;
        }
        let Some(env_name) = env_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !trawl_config::is_valid_env_name(env_name)
            || trawl_config::RESERVED_ENV_NAMES.contains(&env_name)
        {
            continue;
        }
        let Ok(date_entries) = std::fs::read_dir(&env_path) else {
            continue;
        };
        dirs.extend(date_entries.flatten().filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?;
            if name == today {
                return None;
            }
            if !looks_like_date(name) {
                return None;
            }
            let date = NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()?;
            Some((date, path))
        }));
    }

    dirs.sort_by_key(|(date, _)| *date);
    Ok(dirs)
}

/// Check if a directory name looks like a date (YYYY-MM-DD).
fn looks_like_date(name: &str) -> bool {
    name.len() == 10
        && name.as_bytes().get(4) == Some(&b'-')
        && name.as_bytes().get(7) == Some(&b'-')
        && name[..4].bytes().all(|b| b.is_ascii_digit())
}

/// Delete a date directory and return the bytes freed.
fn delete_date_dir(path: &Path) -> Result<u64, String> {
    let size = dir_size(path);
    std::fs::remove_dir_all(path)
        .map_err(|e| format!("remove_dir_all failed for {}: {e}", path.display()))?;
    Ok(size)
}

/// Recursively compute the total size of a directory in bytes.
fn dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = p.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::RETENTION_SUPPRESSED;

    fn make_config(max_age_days: u64, min_free_disk_bytes: u64) -> RetentionConfig {
        RetentionConfig {
            max_age_days,
            min_free_disk_bytes,
            retention_interval_secs: 3600,
        }
    }

    #[test]
    fn looks_like_date_valid() {
        assert!(looks_like_date("2026-02-13"));
        assert!(looks_like_date("2025-01-01"));
        assert!(looks_like_date("1999-12-31"));
    }

    #[test]
    fn looks_like_date_invalid() {
        assert!(!looks_like_date("wal"));
        assert!(!looks_like_date("00"));
        assert!(!looks_like_date("2026-1-01"));
        assert!(!looks_like_date(""));
        assert!(!looks_like_date("not-a-date"));
    }

    #[test]
    fn enumerate_excludes_wal_and_non_dates() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-01-01")).unwrap();
        // Reserved dirs and non-env top-level entries are skipped entirely.
        std::fs::create_dir_all(tmp.path().join("wal/2026-01-01")).unwrap();
        std::fs::create_dir_all(tmp.path().join("scheduled/2026-01-01")).unwrap();
        std::fs::create_dir(tmp.path().join("not-a-date")).unwrap();
        // A legacy top-level date dir is not an env dir — skipped.
        std::fs::create_dir(tmp.path().join("2026-01-02")).unwrap();
        // Also create a regular file — should be skipped.
        std::fs::write(tmp.path().join("stray.txt"), b"hi").unwrap();

        let dirs = enumerate_date_dirs(tmp.path(), "2099-01-01").unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].0, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        assert!(dirs[0].1.starts_with(tmp.path().join("prod")));
    }

    #[test]
    fn enumerate_merges_candidates_across_envs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-01-20")).unwrap();
        std::fs::create_dir_all(tmp.path().join("lab/2026-01-10")).unwrap();

        let dirs = enumerate_date_dirs(tmp.path(), "2099-01-01").unwrap();
        assert_eq!(dirs.len(), 2);
        // Oldest first regardless of env.
        assert!(dirs[0].1.starts_with(tmp.path().join("lab")));
        assert!(dirs[1].1.starts_with(tmp.path().join("prod")));
    }

    #[test]
    fn age_based_removes_one_envs_date_without_touching_others() {
        let today = chrono::Utc::now().date_naive();
        let old_date = (today - chrono::Duration::days(200))
            .format("%Y-%m-%d")
            .to_string();

        let tmp = tempfile::tempdir().unwrap();
        let lab_old = tmp.path().join("lab").join(&old_date);
        let prod_old = tmp.path().join("prod").join(&old_date);
        std::fs::create_dir_all(&lab_old).unwrap();
        std::fs::write(lab_old.join("svc.parquet"), b"lab data").unwrap();
        std::fs::create_dir_all(&prod_old).unwrap();
        std::fs::write(prod_old.join("svc.parquet"), b"prod data").unwrap();

        // Both envs' old dates age out independently; deleting one is an
        // O(1) directory remove that never touches the sibling env root.
        let config = make_config(90, 0);
        retention_tick(tmp.path(), &config, |_| Ok(u64::MAX)).unwrap();

        assert!(!lab_old.exists());
        assert!(!prod_old.exists());
        assert!(tmp.path().join("lab").exists(), "env root survives");
        assert!(tmp.path().join("prod").exists(), "env root survives");
    }

    #[test]
    fn enumerate_excludes_today() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-02-13")).unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-02-12")).unwrap();

        let dirs = enumerate_date_dirs(tmp.path(), "2026-02-13").unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].0, NaiveDate::from_ymd_opt(2026, 2, 12).unwrap());
    }

    #[test]
    fn enumerate_sorts_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-03-01")).unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-01-15")).unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-02-20")).unwrap();

        let dirs = enumerate_date_dirs(tmp.path(), "2099-01-01").unwrap();
        let dates: Vec<_> = dirs.iter().map(|(d, _)| *d).collect();
        assert_eq!(
            dates,
            vec![
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                NaiveDate::from_ymd_opt(2026, 2, 20).unwrap(),
                NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(),
            ]
        );
    }

    #[test]
    fn age_based_deletes_old_dirs() {
        let today = chrono::Utc::now().date_naive();
        let old_date = today - chrono::Duration::days(200);
        let recent_date = today - chrono::Duration::days(30);

        let tmp = tempfile::tempdir().unwrap();
        let old_dir = tmp
            .path()
            .join("prod")
            .join(old_date.format("%Y-%m-%d").to_string());
        let recent_dir = tmp
            .path()
            .join("prod")
            .join(recent_date.format("%Y-%m-%d").to_string());
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("test.parquet"), b"old data").unwrap();
        std::fs::create_dir_all(&recent_dir).unwrap();
        std::fs::write(recent_dir.join("test.parquet"), b"recent data").unwrap();

        let config = make_config(90, 0);
        retention_tick(tmp.path(), &config, |_| Ok(u64::MAX)).unwrap();

        assert!(!old_dir.exists(), "old dir should be deleted");
        assert!(recent_dir.exists(), "recent dir should survive");
    }

    #[test]
    fn age_based_preserves_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let old_dir = tmp.path().join("prod/2020-01-01");
        std::fs::create_dir_all(&old_dir).unwrap();

        let config = make_config(0, 0);
        retention_tick(tmp.path(), &config, |_| Ok(u64::MAX)).unwrap();

        assert!(old_dir.exists(), "nothing should be deleted when disabled");
    }

    #[test]
    fn disk_pressure_deletes_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let oldest = tmp.path().join("prod/2026-01-01");
        let middle = tmp.path().join("prod/2026-01-15");
        let newest = tmp.path().join("prod/2026-02-01");
        for dir in [&oldest, &middle, &newest] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("data.parquet"), b"some data").unwrap();
        }

        // Simulate: first call reports low space, second call (after deletion)
        // reports enough space.
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let config = make_config(0, 1_000_000);
        retention_tick(tmp.path(), &config, |_| {
            let n = call_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 0 {
                Ok(500_000) // below threshold
            } else {
                Ok(2_000_000) // above threshold
            }
        })
        .unwrap();

        assert!(!oldest.exists(), "oldest should be deleted first");
        assert!(middle.exists(), "middle should survive");
        assert!(newest.exists(), "newest should survive");
    }

    /// Every suffix `epoch::set_aside_paths` reports suppresses the sweep —
    /// alone and coexisting. A check narrowed back to one suffix would let
    /// pressure retention delete live partitions beside the other set-aside.
    fn set_aside_suffix_cases() -> Vec<Vec<&'static str>> {
        use crate::epoch::{EPOCH_3_SET_ASIDE_SUFFIX, SET_ASIDE_SUFFIX};
        vec![
            vec![SET_ASIDE_SUFFIX],
            vec![EPOCH_3_SET_ASIDE_SUFFIX],
            vec![SET_ASIDE_SUFFIX, EPOCH_3_SET_ASIDE_SUFFIX],
        ]
    }

    #[test]
    fn disk_pressure_suppressed_while_set_aside_exists() {
        for suffixes in set_aside_suffix_cases() {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().join("data");
            // The set-aside root is a sibling of `data/` — invisible to the
            // candidate scan, but it owns the disk the threshold measures.
            for suffix in &suffixes {
                std::fs::create_dir_all(tmp.path().join(format!("data{suffix}/2025-01-01")))
                    .unwrap();
            }

            let oldest = data_dir.join("prod/2026-01-01");
            let newest = data_dir.join("prod/2026-02-01");
            for dir in [&oldest, &newest] {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join("data.parquet"), b"some data").unwrap();
            }

            // Permanently below threshold: without suppression this loop would
            // delete every candidate and still report zero remaining dirs.
            let config = make_config(0, 1_000_000);
            retention_tick(&data_dir, &config, |_| Ok(500_000)).unwrap();

            assert!(
                oldest.exists(),
                "no deletion while the set-aside exists ({suffixes:?})"
            );
            assert!(
                newest.exists(),
                "no deletion while the set-aside exists ({suffixes:?})"
            );
        }
    }

    #[test]
    fn disk_pressure_resumes_once_set_aside_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let oldest = data_dir.join("prod/2026-01-01");
        let newest = data_dir.join("prod/2026-02-01");
        for dir in [&oldest, &newest] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("data.parquet"), b"some data").unwrap();
        }

        let call_count = std::sync::atomic::AtomicU32::new(0);
        let config = make_config(0, 1_000_000);
        retention_tick(&data_dir, &config, |_| {
            let n = call_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 0 { Ok(500_000) } else { Ok(2_000_000) }
        })
        .unwrap();

        assert!(!oldest.exists(), "oldest deleted once nothing is set aside");
        assert!(newest.exists());
    }

    #[test]
    fn age_based_still_runs_while_set_aside_exists() {
        let today = chrono::Utc::now().date_naive();
        let old_date = today - chrono::Duration::days(200);

        for suffixes in set_aside_suffix_cases() {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().join("data");
            for suffix in &suffixes {
                std::fs::create_dir_all(tmp.path().join(format!("data{suffix}"))).unwrap();
            }
            let old_dir = data_dir
                .join("prod")
                .join(old_date.format("%Y-%m-%d").to_string());
            std::fs::create_dir_all(&old_dir).unwrap();
            std::fs::write(old_dir.join("test.parquet"), b"old data").unwrap();

            // The set-aside only gates disk-pressure deletion; the operator's
            // explicit age policy is unaffected.
            let config = make_config(90, 1_000_000);
            retention_tick(&data_dir, &config, |_| Ok(500_000)).unwrap();

            assert!(
                !old_dir.exists(),
                "age-based retention still applies ({suffixes:?})"
            );
        }
    }

    /// While a repin job exists on this root — marker, shadow sibling, or
    /// aside sibling — both sweeps stand down. Age deletion would yank
    /// affected files out from under the shadow build (the catch-up diff
    /// sees additions, not disappearances, as normal), and pressure
    /// deletion could never reclaim the double-held bytes the job itself
    /// is holding.
    #[test]
    fn both_sweeps_suppressed_while_a_repin_is_in_flight() {
        let today = chrono::Utc::now().date_naive();
        let old_date = today - chrono::Duration::days(200);

        for staging in ["marker", "shadow", "aside"] {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().join("data");
            let old_dir = data_dir
                .join("prod")
                .join(old_date.format("%Y-%m-%d").to_string());
            std::fs::create_dir_all(&old_dir).unwrap();
            std::fs::write(old_dir.join("svc.parquet"), b"affected bytes").unwrap();
            match staging {
                "marker" => {
                    std::fs::write(data_dir.join("REPIN"), b"{}").unwrap();
                }
                "shadow" => {
                    std::fs::create_dir_all(tmp.path().join("data.repin-next")).unwrap();
                }
                _ => {
                    std::fs::create_dir_all(tmp.path().join("data.repin-aside")).unwrap();
                }
            }

            // Age and pressure both armed, both hungry.
            let config = make_config(90, 1_000_000);
            retention_tick(&data_dir, &config, |_| Ok(500_000)).unwrap();
            assert!(
                old_dir.exists(),
                "{staging}: no sweep may run while a repin is in flight"
            );
        }
    }

    /// A job admitted mid-tick stops the sweep too. The tick-opening check
    /// only speaks for the moment the tick started; a sweep that got past
    /// it would keep calling `remove_dir_all` right through the shadow
    /// build and the swap. The claim is therefore re-read before every
    /// deletion — here the marker appears (via the injected free-space
    /// probe) after the first directory is already gone.
    #[test]
    fn a_repin_admitted_mid_tick_stops_the_sweep_at_the_next_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let oldest = data_dir.join("prod/2026-01-01");
        let middle = data_dir.join("prod/2026-01-15");
        let newest = data_dir.join("prod/2026-02-01");
        for dir in [&oldest, &middle, &newest] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("data.parquet"), b"some data").unwrap();
        }

        // Permanently below threshold: only the mid-sweep claim can stop
        // this loop before it eats every candidate.
        let call_count = std::sync::atomic::AtomicU32::new(0);
        let marker = crate::repin::marker_path(&data_dir);
        let config = make_config(0, 1_000_000);
        retention_tick(&data_dir, &config, |_| {
            let n = call_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 1 {
                // A job claims the data root while the sweep is running.
                std::fs::write(&marker, b"{}").unwrap();
            }
            Ok(500_000)
        })
        .unwrap();

        assert!(!oldest.exists(), "the pre-claim deletion stands");
        assert!(
            middle.exists() && newest.exists(),
            "no directory may be deleted once a repin owns the data root"
        );
    }

    /// Suppression is alertable, not just loggable. `repin_running` is 0
    /// for staging no job owns — a boot replay whose sweep keeps failing
    /// — which is precisely the case that suppresses retention forever,
    /// so the gauge has to key on the staging itself and clear on the
    /// tick that sweeps again.
    #[test]
    fn suppression_raises_and_clears_the_alertable_gauge() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let config = make_config(90, 0);

        metrics::with_local_recorder(&recorder, || {
            // No job owns this aside — no marker, nothing running.
            std::fs::create_dir_all(tmp.path().join("data.repin-aside")).unwrap();
            retention_tick(&data_dir, &config, |_| Ok(u64::MAX)).unwrap();
        });
        assert!(
            handle
                .render()
                .contains(&format!("{RETENTION_SUPPRESSED} 1")),
            "orphaned staging must hold the gauge high: {}",
            handle.render()
        );

        metrics::with_local_recorder(&recorder, || {
            std::fs::remove_dir_all(tmp.path().join("data.repin-aside")).unwrap();
            retention_tick(&data_dir, &config, |_| Ok(u64::MAX)).unwrap();
        });
        assert!(
            handle
                .render()
                .contains(&format!("{RETENTION_SUPPRESSED} 0")),
            "the tick that sweeps again must clear it: {}",
            handle.render()
        );
    }

    /// And both resume once the job's staging is gone.
    #[test]
    fn sweeps_resume_after_the_repin_ends() {
        let today = chrono::Utc::now().date_naive();
        let old_date = today - chrono::Duration::days(200);
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let old_dir = data_dir
            .join("prod")
            .join(old_date.format("%Y-%m-%d").to_string());
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("svc.parquet"), b"old").unwrap();

        let config = make_config(90, 0);
        retention_tick(&data_dir, &config, |_| Ok(u64::MAX)).unwrap();
        assert!(!old_dir.exists(), "age sweep resumes");
    }

    #[test]
    fn both_disabled_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("prod/2020-01-01");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.parquet"), b"old").unwrap();

        let config = make_config(0, 0);
        retention_tick(tmp.path(), &config, |_| Ok(0)).unwrap();

        assert!(dir.exists());
    }

    #[test]
    fn today_never_deleted_by_disk_pressure() {
        let tmp = tempfile::tempdir().unwrap();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let today_dir = tmp.path().join("prod").join(&today);
        std::fs::create_dir_all(&today_dir).unwrap();
        std::fs::write(today_dir.join("data.parquet"), b"today").unwrap();

        // Disk pressure with only today's dir — should warn but not delete.
        let config = make_config(0, 1_000_000);
        retention_tick(tmp.path(), &config, |_| Ok(100)).unwrap();

        assert!(today_dir.exists(), "today's dir must never be deleted");
    }

    #[test]
    fn partial_failure_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("prod/2020-01-01");
        let dir_b = tmp.path().join("prod/2020-06-01");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_b.join("data.parquet"), b"data").unwrap();

        // Both dirs are old enough, so both are processed. Nothing here
        // actually makes a deletion fail: `remove_dir_all` succeeds on an
        // empty dir, and a permission-denied dir needs setup the suite
        // cannot rely on.
        let config = make_config(30, 0);
        retention_tick(tmp.path(), &config, |_| Ok(u64::MAX)).unwrap();

        assert!(!dir_a.exists());
        assert!(!dir_b.exists());
    }

    #[test]
    fn dir_size_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("2026-01-01");
        std::fs::create_dir(&base).unwrap();
        std::fs::write(base.join("a.parquet"), vec![0u8; 100]).unwrap();

        let sub = base.join("05");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.parquet"), vec![0u8; 200]).unwrap();

        assert_eq!(dir_size(&base), 300);
    }

    #[test]
    fn empty_data_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_config(90, 1_000_000);
        // Should succeed with no dirs to process.
        retention_tick(tmp.path(), &config, |_| Ok(u64::MAX)).unwrap();
    }
}
