// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ADR-0009 storage-epoch cutover: restartable, filesystem-only.
//!
//! "Legacy data is dropped" needs a mechanism, not an incantation. The
//! marker is `data/EPOCH` with content `2`; the legacy layout has none
//! (epoch 1, implicit). The boot decision table — every branch idempotent:
//!
//! - no `data/` → create `data/` + `EPOCH`, normal boot (fresh install;
//!   also the crash-resume case)
//! - `data/EPOCH` == `2` → normal boot; warn if `data.pre-schema-v2/`
//!   still exists
//! - `data/` without `EPOCH`, no `data.pre-schema-v2/` → legacy root:
//!   rename `data/` → `data.pre-schema-v2/` (atomic), create fresh
//!   `data/` + `EPOCH`, log loudly with counts of what was set aside
//!   (parquet and WAL move together)
//! - `data/` without `EPOCH` **and** `data.pre-schema-v2/` exists →
//!   ambiguous; refuse to start with instructions
//!
//! There is no reachable state with a half-migrated root: the fresh root
//! is assembled as a sibling `data.next` (EPOCH fsynced) and renamed into
//! place, so a crash between the set-aside rename and the fresh-root
//! rename leaves *no* `data/` — which resumes via the first branch.
//! trawl never deletes `data.pre-schema-v2/`; retention skips it; the
//! operator removes it at leisure.

use std::path::{Path, PathBuf};

/// The current storage epoch, written to `data/EPOCH`.
pub const CURRENT_EPOCH: &str = "2";

/// Marker filename inside the data root.
pub const EPOCH_FILE: &str = "EPOCH";

/// Suffix of the set-aside directory for a pre-cutover root.
pub const SET_ASIDE_SUFFIX: &str = ".pre-schema-v2";

/// Staging suffix for the fresh root assembled before the swap.
const NEXT_SUFFIX: &str = ".next";

/// What the boot gate decided (for logging/tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Fresh install or crash-resume: created `data/` + `EPOCH`.
    FreshRoot,
    /// Marker present and current: normal boot. `aside_present` warns the
    /// operator that the set-aside dir is still consuming disk.
    Current { aside_present: bool },
    /// Legacy root set aside; fresh root created. Counts are what moved.
    LegacySetAside {
        parquet_files: u64,
        wal_files: u64,
        external_wal_set_aside: bool,
    },
}

/// Run the epoch gate. Called after config load, before any component
/// touches the data root. Returns an error (refusing to start) for the
/// ambiguous state or an unrecognized epoch.
///
/// `wal_dir` is the *effective* WAL directory: when it lies outside the
/// data root, the legacy-rename branch sets it aside too
/// (`{wal_dir}.pre-schema-v2`) — parquet and WAL move together.
pub fn ensure_current_epoch(data_root: &Path, wal_dir: &Path) -> Result<Outcome, String> {
    let aside = set_aside_path(data_root);
    let marker = data_root.join(EPOCH_FILE);

    if !data_root.exists() {
        // Fresh install — or the crash-resume window between the
        // set-aside rename and the fresh-root rename.
        create_fresh_root(data_root)?;
        return Ok(Outcome::FreshRoot);
    }

    match std::fs::read_to_string(&marker) {
        Ok(content) => {
            let content = content.trim();
            if content == CURRENT_EPOCH {
                let aside_present = aside.exists();
                if aside_present {
                    tracing::warn!(
                        event_type = "epoch_aside_present",
                        path = %aside.display(),
                        "pre-cutover data set-aside directory still exists — \
                         trawl never deletes it; remove it to reclaim disk"
                    );
                }
                Ok(Outcome::Current { aside_present })
            } else {
                Err(format!(
                    "data root {} carries unrecognized storage epoch {content:?} \
                     (this trawld writes epoch {CURRENT_EPOCH}) — refusing to \
                     start. This usually means the data dir was written by a \
                     NEWER trawld; downgrading the binary against it is not \
                     supported",
                    data_root.display()
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if aside.exists() {
                return Err(format!(
                    "ambiguous storage state: {} has no {EPOCH_FILE} marker AND \
                     {} already exists — refusing to start rather than guess. \
                     If the current data root holds pre-cutover (legacy) data \
                     you want set aside, move it elsewhere manually (the \
                     standard set-aside name is taken). If it is stray/empty, \
                     remove it and restart; a fresh epoch-{CURRENT_EPOCH} root \
                     will be created",
                    data_root.display(),
                    aside.display()
                ));
            }
            set_aside_legacy_root(data_root, &aside, wal_dir)
        }
        Err(e) => Err(format!(
            "failed to read epoch marker {}: {e}",
            marker.display()
        )),
    }
}

/// The sibling set-aside path for a data root (`data.pre-schema-v2`).
fn set_aside_path(data_root: &Path) -> PathBuf {
    sibling_with_suffix(data_root, SET_ASIDE_SUFFIX)
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "data".to_owned(), |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!("{name}{suffix}"))
}

/// Assemble a fresh epoch-marked root at `data_root` via a staged sibling
/// rename, so there is never a visible `data/` without its marker.
fn create_fresh_root(data_root: &Path) -> Result<(), String> {
    let next = sibling_with_suffix(data_root, NEXT_SUFFIX);
    // A stale staging dir from an earlier crash is disposable by
    // construction (it never held ingested data).
    if next.exists() {
        std::fs::remove_dir_all(&next)
            .map_err(|e| format!("failed to remove stale {}: {e}", next.display()))?;
    }
    std::fs::create_dir_all(&next)
        .map_err(|e| format!("failed to create staging root {}: {e}", next.display()))?;

    let marker = next.join(EPOCH_FILE);
    std::fs::write(&marker, format!("{CURRENT_EPOCH}\n"))
        .map_err(|e| format!("failed to write {}: {e}", marker.display()))?;
    // fsync the marker so the rename below cannot publish a root whose
    // marker reads back empty after a crash.
    std::fs::File::open(&marker)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("failed to fsync {}: {e}", marker.display()))?;

    std::fs::rename(&next, data_root).map_err(|e| {
        format!(
            "failed to move fresh root {} into place at {}: {e}",
            next.display(),
            data_root.display()
        )
    })?;
    // Make the rename durable.
    if let Some(parent) = data_root.parent()
        && let Err(e) = std::fs::File::open(parent).and_then(|d| d.sync_all())
    {
        tracing::warn!(
            event_type = "epoch_dir_fsync_failed",
            dir = %parent.display(),
            error = %e,
            "parent-dir fsync failed; the fresh root is in place but the \
             rename entry may not survive a crash (safe: it would resume \
             via the fresh-install branch)"
        );
    }
    Ok(())
}

/// The legacy-rename branch: set the whole root (and an external WAL dir)
/// aside, then create the fresh marked root.
fn set_aside_legacy_root(
    data_root: &Path,
    aside: &Path,
    wal_dir: &Path,
) -> Result<Outcome, String> {
    let parquet_files = count_files_with_ext(data_root, "parquet");
    let wal_files = count_files_with_ext(wal_dir, "ndjson");

    // An external WAL dir does not move with the root — set it aside
    // FIRST, so a crash after this rename still resumes correctly (the
    // data root is untouched, the branch re-runs, and the WAL set-aside
    // is a no-op because the source is gone).
    let wal_is_external = !wal_dir.starts_with(data_root);
    let mut external_wal_set_aside = false;
    if wal_is_external && wal_dir.exists() {
        let wal_aside = sibling_with_suffix(wal_dir, SET_ASIDE_SUFFIX);
        if wal_aside.exists() {
            return Err(format!(
                "ambiguous WAL state: both {} and {} exist — refusing to \
                 start rather than guess; move one aside manually",
                wal_dir.display(),
                wal_aside.display()
            ));
        }
        std::fs::rename(wal_dir, &wal_aside).map_err(|e| {
            format!(
                "failed to set aside external WAL dir {} → {}: {e}",
                wal_dir.display(),
                wal_aside.display()
            )
        })?;
        external_wal_set_aside = true;
    }

    std::fs::rename(data_root, aside).map_err(|e| {
        format!(
            "failed to set aside legacy data root {} → {}: {e} — the \
             set-aside parent must be writable for the schema-v2 cutover",
            data_root.display(),
            aside.display()
        )
    })?;

    // A crash HERE leaves no data/ with the aside present — resumed by
    // the fresh-install branch on next boot.
    create_fresh_root(data_root)?;

    tracing::warn!(
        event_type = "epoch_cutover",
        set_aside = %aside.display(),
        parquet_files,
        wal_files,
        external_wal_set_aside,
        "pre-schema-v2 data root set aside (ADR-0009: legacy data is \
         dropped from queries, not deleted) — trawl will never remove the \
         set-aside directory; delete it manually to reclaim disk"
    );

    Ok(Outcome::LegacySetAside {
        parquet_files,
        wal_files,
        external_wal_set_aside,
    })
}

/// Recursively count files with the given extension (for the cutover log).
fn count_files_with_ext(dir: &Path, ext: &str) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += count_files_with_ext(&path, ext);
        } else if path.extension().is_some_and(|e| e == ext) {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_marker(root: &Path) -> String {
        std::fs::read_to_string(root.join(EPOCH_FILE))
            .expect("marker present")
            .trim()
            .to_owned()
    }

    #[test]
    fn fresh_install_creates_marked_root() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = data.join("wal");

        let outcome = ensure_current_epoch(&data, &wal).expect("fresh install boots");
        assert_eq!(outcome, Outcome::FreshRoot);
        assert!(data.is_dir());
        assert_eq!(read_marker(&data), CURRENT_EPOCH);
    }

    #[test]
    fn epoch_2_root_boots_normally() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = data.join("wal");
        ensure_current_epoch(&data, &wal).unwrap();

        // Second boot: no-op, no aside warning.
        let outcome = ensure_current_epoch(&data, &wal).expect("epoch-2 boots");
        assert_eq!(
            outcome,
            Outcome::Current {
                aside_present: false
            }
        );
    }

    #[test]
    fn epoch_2_with_lingering_aside_warns_but_boots() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = data.join("wal");
        ensure_current_epoch(&data, &wal).unwrap();
        std::fs::create_dir_all(tmp.path().join("data.pre-schema-v2")).unwrap();

        let outcome = ensure_current_epoch(&data, &wal).expect("must still boot");
        assert_eq!(
            outcome,
            Outcome::Current {
                aside_present: true
            }
        );
    }

    #[test]
    fn legacy_root_is_set_aside_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        // Legacy layout: date dirs at top level, wal inside, no marker.
        let legacy_parquet = data.join("2026-01-15").join("10");
        std::fs::create_dir_all(&legacy_parquet).unwrap();
        std::fs::write(legacy_parquet.join("nginx.parquet"), b"legacy bytes").unwrap();
        let legacy_wal = data.join("wal");
        std::fs::create_dir_all(&legacy_wal).unwrap();
        std::fs::write(legacy_wal.join("nginx_1_aa.ndjson"), b"wal bytes").unwrap();

        let outcome = ensure_current_epoch(&data, &legacy_wal).expect("cutover applies");
        assert_eq!(
            outcome,
            Outcome::LegacySetAside {
                parquet_files: 1,
                wal_files: 1,
                external_wal_set_aside: false,
            }
        );

        // Fresh root with marker; legacy content byte-identical in the aside.
        assert_eq!(read_marker(&data), CURRENT_EPOCH);
        let aside = tmp.path().join("data.pre-schema-v2");
        assert_eq!(
            std::fs::read(aside.join("2026-01-15/10/nginx.parquet")).unwrap(),
            b"legacy bytes"
        );
        assert_eq!(
            std::fs::read(aside.join("wal/nginx_1_aa.ndjson")).unwrap(),
            b"wal bytes"
        );
        // The fresh root holds nothing but the marker.
        let entries: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![EPOCH_FILE.to_owned()]);
    }

    #[test]
    fn second_apply_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("2026-01-15")).unwrap();
        let wal = data.join("wal");

        let first = ensure_current_epoch(&data, &wal).unwrap();
        assert!(matches!(first, Outcome::LegacySetAside { .. }));

        let second = ensure_current_epoch(&data, &wal).unwrap();
        assert_eq!(
            second,
            Outcome::Current {
                aside_present: true
            }
        );
    }

    #[test]
    fn crash_between_set_aside_and_fresh_root_resumes() {
        // Simulated crash window: the legacy root was renamed aside but the
        // fresh root never landed — no data/, aside present.
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let aside = tmp.path().join("data.pre-schema-v2");
        std::fs::create_dir_all(aside.join("2026-01-15")).unwrap();
        let wal = data.join("wal");

        let outcome = ensure_current_epoch(&data, &wal).expect("crash-resume boots");
        assert_eq!(outcome, Outcome::FreshRoot);
        assert_eq!(read_marker(&data), CURRENT_EPOCH);
        assert!(aside.exists(), "the aside is never touched");
    }

    #[test]
    fn stale_staging_dir_is_replaced() {
        // A crash inside create_fresh_root leaves data.next; the retry
        // must discard and rebuild it.
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(tmp.path().join("data.next")).unwrap();
        std::fs::write(tmp.path().join("data.next/garbage"), b"x").unwrap();
        let wal = data.join("wal");

        let outcome = ensure_current_epoch(&data, &wal).unwrap();
        assert_eq!(outcome, Outcome::FreshRoot);
        assert!(!tmp.path().join("data.next").exists());
        assert!(!data.join("garbage").exists());
    }

    #[test]
    fn ambiguous_state_refuses_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("2026-01-15")).unwrap(); // no marker
        std::fs::create_dir_all(tmp.path().join("data.pre-schema-v2")).unwrap();
        let wal = data.join("wal");

        let err = ensure_current_epoch(&data, &wal).expect_err("must refuse");
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(err.contains("refusing to start"), "got: {err}");
        assert!(data.join("2026-01-15").exists(), "nothing is touched");
    }

    #[test]
    fn unknown_epoch_refuses_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join(EPOCH_FILE), "3\n").unwrap();
        let wal = data.join("wal");

        let err = ensure_current_epoch(&data, &wal).expect_err("must refuse");
        assert!(err.contains("unrecognized storage epoch"), "got: {err}");
    }

    #[test]
    fn external_wal_dir_is_set_aside_with_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("2026-01-15")).unwrap();
        let wal = tmp.path().join("fast-wal");
        std::fs::create_dir_all(&wal).unwrap();
        std::fs::write(wal.join("svc_1_aa.ndjson"), b"wal bytes").unwrap();

        let outcome = ensure_current_epoch(&data, &wal).unwrap();
        assert_eq!(
            outcome,
            Outcome::LegacySetAside {
                parquet_files: 0,
                wal_files: 1,
                external_wal_set_aside: true,
            }
        );
        assert!(!wal.exists(), "external WAL dir moved aside");
        assert_eq!(
            std::fs::read(tmp.path().join("fast-wal.pre-schema-v2/svc_1_aa.ndjson")).unwrap(),
            b"wal bytes"
        );
    }

    #[test]
    fn internal_wal_dir_moves_with_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = data.join("wal");
        std::fs::create_dir_all(&wal).unwrap();
        std::fs::write(wal.join("svc_1_aa.ndjson"), b"w").unwrap();

        let outcome = ensure_current_epoch(&data, &wal).unwrap();
        assert_eq!(
            outcome,
            Outcome::LegacySetAside {
                parquet_files: 0,
                wal_files: 1,
                external_wal_set_aside: false,
            }
        );
        assert!(
            tmp.path()
                .join("data.pre-schema-v2/wal/svc_1_aa.ndjson")
                .exists(),
            "internal WAL rides the root rename"
        );
    }
}
