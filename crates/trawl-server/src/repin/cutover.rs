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
/// where it stopped — and tolerant of an absent shadow root (a crash
/// after the last env moved). Callers hold the exclusion guards (live
/// cutover) or run before anything can query (boot recovery).
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
        let put_aside = aside.join(&env);
        let next = shadow.join(&env);

        if !put_aside.exists() && live.exists() {
            std::fs::rename(&live, &put_aside).map_err(|e| {
                format!(
                    "failed to set aside {} -> {}: {e}",
                    live.display(),
                    put_aside.display()
                )
            })?;
        }
        if live.exists() {
            // Reachable only if something recreated the env dir between
            // the renames — impossible under the exclusion guards and
            // before boot serves. Refuse rather than guess.
            return Err(format!(
                "ambiguous repin cutover state: {} exists beside both its \
                 aside and its shadow — refusing to overwrite",
                live.display()
            ));
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
