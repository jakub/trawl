// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin operation marker and staging-root layout (ADR-0011 slice B).
//!
//! `data/REPIN` is a small JSON document naming the job, the field, the
//! two types and the current phase. It is written via the shared
//! staged-write idiom ([`crate::epoch::publish_marker_staged`]: temp name
//! → fsync → atomic rename → dir fsync) BEFORE any
//! visible change and removed as the job's final act, so boot recovery
//! reads exactly one file to know whether — and where — a repin died.
//!
//! The shadow and aside roots are SIBLINGS of the data root
//! (`data.repin-next/`, `data.repin-aside/`), the epoch set-aside pattern
//! — NOT dot-directories inside it: `DuckDB`'s recursive glob descends
//! into dot-directories (probed by execution in
//! `trawl-engine/tests/duckdb_probe.rs`), so an in-root shadow would be
//! unioned into every fallback-glob query as duplicate rows. A sibling is
//! invisible to every data-root glob and walk by construction and moves
//! nothing that must not move — `wal/`, `scheduled/`, `EPOCH` and
//! `CATALOG` never leave the data root.
//!
//! Being a sibling puts the staging on the PARENT's filesystem, which is
//! the data root's own for the shipped packaging (the volume is mounted
//! at `/var/lib/trawl`, data at `/var/lib/trawl/data`) but not when an
//! operator mounts a volume AT the data dir (`[data] path = "/mnt/logs"`)
//! — and then no hardlink and no rename can cross. That is not a
//! survivable discovery mid-cutover (the swap is forward-only past the
//! marker, so an `EXDEV` there costs the process and every subsequent
//! boot replays the same `EXDEV`), so [`check_staging_filesystem`] proves
//! it BEFORE a job is allowed to build anything.

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepinMarker {
    /// The `repin_jobs` row this marker belongs to.
    pub job_id: i64,
    /// The repinned field (catalog key, folded).
    pub field: String,
    /// The pin at claim time (`DuckDB` spelling).
    pub from_type: String,
    /// The target pin (`DuckDB` spelling).
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

/// Prove the staging siblings will land on the data root's OWN filesystem
/// — i.e. that the data root is not itself a mount point.
///
/// The whole staging design is renames and hardlinks between the data root
/// and its siblings, neither of which crosses a filesystem. Discovering
/// that mid-job is only ever bad: a hardlink `EXDEV` fails the build
/// (clean, but late), and a corpus where nothing needed hardlinking builds
/// fine and then meets the SAME `EXDEV` in the forward-only swap, where
/// the only safe answer is to exit the process — after which every boot
/// replays the marker into the identical failure and refuses to start.
/// One `stat` pair up front turns all of that into a refusal that changes
/// nothing.
pub fn check_staging_filesystem(data_dir: &Path) -> Result<(), String> {
    let parent = data_dir.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    if device_of(data_dir)? == device_of(parent)? {
        return Ok(());
    }
    Err(format!(
        "the data root {} is a mount point, so the repin staging roots \
         ({}, {}) would sit on the parent filesystem — the hardlinks and \
         the atomic per-env swap a repin is built from cannot cross \
         filesystems (EXDEV). Mount the volume one level up and put the \
         data root inside it (the packaged layout: volume at \
         /var/lib/trawl, [data] path = \"/var/lib/trawl/data\")",
        data_dir.display(),
        shadow_root(data_dir).display(),
        aside_root(data_dir).display()
    ))
}

/// The filesystem a path lives on. Non-unix has no device identity to
/// compare, so the check is a no-op there (trawld ships for Linux).
fn device_of(path: &Path) -> Result<u64, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        Ok(std::os::unix::fs::MetadataExt::dev(&meta))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(0)
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

    /// The staging roots are SIBLINGS of the data root — outside every
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
    /// inside its volume — passes the pre-flight.
    #[test]
    fn a_data_root_inside_its_volume_passes_the_staging_check() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        check_staging_filesystem(&data).expect("siblings share the parent's filesystem");
    }

    /// A data root that is itself a mount point is refused BEFORE anything
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
