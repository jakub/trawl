// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin operation marker and staging-root layout (ADR-0011).
//!
//! `data/REPIN` is a small JSON document naming the job, the field, the
//! two types and the current phase. It is written via the shared
//! staged-write idiom ([`crate::epoch::publish_marker_staged`]: temp name
//! → fsync → atomic rename → dir fsync) before any visible change and
//! removed as the job's final act, so boot recovery reads exactly one
//! file to know whether — and where — a repin died.
//!
//! The shadow and aside roots are siblings of the data root
//! (`data.repin-next/`, `data.repin-aside/`), the epoch set-aside pattern,
//! rather than dot-directories inside it: `DuckDB`'s recursive glob
//! descends into dot-directories (probed by execution in
//! `trawl-engine/tests/duckdb_probe.rs`), so an in-root shadow would be
//! unioned into every fallback-glob query as duplicate rows. A sibling is
//! invisible to every data-root glob and walk by construction and moves
//! nothing that must not move — `wal/`, `scheduled/`, `EPOCH` and
//! `CATALOG` never leave the data root.
//!
//! Being a sibling puts the staging on the parent's filesystem, which is
//! the data root's own for the shipped packaging (the volume is mounted
//! at `/var/lib/trawl`, data at `/var/lib/trawl/data`) but not when an
//! operator mounts a volume at the data dir (`[data] path = "/mnt/logs"`)
//! — and then no hardlink and no rename can cross. The root is not the
//! only place a mount can sit: everything the engine touches lives
//! arbitrarily deep under it (the shadow build hardlinks
//! `{env}/{date}/{HH}/{service}.parquet`, the swap renames `{env}`), so a
//! volume mounted at any env/date/hour subtree — tiered storage, a
//! plausible archive layout — breaks the same two halves. That is not a
//! survivable discovery mid-cutover (the swap is forward-only past the
//! marker, so an `EXDEV`/`EBUSY` there costs the process and every
//! subsequent boot replays it), so [`check_staging_filesystem`] proves
//! the whole env subtree is one filesystem before a job is allowed to
//! build anything.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Marker filename inside the data root.
pub const REPIN_MARKER: &str = "REPIN";

/// Suffix of the shadow (next-generation) sibling root.
pub const SHADOW_SUFFIX: &str = ".repin-next";

/// Suffix of the aside (previous-generation) sibling root.
pub const ASIDE_SUFFIX: &str = ".repin-aside";

/// Where a repin job died, as recorded in the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepinPhase {
    /// The shadow generation is being built (or caught up). The live
    /// corpus is untouched; the shadow is disposable.
    Building,
    /// The per-env swap has begun: some envs may already serve the new
    /// generation. Forward is the only safe direction.
    Cutover,
    /// The swap and the pin flip are done; only the aside sweep and the
    /// marker removal remain.
    Cleanup,
}

/// The `data/REPIN` document.
///
/// Dialect-free by design: a repin to `SEVERITY` may assert that the
/// corpus's numerals are syslog PRI, but no replay path ever re-runs a
/// cast — a `building` marker abandons the shadow (the values were never
/// written), and a `cutover`/`cleanup` marker only completes renames over
/// files the job already wrote. The dialect lives on the job row, where
/// the report needs it; putting it here would imply a recovery that could
/// re-read wire text, which forward-only recovery does not do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepinMarker {
    /// The `repin_jobs` row this marker belongs to.
    pub job_id: i64,
    /// The repinned field (catalog key, folded).
    pub field: String,
    /// The pin at claim time (catalog spelling — `SEVERITY` is not
    /// `BIGINT`, and boot recovery parses this back).
    pub from_type: String,
    /// The target pin (catalog spelling, as written by
    /// `CanonicalType::as_catalog`).
    pub to_type: String,
    /// Where the job is.
    pub phase: RepinPhase,
}

/// The shadow sibling root for a data root.
#[must_use]
pub fn shadow_root(data_dir: &Path) -> PathBuf {
    sibling(data_dir, SHADOW_SUFFIX)
}

/// The aside sibling root for a data root.
#[must_use]
pub fn aside_root(data_dir: &Path) -> PathBuf {
    sibling(data_dir, ASIDE_SUFFIX)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "data".to_owned(), |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!("{name}{suffix}"))
}

/// The marker path inside a data root.
#[must_use]
pub fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(REPIN_MARKER)
}

/// Evidence that a repin job owns this data root right now: the marker, a
/// shadow root, or an aside root, named for a log line, or `None` when
/// there is none.
///
/// The one authority on that question. Retention stands down on it and pin
/// gc refuses on it, and both are about not touching a corpus a repin is
/// mid-way through rearranging, so a second implementation is a second
/// opinion about whether it is safe to delete something.
///
/// Fallible, unlike [`Path::exists`], which folds every I/O error into
/// `false`. Here `false` means "go ahead", so a data root that cannot be
/// stat'ed would read as permission to proceed. Callers decide what an
/// unreadable answer means to them; both of today's treat it as evidence.
///
/// # Errors
/// The first `try_exists` error, with the marker checked before the shadow
/// root and the shadow before the aside.
pub fn in_flight_evidence(data_dir: &Path) -> Result<Option<&'static str>, std::io::Error> {
    for (path, what) in [
        (marker_path(data_dir), "marker"),
        (shadow_root(data_dir), "shadow root"),
        (aside_root(data_dir), "aside root"),
    ] {
        if path.try_exists()? {
            return Ok(Some(what));
        }
    }
    Ok(None)
}

/// Prove the staging siblings will land on the data root's own filesystem
/// and that the whole env subtree the engine moves lives on that same
/// filesystem — i.e. that neither the data root nor anything nested under
/// an env directory is a mount point.
///
/// The whole staging design is renames and hardlinks between the data root
/// and its siblings, neither of which crosses a filesystem. Discovering
/// that mid-job is only ever bad: a hardlink `EXDEV` fails the build
/// (clean, but late), and a corpus where nothing needed hardlinking builds
/// fine and then meets the same failure in the forward-only swap, where
/// the only safe answer is to exit the process — after which every boot
/// replays the marker into the identical failure and refuses to start. A
/// nested mount fails identically and even more opaquely: renaming a
/// directory that contains a mount point is `EBUSY`, not `EXDEV`. One
/// stat walk up front turns all of that into a refusal that changes
/// nothing.
pub fn check_staging_filesystem(data_dir: &Path) -> Result<(), String> {
    let parent = data_dir.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let root_dev = device_of(data_dir)?;
    if root_dev != device_of(parent)? {
        return Err(format!(
            "the data root {} is a mount point, so the repin staging roots \
             ({}, {}) would sit on the parent filesystem — the hardlinks and \
             the atomic per-env swap a repin is built from cannot cross \
             filesystems (EXDEV). Mount the volume one level up and put the \
             data root inside it (the packaged layout: volume at \
             /var/lib/trawl, [data] path = \"/var/lib/trawl/data\")",
            data_dir.display(),
            shadow_root(data_dir).display(),
            aside_root(data_dir).display()
        ));
    }
    check_env_subtree_devices(data_dir, root_dev, &|meta| device_of_meta(meta))
}

/// Walk the env directories the swap moves and the shadow build
/// hardlinks, refusing the first entry that does not share `root_dev`.
///
/// The device function is injected so the walk itself is testable without
/// a mount (creating one needs privileges the test suite does not have);
/// production always passes [`device_of_meta`].
fn check_env_subtree_devices<F>(data_dir: &Path, root_dev: u64, dev_of: &F) -> Result<(), String>
where
    F: Fn(&std::fs::Metadata) -> u64,
{
    for (_env, env_dir) in crate::env_dirs::try_list_env_dirs(data_dir)
        .map_err(|e| format!("failed to list env dirs under {}: {e}", data_dir.display()))?
    {
        walk_devices(&env_dir, data_dir, root_dev, dev_of)?;
    }
    Ok(())
}

fn walk_devices<F>(dir: &Path, data_dir: &Path, root_dev: u64, dev_of: &F) -> Result<(), String>
where
    F: Fn(&std::fs::Metadata) -> u64,
{
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("failed to stat {}: {e}", dir.display()))?;
    if dev_of(&meta) != root_dev {
        return Err(nested_mount_message(data_dir, dir));
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
        let path = entry.path();
        // `DirEntry::metadata` is an `lstat`, so a symlink reports the
        // device of the directory holding it rather than of its target —
        // which is what this check wants: the shadow build refuses
        // symlinks outright (`rewrite::snapshot_env_files`), and a link's
        // target is never hardlinked or renamed by this engine.
        let meta = entry
            .metadata()
            .map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
        if dev_of(&meta) != root_dev {
            return Err(nested_mount_message(data_dir, &path));
        }
        if meta.is_dir() {
            walk_devices(&path, data_dir, root_dev, dev_of)?;
        }
    }
    Ok(())
}

fn nested_mount_message(data_dir: &Path, offender: &Path) -> String {
    format!(
        "{} is on a different filesystem than the data root {} (a volume \
         mounted at a subdirectory of the corpus), which a repin cannot \
         stage: the shadow build hardlinks every unaffected file into {} \
         (EXDEV across filesystems) and the cutover renames each env \
         directory, which fails with EBUSY once it contains a mount point \
         — and that half runs past the point where the only safe answer is \
         to exit, so every subsequent boot would replay it. Move the nested \
         volume out of the data root (one filesystem for the whole corpus; \
         the packaged layout: volume at /var/lib/trawl, [data] path = \
         \"/var/lib/trawl/data\") and retry",
        offender.display(),
        data_dir.display(),
        shadow_root(data_dir).display()
    )
}

/// The filesystem a path lives on. Non-unix has no device identity to
/// compare, so the check is a no-op there (trawld ships for Linux).
fn device_of(path: &Path) -> Result<u64, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
    Ok(device_of_meta(&meta))
}

fn device_of_meta(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::dev(meta)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

/// Read the marker, if present. A present-but-unreadable marker is an
/// error, never a silent None — recovery deciding "no repin died here"
/// off a corrupt marker would serve a half-swapped corpus.
pub fn read_marker(data_dir: &Path) -> Result<Option<RepinMarker>, String> {
    let path = marker_path(data_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// Write (or rewrite) the marker through the shared staged-write idiom
/// ([`crate::epoch::publish_marker_staged`]), durable before it is
/// visible: temp name → fsync → atomic rename → dir fsync.
pub fn write_marker(data_dir: &Path, marker: &RepinMarker) -> Result<(), String> {
    let body = serde_json::to_string(marker).map_err(|e| format!("marker serialize: {e}"))?;
    crate::epoch::publish_marker_staged(data_dir, REPIN_MARKER, &body)
}

/// Remove the marker — the job's final act.
pub fn remove_marker(data_dir: &Path) -> Result<(), String> {
    let path = marker_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            crate::epoch::fsync_dir_best_effort(data_dir);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(phase: RepinPhase) -> RepinMarker {
        RepinMarker {
            job_id: 7,
            field: "status".to_owned(),
            from_type: "BIGINT".to_owned(),
            to_type: "VARCHAR".to_owned(),
            phase,
        }
    }

    #[test]
    fn marker_round_trips_through_every_phase() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_marker(tmp.path()).unwrap(), None);
        for phase in [
            RepinPhase::Building,
            RepinPhase::Cutover,
            RepinPhase::Cleanup,
        ] {
            write_marker(tmp.path(), &marker(phase)).unwrap();
            assert_eq!(read_marker(tmp.path()).unwrap(), Some(marker(phase)));
        }
        remove_marker(tmp.path()).unwrap();
        assert_eq!(read_marker(tmp.path()).unwrap(), None);
        remove_marker(tmp.path()).expect("idempotent removal");
    }

    /// A corrupt marker is a loud error: recovery must never conclude "no
    /// repin died here" from a file it could not read.
    #[test]
    fn corrupt_marker_errors_rather_than_reading_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(REPIN_MARKER), b"torn{").unwrap();
        read_marker(tmp.path()).expect_err("corruption must surface");
    }

    /// The staging roots are siblings of the data root — outside every
    /// data-root glob and walk (see the module doc for the probe that
    /// forced this).
    #[test]
    fn staging_roots_are_siblings_of_the_data_root() {
        let data = Path::new("/var/lib/trawl/data");
        assert_eq!(
            shadow_root(data),
            Path::new("/var/lib/trawl/data.repin-next")
        );
        assert_eq!(
            aside_root(data),
            Path::new("/var/lib/trawl/data.repin-aside")
        );
        assert!(!shadow_root(data).starts_with(data));
    }

    /// The shipped shape — a data root that is an ordinary directory
    /// inside its volume, corpus and all — passes the pre-flight, walk
    /// included.
    #[test]
    fn a_data_root_inside_its_volume_passes_the_staging_check() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        check_staging_filesystem(&data).expect("siblings share the parent's filesystem");

        std::fs::create_dir_all(data.join("prod/2026-01-01/10")).unwrap();
        std::fs::write(data.join("prod/2026-01-01/10/svc.parquet"), b"rows").unwrap();
        std::fs::create_dir_all(data.join("wal/prod")).unwrap();
        check_staging_filesystem(&data).expect("one filesystem for the whole corpus");
    }

    /// A volume mounted at an env/date/hour subtree breaks both halves of
    /// the engine exactly as a mount at the root does — the shadow build
    /// hardlinks out of it (EXDEV) and the cutover renames the env
    /// directory containing it (EBUSY, past the point of no return) — so
    /// the pre-flight walks the whole env subtree, not just the root.
    /// A mount cannot be created in the test suite, so the device lookup
    /// is injected; [`check_staging_filesystem`] wires the real one.
    #[cfg(unix)]
    #[test]
    fn a_mount_point_nested_under_the_data_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("prod/2026-01-01/10")).unwrap();
        std::fs::write(data.join("prod/2026-01-01/10/svc.parquet"), b"rows").unwrap();
        let tiered = data.join("prod/2026-01-01/10");

        let foreign_ino = inode(&tiered);
        let dev_of = |meta: &std::fs::Metadata| {
            if inode_of(meta) == foreign_ino {
                7777
            } else {
                0
            }
        };
        let err = check_env_subtree_devices(&data, 0, &dev_of)
            .expect_err("a nested mount must be refused before anything is built");
        assert!(err.contains(&tiered.display().to_string()), "{err}");
        assert!(err.contains("EXDEV"), "{err}");
        assert!(err.contains("EBUSY"), "{err}");
    }

    /// A bind-mounted file is the same defect one level down: the shadow
    /// build hardlinks it, so a different device is EXDEV all the same.
    #[cfg(unix)]
    #[test]
    fn a_foreign_file_under_an_env_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("prod/2026-01-01/10")).unwrap();
        let file = data.join("prod/2026-01-01/10/svc.parquet");
        std::fs::write(&file, b"rows").unwrap();

        let foreign_ino = inode(&file);
        let dev_of = |meta: &std::fs::Metadata| {
            if inode_of(meta) == foreign_ino {
                7777
            } else {
                0
            }
        };
        let err = check_env_subtree_devices(&data, 0, &dev_of).expect_err("EXDEV in waiting");
        assert!(err.contains(&file.display().to_string()), "{err}");
    }

    /// Only what the engine moves is in scope: `wal/` and `scheduled/`
    /// never leave the data root, so a volume mounted there is not this
    /// job's business and must not refuse it.
    #[cfg(unix)]
    #[test]
    fn a_foreign_filesystem_outside_the_env_dirs_is_not_this_checks_business() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("prod/2026-01-01/10")).unwrap();
        std::fs::create_dir_all(data.join("wal/prod")).unwrap();
        std::fs::write(data.join("wal/prod/batch.ndjson"), b"{}").unwrap();

        let foreign_ino = inode(&data.join("wal/prod"));
        let dev_of = |meta: &std::fs::Metadata| {
            if inode_of(meta) == foreign_ino {
                7777
            } else {
                0
            }
        };
        check_env_subtree_devices(&data, 0, &dev_of).expect("wal/ never rides the swap");
    }

    /// Singles one entry out of a tempdir where everything really does
    /// share a device: inode identity stands in for "this one is the
    /// mount point".
    #[cfg(unix)]
    fn inode(path: &Path) -> u64 {
        inode_of(&std::fs::symlink_metadata(path).unwrap())
    }

    #[cfg(unix)]
    fn inode_of(meta: &std::fs::Metadata) -> u64 {
        std::os::unix::fs::MetadataExt::ino(meta)
    }

    /// A data root that is itself a mount point is refused before anything
    /// is built — the shape `[data] path = "/mnt/logs"` creates, and the
    /// one the forward-only cutover cannot survive. `/proc` is a real
    /// mount under `/` on every Linux box, so this is a genuine
    /// cross-device stat rather than a mocked one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_data_root_on_its_own_mount_is_refused() {
        let mount = Path::new("/proc");
        if !mount.exists() || device_of(mount).unwrap() == device_of(Path::new("/")).unwrap() {
            return; // no /proc to lean on; nothing to prove here
        }
        let err = check_staging_filesystem(mount).expect_err("a mount point must be refused");
        assert!(err.contains("/proc.repin-next"), "{err}");
        assert!(err.contains("EXDEV"), "{err}");
    }

    /// A relative data root has an empty `parent()`; the check must read
    /// that as the working directory rather than stat `""` and fail.
    #[test]
    fn a_relative_single_component_data_root_checks_against_the_cwd() {
        check_staging_filesystem(Path::new(".")).expect("the cwd shares its own filesystem");
    }
}
