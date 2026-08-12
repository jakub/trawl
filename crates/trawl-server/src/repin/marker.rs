// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin operation marker and staging-root layout (ADR-0011 slice B).
//!
//! `data/REPIN` is a small JSON document naming the job, the field, the
//! two types and the current phase. It is written via the staged-write
//! idiom (temp name → fsync → atomic rename → dir fsync) BEFORE any
//! visible change and removed as the job's final act, so boot recovery
//! reads exactly one file to know whether — and where — a repin died.
//!
//! The shadow and aside roots are SIBLINGS of the data root
//! (`data.repin-next/`, `data.repin-aside/`), the epoch set-aside pattern
//! — NOT dot-directories inside it: `DuckDB`'s recursive glob descends
//! into dot-directories (probed by execution in
//! `trawl-engine/tests/duckdb_probe.rs`), so an in-root shadow would be
//! unioned into every fallback-glob query as duplicate rows. A sibling is
//! invisible to every data-root glob and walk by construction, sits on
//! the same filesystem (hardlinks and renames are guaranteed), and moves
//! nothing that must not move — `wal/`, `scheduled/`, `EPOCH` and
//! `CATALOG` never leave the data root.

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

/// Write (or rewrite) the marker via the staged-write idiom, durable
/// before it is visible: temp name → fsync → atomic rename → dir fsync.
pub fn write_marker(data_dir: &Path, marker: &RepinMarker) -> Result<(), String> {
    let staged = data_dir.join(format!("{REPIN_MARKER}.next.{}", std::process::id()));
    let body = serde_json::to_string(marker).map_err(|e| format!("marker serialize: {e}"))?;
    std::fs::write(&staged, body)
        .map_err(|e| format!("failed to write {}: {e}", staged.display()))?;
    std::fs::File::open(&staged)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("failed to fsync {}: {e}", staged.display()))?;
    let path = marker_path(data_dir);
    std::fs::rename(&staged, &path)
        .map_err(|e| format!("failed to publish {}: {e}", path.display()))?;
    crate::epoch::fsync_dir_best_effort(data_dir);
    Ok(())
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
}
