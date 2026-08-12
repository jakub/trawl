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

use std::path::Path;

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
                    "failed to set aside {} -> {}: {e}",
                    live.display(),
                    put_aside.display()
                )
            })?;
        }
        std::fs::rename(&next, &live).map_err(|e| {
            format!(
                "failed to publish {} -> {}: {e}",
                next.display(),
                live.display()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
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
