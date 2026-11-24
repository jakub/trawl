// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Keeping the results root and its rows in agreement (ADR-0057).
//!
//! Neither logs a capture's id: the counts say what happened, and an id is
//! a key's private handle.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::ResultsState;
use super::root::ResultsRoot;
use crate::config::CaptureConfig;
use crate::error::{CauseKind, join_failure_text};
use crate::store::{ExpiredFile, StoreError};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub stages: usize,
    /// Published files removed because no live row names them: a crash, or
    /// a shutdown, between publish and the row's commit, or a retired row
    /// whose file survived its unlink.
    pub orphans: usize,
    /// Live rows newly marked `file_missing`: they name a file and none is
    /// there.
    pub missing: usize,
    /// Temporary rows already past their deadline, marked `expired`.
    pub expired: usize,
    /// Temporary rows pruned to bring a key, then the installation, within
    /// the `[capture]` quotas.
    pub pruned: usize,
    /// Kept rows pruned because their investigation's evidence retention
    /// ended while the server was down (ADR-0059).
    pub evidence_pruned: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Temporary rows past their deadline marked `expired`.
    pub expired: usize,
    /// Kept rows marked `pruned` because their investigation's evidence
    /// retention ended (ADR-0059).
    pub evidence_pruned: usize,
    /// Files of the expired and the pruned rows unlinked.
    pub unlinked: usize,
    pub tombstones: u64,
    /// Published files no live row names, older than the capture deadline,
    /// unlinked.
    pub orphans: usize,
    pub errors: usize,
}

/// Reconcile the results root with the metadata rows, before the server
/// answers anything.
///
/// The root's lock is the authority for removing files here, as the scratch
/// lock is for clearing scratch (ADR-0056): this process holds it, so no
/// other process can be publishing into the root.
///
/// # Errors
/// A filesystem or database failure, as text that names the step. It stops
/// the boot.
pub async fn reconcile_at_boot(results: &ResultsState) -> Result<Reconciled, String> {
    let now = results.clock.now();
    let store = &results.store;
    let fail = |step: &str, error: &dyn std::fmt::Display| {
        format!(
            "results root {}: {step} failed: {error}",
            results.root.path().display()
        )
    };

    let (stages, files) = on_root(&results.root, |root| {
        Ok((root.remove_stages()?, root.list_ids()?))
    })
    .await
    .map_err(|e| fail("listing the root", &e))?;
    let live = store
        .live_ids()
        .await
        .map_err(|e| fail("reading the live captures", &e))?;

    // No capture is in flight and no fence or commit grace runs across a
    // restart (`service::COMMIT_GRACE`): a row committed by now keeps the
    // file, and a file no live row names goes.
    let live_ids: HashSet<&str> = live.iter().map(|(id, _)| id.as_str()).collect();
    let orphans: Vec<String> = files
        .iter()
        .filter(|id| !live_ids.contains(id.as_str()))
        .cloned()
        .collect();
    let orphans = unlink_all(&results.root, orphans)
        .await
        .map_err(|e| fail("removing files no capture names", &e))?;

    let on_disk: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut missing = 0;
    for (id, _) in live
        .iter()
        .filter(|(id, file_bytes)| *file_bytes > 0 && !on_disk.contains(id.as_str()))
    {
        if store
            .mark_file_missing(id)
            .await
            .map_err(|e| fail("marking a capture's missing file", &e))?
        {
            missing += 1;
        }
    }

    // Expiring before the prunes keeps a dead capture from counting against
    // a quota.
    let expired = store
        .expire_due(now)
        .await
        .map_err(|e| fail("expiring due captures", &e))?;
    let expired_rows = expired.len();
    unlink_all(&results.root, with_files(expired))
        .await
        .map_err(|e| fail("removing expired captures' files", &e))?;

    let evidence_rows = prune_evidence_at_boot(results, now, &fail).await?;

    let capture = &results.capture;
    let mut pruned = Vec::new();
    for key in store
        .live_keys(now)
        .await
        .map_err(|e| fail("reading the keys with captures", &e))?
    {
        pruned.extend(
            store
                .prune_oldest(key, capture.per_key_results, capture.per_key_bytes, now)
                .await
                .map_err(|e| fail("pruning a key over its capture quota", &e))?,
        );
    }
    pruned.extend(
        store
            .prune_oldest_installation(
                capture.installation_results,
                capture.installation_bytes,
                now,
            )
            .await
            .map_err(|e| fail("pruning the installation's captures", &e))?,
    );
    let pruned_rows = pruned.len();
    unlink_all(&results.root, with_files(pruned))
        .await
        .map_err(|e| fail("removing pruned captures' files", &e))?;

    let reconciled = Reconciled {
        stages,
        orphans,
        missing,
        expired: expired_rows,
        pruned: pruned_rows,
        evidence_pruned: evidence_rows,
    };
    tracing::info!(
        event_type = "results_reconcile",
        stages = reconciled.stages,
        orphans = reconciled.orphans,
        missing = reconciled.missing,
        expired = reconciled.expired,
        pruned = reconciled.pruned,
        evidence_pruned = reconciled.evidence_pruned,
        "results root reconciled with the captured-result rows"
    );
    Ok(reconciled)
}

async fn prune_evidence_at_boot(
    results: &ResultsState,
    now: DateTime<Utc>,
    fail: &impl Fn(&str, &dyn std::fmt::Display) -> String,
) -> Result<usize, String> {
    let evidence = results
        .store
        .prune_evidence(now)
        .await
        .map_err(|e| fail("pruning evidence past its retention", &e))?;
    let rows = evidence.len();
    unlink_all(&results.root, with_files(evidence))
        .await
        .map_err(|e| fail("removing pruned evidence files", &e))?;
    Ok(rows)
}

/// A capture between its publish and its row's commit is in flight, and
/// its insert runs past the capture deadline when the store is slow, so no
/// age makes its file an orphan; the in-flight set does. A capture whose
/// commit is in doubt stays in flight through its grace
/// ([`super::service::COMMIT_GRACE`]). The age floor gives the rest, a
/// file whose row still could not be read when the grace ended, the
/// deadline before it is judged. A file whose row is a tombstone goes too,
/// since nothing can read it.
pub async fn sweep(results: &ResultsState, now: DateTime<Utc>) -> Sweep {
    let mut outcome = Sweep::default();
    let store = &results.store;
    let capture = &results.capture;

    match store.expire_due(now).await {
        Ok(expired) => {
            outcome.expired = expired.len();
            let (unlinked, errors) = unlink_counting(&results.root, with_files(expired)).await;
            outcome.unlinked = unlinked;
            outcome.errors += errors;
        }
        Err(e) => outcome.errors += warn_store("expiring due captures", &e),
    }

    match store.prune_evidence(now).await {
        Ok(pruned) => {
            outcome.evidence_pruned = pruned.len();
            let (unlinked, errors) = unlink_counting(&results.root, with_files(pruned)).await;
            outcome.unlinked += unlinked;
            outcome.errors += errors;
        }
        Err(e) => outcome.errors += warn_store("pruning evidence past its retention", &e),
    }

    match store
        .delete_tombstones(before(now, capture.tombstone_secs))
        .await
    {
        Ok(deleted) => outcome.tombstones = deleted,
        Err(e) => outcome.errors += warn_store("deleting capture tombstones", &e),
    }

    // List the files, then the captures in flight, then the rows. A
    // capture enters before its publish and leaves once its insert is
    // decided, or once its commit grace is over, so a listed file whose
    // capture is not in the snapshot had its insert decided, or given its
    // grace, before the rows are read: its row is there, or it has none.
    match on_root(&results.root, ResultsRoot::list_ids).await {
        Err(e) => outcome.errors += warn("listing the results root", &e, cause_of(&e)),
        Ok(files) => match (results.in_flight.snapshot(), store.live_ids().await) {
            (_, Err(e)) => outcome.errors += warn_store("reading the live captures", &e),
            (in_flight, Ok(live)) => {
                let live: HashSet<String> = live.into_iter().map(|(id, _)| id).collect();
                let unnamed = files
                    .into_iter()
                    .filter(|id| !live.contains(id) && !in_flight.contains(id))
                    .collect();
                let cutoff = before(now, capture.deadline_secs);
                let (orphans, errors) = remove_orphans(&results.root, unnamed, cutoff).await;
                outcome.orphans = orphans;
                outcome.errors += errors;
            }
        },
    }

    tracing::info!(
        event_type = "results_sweep",
        expired = outcome.expired,
        evidence_pruned = outcome.evidence_pruned,
        unlinked = outcome.unlinked,
        tombstones = outcome.tombstones,
        orphans = outcome.orphans,
        errors = outcome.errors,
        "captured results swept"
    );
    outcome
}

pub fn sweep_interval(capture: &CaptureConfig) -> Duration {
    Duration::from_secs(capture.ttl_secs.clamp(1, 3600))
}

pub fn spawn_sweep(
    results: Arc<ResultsState>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            event_type = "lifecycle",
            action = "results_sweep_start",
            interval_secs = interval.as_secs(),
            "captured-results sweep task started"
        );
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                _ = shutdown_rx.changed() => {
                    tracing::info!(
                        event_type = "lifecycle",
                        action = "results_sweep_stop",
                        "captured-results sweep task shutting down"
                    );
                    return;
                }
            }
            sweep(&results, results.clock.now()).await;
        }
    })
}

fn before(now: DateTime<Utc>, secs: u64) -> DateTime<Utc> {
    i64::try_from(secs)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .and_then(|span| now.checked_sub_signed(span))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn with_files(retired: Vec<ExpiredFile>) -> Vec<String> {
    retired
        .into_iter()
        .filter(|file| file.file_bytes > 0)
        .map(|file| file.id)
        .collect()
}

fn warn(step: &str, error: &dyn std::fmt::Display, cause_kind: CauseKind) -> usize {
    tracing::debug!(
        event_type = "results_sweep_error",
        step,
        error = %error,
        "a captured-results sweep step failed; the next sweep retries it"
    );
    tracing::warn!(
        event_type = "results_sweep_error",
        step,
        cause_kind = cause_kind.as_str(),
        "a captured-results sweep step failed; the next sweep retries it"
    );
    1
}

/// The error is logged by its closed class only: its text can carry the
/// backend's diagnostics, and the event lands in the retained
/// `service=trawld` corpus.
fn warn_store(step: &str, error: &StoreError) -> usize {
    tracing::warn!(
        event_type = "results_sweep_error",
        step,
        error_class = error.class(),
        "a captured-results sweep step failed; the next sweep retries it"
    );
    1
}

async fn on_root<T: Send + 'static>(
    root: &Arc<ResultsRoot>,
    work: impl FnOnce(&ResultsRoot) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let root = Arc::clone(root);
    tokio::task::spawn_blocking(move || work(&root))
        .await
        .map_err(join_failure)?
}

/// A blocking task on the root that panicked or was cancelled, carried
/// inside an [`io::Error`] as its own type, so [`cause_of`] can tell it
/// from a filesystem error by type and never by text. It displays
/// [`join_failure_text`]'s fixed words.
#[derive(Debug)]
struct JoinFailure(String);

impl std::fmt::Display for JoinFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JoinFailure {}

fn join_failure(error: tokio::task::JoinError) -> io::Error {
    io::Error::other(JoinFailure(join_failure_text("results root work", error)))
}

fn cause_of(error: &io::Error) -> CauseKind {
    if error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<JoinFailure>)
    {
        CauseKind::Unknown
    } else {
        CauseKind::of_io(error)
    }
}

async fn unlink_all(root: &Arc<ResultsRoot>, ids: Vec<String>) -> io::Result<usize> {
    on_root(root, move |root| {
        let mut removed = 0;
        for id in &ids {
            if root.unlink(id)? {
                removed += 1;
            }
        }
        Ok(removed)
    })
    .await
}

async fn unlink_counting(root: &Arc<ResultsRoot>, ids: Vec<String>) -> (usize, usize) {
    let unlinked = on_root(root, move |root| {
        let mut outcome = (0, 0);
        for id in &ids {
            match root.unlink(id) {
                Ok(true) => outcome.0 += 1,
                Ok(false) => {}
                Err(e) => {
                    outcome.1 += warn(
                        "unlinking a retired capture's file",
                        &e,
                        CauseKind::of_io(&e),
                    );
                }
            }
        }
        Ok(outcome)
    })
    .await;
    unlinked.unwrap_or_else(|e| {
        (
            0,
            warn("unlinking retired captures' files", &e, CauseKind::Unknown),
        )
    })
}

async fn remove_orphans(
    root: &Arc<ResultsRoot>,
    ids: Vec<String>,
    cutoff: DateTime<Utc>,
) -> (usize, usize) {
    let removed = on_root(root, move |root| {
        let mut outcome = (0, 0);
        for id in &ids {
            let old = match root.modified(id) {
                Ok(Some(written)) => DateTime::<Utc>::from(written) <= cutoff,
                Ok(None) => false,
                Err(e) => {
                    outcome.1 += warn(
                        "reading an unnamed capture file's age",
                        &e,
                        CauseKind::of_io(&e),
                    );
                    continue;
                }
            };
            if !old {
                continue;
            }
            match root.unlink(id) {
                Ok(true) => outcome.0 += 1,
                Ok(false) => {}
                Err(e) => {
                    outcome.1 += warn(
                        "unlinking a capture file no row names",
                        &e,
                        CauseKind::of_io(&e),
                    );
                }
            }
        }
        Ok(outcome)
    })
    .await;
    removed.unwrap_or_else(|e| {
        (
            0,
            warn(
                "removing capture files no row names",
                &e,
                CauseKind::Unknown,
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::logged::{assert_class_only, logged, sentinel};

    #[test]
    fn a_store_step_warning_logs_the_errors_class_and_not_its_text() {
        let error = sentinel();
        let mut counted = 0;
        let logged = logged(|| counted = warn_store("expiring due captures", &error));
        assert_class_only(&logged, "results_sweep_error", &error);
        assert!(
            logged.contains("step=\"expiring due captures\""),
            "{logged}"
        );
        assert_eq!(counted, 1);
    }

    #[tokio::test]
    async fn a_root_task_that_fails_to_join_has_no_io_cause() {
        let joined = tokio::task::spawn_blocking(|| panic!("listing task panicked"))
            .await
            .unwrap_err();
        let error = join_failure(joined);
        assert_eq!(cause_of(&error), CauseKind::Unknown);
        assert!(
            !error.to_string().contains("listing task panicked"),
            "{error}"
        );
        assert_eq!(
            cause_of(&io::Error::from(io::ErrorKind::NotFound)),
            CauseKind::IoNotFound
        );
        assert_eq!(cause_of(&io::Error::other("other")), CauseKind::IoOther);
    }

    #[test]
    fn before_saturates_instead_of_overflowing() {
        let now = DateTime::parse_from_rfc3339("2026-10-23T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(before(now, 15), now - TimeDelta::seconds(15));
        assert_eq!(before(now, 0), now);
        assert_eq!(before(now, u64::MAX), DateTime::<Utc>::MIN_UTC);
        assert_eq!(
            before(now, u64::try_from(i64::MAX).unwrap()),
            DateTime::<Utc>::MIN_UTC
        );
    }

    #[test]
    fn the_sweep_interval_is_the_ttl_between_one_second_and_one_hour() {
        let interval = |ttl_secs| {
            sweep_interval(&CaptureConfig {
                ttl_secs,
                ..CaptureConfig::default()
            })
        };
        assert_eq!(interval(0), Duration::from_secs(1));
        assert_eq!(interval(90), Duration::from_secs(90));
        assert_eq!(interval(3600), Duration::from_secs(3600));
        assert_eq!(interval(86_400), Duration::from_secs(3600));
    }

    #[test]
    fn only_retired_results_with_a_file_are_unlinked() {
        let retired = vec![
            ExpiredFile {
                id: "a".into(),
                file_bytes: 10,
            },
            ExpiredFile {
                id: "b".into(),
                file_bytes: 0,
            },
        ];
        assert_eq!(with_files(retired), ["a"]);
    }
}
