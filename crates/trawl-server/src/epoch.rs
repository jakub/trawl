// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Current storage-format gate. No storage conversion runs at startup.
//!
//! Owned roots require `EPOCH = 3`. A missing or empty root can be initialized;
//! an interrupted initial marker write can be retried on an otherwise empty
//! owned root. Any other nonempty root without its marker must be restored
//! from a complete current backup or replaced with a new empty root selected
//! by the operator.
//! Query-only nodes can also read unversioned generic archives without Trawl
//! ownership markers. They never initialize those archives or inspect unused WAL.
//! An explicit incompatible epoch refuses in every mode.
//!
//! The gate precedes repin recovery. Current repin swaps only environment
//! directories, so it cannot change EPOCH. Recovery still finishes before
//! readers or conformance inspect the corpus.

use std::io::{Read as _, Write as _};
use std::path::Path;

/// The current storage epoch, written to `data/EPOCH`.
pub const CURRENT_EPOCH: &str = "3";
/// Marker filename inside the data root.
pub const EPOCH_FILE: &str = "EPOCH";
/// Suffix for staged marker files shared with catalog, repin, and rollup.
const NEXT_SUFFIX: &str = ".next";

/// What the boot gate decided, for logging and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Created a missing root and published its current marker.
    FreshRoot,
    /// The existing marker names the current format.
    Current,
    /// Published the current marker in a precreated empty root.
    InitializedEmpty,
    /// Unversioned generic archive, left untouched by this query-only node.
    ReadOnlyArchive,
}

/// Validate storage before any recovery can mutate the data root or siblings.
/// All refusal checks run before fresh-root initialization. Filesystem errors
/// are errors, never evidence of an empty directory or an absent marker.
pub fn ensure_current_epoch(
    data_root: &Path,
    wal_dir: &Path,
    ingest_enabled: bool,
) -> Result<Outcome, String> {
    if ingest_enabled {
        validate_wal(wal_dir)?;
    }
    let Some(meta) = metadata_if_present(data_root)? else {
        if !ingest_enabled {
            return Ok(Outcome::ReadOnlyArchive);
        }
        if let Some(parent) = data_root.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create data parent {}: {e}", parent.display()))?;
        }
        // Do not adopt a root another process created after the absence check.
        std::fs::create_dir(data_root)
            .map_err(|e| format!("failed to create data root {}: {e}", data_root.display()))?;
        publish_epoch(data_root)?;
        if let Some(parent) = data_root.parent() {
            fsync_dir_best_effort(parent);
        }
        return Ok(Outcome::FreshRoot);
    };
    if !meta.is_dir() {
        return Err(format!(
            "data root {} is not a directory; refusing to start",
            data_root.display()
        ));
    }

    let marker = data_root.join(EPOCH_FILE);
    if metadata_if_present(&marker)?.is_some() {
        let content = std::fs::read_to_string(&marker)
            .map_err(|e| format!("failed to read epoch marker {}: {e}", marker.display()))?;
        if content.trim() != CURRENT_EPOCH {
            return Err(format!(
                "data root {} carries unsupported storage epoch {:?}; this trawld requires \
                 epoch {CURRENT_EPOCH}. Refusing to start without changing storage. \
                 Select new empty data and WAL directories, or restore a complete epoch-{CURRENT_EPOCH} \
                 backup. Do not relabel an incompatible corpus by editing EPOCH",
                data_root.display(),
                content.trim()
            ));
        }
        return Ok(Outcome::Current);
    }

    let entries = std::fs::read_dir(data_root)
        .map_err(|e| format!("failed to inspect data root {}: {e}", data_root.display()))?;
    let mut empty = true;
    let mut owned = false;
    let mut staged_epochs = Vec::new();
    let mut refused_staging = None;
    for entry in entries {
        let entry = entry
            .map_err(|e| format!("failed to inspect data root {}: {e}", data_root.display()))?;
        // Follow symlinks to detect dangling or inaccessible entries rather
        // than treating metadata failure as an unowned generic archive.
        let meta = std::fs::metadata(entry.path())
            .map_err(|e| format!("failed to inspect {}: {e}", entry.path().display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        owned |= matches!(name.as_ref(), "wal" | "CATALOG" | "REPIN" | "scheduled")
            || name.starts_with("EPOCH.next.")
            || (meta.is_dir() && is_date_partition(&name));
        // A PID name alone is not authority to discard an entry. An initial
        // publication can leave only a regular file containing a prefix of
        // the current marker, including zero bytes before its first write.
        if ingest_enabled && is_staged_epoch(&entry)? {
            staged_epochs.push(entry.path());
        } else {
            empty = false;
            if name.starts_with("EPOCH.next.") {
                refused_staging = Some(entry.path());
            }
        }
    }
    if !ingest_enabled && !owned {
        return Ok(Outcome::ReadOnlyArchive);
    }
    if ingest_enabled && empty {
        // Inspect the entire root and WAL before removing any staging file.
        // A crash during cleanup leaves either valid staging or an empty
        // directory; both can be retried. Unlinking never follows a symlink.
        for staged in staged_epochs {
            std::fs::remove_file(&staged)
                .map_err(|e| format!("failed to remove {}: {e}", staged.display()))?;
        }
        publish_epoch(data_root)?;
        return Ok(Outcome::InitializedEmpty);
    }
    if let Some(staged) = refused_staging.as_ref().or_else(|| staged_epochs.first()) {
        return Err(format!(
            "data root {} is nonempty but has no EPOCH marker; refusing to start without \
             changing storage. Cannot recover staged epoch entry {} automatically; inspect \
             its type, contents, and origin along with the data root before choosing a recovery \
             action. Startup has not removed or relabeled this entry",
            data_root.display(),
            staged.display()
        ));
    }
    Err(format!(
        "data root {} is nonempty but has no EPOCH marker; refusing to start without \
         changing storage. Select new empty data and WAL directories, or restore a complete \
         epoch-{CURRENT_EPOCH} backup including EPOCH. Unversioned generic archives require \
         ingest disabled and must not contain Trawl ownership markers",
        data_root.display()
    ))
}

/// Recognize only names and bytes an interrupted current publication writes.
fn is_staged_epoch(entry: &std::fs::DirEntry) -> Result<bool, String> {
    let name = entry.file_name();
    let Some(pid) = name
        .to_str()
        .and_then(|name| name.strip_prefix("EPOCH.next."))
    else {
        return Ok(false);
    };
    if !pid
        .parse::<u32>()
        .is_ok_and(|value| value > 0 && value.to_string() == pid)
    {
        return Ok(false);
    }
    let kind = entry
        .file_type()
        .map_err(|e| format!("failed to inspect {}: {e}", entry.path().display()))?;
    if !kind.is_file() {
        return Ok(false);
    }
    let body = format!("{CURRENT_EPOCH}\n");
    let mut bytes = Vec::new();
    std::fs::File::open(entry.path())
        .and_then(|file| file.take(body.len() as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| {
            format!(
                "failed to read staged epoch {}: {e}",
                entry.path().display()
            )
        })?;
    Ok(body.as_bytes().starts_with(&bytes))
}

/// Preserve read errors, including dangling symlinks. Only a genuinely absent
/// path is `None`; following an existing symlink must succeed.
fn metadata_if_present(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::metadata(path)
            .map(Some)
            .map_err(|e| format!("failed to inspect {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("failed to inspect {}: {e}", path.display())),
    }
}

/// Current WAL batches live under environment directories. Flat batches must
/// not be stranded outside the compactor's scan, even beside a current root.
fn validate_wal(wal_dir: &Path) -> Result<(), String> {
    if metadata_if_present(wal_dir)?.is_none() {
        return Ok(());
    }
    let entries = std::fs::read_dir(wal_dir)
        .map_err(|e| format!("failed to inspect WAL directory {}: {e}", wal_dir.display()))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| format!("failed to inspect WAL directory {}: {e}", wal_dir.display()))?;
        std::fs::metadata(entry.path())
            .map_err(|e| format!("failed to inspect {}: {e}", entry.path().display()))?;
        if entry.path().extension().is_some_and(|ext| ext == "ndjson") {
            return Err(format!(
                "unsupported flat WAL batch at {}; current WAL requires environment \
                 directories. Refusing to start without changing storage. Select a new \
                 empty WAL directory or restore the WAL from a complete epoch-{CURRENT_EPOCH} backup",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

/// A top-level date partition is an owned storage layout, not a generic archive.
fn is_date_partition(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn publish_epoch(data_root: &Path) -> Result<(), String> {
    let staged = data_root.join(format!("EPOCH.next.{}", std::process::id()));
    // Admission removed only proven initial-publication remnants. Exclusive
    // creation refuses any replacement entry, including a symlink, instead
    // of truncating or following it. Keep the same handle through fsync.
    let mut file = std::fs::File::create_new(&staged)
        .map_err(|e| format!("failed to create {}: {e}", staged.display()))?;
    file.write_all(format!("{CURRENT_EPOCH}\n").as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("failed to write and fsync {}: {e}", staged.display()))?;
    std::fs::rename(&staged, data_root.join(EPOCH_FILE))
        .map_err(|e| format!("failed to publish epoch in {}: {e}", data_root.display()))?;
    fsync_dir_best_effort(data_root);
    Ok(())
}

/// Publish a small marker file into a directory via the staged-write
/// idiom, durable before it is visible: staged temp name → fsync → atomic
/// rename → dir fsync. A crash can then never publish a half-written
/// marker — every reader sees either the previous content or the new one.
///
/// The catalog identity marker (`catalog::conform::publish_marker`) and the
/// repin marker (`repin::marker::write_marker`) publish through this helper.
/// Initial epoch publication uses exclusive staging creation in
/// [`publish_epoch`] after admission validates any interrupted initial write.
///
/// The staged name is PID-unique: concurrent publishers (test harnesses
/// share a fixture corpus) must not clobber each other's staged file
/// between the write and the rename.
pub(crate) fn publish_marker_staged(dir: &Path, name: &str, body: &str) -> Result<(), String> {
    // A hidden marker's staged file must not share its discovery prefix.
    // In particular, rollup recovery recognizes `.rollup-*`; it must never
    // read a partially written `..rollup-*.next.<pid>` as a complete marker.
    let prefix = if name.starts_with('.') { "." } else { "" };
    let staged = dir.join(format!(
        "{prefix}{name}{NEXT_SUFFIX}.{}",
        std::process::id()
    ));
    std::fs::write(&staged, body)
        .map_err(|e| format!("failed to write {}: {e}", staged.display()))?;
    std::fs::File::open(&staged)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("failed to fsync {}: {e}", staged.display()))?;

    let marker = dir.join(name);
    std::fs::rename(&staged, &marker)
        .map_err(|e| format!("failed to publish {}: {e}", marker.display()))?;
    fsync_dir_best_effort(dir);
    Ok(())
}

/// fsync a directory so the rename entry inside it survives a crash. Never
/// fatal: every branch of the gate is idempotent, so a lost entry simply
/// re-runs the same decision on the next boot.
pub(crate) fn fsync_dir_best_effort(dir: &Path) {
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::warn!(
            event_type = "epoch_dir_fsync_failed",
            dir = %dir.display(),
            error = %e,
            "directory fsync failed; the epoch marker is in place but the \
             rename entry may not survive a crash (safe: the boot gate is \
             idempotent and would re-run)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    result.insert(path.strip_prefix(root).unwrap().to_owned(), vec![]);
                    walk(root, &path, result);
                } else {
                    result.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        std::fs::read(&path).unwrap(),
                    );
                }
            }
        }
        let mut result = BTreeMap::new();
        walk(root, root, &mut result);
        result
    }

    #[test]
    fn fresh_and_empty_roots_initialize_and_restart() {
        for precreated in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            if precreated {
                std::fs::create_dir(&data).unwrap();
            }
            let wal = data.join("wal");
            let first = ensure_current_epoch(&data, &wal, true).unwrap();
            assert_eq!(
                first,
                if precreated {
                    Outcome::InitializedEmpty
                } else {
                    Outcome::FreshRoot
                }
            );
            assert_eq!(
                std::fs::read_to_string(data.join(EPOCH_FILE)).unwrap(),
                "3\n"
            );
            let before = snapshot(tmp.path());
            assert_eq!(
                ensure_current_epoch(&data, &wal, true).unwrap(),
                Outcome::Current
            );
            assert_eq!(snapshot(tmp.path()), before);
        }
    }

    #[test]
    fn interrupted_epoch_publication_retries_then_restarts() {
        for content in [b"".as_slice(), b"3", b"3\n"] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            std::fs::create_dir(&data).unwrap();
            for pid in [123, std::process::id()] {
                std::fs::write(data.join(format!("EPOCH.next.{pid}")), content).unwrap();
            }
            assert_eq!(
                ensure_current_epoch(&data, &data.join("wal"), true).unwrap(),
                Outcome::InitializedEmpty
            );
            assert_eq!(std::fs::read(data.join(EPOCH_FILE)).unwrap(), b"3\n");
            assert_eq!(std::fs::read_dir(&data).unwrap().count(), 1);
            let before = snapshot(tmp.path());
            assert_eq!(
                ensure_current_epoch(&data, &data.join("wal"), true).unwrap(),
                Outcome::Current
            );
            assert_eq!(snapshot(tmp.path()), before);
        }
    }

    #[test]
    fn staging_files_never_admit_mixed_or_query_only_roots() {
        for extra in [
            None,
            Some("notes.txt"),
            Some("prod/events.parquet"),
            Some("wal/prod/batch.ndjson"),
        ] {
            for ingest in [false, true] {
                if extra.is_none() && ingest {
                    continue;
                }
                let tmp = tempfile::tempdir().unwrap();
                let data = tmp.path().join("data");
                std::fs::create_dir(&data).unwrap();
                std::fs::write(data.join("EPOCH.next.123"), b"3\n").unwrap();
                if let Some(extra) = extra {
                    let path = data.join(extra);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, b"preserve").unwrap();
                }
                let before = snapshot(tmp.path());
                assert!(ensure_current_epoch(&data, &data.join("wal"), ingest).is_err());
                assert_eq!(snapshot(tmp.path()), before);
            }
        }
    }

    #[test]
    fn staging_retry_requires_current_bytes_and_generated_name() {
        for (name, content) in [
            ("EPOCH.next.123", "2\n"),
            ("EPOCH.next.123", "garbage"),
            ("EPOCH.next.123", "3\nextra"),
            ("EPOCH.next.123", " 3\n"),
            ("EPOCH.next.", "3\n"),
            ("EPOCH.next.backup", "3\n"),
            ("EPOCH.next.0", "3\n"),
            ("EPOCH.next.0123", "3\n"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            std::fs::create_dir(&data).unwrap();
            std::fs::write(data.join("EPOCH.next.456"), b"3\n").unwrap();
            std::fs::write(data.join(name), content).unwrap();
            let before = snapshot(tmp.path());
            let err = ensure_current_epoch(&data, &data.join("wal"), true).unwrap_err();
            assert!(
                err.contains(&data.join(name).display().to_string()),
                "{err}"
            );
            assert!(err.contains("inspect"), "{err}");
            assert!(!err.contains("delete"), "{err}");
            assert_eq!(snapshot(tmp.path()), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn staged_epoch_symlinks_and_directories_are_never_retried() {
        use std::os::unix::fs::symlink;
        for kind in ["symlink", "dangling", "directory"] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            std::fs::create_dir(&data).unwrap();
            let target = tmp.path().join("target");
            std::fs::write(&target, b"3\n").unwrap();
            let stage = data.join(format!("EPOCH.next.{}", std::process::id()));
            match kind {
                "directory" => std::fs::create_dir(&stage).unwrap(),
                "dangling" => symlink(tmp.path().join("absent"), &stage).unwrap(),
                _ => symlink(&target, &stage).unwrap(),
            }
            std::fs::write(data.join("EPOCH.next.123"), b"3\n").unwrap();
            for ingest in [false, true] {
                assert!(ensure_current_epoch(&data, &data.join("wal"), ingest).is_err());
                assert!(std::fs::symlink_metadata(&stage).is_ok());
                assert_eq!(std::fs::read(&target).unwrap(), b"3\n");
                assert!(!data.join(EPOCH_FILE).exists());
                assert!(data.join("EPOCH.next.123").exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn epoch_publication_refuses_a_replacement_staging_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp
            .path()
            .join(format!("EPOCH.next.{}", std::process::id()));
        let target = tmp.path().join("target");
        std::fs::write(&target, b"preserve").unwrap();
        std::os::unix::fs::symlink(&target, &staged).unwrap();
        assert!(publish_epoch(tmp.path()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
        assert!(std::fs::symlink_metadata(&staged).unwrap().is_symlink());
        assert!(!tmp.path().join(EPOCH_FILE).exists());
    }

    #[test]
    fn explicit_noncurrent_epochs_refuse_in_every_mode_without_mutation() {
        for content in ["1\n", "2\n", "4\n", "", "3 extra", "garbage"] {
            for ingest in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let data = tmp.path().join("data");
                std::fs::create_dir_all(data.join("prod/2026-01-01/10")).unwrap();
                std::fs::write(data.join("prod/2026-01-01/10/a.parquet"), b"corpus bytes").unwrap();
                std::fs::write(data.join(EPOCH_FILE), content).unwrap();
                let before = snapshot(tmp.path());
                let err = ensure_current_epoch(&data, &data.join("wal"), ingest).unwrap_err();
                assert!(err.contains("unsupported storage epoch"), "{err}");
                assert!(err.contains("Do not relabel"), "{err}");
                assert_eq!(snapshot(tmp.path()), before);
            }
        }
    }

    #[test]
    fn markerless_nonempty_ingest_roots_refuse_without_mutation() {
        for path in [
            "2026-01-01/a.parquet",
            "prod/2026-01-01/10/a.parquet",
            "a.parquet",
            "wal/prod/a.ndjson",
            "scheduled/run.parquet",
            "notes.txt",
            "EPOCH.next.123",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            let file = data.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"preserve").unwrap();
            let before = snapshot(tmp.path());
            let err = ensure_current_epoch(&data, &data.join("wal"), true).unwrap_err();
            assert!(err.contains("nonempty but has no EPOCH"), "{err}");
            assert_eq!(snapshot(tmp.path()), before);
        }
    }

    #[test]
    fn flat_wal_refuses_before_fresh_empty_or_current_root_mutation() {
        for root_state in ["missing", "empty", "staged", "current"] {
            for internal in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let data = tmp.path().join("data");
                if root_state != "missing" {
                    std::fs::create_dir(&data).unwrap();
                }
                if root_state == "current" {
                    std::fs::write(data.join(EPOCH_FILE), "3\n").unwrap();
                }
                if root_state == "staged" {
                    std::fs::write(data.join("EPOCH.next.123"), "3\n").unwrap();
                }
                let wal = if internal {
                    data.join("wal")
                } else {
                    tmp.path().join("wal")
                };
                std::fs::create_dir_all(&wal).unwrap();
                std::fs::write(wal.join("svc.ndjson"), b"unread batch").unwrap();
                let before = snapshot(tmp.path());
                let err = ensure_current_epoch(&data, &wal, true).unwrap_err();
                assert!(err.contains("unsupported flat WAL"), "{err}");
                assert_eq!(snapshot(tmp.path()), before);
            }
        }
    }

    #[test]
    fn current_environment_wal_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = tmp.path().join("wal");
        std::fs::create_dir_all(wal.join("prod")).unwrap();
        std::fs::write(wal.join("prod/svc.ndjson"), b"current batch").unwrap();
        ensure_current_epoch(&data, &wal, true).unwrap();
        let before = snapshot(tmp.path());
        assert_eq!(
            ensure_current_epoch(&data, &wal, true).unwrap(),
            Outcome::Current
        );
        assert_eq!(snapshot(tmp.path()), before);
    }

    #[test]
    fn query_only_generic_archives_and_unused_wal_are_untouched() {
        for root_state in ["missing", "empty", "archive", "current"] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            let wal = tmp.path().join("wal");
            // Unused WAL is not even a directory: the read-only gate ignores it.
            std::fs::write(&wal, b"not in use").unwrap();
            if root_state != "missing" {
                std::fs::create_dir(&data).unwrap();
            }
            if root_state == "archive" {
                std::fs::write(data.join("export.parquet"), b"generic bytes").unwrap();
            }
            if root_state == "current" {
                std::fs::write(data.join(EPOCH_FILE), "3\n").unwrap();
            }
            let before = snapshot(tmp.path());
            let result = ensure_current_epoch(&data, &wal, false).unwrap();
            assert_eq!(
                result,
                if root_state == "current" {
                    Outcome::Current
                } else {
                    Outcome::ReadOnlyArchive
                }
            );
            assert_eq!(snapshot(tmp.path()), before);
        }
    }

    #[test]
    fn query_only_owned_roots_still_require_their_epoch_marker() {
        for name in [
            "wal",
            "CATALOG",
            "REPIN",
            "scheduled",
            "2026-01-01",
            "EPOCH.next.123",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            std::fs::create_dir_all(data.join(name)).unwrap();
            let before = snapshot(tmp.path());
            let err = ensure_current_epoch(&data, &data.join("wal"), false).unwrap_err();
            assert!(err.contains("nonempty but has no EPOCH"), "{err}");
            assert_eq!(snapshot(tmp.path()), before);
        }
    }

    #[test]
    fn suffix_siblings_and_report_results_are_never_adopted_or_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        for suffix in [".next", ".pre-schema-v2", ".pre-epoch-3"] {
            let report = tmp
                .path()
                .join(format!("data{suffix}/scheduled/run.parquet"));
            std::fs::create_dir_all(report.parent().unwrap()).unwrap();
            std::fs::write(report, b"old result").unwrap();
        }
        let before = snapshot(tmp.path());
        ensure_current_epoch(&data, &data.join("wal"), true).unwrap();
        assert!(!data.join("scheduled").exists());
        for (path, bytes) in before {
            assert!(tmp.path().join(&path).exists());
            if tmp.path().join(&path).is_file() {
                assert_eq!(std::fs::read(tmp.path().join(path)).unwrap(), bytes);
            }
        }
        let initialized = snapshot(tmp.path());
        ensure_current_epoch(&data, &data.join("wal"), true).unwrap();
        assert_eq!(snapshot(tmp.path()), initialized);
    }

    #[test]
    fn invalid_path_types_fail_before_initialization() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = tmp.path().join("wal");
        std::fs::write(&wal, b"not a directory").unwrap();
        assert!(
            ensure_current_epoch(&data, &wal, true)
                .unwrap_err()
                .contains("failed to inspect WAL")
        );
        assert!(!data.exists());
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(data.join(EPOCH_FILE)).unwrap();
        assert!(
            ensure_current_epoch(&data, &wal, false)
                .unwrap_err()
                .contains("failed to read epoch marker")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_are_errors_not_absence() {
        use std::os::unix::fs::symlink;
        for location in ["data", "data/EPOCH", "data/export.parquet", "wal"] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            let wal = tmp.path().join("wal");
            let link = tmp.path().join(location);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(tmp.path().join("missing"), &link).unwrap();
            let err = ensure_current_epoch(&data, &wal, true).unwrap_err();
            assert!(err.contains("failed to inspect"), "{location}: {err}");
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(!data.join("EPOCH").is_file());
        }
    }
}
