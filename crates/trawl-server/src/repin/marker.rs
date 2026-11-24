// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin operation marker and staging-root layout (ADR-0011).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const REPIN_MARKER: &str = trawl_config::REPIN_MARKER;

/// Suffix of the shadow (next-generation) sibling root. Config owns the
/// spelling, because its query scratch check refuses these roots too.
pub const SHADOW_SUFFIX: &str = trawl_config::REPIN_SHADOW_SUFFIX;

pub const ASIDE_SUFFIX: &str = trawl_config::REPIN_ASIDE_SUFFIX;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepinMarker {
    pub job_id: i64,
    /// The repinned field (catalog key, folded).
    pub field: String,
    /// The pin at claim time (catalog spelling — `SEVERITY` is not
    /// `BIGINT`, and boot recovery parses this back).
    pub from_type: String,
    /// The target pin (catalog spelling, as written by
    /// `CanonicalType::as_catalog`).
    pub to_type: String,
    pub phase: RepinPhase,
}

impl RepinPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Cutover => "cutover",
            Self::Cleanup => "cleanup",
        }
    }

    #[must_use]
    pub const fn query_only_refuses(self) -> bool {
        match self {
            Self::Building => false,
            Self::Cutover | Self::Cleanup => true,
        }
    }
}

impl RepinMarker {
    #[must_use]
    pub fn target_type(&self) -> Option<trawl_core::schema::CanonicalType> {
        trawl_core::schema::CanonicalType::from_catalog(&self.to_type)
    }
}

#[must_use]
pub fn shadow_root(data_dir: &Path) -> PathBuf {
    trawl_config::data_root_sibling(data_dir, SHADOW_SUFFIX)
}

#[must_use]
pub fn aside_root(data_dir: &Path) -> PathBuf {
    trawl_config::data_root_sibling(data_dir, ASIDE_SUFFIX)
}

#[must_use]
pub fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(REPIN_MARKER)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagingEvidence {
    Marker,
    ShadowRoot,
    AsideRoot,
}

impl StagingEvidence {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Marker => "marker",
            Self::ShadowRoot => "shadow root",
            Self::AsideRoot => "aside root",
        }
    }
}

/// The marker, shadow root or aside root showing a repin job owns this
/// data root now. The one authority on that question: retention stands
/// down on it, pin gc refuses on it, and repin admission refuses over it.
///
/// Fallible, unlike [`Path::exists`], which folds every I/O error into
/// `false`. `None` means "go ahead", so a data root that cannot be
/// stat'ed would read as permission to proceed.
pub fn staging_evidence(data_dir: &Path) -> Result<Option<StagingEvidence>, std::io::Error> {
    for (path, found) in [
        (marker_path(data_dir), StagingEvidence::Marker),
        (shadow_root(data_dir), StagingEvidence::ShadowRoot),
        (aside_root(data_dir), StagingEvidence::AsideRoot),
    ] {
        if path.try_exists()? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

pub fn in_flight_evidence(data_dir: &Path) -> Result<Option<&'static str>, std::io::Error> {
    Ok(staging_evidence(data_dir)?.map(StagingEvidence::label))
}

/// Prove the staging siblings will land on the data root's own filesystem
/// and that the whole env subtree the engine moves lives on that same
/// filesystem — i.e. that neither the data root nor anything nested under
/// an env directory is a mount point.
///
/// Found mid-job, it fails the build late, or reaches the forward-only
/// swap, which can only exit the process; every boot then replays the
/// marker into the identical failure. A nested mount fails as `EBUSY`,
/// not `EXDEV`.
pub fn check_staging_filesystem(data_dir: &Path) -> Result<(), String> {
    let parent = data_dir.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let root_dev = device_of(data_dir, "the data root")?;
    if root_dev != device_of(parent, "the data root's parent directory")? {
        return Err(mount_point_message(data_dir));
    }
    check_env_subtree_devices(data_dir, root_dev, &|meta| device_of_meta(meta))
}

fn mount_point_message(data_dir: &Path) -> String {
    format!(
        "the data root ([data] path) is a mount point, so the repin staging \
         roots beside it ({}, {}) would sit on the parent filesystem — the \
         hardlinks and the atomic per-env swap a repin is built from cannot \
         cross filesystems (EXDEV). Mount the volume one level up and put \
         the data root inside it (the packaged layout: volume at \
         /var/lib/trawl, [data] path = \"/var/lib/trawl/data\")",
        sibling_name(&shadow_root(data_dir)),
        sibling_name(&aside_root(data_dir))
    )
}

fn sibling_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || "a sibling of the data root".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn io_refusal(what: &str, path: &Path, e: &std::io::Error) -> String {
    tracing::debug!(
        event_type = "repin_staging_preflight_io",
        path = %path.display(),
        error = %e,
        "the repin staging pre-flight could not read a path"
    );
    format!(
        "the repin staging pre-flight failed to {what} ({})",
        e.kind()
    )
}

/// The device function is injected so the walk itself is testable without
/// a mount (creating one needs privileges the test suite does not have).
fn check_env_subtree_devices<F>(data_dir: &Path, root_dev: u64, dev_of: &F) -> Result<(), String>
where
    F: Fn(&std::fs::Metadata) -> u64,
{
    for (_env, env_dir) in crate::env_dirs::try_list_env_dirs(data_dir)
        .map_err(|e| io_refusal("list the env dirs under the data root", data_dir, &e))?
    {
        walk_devices(&env_dir, data_dir, root_dev, dev_of)?;
    }
    Ok(())
}

fn walk_devices<F>(dir: &Path, data_dir: &Path, root_dev: u64, dev_of: &F) -> Result<(), String>
where
    F: Fn(&std::fs::Metadata) -> u64,
{
    let rel = |path: &Path| crate::catalog::gc::root_relative(data_dir, path);
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| io_refusal(&format!("stat {}", rel(dir)), dir, &e))?;
    // Only an env directory can be a link here: the listing follows one,
    // and the walk below never descends into a link. Reading on would
    // walk the link's target outside the data root and name its children
    // as entries of this env.
    if meta.is_symlink() {
        return Err(format!(
            "the env directory {} is a symlink: the repin shadow cannot carry \
             symlinked entries under the data root — replace it with the real \
             directory and retry",
            rel(dir)
        ));
    }
    if dev_of(&meta) != root_dev {
        return Err(nested_mount_message(data_dir, dir));
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| io_refusal(&format!("read {}", rel(dir)), dir, &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_refusal(&format!("read {}", rel(dir)), dir, &e))?;
        let path = entry.path();
        // `DirEntry::metadata` is an `lstat`, so a symlink reports the
        // device of the directory holding it rather than of its target —
        // which is what this check wants: the shadow build refuses
        // symlinks outright (`rewrite::snapshot_env_files`), and a link's
        // target is never hardlinked or renamed by this engine.
        let meta = entry
            .metadata()
            .map_err(|e| io_refusal(&format!("stat {}", rel(&path)), &path, &e))?;
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
        "{} is on a different filesystem than the data root (a volume \
         mounted at a subdirectory of the corpus), which a repin cannot \
         stage: the shadow build hardlinks every unaffected file into {} \
         (EXDEV across filesystems) and the cutover renames each env \
         directory, which fails with EBUSY once it contains a mount point \
         — and that half runs past the point where the only safe answer is \
         to exit, so every subsequent boot would replay it. Move the nested \
         volume out of the data root (one filesystem for the whole corpus; \
         the packaged layout: volume at /var/lib/trawl, [data] path = \
         \"/var/lib/trawl/data\") and retry",
        crate::catalog::gc::root_relative(data_dir, offender),
        sibling_name(&shadow_root(data_dir))
    )
}

/// The filesystem a path lives on. Non-unix has no device identity to
/// compare, so the check is a no-op there (trawld ships for Linux).
fn device_of(path: &Path, what: &str) -> Result<u64, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| io_refusal(&format!("stat {what}"), path, &e))?;
    Ok(device_of_meta(&meta))
}

pub(crate) fn device_of_meta(meta: &std::fs::Metadata) -> u64 {
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
    parse_marker(&raw)
        .map(Some)
        .map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// Parse a marker's text: the one decoder boot recovery and
/// `trawld --doctor` share.
pub fn parse_marker(raw: &str) -> Result<RepinMarker, serde_json::Error> {
    serde_json::from_str(raw)
}

pub fn write_marker(data_dir: &Path, marker: &RepinMarker) -> Result<(), String> {
    let body = serde_json::to_string(marker).map_err(|e| format!("marker serialize: {e}"))?;
    crate::epoch::publish_marker_staged(data_dir, REPIN_MARKER, &body)
}

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

    #[test]
    fn a_phase_reads_as_the_marker_spells_it() {
        for phase in [
            RepinPhase::Building,
            RepinPhase::Cutover,
            RepinPhase::Cleanup,
        ] {
            assert_eq!(
                serde_json::to_string(&phase).unwrap(),
                format!("\"{}\"", phase.as_str())
            );
        }
    }

    #[test]
    fn staging_evidence_names_the_marker_then_the_shadow_then_the_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        assert_eq!(staging_evidence(&data).unwrap(), None);
        assert_eq!(in_flight_evidence(&data).unwrap(), None);

        std::fs::create_dir(aside_root(&data)).unwrap();
        assert_eq!(
            staging_evidence(&data).unwrap(),
            Some(StagingEvidence::AsideRoot)
        );
        std::fs::create_dir(shadow_root(&data)).unwrap();
        assert_eq!(
            staging_evidence(&data).unwrap(),
            Some(StagingEvidence::ShadowRoot)
        );
        write_marker(&data, &marker(RepinPhase::Cleanup)).unwrap();
        assert_eq!(
            staging_evidence(&data).unwrap(),
            Some(StagingEvidence::Marker)
        );
        assert_eq!(in_flight_evidence(&data).unwrap(), Some("marker"));
    }

    #[test]
    fn corrupt_marker_errors_rather_than_reading_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(REPIN_MARKER), b"torn{").unwrap();
        read_marker(tmp.path()).expect_err("corruption must surface");
    }

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
        assert!(err.starts_with("prod/2026-01-01/10 is on"), "{err}");
        assert!(err.contains("into data.repin-next"), "{err}");
        assert!(!err.contains(&tmp.path().display().to_string()), "{err}");
        assert!(err.contains("EXDEV"), "{err}");
        assert!(err.contains("EBUSY"), "{err}");
    }

    #[test]
    fn staging_refusals_carry_no_host_path_or_io_text() {
        const MARKER: &str = "zz_io_marker_/srv/secret";
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let abs = tmp.path().display().to_string();

        let injected = std::io::Error::other(MARKER);
        let msg = io_refusal("stat prod/sub", &data.join("prod/sub"), &injected);
        assert_eq!(
            msg,
            "the repin staging pre-flight failed to stat prod/sub (other error)"
        );

        let msg = mount_point_message(&data);
        assert!(!msg.contains(&abs), "{msg}");
        assert!(msg.contains("(data.repin-next, data.repin-aside)"), "{msg}");
        assert!(msg.contains("Mount the volume one level up"), "{msg}");

        let err = check_staging_filesystem(&data).expect_err("nothing to stat");
        assert!(!err.contains(&abs), "{err}");
        assert!(!err.contains("os error"), "{err}");
        assert_eq!(
            err,
            "the repin staging pre-flight failed to stat the data root (entity not found)"
        );
    }

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
        assert!(
            err.starts_with("prod/2026-01-01/10/svc.parquet is on"),
            "{err}"
        );
        assert!(!err.contains(&tmp.path().display().to_string()), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn staging_preflight_refuses_a_symlinked_env_dir() {
        const MARKER: &str = "MARKER-outside-file";
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let planted = outside.path().join(MARKER);
        std::fs::write(&planted, b"rows").unwrap();
        std::os::unix::fs::symlink(outside.path(), data.join("prod")).unwrap();
        let outside_abs = outside.path().display().to_string();
        let expected = "the env directory prod is a symlink: the repin shadow cannot \
                        carry symlinked entries under the data root — replace it with \
                        the real directory and retry";

        // The real device lookup: both tempdirs share a filesystem, so
        // only the link itself can refuse.
        let err = check_staging_filesystem(&data).expect_err("a symlinked env must refuse");
        assert_eq!(err, expected);

        // A link target's child on another device must not be named.
        let foreign_ino = inode(&planted);
        let dev_of = |meta: &std::fs::Metadata| {
            if inode_of(meta) == foreign_ino {
                7777
            } else {
                0
            }
        };
        let err =
            check_env_subtree_devices(&data, 0, &dev_of).expect_err("a symlinked env must refuse");
        assert!(!err.contains(MARKER), "{err}");
        assert!(!err.contains(&outside_abs), "{err}");
        assert_eq!(err, expected);
    }

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

    /// `/proc` is a real mount under `/` on every Linux box, so this is a
    /// genuine cross-device stat rather than a mocked one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_data_root_on_its_own_mount_is_refused() {
        let mount = Path::new("/proc");
        if !mount.exists()
            || device_of(mount, "/proc").unwrap() == device_of(Path::new("/"), "/").unwrap()
        {
            return;
        }
        let err = check_staging_filesystem(mount).expect_err("a mount point must be refused");
        assert!(err.contains("(proc.repin-next, proc.repin-aside)"), "{err}");
        assert!(!err.contains("/proc"), "{err}");
        assert!(err.contains("EXDEV"), "{err}");
    }

    /// A relative data root has an empty `parent()`; the check must read
    /// that as the working directory rather than stat `""` and fail.
    #[test]
    fn a_relative_single_component_data_root_checks_against_the_cwd() {
        check_staging_filesystem(Path::new(".")).expect("the cwd shares its own filesystem");
    }
}
