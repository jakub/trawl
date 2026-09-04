// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boot recovery for an interrupted repin (ADR-0011) — the marker-driven
//! decision table, split in two halves because the two resources come up
//! at different times:
//!
//! - the filesystem half runs before `ensure_current_epoch`
//!   (non-negotiable: a half-swapped root must be finished before the
//!   epoch gate forms an opinion of it), needs no postgres, and is one
//!   `stat` on the marker-less fast path;
//! - the postgres half runs after `AppState::from_config`, finishes the
//!   job row (idempotent flip or failure), re-arms the boot conformance
//!   pass for any recovered cutover, sweeps the aside, and removes the
//!   marker — then reconciles any orphaned `running` rows.
//!
//! Every branch is idempotent: recovery interrupted mid-recovery is the
//! same state re-entered.

use std::path::Path;

use trawl_core::schema::CanonicalType;

use crate::catalog::FieldCatalog;
use crate::repin::cutover::{swap_envs, sweep_dir, sweep_pre_swap_staging};
use crate::repin::marker::{
    RepinMarker, RepinPhase, aside_root, read_marker, remove_marker, shadow_root,
};
use crate::store::{RepinJobStatus, StorageState};

/// What the filesystem half did, for the postgres half to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredAction {
    /// The job died building: the disposable shadow was deleted, the live
    /// corpus was never touched — the job row fails.
    AbandonedBuild,
    /// The job died mid- or post-swap: the per-env renames were completed
    /// forward — the job row completes via the idempotent flip.
    CompletedCutover,
    /// The job died sweeping: the swap and flip are done — redo the
    /// idempotent flip, sweep, done.
    SweptCleanup,
}

/// A recovered marker plus the action the filesystem half took.
#[derive(Debug, Clone)]
pub struct Recovered {
    /// The marker as read.
    pub marker: RepinMarker,
    /// What was done.
    pub action: RecoveredAction,
    /// Whether every staging sweep this half owed actually left the
    /// staging root gone. A failed sweep keeps the marker (see
    /// `reconcile_store`): the leftover root suppresses retention and
    /// would make the next cutover's forward-only swap ambiguous, so the
    /// replay must run again at the next boot rather than be forgotten.
    pub swept: bool,
}

/// The filesystem half of the decision table.
///
/// A query-only node (`ingest_enabled == false`) owns nothing here: a
/// `building` marker is harmless (the live corpus was never touched —
/// warn and serve, leaving the shadow for the owning node), but a
/// `cutover`/`cleanup` marker means the corpus may be half-swapped, which
/// probes showed does not even error — mixed scalar types silently
/// promote — so the only honest answer is to refuse the boot.
pub fn recover_filesystem(
    data_dir: &Path,
    ingest_enabled: bool,
) -> Result<Option<Recovered>, String> {
    let shadow = shadow_root(data_dir);
    let aside = aside_root(data_dir);
    let Some(marker) = read_marker(data_dir)? else {
        // No marker: staging siblings should not exist. A stray shadow is
        // disposable by construction (never authoritative); a stray aside
        // is somebody's data and is left with a warning.
        if ingest_enabled && shadow.exists() {
            tracing::warn!(
                event_type = "repin_stale_shadow",
                path = %shadow.display(),
                "marker-less repin shadow root found (crashed before the \
                 marker landed?); removing the disposable staging"
            );
            sweep_dir(&shadow, "stale shadow");
        }
        if aside.exists() {
            tracing::warn!(
                event_type = "repin_stale_aside",
                path = %aside.display(),
                "marker-less repin aside root found — trawl will not \
                 delete it without a marker naming the job; remove it \
                 manually to reclaim disk"
            );
        }
        return Ok(None);
    };

    if !ingest_enabled {
        return match marker.phase {
            RepinPhase::Building => {
                tracing::warn!(
                    event_type = "repin_recovery_deferred",
                    job_id = marker.job_id,
                    "repin marker (phase=building) found on a query-only \
                     node — the live corpus is untouched, so serving is \
                     safe; boot the owning ingest node to reconcile the job"
                );
                Ok(None)
            }
            RepinPhase::Cutover | RepinPhase::Cleanup => Err(format!(
                "a repin job (id={}, field={:?}) died mid-cutover on this \
                 data root and this node runs with ingest disabled, so it \
                 must not repair the half-swapped corpus — and serving one \
                 would silently promote mixed types. Boot once with \
                 [ingest] enabled = true to complete the recovery",
                marker.job_id, marker.field
            )),
        };
    }

    let (action, swept) = match marker.phase {
        // Both staging roots: this job made no aside (it died before the
        // swap), so a leftover one is an earlier job's, and this marker is
        // the last license to delete it — see `sweep_pre_swap_staging`.
        RepinPhase::Building => (
            RecoveredAction::AbandonedBuild,
            sweep_pre_swap_staging(data_dir),
        ),
        RepinPhase::Cutover => {
            swap_envs(data_dir, &shadow, &aside)?;
            // `swap_envs` removes the spent shadow best-effort; ask the
            // filesystem rather than trust the warn.
            (RecoveredAction::CompletedCutover, !shadow.exists())
        }
        RepinPhase::Cleanup => (
            RecoveredAction::SweptCleanup,
            sweep_dir(&shadow, "cleanup shadow remnant"),
        ),
    };
    tracing::info!(
        event_type = "repin_recovered",
        job_id = marker.job_id,
        field = %marker.field,
        phase = ?marker.phase,
        action = ?action,
        swept,
        "interrupted repin job recovered on the filesystem side"
    );
    Ok(Some(Recovered {
        marker,
        action,
        swept,
    }))
}

/// The postgres half: finish the recovered job, re-arm conformance for a
/// recovered cutover, sweep the aside, remove the marker — then fail any
/// orphaned `running` rows (a marker-less orphan means the process died
/// before the cutover: the corpus is untouched).
pub async fn reconcile_store(
    storage: &StorageState,
    cache: &FieldCatalog,
    data_dir: &Path,
    recovered: Option<Recovered>,
) -> Result<(), String> {
    if let Some(recovered) = recovered {
        let marker = &recovered.marker;
        let mut swept = recovered.swept;
        match recovered.action {
            RecoveredAction::AbandonedBuild => {
                // Conditional: the marker's job is `running` by
                // construction here, and a row that somehow already carries
                // a verdict keeps it rather than being restamped `failed`
                // by a replay.
                storage
                    .repin
                    .finish_if_running(
                        marker.job_id,
                        RepinJobStatus::Failed,
                        Some("interrupted while building the shadow generation; corpus untouched"),
                        None,
                    )
                    .await
                    .map_err(|e| format!("failed to fail recovered repin job: {e}"))?;
            }
            RecoveredAction::CompletedCutover | RecoveredAction::SweptCleanup => {
                // `from_catalog`, matching what the marker writes
                // (`to.as_catalog()`): the physical parse (`from_duckdb`)
                // has no `SEVERITY` spelling, so it would refuse a severity
                // cutover marker. This path must never refuse — the corpus
                // is already half-swapped.
                let to = CanonicalType::from_catalog(&marker.to_type).ok_or_else(|| {
                    format!("repin marker names non-canonical type {:?}", marker.to_type)
                })?;
                let outcome = storage
                    .repin
                    .finish_cutover(marker.job_id, &marker.field, to)
                    .await
                    .map_err(|e| format!("failed to complete recovered repin flip: {e}"))?;
                // The clear is audited wherever it happens. `cleared_ack` is
                // true only on the call that completed the job, so a boot
                // that replays an already-finished cutover stays silent.
                // Same contract as the live path: one line per OBSERVED
                // clear, and a crash between the commit above and this emit
                // loses it for good.
                if outcome.cleared_ack {
                    tracing::info!(
                        event_type = "field_degraded_ack_cleared",
                        field = %marker.field,
                        reason = "repin",
                        job_id = marker.job_id,
                        "the acknowledged pin was repinned by a recovered \
                         cutover; the acknowledgement went with the evidence"
                    );
                }
                cache.repin(&marker.field, to);
                // The backstop: an interrupted cutover may have missed a
                // file, so the corpus is re-proven this very boot.
                storage
                    .catalog
                    .clear_conformed()
                    .await
                    .map_err(|e| format!("failed to re-arm the conformance pass: {e}"))?;
                swept &= sweep_dir(&aside_root(data_dir), "recovered aside");
            }
        }
        // The marker is the only record that a staging root is trawl's to
        // delete: dropping it over a failed sweep strands the leftover
        // root forever (retention stays suppressed by its mere existence,
        // and a later cutover meets an aside beside a live env and refuses
        // the forward-only swap). The store side is idempotent by
        // construction, so keeping the marker just replays this at the
        // next boot — which is exactly the retry.
        if swept {
            remove_marker(data_dir)?;
        } else {
            tracing::warn!(
                event_type = "repin_recovery_incomplete",
                job_id = marker.job_id,
                field = %marker.field,
                action = ?recovered.action,
                "a repin staging root survived the recovery sweep; keeping \
                 the marker so the next boot retries the cleanup (retention \
                 stays suppressed until it is gone)"
            );
        }
    }

    let orphaned = storage
        .repin
        .reconcile_orphans(None)
        .await
        .map_err(|e| format!("failed to reconcile orphaned repin jobs: {e}"))?;
    if orphaned > 0 {
        tracing::warn!(
            event_type = "repin_orphans_reconciled",
            orphaned,
            "orphaned running repin job rows marked failed (killed before \
             any cutover; corpus untouched)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repin::marker::write_marker;

    fn marker(phase: RepinPhase) -> RepinMarker {
        RepinMarker {
            job_id: 9,
            field: "status".to_owned(),
            from_type: "BIGINT".to_owned(),
            to_type: "VARCHAR".to_owned(),
            phase,
        }
    }

    /// Build a live root with one env holding one file.
    fn live_root(tmp: &Path) -> std::path::PathBuf {
        let data = tmp.join("data");
        let dir = data.join("prod/2026-01-01/10");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("svc.parquet"), b"old generation").unwrap();
        data
    }

    fn shadow_with_new_generation(data: &Path) -> std::path::PathBuf {
        let shadow = shadow_root(data);
        let dir = shadow.join("prod/2026-01-01/10");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("svc.parquet"), b"new generation").unwrap();
        shadow
    }

    #[test]
    fn building_crash_abandons_the_shadow_and_leaves_the_corpus() {
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        let shadow = shadow_with_new_generation(&data);
        write_marker(&data, &marker(RepinPhase::Building)).unwrap();

        for _ in 0..2 {
            let recovered = recover_filesystem(&data, true).unwrap().expect("marker");
            assert_eq!(recovered.action, RecoveredAction::AbandonedBuild);
            assert!(recovered.swept, "the shadow is gone, so the sweep is done");
            assert!(!shadow.exists(), "the shadow is disposable");
            assert_eq!(
                std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
                b"old generation",
                "the live corpus is untouched"
            );
        }
    }

    /// A leftover aside from an earlier job's failed sweep is reclaimed by
    /// the abandoned build rather than inherited: the marker standing over
    /// it is the last one that will ever name it, and leaving it while the
    /// marker goes strands it (retention stays suppressed by its mere
    /// existence, and nothing deletes a marker-less aside).
    #[test]
    fn an_abandoned_build_reclaims_a_leftover_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        let shadow = shadow_with_new_generation(&data);
        let aside = aside_root(&data);
        std::fs::create_dir_all(aside.join("prod/2026-01-01/10")).unwrap();
        std::fs::write(
            aside.join("prod/2026-01-01/10/svc.parquet"),
            b"superseded generation",
        )
        .unwrap();
        write_marker(&data, &marker(RepinPhase::Building)).unwrap();

        let recovered = recover_filesystem(&data, true).unwrap().expect("marker");
        assert_eq!(recovered.action, RecoveredAction::AbandonedBuild);
        assert!(
            recovered.swept,
            "both staging roots are gone, so the marker may be dropped"
        );
        assert!(!shadow.exists());
        assert!(!aside.exists(), "the leftover aside is reclaimed too");
        assert_eq!(
            std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
            b"old generation",
            "the live corpus is untouched"
        );
    }

    /// A staging root that survives its sweep reports `swept == false` —
    /// the signal `reconcile_store` gates marker removal on, so the
    /// leftover root keeps its marker and the next boot retries.
    #[cfg(unix)]
    #[test]
    fn a_surviving_staging_root_reports_an_unfinished_sweep() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        let shadow = shadow_with_new_generation(&data);
        let stuck = shadow.join("prod/2026-01-01/10");
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::remove_file(stuck.join("svc.parquet")).is_ok() {
            // Running as root: mode bits are not enforced.
            let _ = std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755));
            return;
        }
        write_marker(&data, &marker(RepinPhase::Building)).unwrap();

        let recovered = recover_filesystem(&data, true).unwrap().expect("marker");
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(recovered.action, RecoveredAction::AbandonedBuild);
        assert!(
            !recovered.swept,
            "an undeletable shadow must not report a finished sweep"
        );
        assert!(shadow.exists(), "the staging root is still on disk");
    }

    /// The cutover crash windows: before any rename, between the two
    /// renames of an env, and after every rename — all resume forward to
    /// the completed repin, and re-running recovery is a no-op.
    #[test]
    fn cutover_crash_windows_all_resume_to_the_new_generation() {
        // (a) marker written, no renames yet.
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        shadow_with_new_generation(&data);
        write_marker(&data, &marker(RepinPhase::Cutover)).unwrap();
        for _ in 0..2 {
            let recovered = recover_filesystem(&data, true).unwrap().expect("marker");
            assert_eq!(recovered.action, RecoveredAction::CompletedCutover);
            assert_eq!(
                std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
                b"new generation"
            );
            assert_eq!(
                std::fs::read(aside_root(&data).join("prod/2026-01-01/10/svc.parquet")).unwrap(),
                b"old generation",
                "the previous generation is aside until the sweep"
            );
            assert!(!shadow_root(&data).exists());
        }

        // (b) between the two renames: live env moved aside, shadow not
        // yet published — no data/prod at all.
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        shadow_with_new_generation(&data);
        let aside = aside_root(&data);
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::rename(data.join("prod"), aside.join("prod")).unwrap();
        write_marker(&data, &marker(RepinPhase::Cutover)).unwrap();
        for _ in 0..2 {
            let recovered = recover_filesystem(&data, true).unwrap().expect("marker");
            assert_eq!(recovered.action, RecoveredAction::CompletedCutover);
            assert_eq!(
                std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
                b"new generation"
            );
        }

        // (c) every rename done, crash before the pin flip: the shadow
        // root is gone, the marker still says cutover.
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        let shadow = shadow_with_new_generation(&data);
        let aside = aside_root(&data);
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::rename(data.join("prod"), aside.join("prod")).unwrap();
        std::fs::rename(shadow.join("prod"), data.join("prod")).unwrap();
        std::fs::remove_dir_all(&shadow).unwrap();
        write_marker(&data, &marker(RepinPhase::Cutover)).unwrap();
        for _ in 0..2 {
            let recovered = recover_filesystem(&data, true).unwrap().expect("marker");
            assert_eq!(recovered.action, RecoveredAction::CompletedCutover);
            assert_eq!(
                std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
                b"new generation"
            );
        }
    }

    #[test]
    fn cleanup_crash_sweeps_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        write_marker(&data, &marker(RepinPhase::Cleanup)).unwrap();
        for _ in 0..2 {
            let recovered = recover_filesystem(&data, true).unwrap().expect("marker");
            assert_eq!(recovered.action, RecoveredAction::SweptCleanup);
        }
    }

    #[test]
    fn marker_less_boot_sweeps_a_stray_shadow_only() {
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        let shadow = shadow_with_new_generation(&data);
        let aside = aside_root(&data);
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(aside.join("evidence"), b"x").unwrap();

        assert!(recover_filesystem(&data, true).unwrap().is_none());
        assert!(!shadow.exists(), "a marker-less shadow is disposable");
        assert!(
            aside.join("evidence").exists(),
            "a marker-less aside is somebody's data — never deleted"
        );
    }

    /// A query-only node: a building marker warns and serves (the corpus
    /// was never touched); a cutover marker refuses the boot — the
    /// half-swapped corpus would silently promote, not error.
    #[test]
    fn query_only_node_serves_building_and_refuses_cutover() {
        let tmp = tempfile::tempdir().unwrap();
        let data = live_root(tmp.path());
        let shadow = shadow_with_new_generation(&data);
        write_marker(&data, &marker(RepinPhase::Building)).unwrap();
        assert!(recover_filesystem(&data, false).unwrap().is_none());
        assert!(shadow.exists(), "not this node's staging to sweep");

        write_marker(&data, &marker(RepinPhase::Cutover)).unwrap();
        let err = recover_filesystem(&data, false).expect_err("must refuse");
        assert!(err.contains("ingest"), "{err}");
    }
}
