// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Background data retention task.
//!
//! Periodically scans the data directory for date-partitioned directories
//! and enforces two independent retention policies (ADR-0018 §1-5):
//!
//! 1. **Age-based**: deletes a date directory once it is older than its
//!    env's effective `max_age_days` — the `[retention.env.<name>]`
//!    override when one exists, else the global value
//!    ([`RetentionConfig::max_age_days_for`]). 0 keeps that env's data
//!    forever as far as age goes.
//! 2. **Disk pressure**: if free disk space drops below
//!    `min_free_disk_bytes`, deletes by expiry ratio — a directory's age
//!    over its env's effective `max_age_days`, highest ratio first — until
//!    the threshold clears. A keep-forever env ranks last but stays
//!    eligible: nothing is exempt from pressure, because a sweep that
//!    cannot reach the floor is a wedged daemon. Age is a maximum, never a
//!    guaranteed minimum.
//!
//! Today's directory is never deleted (compaction writes there actively).
//! Disk-pressure retention is disabled by setting its threshold to 0.
//!
//! A repin in flight suppresses both sweeps (marker or either staging sibling).
//! A date directory that a pending publication marker claims (ADR-0041) is
//! kept, and a WAL root whose markers cannot be read suppresses both sweeps.
//!
//! The field catalog's `field_services` observations are ever-observed:
//! retention deleting a partition deliberately never reconciles them, and
//! nothing else removes a row either (ADR-0009 — "which services ever
//! carried this field" is historical fact, not an index over live files).
//! Consumers window on `last_seen`; the field axis is bounded by the pin
//! cap ([`crate::store::MAX_PINNED_FIELDS`]).

use std::cmp::Ordering;
use std::num::NonZeroU64;
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
    wal_dir: PathBuf,
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
                    let wal = wal_dir.clone();
                    let cfg = config.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        retention_tick(&dir, &wal, &cfg, |p| fs4::available_space(p))
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
                                error = %crate::error::join_failure_text("retention", e),
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
/// `None` when any env keeps its data forever.
///
/// The set is the global `max_age_days` plus every `[retention.env.*]`
/// entry. The global always participates: an env with no entry inherits
/// it, and an install always has envs the config never names. A single 0
/// anywhere is `None` — that env keeps everything, so no finite span
/// bounds what the corpus still holds. Otherwise the answer is the
/// maximum, never the minimum: the shortest-lived env says nothing about
/// data a longer-lived one still stores.
///
/// Config domain only. Which env directories exist on disk never enters
/// it: a horizon that read the filesystem would move as data landed and
/// aged out, and both callers want a per-process constant.
///
/// Two callers ask the same question. `/api/v1/schema` windows catalog
/// fields on `last_seen` against it, so autocomplete stops offering
/// fields whose data has aged out everywhere. Pin garbage collection
/// ([`crate::catalog::gc`]) floors its dead window here: calling a field
/// dead over a span shorter than the corpus trawl still stores would
/// reclaim a pin whose data is right there on disk. Disk-pressure
/// retention contributes nothing: it deletes by free space rather than by
/// age, so it names no window a pin could be judged against.
///
/// `max_age_days` is an unvalidated operator `u64`, so the multiply
/// saturates; an "effectively never" setting floors the window at
/// "effectively never", which refuses every candidate. That is the right
/// answer for an install that keeps everything.
#[must_use]
pub fn maximum_enabled_age_secs(config: &RetentionConfig) -> Option<u64> {
    const SECS_PER_DAY: u64 = 86_400;
    let mut longest = config.max_age_days;
    if longest == 0 {
        return None;
    }
    for env in config.env.values() {
        if env.max_age_days == 0 {
            return None;
        }
        longest = longest.max(env.max_age_days);
    }
    Some(longest.saturating_mul(SECS_PER_DAY))
}

/// One deletion candidate: a date directory under an env directory
/// (`data/{env}/{date}/`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DateDir {
    env: String,
    date: NaiveDate,
    path: PathBuf,
}

/// Calendar days from a directory's date to `today`; a future-dated
/// directory is 0 days old, never negative.
///
/// The one age numerator. Phase 1's cutoff and phase 2's rank both read
/// it, against the one `today` the tick sampled, so the two phases can
/// never disagree about how old a directory is. Neither this nor anything
/// downstream of it reads the clock.
fn candidate_age_days(today: NaiveDate, date: NaiveDate) -> u64 {
    // `num_days` is negative exactly for a future date, which is the one
    // case `try_from` refuses; that clamps to 0.
    u64::try_from((today - date).num_days()).unwrap_or(0)
}

/// Whether `dir` is older than its env's effective age limit.
///
/// Reads the limit through [`RetentionConfig::max_age_days_for`], the one
/// fallback lookup, so an env without an entry inherits the global. A 0 —
/// global or override — never expires anything. No `chrono::Duration` is
/// built from the limit: a "keep for 10^12 days" setting is legal config,
/// and subtracting it from a date panics inside chrono.
fn is_age_expired(dir: &DateDir, today: NaiveDate, config: &RetentionConfig) -> bool {
    let max = config.max_age_days_for(&dir.env);
    max != 0 && candidate_age_days(today, dir.date) > max
}

/// Where a candidate stands in the disk-pressure order (ADR-0018 §2-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiryRank {
    /// The env has a finite age limit; the candidate has spent
    /// `age_days / max_age_days` of it.
    Expiring {
        age_days: u64,
        max_age_days: NonZeroU64,
    },
    /// The env's effective age is 0. Ranked after every `Expiring`
    /// candidate, still a candidate: keep-forever is a rank, never a
    /// filter.
    KeepForever,
}

/// Rank `dir` for the pressure sweep, reading the same effective age and
/// the same age numerator phase 1 read.
fn expiry_rank(dir: &DateDir, config: &RetentionConfig, today: NaiveDate) -> ExpiryRank {
    match NonZeroU64::new(config.max_age_days_for(&dir.env)) {
        Some(max_age_days) => ExpiryRank::Expiring {
            age_days: candidate_age_days(today, dir.date),
            max_age_days,
        },
        None => ExpiryRank::KeepForever,
    }
}

/// Compare two candidates by deletion priority: `Less` deletes first.
///
/// `Expiring` always precedes `KeepForever`; two `KeepForever` are equal
/// and the date/path keys decide. Two `Expiring` compare by expiry ratio,
/// highest first: `a` deletes first iff `a.age / a.max > b.age / b.max`,
/// evaluated as `a.age * b.max > b.age * a.max` in `u128`. Cross-
/// multiplication keeps the comparison exact at every magnitude a `u64`
/// can hold (the product of two `u64::MAX` fits `u128`), where an `f64`
/// division would tie candidates that differ by one day.
fn cmp_priority(a: ExpiryRank, b: ExpiryRank) -> Ordering {
    use ExpiryRank::{Expiring, KeepForever};
    match (a, b) {
        (KeepForever, KeepForever) => Ordering::Equal,
        (Expiring { .. }, KeepForever) => Ordering::Less,
        (KeepForever, Expiring { .. }) => Ordering::Greater,
        (
            Expiring {
                age_days: a_age,
                max_age_days: a_max,
            },
            Expiring {
                age_days: b_age,
                max_age_days: b_max,
            },
        ) => {
            let a_scaled = u128::from(a_age) * u128::from(b_max.get());
            let b_scaled = u128::from(b_age) * u128::from(a_max.get());
            // A larger scaled ratio deletes first, so the operands are
            // swapped: `a_scaled > b_scaled` is `Less`.
            b_scaled.cmp(&a_scaled)
        }
    }
}

/// A single retention tick. Samples `today` once and sweeps against it.
fn retention_tick(
    data_dir: &Path,
    wal_dir: &Path,
    config: &RetentionConfig,
    free_space_fn: impl Fn(&Path) -> std::io::Result<u64>,
) -> Result<(), String> {
    let today = chrono::Utc::now().date_naive();
    retention_tick_at(
        data_dir,
        wal_dir,
        config,
        today,
        free_space_fn,
        delete_date_dir,
    )
}

/// The tick body, with the clock and the deletion injected.
///
/// `today` is threaded into both phases and never resampled: the age
/// cutoff and the pressure rank must agree on how old every directory is,
/// and a midnight rollover between the phases would otherwise rank a
/// directory that survived phase 1 as if it had expired. `delete_fn` is
/// [`delete_date_dir`] in production; a test injects it to plant a repin
/// or publication marker after a specific deletion, which no filesystem
/// arrangement can do on its own (deleting a directory can only make
/// evidence vanish).
fn retention_tick_at(
    data_dir: &Path,
    wal_dir: &Path,
    config: &RetentionConfig,
    today: NaiveDate,
    free_space_fn: impl Fn(&Path) -> std::io::Result<u64>,
    delete_fn: impl Fn(&Path) -> Result<u64, String>,
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

    // A pending publication marker (ADR-0041) names a canonical output, its
    // temporary output, or both, by identity. Deleting either turns the
    // marker into a contradiction that blocks its service until an operator
    // acts, so a claimed date directory is kept. Markers that cannot be
    // read may claim anything, so, as with unreadable repin evidence, the
    // sweep stands down rather than delete under them.
    //
    // This scan decides which directories the tick considers at all, and
    // each deletion re-scans just before it runs (see
    // `publication_claimed_mid_sweep`). There is no lock against
    // compaction. A publish names the hour compaction read from the clock,
    // and this tick never deletes the date it reads as today, so a marker
    // written during the tick normally names a directory the tick keeps
    // anyway. The exception is a publish that straddles midnight while a
    // sweep reaches yesterday: compaction reads the old date and this tick
    // reads the new one. The re-scan keeps that directory if the marker
    // exists when the re-scan runs. What remains is the window between the
    // re-scan and the `remove_dir_all`: a marker written there names a
    // directory the sweep then deletes. Each of that publish's rows is
    // then either still in the WAL or was published into the deleted date:
    // none is counted twice, and none is lost that retention was not
    // already deleting with its date. The marker can outlive its output;
    // recovery then reports it as contradictory,
    // `TrawlPublicationRecoveryBlocked` fires, and the service stays
    // blocked until an operator resolves the marker.
    let claims = match crate::ingest::publication_marker::scan_claims(wal_dir) {
        Ok(claims) => claims,
        Err(e) => {
            metrics::gauge!(crate::metrics::RETENTION_SUPPRESSED).set(1.0);
            tracing::warn!(
                event_type = "retention_publication_claims_unreadable",
                error = %e,
                "could not read the publication markers under the WAL root; \
                 suppressing this sweep rather than deleting files a marker \
                 may claim"
            );
            return Ok(());
        }
    };
    metrics::gauge!(crate::metrics::RETENTION_SUPPRESSED).set(0.0);

    let (claimed, candidates): (Vec<DateDir>, Vec<DateDir>) = enumerate_date_dirs(data_dir, today)?
        .into_iter()
        .partition(|dir| claims.claims_date(&dir.env, dir.date));
    for dir in &claimed {
        tracing::info!(
            event_type = "retention_publication_claimed",
            retention_env = %dir.env,
            date = %dir.date,
            "kept a date directory that a pending publication marker claims; \
             retention considers it again once recovery resolves the marker"
        );
    }

    let mut total_bytes_freed: u64 = 0;
    let mut total_dirs_deleted: u64 = 0;

    // Phase 1: age-based retention, decided per candidate against its own
    // env's effective age. There is no global on/off switch: a global 0
    // with a finite override still sweeps that env, and a finite global
    // with a 0 override spares it. The partition hands phase 2 exactly
    // what age retention did not attempt — a failed age deletion is not
    // retried under pressure in the same tick.
    let (age_targets, candidates): (Vec<DateDir>, Vec<DateDir>) = candidates
        .into_iter()
        .partition(|dir| is_age_expired(dir, today, config));

    for dir in &age_targets {
        if repin_claimed_mid_sweep(data_dir) {
            return Ok(());
        }
        match publication_claimed_mid_sweep(wal_dir, dir) {
            MidSweepClaim::None => {}
            MidSweepClaim::Claimed => continue,
            MidSweepClaim::Unreadable => return Ok(()),
        }
        match delete_fn(&dir.path) {
            Ok(bytes) => {
                tracing::info!(
                    event_type = "retention_delete",
                    retention_env = %dir.env,
                    date = %dir.date,
                    max_age_days = config.max_age_days_for(&dir.env),
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
                    path = %dir.path.display(),
                    error = %e,
                    "failed to delete date directory"
                );
            }
        }
    }

    // Phase 2: disk-pressure retention, install-wide over what phase 1
    // left alone.
    if config.min_free_disk_bytes > 0 {
        let (bytes, dirs) = disk_pressure_sweep(
            data_dir,
            wal_dir,
            config,
            candidates,
            today,
            free_space_fn,
            delete_fn,
        )?;
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

/// Delete by expiry ratio until free space clears `min_free_disk_bytes`
/// or there is nothing left to delete. Returns `(bytes_freed,
/// dirs_deleted)`.
fn disk_pressure_sweep(
    data_dir: &Path,
    wal_dir: &Path,
    config: &RetentionConfig,
    mut candidates: Vec<DateDir>,
    today: NaiveDate,
    free_space_fn: impl Fn(&Path) -> std::io::Result<u64>,
    delete_fn: impl Fn(&Path) -> Result<u64, String>,
) -> Result<(u64, u64), String> {
    let mut total_bytes_freed: u64 = 0;
    let mut total_dirs_deleted: u64 = 0;

    // Rank once, before the loop. The order is a pure function of the
    // config, the candidate set and the tick's `today`, none of which
    // moves while the loop runs: highest expiry ratio first, keep-forever
    // last, then oldest date, then path. Three keys make it a total
    // order, so a sweep over a given tree is reproducible.
    candidates.sort_by(|a, b| {
        cmp_priority(expiry_rank(a, config, today), expiry_rank(b, config, today))
            .then_with(|| a.date.cmp(&b.date))
            .then_with(|| a.path.cmp(&b.path))
    });

    loop {
        let available =
            free_space_fn(data_dir).map_err(|e| format!("failed to check free disk space: {e}"))?;

        if available >= config.min_free_disk_bytes {
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

        // Delete the highest-ranked remaining dir, unless a publication
        // marker has claimed it since the tick's scan.
        let dir = candidates.remove(0);
        match publication_claimed_mid_sweep(wal_dir, &dir) {
            MidSweepClaim::None => {}
            MidSweepClaim::Claimed => continue,
            MidSweepClaim::Unreadable => break,
        }
        match delete_fn(&dir.path) {
            Ok(bytes) => {
                tracing::info!(
                    event_type = "retention_delete",
                    retention_env = %dir.env,
                    date = %dir.date,
                    max_age_days = config.max_age_days_for(&dir.env),
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
                    path = %dir.path.display(),
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

/// What a publication-claim re-scan found for one date directory.
enum MidSweepClaim {
    /// No pending marker claims the directory.
    None,
    /// A marker written since the tick's scan claims it: keep it and move
    /// on to the next candidate.
    Claimed,
    /// The markers could not be read: stand the sweep down.
    Unreadable,
}

/// Re-scan the publication markers immediately before deleting `dir`.
///
/// The tick's own scan only covers markers that existed when the tick
/// started. Compaction can publish into yesterday's directory after that
/// scan when its clock read falls before midnight and this tick's falls
/// after it. Re-scanning here narrows that race to the time between this
/// call and the deletion that follows it. An unreadable scan stands the
/// sweep down and raises the suppression gauge, as the tick's own scan
/// does.
fn publication_claimed_mid_sweep(wal_dir: &Path, dir: &DateDir) -> MidSweepClaim {
    match crate::ingest::publication_marker::scan_claims(wal_dir) {
        Ok(claims) if claims.claims_date(&dir.env, dir.date) => {
            tracing::info!(
                event_type = "retention_publication_claimed",
                retention_env = %dir.env,
                date = %dir.date,
                "kept a date directory that a publication marker claimed \
                 during this tick; retention considers it again once \
                 recovery resolves the marker"
            );
            MidSweepClaim::Claimed
        }
        Ok(_) => MidSweepClaim::None,
        Err(e) => {
            metrics::gauge!(crate::metrics::RETENTION_SUPPRESSED).set(1.0);
            tracing::warn!(
                event_type = "retention_publication_claims_unreadable",
                error = %e,
                "could not re-read the publication markers under the WAL \
                 root before a deletion; suppressing the rest of this sweep \
                 rather than deleting files a marker may claim"
            );
            MidSweepClaim::Unreadable
        }
    }
}

/// Evidence that a repin job owns this data root right now, if any.
///
/// One line of policy over [`crate::repin::in_flight_evidence`], the shared
/// authority: an unreadable answer counts as evidence and suppresses the
/// sweep. Retention deletes files, so "I could not tell" has to fall on the
/// side of not deleting them, and the tick that follows would fail reading
/// the same directory anyway.
fn repin_in_flight(data_dir: &Path) -> Option<&'static str> {
    crate::repin::in_flight_evidence(data_dir).unwrap_or_else(|e| {
        tracing::warn!(
            event_type = "retention_repin_evidence_unreadable",
            error = %e,
            "could not tell whether a repin owns the data root; suppressing \
             this sweep rather than deleting under a job that may exist"
        );
        Some("unreadable")
    })
}

/// Enumerate date-formatted directories across every env directory in
/// `data_dir` (ADR-0009 layout: `data/{env}/{date}/`), excluding today
/// and non-date directories. Env directories are recognised by the env
/// charset with `wal`/`scheduled` reserved — anything else at the top
/// level (a stray file, the EPOCH marker, a set-aside dir) is skipped.
/// Candidates are merged across envs and sorted by (date, path): a
/// deterministic base order for phase 1, which deletes in it, and for
/// phase 2, which re-sorts by expiry ratio with these two as tie-breaks.
/// Deleting one env's date dir stays O(1) and never touches other envs.
fn enumerate_date_dirs(data_dir: &Path, today: NaiveDate) -> Result<Vec<DateDir>, String> {
    let entries = std::fs::read_dir(data_dir)
        .map_err(|e| format!("failed to read data directory {}: {e}", data_dir.display()))?;

    let mut dirs: Vec<DateDir> = Vec::new();
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
            if !looks_like_date(name) {
                return None;
            }
            let date = NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()?;
            if date == today {
                return None;
            }
            Some(DateDir {
                env: env_name.to_owned(),
                date,
                path,
            })
        }));
    }

    dirs.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.path.cmp(&b.path)));
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
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    use super::*;
    use crate::metrics::RETENTION_SUPPRESSED;

    fn make_config(max_age_days: u64, min_free_disk_bytes: u64) -> RetentionConfig {
        RetentionConfig {
            max_age_days,
            min_free_disk_bytes,
            retention_interval_secs: 3600,
            env: std::collections::BTreeMap::new(),
        }
    }

    /// A config with a global age and `[retention.env.*]` overrides.
    /// Disk pressure is off; a test that wants it sets
    /// `min_free_disk_bytes` through struct update.
    fn config_with_envs(max_age_days: u64, envs: &[(&str, u64)]) -> RetentionConfig {
        RetentionConfig {
            env: envs
                .iter()
                .map(|(name, days)| {
                    (
                        (*name).to_owned(),
                        crate::config::EnvRetention {
                            max_age_days: *days,
                        },
                    )
                })
                .collect(),
            ..make_config(max_age_days, 0)
        }
    }

    /// A `today` the sweep is handed, so a tree's ages are fixed by the
    /// test and not by the wall clock.
    fn fixed_today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 4).unwrap()
    }

    fn days_before(today: NaiveDate, days: i64) -> NaiveDate {
        today - chrono::Duration::days(days)
    }

    /// A WAL root with no publication markers: a missing root has none.
    fn no_wal() -> PathBuf {
        PathBuf::from("/nonexistent/trawl-retention-test/wal")
    }

    /// Plant a pending publication marker, through the real writer, that
    /// claims `svc`'s hour 07 output on `date` in `env`.
    fn plant_publication_marker(wal_dir: &Path, env: &str, date: NaiveDate) -> PathBuf {
        use crate::ingest::publication_marker::{OutputIdentity, ValidatedMarker, write_marker};
        let marker = ValidatedMarker::new(
            env,
            "svc",
            date,
            7,
            vec!["svc_1730000000000_0001.ndjson".to_owned()],
            OutputIdentity {
                size: 4,
                hash: blake3::hash(b"data"),
            },
        )
        .unwrap();
        std::fs::create_dir_all(marker.wal_env_dir(wal_dir)).unwrap();
        write_marker(wal_dir, &marker).unwrap();
        marker.marker_path(wal_dir)
    }

    /// Create `data_dir/{env}/{date}/svc.parquet` and return the date dir.
    fn plant(data_dir: &Path, env: &str, date: NaiveDate) -> PathBuf {
        let dir = data_dir.join(env).join(date.format("%Y-%m-%d").to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("svc.parquet"), b"data").unwrap();
        dir
    }

    /// A deletion that records the order in which the sweep asked for it.
    fn recording_delete(log: &Mutex<Vec<PathBuf>>) -> impl Fn(&Path) -> Result<u64, String> {
        move |path| {
            log.lock().unwrap().push(path.to_path_buf());
            delete_date_dir(path)
        }
    }

    /// A free-space probe that stays below a 1 MB threshold forever.
    fn always_pressured() -> impl Fn(&Path) -> std::io::Result<u64> {
        |_| Ok(500_000)
    }

    /// The horizon is the LONGEST age anything still keeps, and a single
    /// keep-forever setting lifts it entirely. A minimum would tell the
    /// schema window and the pin-gc floor that data is gone while a
    /// long-retention env still stores it.
    #[test]
    fn maximum_enabled_age_is_the_longest_age_any_env_keeps() {
        const DAY: u64 = 86_400;

        assert_eq!(
            maximum_enabled_age_secs(&config_with_envs(90, &[("prod", 365), ("lab", 7)])),
            Some(365 * DAY),
            "the longest-lived env sets the horizon"
        );
        assert_eq!(
            maximum_enabled_age_secs(&config_with_envs(90, &[("prod", 0)])),
            None,
            "one env keeping data forever leaves no finite horizon"
        );
        assert_eq!(
            maximum_enabled_age_secs(&config_with_envs(0, &[("prod", 365)])),
            None,
            "the global always participates — envs without an entry inherit it"
        );
        assert_eq!(
            maximum_enabled_age_secs(&config_with_envs(90, &[])),
            Some(90 * DAY),
            "no entries: the global alone, exactly as before per-env retention"
        );
    }

    /// `max_age_days` is an unvalidated operator `u64` in the global and in
    /// every override, and "effectively never" values are what an operator
    /// reaches for. The multiply saturates rather than wrapping; the
    /// `/api/v1/schema` end of the same value is covered by
    /// `handlers::tests::since_from_secs_saturates_instead_of_panicking`,
    /// which lands `u64::MAX` seconds on the unix epoch.
    #[test]
    fn maximum_enabled_age_saturates_instead_of_wrapping() {
        assert_eq!(
            maximum_enabled_age_secs(&config_with_envs(90, &[("archive", u64::MAX)])),
            Some(u64::MAX),
            "an effectively-never override saturates at u64::MAX seconds"
        );
        assert_eq!(
            maximum_enabled_age_secs(&make_config(u64::MAX, 0)),
            Some(u64::MAX),
            "the global saturates the same way"
        );
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
    fn candidate_age_is_calendar_days_clamped_at_zero() {
        let today = fixed_today();
        assert_eq!(candidate_age_days(today, days_before(today, 8)), 8);
        assert_eq!(candidate_age_days(today, today), 0);
        assert_eq!(
            candidate_age_days(today, days_before(today, -3)),
            0,
            "a future-dated directory is 0 days old, not negative"
        );
    }

    /// Phase 1's cutoff and phase 2's rank read the same effective age
    /// for the same env, through the one lookup. If they ever diverged, a
    /// directory could survive age retention and still be ranked as
    /// expired, or the reverse.
    #[test]
    fn age_cutoff_and_rank_read_one_effective_age() {
        let today = fixed_today();
        let config = config_with_envs(90, &[("lab", 7), ("archive", 0)]);
        let dir = |env: &str, days: i64| DateDir {
            env: env.to_owned(),
            date: days_before(today, days),
            path: PathBuf::from(env),
        };

        let lab = dir("lab", 8);
        assert!(is_age_expired(&lab, today, &config));
        assert_eq!(
            expiry_rank(&lab, &config, today),
            ExpiryRank::Expiring {
                age_days: 8,
                max_age_days: NonZeroU64::new(7).unwrap(),
            }
        );

        let prod = dir("prod", 8);
        assert!(!is_age_expired(&prod, today, &config), "inherits 90");
        assert_eq!(
            expiry_rank(&prod, &config, today),
            ExpiryRank::Expiring {
                age_days: 8,
                max_age_days: NonZeroU64::new(90).unwrap(),
            }
        );

        let archive = dir("archive", 5000);
        assert!(!is_age_expired(&archive, today, &config), "0 keeps forever");
        assert_eq!(
            expiry_rank(&archive, &config, today),
            ExpiryRank::KeepForever
        );

        let boundary = dir("lab", 7);
        assert!(
            !is_age_expired(&boundary, today, &config),
            "age must exceed the limit, not merely reach it"
        );
    }

    /// The comparator table. `Less` deletes first.
    #[test]
    fn cmp_priority_orders_by_exact_expiry_ratio() {
        use ExpiryRank::{Expiring, KeepForever};
        let expiring = |age_days: u64, max_age_days: u64| Expiring {
            age_days,
            max_age_days: NonZeroU64::new(max_age_days).unwrap(),
        };

        // ADR-0018 §2: 300 of 365 (0.82) is less spent than 6 of 7
        // (0.86), so the lab noise goes before the prod evidence, in
        // either operand order.
        let prod = expiring(300, 365);
        let lab = expiring(6, 7);
        assert_eq!(cmp_priority(lab, prod), Ordering::Less);
        assert_eq!(cmp_priority(prod, lab), Ordering::Greater);

        // Keep-forever candidates are equal among themselves (date and
        // path decide) and follow every finite candidate, even one that
        // has spent none of its allowance.
        assert_eq!(cmp_priority(KeepForever, KeepForever), Ordering::Equal);
        let fresh = expiring(0, 7);
        assert_eq!(cmp_priority(fresh, KeepForever), Ordering::Less);
        assert_eq!(cmp_priority(KeepForever, fresh), Ordering::Greater);

        // Exactness at the top of the domain. `u64::MAX - 1` and
        // `u64::MAX` are the same f64 (2^64), so an f64 ratio would tie
        // these; cross-multiplying in u128 sees one extra day.
        let full = expiring(u64::MAX, u64::MAX);
        let one_day_shy = expiring(u64::MAX - 1, u64::MAX);
        assert_eq!(cmp_priority(full, one_day_shy), Ordering::Less);
        assert_eq!(cmp_priority(one_day_shy, full), Ordering::Greater);
        let also_full = expiring(u64::MAX - 1, u64::MAX - 1);
        assert_eq!(
            cmp_priority(full, also_full),
            Ordering::Equal,
            "two ratios of exactly 1 are equal at any magnitude"
        );
        assert_eq!(cmp_priority(full, full), Ordering::Equal);

        // A ratio above 1 (an age target whose deletion failed is not
        // here, but nothing forbids the value) still orders.
        assert_eq!(cmp_priority(expiring(2, 1), expiring(1, 1)), Ordering::Less);
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

        let dirs = enumerate_date_dirs(tmp.path(), fixed_today()).unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].env, "prod");
        assert_eq!(dirs[0].date, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        assert!(dirs[0].path.starts_with(tmp.path().join("prod")));
    }

    #[test]
    fn enumerate_merges_candidates_across_envs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-01-20")).unwrap();
        std::fs::create_dir_all(tmp.path().join("lab/2026-01-10")).unwrap();

        let dirs = enumerate_date_dirs(tmp.path(), fixed_today()).unwrap();
        assert_eq!(dirs.len(), 2);
        // The base order is by date regardless of env; phase 2 re-ranks.
        assert_eq!(dirs[0].env, "lab");
        assert_eq!(dirs[1].env, "prod");
    }

    #[test]
    fn age_based_removes_one_envs_date_without_touching_others() {
        let today = chrono::Utc::now().date_naive();
        let old_date = days_before(today, 200);

        let tmp = tempfile::tempdir().unwrap();
        let lab_old = plant(tmp.path(), "lab", old_date);
        let prod_old = plant(tmp.path(), "prod", old_date);

        // Both envs' old dates age out independently; deleting one is an
        // O(1) directory remove that never touches the sibling env root.
        let config = make_config(90, 0);
        retention_tick(tmp.path(), &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();

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

        let today = NaiveDate::from_ymd_opt(2026, 2, 13).unwrap();
        let dirs = enumerate_date_dirs(tmp.path(), today).unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].date, NaiveDate::from_ymd_opt(2026, 2, 12).unwrap());
    }

    #[test]
    fn enumerate_sorts_by_date_then_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-03-01")).unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-01-15")).unwrap();
        std::fs::create_dir_all(tmp.path().join("prod/2026-02-20")).unwrap();
        std::fs::create_dir_all(tmp.path().join("lab/2026-02-20")).unwrap();

        let dirs = enumerate_date_dirs(tmp.path(), fixed_today()).unwrap();
        let keys: Vec<_> = dirs.iter().map(|d| (d.date, d.env.as_str())).collect();
        assert_eq!(
            keys,
            vec![
                (NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(), "prod"),
                (NaiveDate::from_ymd_opt(2026, 2, 20).unwrap(), "lab"),
                (NaiveDate::from_ymd_opt(2026, 2, 20).unwrap(), "prod"),
                (NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(), "prod"),
            ]
        );
    }

    #[test]
    fn age_based_deletes_old_dirs() {
        let today = chrono::Utc::now().date_naive();
        let tmp = tempfile::tempdir().unwrap();
        let old_dir = plant(tmp.path(), "prod", days_before(today, 200));
        let recent_dir = plant(tmp.path(), "prod", days_before(today, 30));

        let config = make_config(90, 0);
        retention_tick(tmp.path(), &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();

        assert!(!old_dir.exists(), "old dir should be deleted");
        assert!(recent_dir.exists(), "recent dir should survive");
    }

    #[test]
    fn age_based_preserves_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let old_dir = tmp.path().join("prod/2020-01-01");
        std::fs::create_dir_all(&old_dir).unwrap();

        let config = make_config(0, 0);
        retention_tick(tmp.path(), &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();

        assert!(old_dir.exists(), "nothing should be deleted when disabled");
    }

    /// AC1: each env ages out against its own limit, an env without an
    /// entry inherits the global, and the limit is exclusive (age must
    /// exceed it).
    #[test]
    fn per_env_age_cutoffs_apply_per_env() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let lab_8 = plant(&data_dir, "lab", days_before(today, 8));
        let lab_7 = plant(&data_dir, "lab", days_before(today, 7));
        let prod_8 = plant(&data_dir, "prod", days_before(today, 8));
        let prod_300 = plant(&data_dir, "prod", days_before(today, 300));
        let prod_366 = plant(&data_dir, "prod", days_before(today, 366));
        let staging_8 = plant(&data_dir, "staging", days_before(today, 8));
        let staging_90 = plant(&data_dir, "staging", days_before(today, 90));
        let staging_91 = plant(&data_dir, "staging", days_before(today, 91));

        let config = config_with_envs(90, &[("prod", 365), ("lab", 7)]);
        retention_tick_at(
            &data_dir,
            &no_wal(),
            &config,
            today,
            |_| Ok(u64::MAX),
            delete_date_dir,
        )
        .unwrap();

        assert!(!lab_8.exists(), "lab keeps 7 days: 8 is out");
        assert!(lab_7.exists(), "7 days is not older than 7");
        assert!(prod_8.exists(), "prod keeps a year");
        assert!(prod_300.exists());
        assert!(!prod_366.exists(), "prod's own limit still applies");
        assert!(staging_8.exists(), "unlisted env inherits the global 90");
        assert!(staging_90.exists(), "90 days is not older than 90");
        assert!(!staging_91.exists(), "unlisted env inherits the global 90");
    }

    /// The old phase-1 gate (`if max_age_days > 0`) is gone: a global 0
    /// no longer switches off an env that asked for a finite limit.
    #[test]
    fn global_zero_with_a_finite_override_still_ages_that_env_out() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let prod_40 = plant(&data_dir, "prod", days_before(today, 40));
        let prod_20 = plant(&data_dir, "prod", days_before(today, 20));
        let lab_4000 = plant(&data_dir, "lab", days_before(today, 4000));

        let config = config_with_envs(0, &[("prod", 30)]);
        retention_tick_at(
            &data_dir,
            &no_wal(),
            &config,
            today,
            |_| Ok(u64::MAX),
            delete_date_dir,
        )
        .unwrap();

        assert!(
            !prod_40.exists(),
            "prod's own 30 days applies under a global 0"
        );
        assert!(prod_20.exists());
        assert!(lab_4000.exists(), "lab inherits the global 0: keep forever");
    }

    /// The mirror image: an override of 0 spares that env from age
    /// retention under a finite global.
    #[test]
    fn per_env_zero_under_a_finite_global_keeps_that_env() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let archive_4000 = plant(&data_dir, "archive", days_before(today, 4000));
        let prod_200 = plant(&data_dir, "prod", days_before(today, 200));

        let config = config_with_envs(90, &[("archive", 0)]);
        retention_tick_at(
            &data_dir,
            &no_wal(),
            &config,
            today,
            |_| Ok(u64::MAX),
            delete_date_dir,
        )
        .unwrap();

        assert!(archive_4000.exists(), "archive keeps forever");
        assert!(!prod_200.exists(), "prod inherits the global 90");
    }

    /// An "effectively never" age is legal config. The previous sweep
    /// built a `chrono::Duration` from it and panicked past ~1.07e11
    /// days; the tick has to complete and delete nothing.
    #[test]
    fn huge_max_age_does_not_panic() {
        for huge in [u64::MAX, 999_999_999_999] {
            let today = fixed_today();
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().join("data");
            let prod = plant(
                &data_dir,
                "prod",
                NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            );
            let lab = plant(
                &data_dir,
                "lab",
                NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            );

            let config = config_with_envs(huge, &[("lab", huge)]);
            retention_tick_at(
                &data_dir,
                &no_wal(),
                &config,
                today,
                |_| Ok(u64::MAX),
                delete_date_dir,
            )
            .unwrap();
            assert!(prod.exists(), "{huge}: age retention deletes nothing");
            assert!(lab.exists(), "{huge}: age retention deletes nothing");

            // And the rank's cross-multiplication at that magnitude fits
            // u128: pressure drains the same tree without panicking.
            let config = RetentionConfig {
                min_free_disk_bytes: 1_000_000,
                ..config
            };
            retention_tick_at(
                &data_dir,
                &no_wal(),
                &config,
                today,
                always_pressured(),
                delete_date_dir,
            )
            .unwrap();
            assert!(
                !prod.exists() && !lab.exists(),
                "{huge}: pressure still reaches the floor"
            );
        }
    }

    #[test]
    fn disk_pressure_breaks_equal_ranks_oldest_first() {
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
        let call_count = AtomicU32::new(0);
        let config = make_config(0, 1_000_000);
        retention_tick(tmp.path(), &no_wal(), &config, |_| {
            let n = call_count.fetch_add(1, AtomicOrdering::Relaxed);
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

    /// AC2: pressure deletes by expiry ratio, a keep-forever env goes
    /// last, and "last" is not "never": once nothing else is left, the
    /// sweep takes it too, because a sweep that cannot reach the floor
    /// is a wedged daemon.
    #[test]
    fn disk_pressure_ranks_by_ratio_and_reaches_keep_forever_last() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        // Ratios: lab 6/7 = 0.857, prod 300/365 = 0.822, prod 30/365 =
        // 0.082, archive keep-forever. Under global oldest-first the
        // archive dir would go first and the lab noise last.
        let archive = plant(
            &data_dir,
            "archive",
            NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
        );
        let prod_300 = plant(&data_dir, "prod", days_before(today, 300));
        let prod_30 = plant(&data_dir, "prod", days_before(today, 30));
        let lab_6 = plant(&data_dir, "lab", days_before(today, 6));
        let config = RetentionConfig {
            min_free_disk_bytes: 1_000_000,
            ..config_with_envs(90, &[("prod", 365), ("lab", 7), ("archive", 0)])
        };

        // Three deletions clear the threshold: the keep-forever dir is
        // the one still standing.
        let call_count = AtomicU32::new(0);
        let log = Mutex::new(Vec::new());
        retention_tick_at(
            &data_dir,
            &no_wal(),
            &config,
            today,
            |_| {
                let n = call_count.fetch_add(1, AtomicOrdering::Relaxed);
                if n < 3 { Ok(500_000) } else { Ok(2_000_000) }
            },
            recording_delete(&log),
        )
        .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec![lab_6.clone(), prod_300.clone(), prod_30.clone()],
            "highest expiry ratio first"
        );
        assert!(
            archive.exists(),
            "keep-forever ranks after every expiring candidate"
        );

        // Still under the floor next tick: keep-forever is a rank, not
        // an exemption.
        retention_tick_at(
            &data_dir,
            &no_wal(),
            &config,
            today,
            always_pressured(),
            delete_date_dir,
        )
        .unwrap();
        assert!(!archive.exists(), "the sweep can always reach the floor");
    }

    #[test]
    fn unrelated_siblings_do_not_suppress_pressure_or_become_candidates() {
        for suffix in [".pre-schema-v2", ".pre-epoch-3", ".next", ".archive"] {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().join("data");
            let sibling = tmp.path().join(format!("data{suffix}/prod/2025-01-01"));
            std::fs::create_dir_all(&sibling).unwrap();
            std::fs::write(sibling.join("old.parquet"), b"unrelated bytes").unwrap();
            let today = fixed_today();
            let old = plant(&data_dir, "prod", days_before(today, 3));
            let active = plant(&data_dir, "prod", today);
            retention_tick_at(
                &data_dir,
                &no_wal(),
                &make_config(0, 1_000_000),
                today,
                always_pressured(),
                delete_date_dir,
            )
            .unwrap();
            assert!(!old.exists(), "pressure still applies beside {suffix}");
            assert!(active.exists(), "today is preserved");
            assert_eq!(
                std::fs::read(sibling.join("old.parquet")).unwrap(),
                b"unrelated bytes"
            );
        }
    }

    /// While a repin job exists on this root — marker, shadow sibling, or
    /// aside sibling — both sweeps stand down. Age deletion would yank
    /// affected files out from under the shadow build (the catch-up diff
    /// sees additions, not disappearances, as normal), and pressure
    /// deletion could never reclaim the double-held bytes the job itself
    /// is holding. A per-env override licenses nothing the gate denies
    /// (ADR-0018 §4).
    #[test]
    fn both_sweeps_suppressed_while_a_repin_is_in_flight() {
        let today = fixed_today();

        for staging in ["marker", "shadow", "aside"] {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().join("data");
            let prod_200 = plant(&data_dir, "prod", days_before(today, 200));
            let lab_8 = plant(&data_dir, "lab", days_before(today, 8));
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

            // Age (global and per-env) and pressure all armed, all hungry.
            let config = RetentionConfig {
                min_free_disk_bytes: 1_000_000,
                ..config_with_envs(90, &[("lab", 7)])
            };
            retention_tick_at(
                &data_dir,
                &no_wal(),
                &config,
                today,
                always_pressured(),
                delete_date_dir,
            )
            .unwrap();
            assert!(
                prod_200.exists() && lab_8.exists(),
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
        let call_count = AtomicU32::new(0);
        let marker = crate::repin::marker_path(&data_dir);
        let config = make_config(0, 1_000_000);
        retention_tick(&data_dir, &no_wal(), &config, |_| {
            let n = call_count.fetch_add(1, AtomicOrdering::Relaxed);
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

    /// The same re-read guards the age phase. Phase 1 never consults the
    /// free-space probe, so the marker is planted by the injected deletion
    /// itself, right after the first age target is gone: the second age
    /// target survives, and so does everything pressure would have taken
    /// (the claim ends the whole tick, not just the phase).
    #[test]
    fn a_repin_admitted_mid_age_sweep_stops_before_the_next_age_deletion() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let first = plant(&data_dir, "prod", days_before(today, 200));
        let second = plant(&data_dir, "prod", days_before(today, 150));
        let recent = plant(&data_dir, "prod", days_before(today, 10));

        let marker = crate::repin::marker_path(&data_dir);
        let deleted = AtomicU32::new(0);
        let config = make_config(90, 1_000_000);
        retention_tick_at(
            &data_dir,
            &no_wal(),
            &config,
            today,
            always_pressured(),
            |path| {
                let bytes = delete_date_dir(path)?;
                if deleted.fetch_add(1, AtomicOrdering::Relaxed) == 0 {
                    std::fs::write(&marker, b"{}").unwrap();
                }
                Ok(bytes)
            },
        )
        .unwrap();

        assert!(!first.exists(), "the pre-claim age deletion stands");
        assert!(second.exists(), "the next age target survives the claim");
        assert!(
            recent.exists(),
            "pressure never runs once the claim ends the tick"
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
            retention_tick(&data_dir, &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();
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
            retention_tick(&data_dir, &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();
        });
        assert!(
            handle
                .render()
                .contains(&format!("{RETENTION_SUPPRESSED} 0")),
            "the tick that sweeps again must clear it: {}",
            handle.render()
        );
    }

    /// A date directory a pending publication marker claims survives both
    /// sweeps, while an unclaimed one just as old goes. Once the marker is
    /// resolved, the next tick deletes the directory too.
    #[test]
    fn a_date_a_publication_marker_claims_survives_both_sweeps() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let wal_dir = tmp.path().join("wal");
        let claimed = plant(&data_dir, "prod", days_before(today, 200));
        let unclaimed = plant(&data_dir, "prod", days_before(today, 201));
        let other_env = plant(&data_dir, "lab", days_before(today, 200));
        let marker = plant_publication_marker(&wal_dir, "prod", days_before(today, 200));

        // Age and pressure both armed: every candidate is expired and the
        // disk stays full.
        let config = RetentionConfig {
            min_free_disk_bytes: 1_000_000,
            ..make_config(90, 0)
        };
        retention_tick_at(
            &data_dir,
            &wal_dir,
            &config,
            today,
            always_pressured(),
            delete_date_dir,
        )
        .unwrap();
        assert!(claimed.exists(), "the claimed date survives");
        assert!(!unclaimed.exists(), "an unclaimed date is deleted");
        assert!(!other_env.exists(), "the claim is per env");

        crate::ingest::publication_marker::remove_marker_durably(&marker).unwrap();
        retention_tick_at(
            &data_dir,
            &wal_dir,
            &config,
            today,
            always_pressured(),
            delete_date_dir,
        )
        .unwrap();
        assert!(!claimed.exists(), "a resolved marker no longer claims");
    }

    /// A marker written after the tick's own scan still keeps its date: the
    /// sweep re-scans just before each deletion. In the age phase the
    /// injected deletion plants the marker right after the first target
    /// goes, naming the second. The second survives and the third, which
    /// no marker claims, is still deleted.
    #[test]
    fn a_publication_marker_written_mid_age_sweep_keeps_its_date() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let wal_dir = tmp.path().join("wal");
        let first = plant(&data_dir, "prod", days_before(today, 202));
        let second = plant(&data_dir, "prod", days_before(today, 201));
        let third = plant(&data_dir, "prod", days_before(today, 200));

        let deleted = AtomicU32::new(0);
        let config = make_config(90, 0);
        retention_tick_at(
            &data_dir,
            &wal_dir,
            &config,
            today,
            always_pressured(),
            |path| {
                let bytes = delete_date_dir(path)?;
                if deleted.fetch_add(1, AtomicOrdering::Relaxed) == 0 {
                    plant_publication_marker(&wal_dir, "prod", days_before(today, 201));
                }
                Ok(bytes)
            },
        )
        .unwrap();

        assert!(!first.exists(), "the deletion before the marker stands");
        assert!(second.exists(), "the date the new marker claims is kept");
        assert!(!third.exists(), "the sweep goes on past the claimed date");
    }

    /// The same re-scan guards the pressure phase, and this is the midnight
    /// case: compaction read yesterday's date and publishes into it while
    /// this tick, already on the new date, sweeps under pressure. The
    /// free-space probe runs after the tick's scan and before each pressure
    /// deletion, so its first call plants a marker naming yesterday. The
    /// older date, which no marker claims, is still deleted.
    #[test]
    fn a_publication_marker_written_mid_pressure_sweep_keeps_its_date() {
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let wal_dir = tmp.path().join("wal");
        let yesterday = days_before(today, 1);
        let older = plant(&data_dir, "prod", days_before(today, 2));
        let publishing = plant(&data_dir, "prod", yesterday);

        // Keep-forever everywhere, so only pressure deletes, oldest first.
        let calls = AtomicU32::new(0);
        let config = make_config(0, 1_000_000);
        retention_tick_at(
            &data_dir,
            &wal_dir,
            &config,
            today,
            |_| {
                if calls.fetch_add(1, AtomicOrdering::Relaxed) == 0 {
                    plant_publication_marker(&wal_dir, "prod", yesterday);
                }
                Ok(500_000)
            },
            delete_date_dir,
        )
        .unwrap();

        assert!(!older.exists(), "an unclaimed date is still deleted");
        assert!(
            publishing.exists(),
            "the date the new marker claims is kept"
        );
    }

    /// If the re-scan cannot read the markers, the sweep stops before the
    /// next deletion and raises the suppression gauge.
    #[test]
    fn markers_unreadable_mid_sweep_stop_the_next_deletion() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let wal_dir = tmp.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();
        let first = plant(&data_dir, "prod", days_before(today, 201));
        let second = plant(&data_dir, "prod", days_before(today, 200));
        let recent = plant(&data_dir, "prod", days_before(today, 10));

        let deleted = AtomicU32::new(0);
        let config = make_config(90, 1_000_000);
        metrics::with_local_recorder(&recorder, || {
            retention_tick_at(
                &data_dir,
                &wal_dir,
                &config,
                today,
                always_pressured(),
                |path| {
                    let bytes = delete_date_dir(path)?;
                    if deleted.fetch_add(1, AtomicOrdering::Relaxed) == 0 {
                        std::fs::remove_dir(&wal_dir).unwrap();
                        std::fs::write(&wal_dir, b"not a directory").unwrap();
                    }
                    Ok(bytes)
                },
            )
            .unwrap();
        });

        assert!(!first.exists(), "the deletion before the failure stands");
        assert!(second.exists(), "no deletion under unreadable markers");
        assert!(recent.exists(), "pressure never runs after the stand-down");
        assert!(
            handle
                .render()
                .contains(&format!("{RETENTION_SUPPRESSED} 1")),
            "{}",
            handle.render()
        );
    }

    /// Markers that cannot be read may claim any date, so an unreadable WAL
    /// root stands both sweeps down and raises the suppression gauge, as
    /// unreadable repin evidence does.
    #[test]
    fn an_unreadable_wal_root_suppresses_both_sweeps() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let today = fixed_today();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let wal_dir = tmp.path().join("wal");
        std::fs::write(&wal_dir, b"not a directory").unwrap();
        let old = plant(&data_dir, "prod", days_before(today, 200));
        let config = RetentionConfig {
            min_free_disk_bytes: 1_000_000,
            ..make_config(90, 0)
        };
        metrics::with_local_recorder(&recorder, || {
            retention_tick_at(
                &data_dir,
                &wal_dir,
                &config,
                today,
                always_pressured(),
                delete_date_dir,
            )
            .unwrap();
        });
        assert!(old.exists(), "nothing is deleted under unreadable markers");
        assert!(
            handle
                .render()
                .contains(&format!("{RETENTION_SUPPRESSED} 1")),
            "{}",
            handle.render()
        );
    }

    /// And both resume once the job's staging is gone.
    #[test]
    fn sweeps_resume_after_the_repin_ends() {
        let today = chrono::Utc::now().date_naive();
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let old_dir = plant(&data_dir, "prod", days_before(today, 200));

        let config = make_config(90, 0);
        retention_tick(&data_dir, &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();
        assert!(!old_dir.exists(), "age sweep resumes");
    }

    #[test]
    fn both_disabled_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("prod/2020-01-01");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.parquet"), b"old").unwrap();

        let config = make_config(0, 0);
        retention_tick(tmp.path(), &no_wal(), &config, |_| Ok(0)).unwrap();

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
        retention_tick(tmp.path(), &no_wal(), &config, |_| Ok(100)).unwrap();

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
        retention_tick(tmp.path(), &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();

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
        retention_tick(tmp.path(), &no_wal(), &config, |_| Ok(u64::MAX)).unwrap();
    }

    /// One planted directory as the oracle sees it. `age` and `max` are
    /// `u32` so the oracle's ratio can go through `f64::from`: with ages
    /// under 1000 and limits under 500, two distinct ratios differ by at
    /// least 1/(500*500), far above f64's rounding error at that size, so
    /// float division orders them exactly and equal rationals compare
    /// equal. That is a different arithmetic from the sweep's u128
    /// cross-multiplication, which is the point of an oracle.
    struct Planted {
        env: &'static str,
        date: NaiveDate,
        path: PathBuf,
        age: u32,
        max: u32,
    }

    impl Planted {
        fn expired(&self) -> bool {
            self.max != 0 && self.age > self.max
        }

        /// `None` is keep-forever.
        fn ratio(&self) -> Option<f64> {
            (self.max != 0).then(|| f64::from(self.age) / f64::from(self.max))
        }
    }

    /// The reference deletion order for a tree under permanent pressure:
    /// phase 1's expired set in (date, env) order, then phase 2's
    /// survivors by ratio descending with keep-forever last, then date,
    /// then env (the path's only varying component).
    fn reference_order(planted: &[Planted]) -> Vec<PathBuf> {
        let mut expired: Vec<&Planted> = planted.iter().filter(|p| p.expired()).collect();
        expired.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.env.cmp(b.env)));

        let mut survivors: Vec<&Planted> = planted.iter().filter(|p| !p.expired()).collect();
        survivors.sort_by(|a, b| {
            let by_ratio = match (a.ratio(), b.ratio()) {
                (None, None) => Ordering::Equal,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(ra), Some(rb)) => rb.partial_cmp(&ra).unwrap(),
            };
            by_ratio
                .then_with(|| a.date.cmp(&b.date))
                .then_with(|| a.env.cmp(b.env))
        });

        expired
            .iter()
            .chain(survivors.iter())
            .map(|p| p.path.clone())
            .collect()
    }

    /// Build one random tree: 1-4 envs, each inheriting, keep-forever or
    /// finite; 1-6 distinct dates per env from 30 days ahead to 800 days
    /// back, never today itself (that one is planted separately and must
    /// survive). Returns the config and the oracle's view of the tree.
    fn random_tree(
        rng: &mut impl rand::Rng,
        data_dir: &Path,
        today: NaiveDate,
    ) -> (RetentionConfig, Vec<Planted>) {
        const ENVS: [&str; 4] = ["archive", "lab", "prod", "staging"];
        let envs = &ENVS[..rng.gen_range(1..=ENVS.len())];
        let global: u32 = if rng.gen_bool(0.2) {
            0
        } else {
            rng.gen_range(1..=400)
        };
        let mut overrides: Vec<(&str, u64)> = Vec::new();
        for &env in envs {
            match rng.gen_range(0..3) {
                0 => {}
                1 => overrides.push((env, 0)),
                _ => overrides.push((env, u64::from(rng.gen_range(1..=400u32)))),
            }
        }
        let config = RetentionConfig {
            min_free_disk_bytes: 1_000_000,
            ..config_with_envs(u64::from(global), &overrides)
        };

        let mut planted = Vec::new();
        for &env in envs {
            let max = overrides
                .iter()
                .find(|(name, _)| *name == env)
                .map_or(global, |(_, days)| u32::try_from(*days).unwrap());
            let dates: BTreeSet<NaiveDate> = (0..rng.gen_range(1..=6))
                .map(|_| rng.gen_range(-30i64..=800))
                .filter(|offset| *offset != 0)
                .map(|offset| days_before(today, offset))
                .collect();
            for date in dates {
                planted.push(Planted {
                    env,
                    date,
                    path: plant(data_dir, env, date),
                    age: u32::try_from((today - date).num_days().max(0)).unwrap(),
                    max,
                });
            }
        }
        (config, planted)
    }

    /// Every date dir under `data_dir`, listed without the enumerator
    /// under test.
    fn remaining_date_dirs(data_dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for env in std::fs::read_dir(data_dir).unwrap().flatten() {
            for date in std::fs::read_dir(env.path()).unwrap().flatten() {
                out.push(date.path());
            }
        }
        out.sort();
        out
    }

    /// Under permanent pressure a tick drains every non-today directory
    /// (keep-forever included), in exactly the order an independent
    /// oracle computes, and the ratios pressure deletes at never rise
    /// along the way. Seeded, so a failure names its tree.
    #[test]
    fn generative_full_sweep_drains_in_reference_order() {
        use rand::SeedableRng;

        let today = fixed_today();
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x0108);
        for tree in 0..200 {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().join("data");
            let (config, planted) = random_tree(&mut rng, &data_dir, today);
            let today_dir = plant(&data_dir, "prod", today);
            let expected = reference_order(&planted);

            let log = Mutex::new(Vec::new());
            retention_tick_at(
                &data_dir,
                &no_wal(),
                &config,
                today,
                always_pressured(),
                recording_delete(&log),
            )
            .unwrap();
            let observed = log.into_inner().unwrap();

            assert_eq!(
                observed, expected,
                "tree {tree}: delete sequence differs from the reference order"
            );
            assert_eq!(
                remaining_date_dirs(&data_dir),
                vec![today_dir.clone()],
                "tree {tree}: the sweep drains everything but today"
            );

            // Along the pressure part of the sequence, ratios never rise
            // and no expiring candidate follows a keep-forever one. The
            // age part is date-ordered by design, so it is not
            // ratio-monotone and is excluded.
            let phase_two = observed
                .iter()
                .skip(planted.iter().filter(|p| p.expired()).count());
            let mut previous: Option<Option<f64>> = None;
            for path in phase_two {
                let ratio = planted.iter().find(|p| p.path == *path).unwrap().ratio();
                if let Some(prev) = previous {
                    match (prev, ratio) {
                        (Some(a), Some(b)) => assert!(
                            b <= a,
                            "tree {tree}: ratio rose from {a} to {b} at {}",
                            path.display()
                        ),
                        (None, Some(_)) => panic!(
                            "tree {tree}: expiring candidate {} after keep-forever",
                            path.display()
                        ),
                        (_, None) => {}
                    }
                }
                previous = Some(ratio);
            }
        }
    }
}
