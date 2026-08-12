// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-env swap (ADR-0011 slice B) — idempotent and forward-only, so
//! the live cutover and boot recovery are ONE function.
//!
//! An ADR-0011 amendment records why this is a per-env swap under
//! exclusion rather than the ADR's literal whole-root rename: the WAL
//! lives inside the data root by default, so a root swap would strand it
//! (or force WAL-writer gating plus grafting), and the M1 probes proved a
//! mixed corpus does not even error — it silently promotes — so atomicity
//! comes from the exclusion primitives either way. The swap itself is two
//! renames per env dir; `wal/`, `scheduled/`, `EPOCH`, `CATALOG` and the
//! `REPIN` marker never move.

use std::path::{Path, PathBuf};

/// Move every env dir the shadow generation carries into place:
/// `data/{env}` → `aside/{env}`, then `shadow/{env}` → `data/{env}`.
///
/// Idempotent per env — a crash between the two renames resumes exactly
/// where it stopped (the set-aside is skipped precisely because the live
/// dir is already gone) — tolerant of an absent shadow root (a crash
/// after the last env moved) and of a LEFTOVER aside from an earlier
/// job's failed sweep, which is parked beside rather than refused (see
/// [`free_aside_slot`]). Callers hold the exclusion guards (live cutover)
/// or run before anything can query (boot recovery).
pub(crate) fn swap_envs(data_dir: &Path, shadow: &Path, aside: &Path) -> Result<(), String> {
    let entries = match std::fs::read_dir(shadow) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("failed to read {}: {e}", shadow.display())),
    };
    std::fs::create_dir_all(aside)
        .map_err(|e| format!("failed to create {}: {e}", aside.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read {}: {e}", shadow.display()))?;
        if !entry.path().is_dir() {
            continue;
        }
        let env = entry.file_name();
        let live = data_dir.join(&env);
        let next = shadow.join(&env);

        if live.exists() {
            let put_aside = free_aside_slot(aside, &env)?;
            std::fs::rename(&live, &put_aside).map_err(|e| {
                format!(
                    "failed to set aside {} -> {}: {e}{}",
                    live.display(),
                    put_aside.display(),
                    cross_device_hint(&e)
                )
            })?;
        }
        std::fs::rename(&next, &live).map_err(|e| {
            format!(
                "failed to publish {} -> {}: {e}{}",
                next.display(),
                live.display(),
                cross_device_hint(&e)
            )
        })?;
    }
    crate::epoch::fsync_dir_best_effort(data_dir);
    crate::epoch::fsync_dir_best_effort(aside);

    // Everything moved out: the shadow root is spent.
    if let Err(e) = std::fs::remove_dir_all(shadow)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            event_type = "repin_shadow_sweep_failed",
            path = %shadow.display(),
            error = %e,
            "failed to remove the spent shadow root (harmless; retried \
             at the next boot's marker replay)"
        );
    }
    Ok(())
}

/// The two failures this text must explain rather than merely report,
/// both of them a mount in the wrong place: a data root that is itself a
/// mount point puts the staging siblings on the parent filesystem so no
/// rename can cross (`EXDEV`), and a volume mounted at a subtree of an env
/// directory makes that directory unrenameable (`EBUSY`).
/// [`check_staging_filesystem`](crate::repin::marker::check_staging_filesystem)
/// refuses a job that would meet either, but a marker written before the
/// volume was mounted (or by an older build) still replays here at
/// every boot, where the bare `Invalid cross-device link` / `Device or
/// resource busy` says nothing about why the node will not start or what
/// to do about it. Nothing has moved when the `EXDEV` fires — every rename
/// fails alike — so the corpus stands at its pre-repin generation; the
/// `EBUSY` is per env, so envs without a nested mount may already serve
/// the new generation and finishing the swap (not undoing it) is the way
/// out.
fn cross_device_hint(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::CrossesDevices => {
            " — the repin staging root is on a DIFFERENT filesystem than the \
             data root (the data root is itself a mount point), so no rename \
             in this swap can complete and the corpus stands at its pre-repin \
             generation. Mount the volume one level up so the data root is a \
             directory INSIDE it (the packaged layout: volume at \
             /var/lib/trawl, data at /var/lib/trawl/data), then restart to let \
             the marker replay finish the swap"
        }
        std::io::ErrorKind::ResourceBusy => {
            " — this env directory CONTAINS a mount point (a volume mounted \
             at a date/hour subtree of the corpus), and a directory holding \
             one cannot be renamed. Unmount that volume and move its contents \
             onto the data root's own filesystem — one filesystem for the \
             whole corpus — then restart to let the marker replay finish the \
             swap"
        }
        _ => "",
    }
}

/// The slot this env's outgoing generation is parked in: `aside/{env}`
/// normally, and `aside/{env}.{n}` when that name is already taken by a
/// LEFTOVER aside — a previous job's outgoing generation whose sweep
/// failed (the sweep is best-effort; a failure keeps the marker and warns,
/// but nothing removes the root before the next job runs).
///
/// Parking rather than refusing is the only safe answer past the cutover
/// marker: this runs both under the live exclusion guards, where an error
/// costs the process (forward is the only direction), and in boot
/// recovery, where the same error would refuse every subsequent boot. A
/// dot cannot appear in an env name (charset `[a-z0-9_-]{1,32}`), so a
/// numbered slot can never collide with another env's, and the whole
/// aside root is swept as one at the end of the job — so the leftover is
/// reclaimed rather than inherited again.
fn free_aside_slot(aside: &Path, env: &std::ffi::OsStr) -> Result<std::path::PathBuf, String> {
    let plain = aside.join(env);
    if !plain.exists() {
        return Ok(plain);
    }
    for n in 1..1000 {
        let mut name = env.to_owned();
        name.push(format!(".{n}"));
        let slot = aside.join(&name);
        if !slot.exists() {
            tracing::warn!(
                event_type = "repin_stale_aside_parked",
                leftover = %plain.display(),
                slot = %slot.display(),
                "a previous repin's aside root survived its sweep; parking \
                 this generation beside it so the swap goes forward (both \
                 are swept together when the job finishes)"
            );
            return Ok(slot);
        }
    }
    Err(format!(
        "no free aside slot for {} — remove the leftover generations under \
         {} to reclaim the disk and unblock the cutover",
        plain.display(),
        aside.display()
    ))
}

/// Best-effort recursive removal with a warn — used for the aside sweep
/// and abandoned shadows, where a failure costs disk, never correctness.
pub(crate) fn sweep_dir(path: &Path, what: &'static str) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(
                event_type = "repin_sweep_failed",
                what,
                path = %path.display(),
                error = %e,
                "failed to sweep a repin staging root; disk is not \
                 reclaimed until it is removed"
            );
            false
        }
    }
}

/// Clear the shadow root and hand back an EMPTY one for a build to fill.
///
/// This is the one sweep that is not best-effort. Elsewhere a surviving
/// staging root only costs disk, because nothing writes into it again;
/// here `create_dir_all` succeeds on the survivor and the build layers
/// its generation onto whatever it holds. Nothing downstream repairs
/// that: a pass only retires shadow entries whose source it has seen in
/// THIS job's `BuildState` (empty on pass 0), so a survivor's files —
/// conformed to an earlier job's pin, or copied from live data that
/// retention has since deleted — ride [`swap_envs`] into the live corpus
/// as a resurrected, mixed-type generation. Refuse the build instead and
/// leave the root for the operator (the marker replay retries the
/// sweep).
pub(crate) fn prepare_shadow_root(data_dir: &Path) -> Result<PathBuf, String> {
    let shadow = crate::repin::marker::shadow_root(data_dir);
    if !sweep_dir(&shadow, "stale shadow") {
        return Err(format!(
            "a previous repin's shadow root survived its sweep at {} — \
             building over it would publish that generation's files into \
             the live corpus at the swap; remove it to unblock repins",
            shadow.display()
        ));
    }
    std::fs::create_dir_all(&shadow).map_err(|e| format!("failed to create shadow root: {e}"))?;
    Ok(shadow)
}

/// Sweep BOTH staging roots for a job that ends before any swap — the
/// live abandon path and the boot replay of a `building` marker — and
/// report whether both are gone (the signal marker removal is gated on).
///
/// Such a job made no aside of its own, so anything under the aside root
/// is an EARLIER job's outgoing generation whose best-effort sweep
/// failed. It is superseded data by construction (an aside exists only
/// past a cutover, and the swap is forward-only: the corpus already
/// serves the generation that replaced it), and the marker standing right
/// now is the LAST license to delete it — a marker-less aside is never
/// removed ([`crate::repin::recover::recover_filesystem`] leaves it for
/// an operator). Sweeping only the shadow and then dropping the marker is
/// what strands it forever, with retention suppressed the whole time
/// because [`crate::retention`] treats either staging root as a repin in
/// flight.
pub(crate) fn sweep_pre_swap_staging(data_dir: &Path) -> bool {
    // Not `&&`: a shadow that survives must not skip the aside sweep.
    let shadow_swept = sweep_dir(
        &crate::repin::marker::shadow_root(data_dir),
        "abandoned shadow",
    );
    let aside_swept = sweep_dir(
        &crate::repin::marker::aside_root(data_dir),
        "leftover aside",
    );
    shadow_swept && aside_swept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The pre-swap sweep takes BOTH roots, and an undeletable shadow does
    /// not short-circuit the aside sweep — a leftover aside would then keep
    /// suppressing retention with no marker left to license its removal.
    #[cfg(unix)]
    #[test]
    fn the_pre_swap_sweep_takes_the_aside_even_when_the_shadow_survives() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let shadow = crate::repin::marker::shadow_root(&data);
        let aside = crate::repin::marker::aside_root(&data);
        write(&data.join("prod/2026-01-01/10/svc.parquet"), b"current");
        write(&shadow.join("prod/2026-01-01/10/svc.parquet"), b"new");
        write(&aside.join("prod/2026-01-01/10/svc.parquet"), b"leftover");

        let stuck = shadow.join("prod/2026-01-01/10");
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::remove_file(stuck.join("svc.parquet")).is_ok() {
            // Running as root: mode bits are not enforced.
            let _ = std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755));
            return;
        }

        assert!(
            !sweep_pre_swap_staging(&data),
            "a surviving shadow keeps the marker"
        );
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!aside.exists(), "the aside is swept regardless");
        assert!(shadow.exists());
        assert!(
            sweep_pre_swap_staging(&data),
            "with the permissions repaired the retry finishes"
        );
    }

    /// A shadow root that survives its sweep REFUSES the next build rather
    /// than letting it layer on top: nothing retires the survivor's files
    /// (a pass only retires sources it has seen in its own state), so the
    /// swap would publish an earlier job's generation into the live corpus.
    #[cfg(unix)]
    #[test]
    fn a_surviving_shadow_refuses_the_next_build_instead_of_layering() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let shadow = crate::repin::marker::shadow_root(&data);
        write(
            &shadow.join("prod/2026-01-01/10/svc.parquet"),
            b"earlier job",
        );

        let stuck = shadow.join("prod/2026-01-01/10");
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::remove_file(stuck.join("svc.parquet")).is_ok() {
            // Running as root: mode bits are not enforced.
            let _ = std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755));
            return;
        }

        let err = prepare_shadow_root(&data).expect_err("the build is refused");
        assert!(err.contains("survived its sweep"), "{err}");
        assert_eq!(
            std::fs::read(stuck.join("svc.parquet")).unwrap(),
            b"earlier job",
            "the survivor is left for the operator, not built over"
        );

        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = prepare_shadow_root(&data).expect("with the permissions repaired it proceeds");
        assert_eq!(root, shadow);
        assert!(root.is_dir());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0, "empty root");
    }

    /// A leftover `aside/{env}` from an earlier job's failed sweep must not
    /// wedge the next cutover: the swap goes forward (its caller past the
    /// marker can only exit the process, and boot recovery replaying the
    /// same error would refuse every subsequent boot), the outgoing
    /// generation is parked beside the leftover, and neither is lost
    /// before the job's own aside sweep reclaims both.
    #[test]
    fn a_leftover_aside_is_parked_beside_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let shadow = tmp.path().join("data.repin-next");
        let aside = tmp.path().join("data.repin-aside");
        write(&data.join("prod/2026-01-01/10/svc.parquet"), b"current");
        write(&shadow.join("prod/2026-01-01/10/svc.parquet"), b"new");
        write(&aside.join("prod/2026-01-01/10/svc.parquet"), b"leftover");

        swap_envs(&data, &shadow, &aside).expect("the swap goes forward");

        assert_eq!(
            std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
            b"new",
            "the shadow generation is published"
        );
        assert_eq!(
            std::fs::read(aside.join("prod.1/2026-01-01/10/svc.parquet")).unwrap(),
            b"current",
            "the outgoing generation is parked in a free slot"
        );
        assert_eq!(
            std::fs::read(aside.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
            b"leftover",
            "the earlier job's generation is still there to be swept"
        );
        assert!(!shadow.exists(), "the spent shadow is gone");

        // Replaying the finished swap is a no-op: no shadow, so no second
        // slot is spent.
        swap_envs(&data, &shadow, &aside).expect("idempotent");
        assert!(!aside.join("prod.2").exists());
    }

    /// The normal path keeps the plain slot, and a crash between the two
    /// renames resumes without parking anything.
    #[test]
    fn the_plain_slot_is_used_and_a_half_swap_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let shadow = tmp.path().join("data.repin-next");
        let aside = tmp.path().join("data.repin-aside");
        write(&data.join("prod/2026-01-01/10/svc.parquet"), b"current");
        write(&shadow.join("prod/2026-01-01/10/svc.parquet"), b"new");

        swap_envs(&data, &shadow, &aside).unwrap();
        assert_eq!(
            std::fs::read(aside.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
            b"current"
        );
        assert!(!aside.join("prod.1").exists());

        // Half-swapped: live already set aside, shadow not yet published.
        write(&shadow.join("prod/2026-01-01/10/svc.parquet"), b"newer");
        std::fs::remove_dir_all(data.join("prod")).unwrap();
        swap_envs(&data, &shadow, &aside).unwrap();
        assert_eq!(
            std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
            b"newer"
        );
        assert!(
            !aside.join("prod.1").exists(),
            "no live dir to set aside, so no slot is spent"
        );
    }
}
