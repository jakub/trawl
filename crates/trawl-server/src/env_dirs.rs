// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What counts as an env directory on disk (ADR-0009).
//!
//! The single definition, shared by the query planner (`source.rs`, which
//! globs `data/{env}/…`), the compactor (`ingest::compaction`, which walks
//! both `wal/{env}/` and `data/{env}/`) and the repin engine. They must all
//! agree on the set of envs, so the rule lives here once.
//!
//! [`date_partition`] extends it one level down: which `{env}/{date}`
//! directory is a date partition. Retention deletes by it and the storage
//! scan buckets bytes by it (ADR-0042), so the two agree on what a
//! partition is.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

/// Whether a top-level directory name is an env: the env charset, with the
/// reserved names (`wal`, `scheduled`, …) excluded.
pub(crate) fn is_env_dir_name(name: &str) -> bool {
    trawl_config::is_valid_env_name(name) && !trawl_config::RESERVED_ENV_NAMES.contains(&name)
}

/// The date of the partition directory `{env}/{date}`, or `None` when the
/// pair is not one: `env` must be an env directory name and `date` a
/// `YYYY-MM-DD` calendar date.
///
/// The single env/date recognition rule. It says nothing about today:
/// retention skips today's partition, the storage scan counts it.
pub(crate) fn date_partition(env: &str, date: &str) -> Option<NaiveDate> {
    if !is_env_dir_name(env) || !looks_like_date(date) {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Check if a directory name looks like a date (YYYY-MM-DD).
fn looks_like_date(name: &str) -> bool {
    name.len() == 10
        && name.as_bytes().get(4) == Some(&b'-')
        && name.as_bytes().get(7) == Some(&b'-')
        && name[..4].bytes().all(|b| b.is_ascii_digit())
}

/// Enumerate env directories under `root`, reporting an unreadable root.
///
/// A *missing* root is a legitimate cold start — no data has been written
/// yet — and yields an empty list. Any other `read_dir` failure (permissions,
/// I/O, a file where the root should be) is returned: it means the env set is
/// unknown, not empty, and a caller that cannot tell the two apart reports a
/// clean run while nothing gets done (ADR-0008: no silent loss).
///
/// Sorted by name for deterministic ordering (glob order, compaction order).
pub(crate) fn try_list_env_dirs(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    try_list_env_dirs_observed(root, || {})
}

/// Preserve the listing policy while observing otherwise skipped errors.
///
/// The callback observes non-`NotFound` entry and metadata errors only.
/// Fatal root-listing errors are returned, without calling the observer.
/// Callers can combine these observations into one failed scan attempt.
pub(crate) fn try_list_env_dirs_observed(
    root: &Path,
    mut on_skipped_error: impl FnMut(),
) -> std::io::Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        on_skipped_error();
                    }
                    return None;
                }
            };
            let path = entry.path();
            // Path::metadata follows symlinks, as the previous is_dir did.
            let metadata = match path.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        on_skipped_error();
                    }
                    return None;
                }
            };
            if !metadata.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_owned();
            if !is_env_dir_name(&name) {
                return None;
            }
            Some((name, path))
        })
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Enumerate env directories under `root`, treating an unreadable root as
/// empty. For callers where "no envs" and "cannot tell" are the same
/// outcome — the query planner (a broader source is never incorrect) and
/// best-effort housekeeping sweeps. The failure is logged rather than
/// swallowed silently; anything that must not proceed on an unknown env set
/// calls [`try_list_env_dirs`] instead.
pub(crate) fn list_env_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    try_list_env_dirs(root).unwrap_or_else(|e| {
        tracing::warn!(
            dir = %root.display(),
            error = %e,
            "failed to list env directories, treating as empty"
        );
        Vec::new()
    })
}

/// Env names under `root`, as [`list_env_dirs`] but dropping the paths.
pub(crate) fn list_env_names(root: &Path) -> Vec<String> {
    list_env_dirs(root)
        .into_iter()
        .map(|(name, _path)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_date_valid() {
        assert!(looks_like_date("2026-02-13"));
        assert!(looks_like_date("2025-01-01"));
        assert!(looks_like_date("1999-12-31"));
    }

    #[test]
    fn looks_like_date_invalid() {
        assert!(!looks_like_date("wal"));
        assert!(!looks_like_date("00"));
        assert!(!looks_like_date("2026-1-01"));
        assert!(!looks_like_date(""));
        assert!(!looks_like_date("not-a-date"));
    }

    #[test]
    fn date_partition_needs_an_env_and_a_calendar_date() {
        let date = NaiveDate::from_ymd_opt(2026, 2, 13).unwrap();
        assert_eq!(date_partition("prod", "2026-02-13"), Some(date));
        // Reserved and out-of-charset env names are not envs.
        assert_eq!(date_partition("wal", "2026-02-13"), None);
        assert_eq!(date_partition("scheduled", "2026-02-13"), None);
        assert_eq!(date_partition("Prod", "2026-02-13"), None);
        // Date-shaped but not a calendar date, and not date-shaped at all.
        assert_eq!(date_partition("prod", "2026-02-30"), None);
        assert_eq!(date_partition("prod", "2026-2-13"), None);
        assert_eq!(date_partition("prod", "00"), None);
    }

    #[test]
    fn keeps_valid_envs_sorted_and_skips_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        // Reserved names and a name outside the env charset must not
        // surface as envs; a plain file must not either.
        for dir in ["prod", "lab", "wal", "scheduled", "Staging"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        std::fs::write(tmp.path().join("nginx.parquet"), b"not a dir").unwrap();

        let names = list_env_names(tmp.path());
        assert_eq!(names, vec!["lab".to_owned(), "prod".to_owned()]);

        let dirs = list_env_dirs(tmp.path());
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].1, tmp.path().join("lab"));
        assert_eq!(dirs[1].1, tmp.path().join("prod"));

        let mut skipped_errors = 0;
        let observed = try_list_env_dirs_observed(tmp.path(), || skipped_errors += 1).unwrap();
        assert_eq!(observed, dirs);
        assert_eq!(skipped_errors, 0);
    }

    #[test]
    fn missing_root_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list_env_dirs(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn observed_listing_does_not_callback_for_missing_or_fatal_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let mut skipped_errors = 0;
        assert!(
            try_list_env_dirs_observed(&root, || skipped_errors += 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(skipped_errors, 0);
        std::fs::write(&root, b"not a directory").unwrap();
        assert!(try_list_env_dirs_observed(&root, || skipped_errors += 1).is_err());
        assert_eq!(
            skipped_errors, 0,
            "the returned root error has its own owner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn observed_listing_follows_symlinks_and_ignores_confirmed_not_found() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let target = tmp.path().join("target");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&target).unwrap();
        symlink(&target, root.join("prod")).unwrap();
        let missing = tmp.path().join("missing");
        symlink(&missing, root.join("gone")).unwrap();
        assert_eq!(
            root.join("gone").metadata().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let expected = vec![("prod".to_owned(), root.join("prod"))];
        let mut skipped_errors = 0;
        let observed = try_list_env_dirs_observed(&root, || skipped_errors += 1).unwrap();
        assert_eq!(observed, expected);
        assert_eq!(skipped_errors, 0);

        // A symlink loop supplies a deterministic non-NotFound metadata
        // error without permissions, while preserving the skipped entry.
        symlink("cycle", root.join("cycle")).unwrap();
        assert_ne!(
            root.join("cycle").metadata().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let observed = try_list_env_dirs_observed(&root, || skipped_errors += 1).unwrap();
        assert_eq!(observed, expected);
        assert_eq!(skipped_errors, 1);
        assert_eq!(try_list_env_dirs(&root).unwrap(), expected);
    }

    /// The fallible listing separates "nothing written yet" from "cannot
    /// tell": only the former is an empty list, so a caller that must not
    /// proceed on an unknown env set can refuse to.
    #[test]
    fn try_list_separates_cold_start_from_unreadable_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            try_list_env_dirs(&tmp.path().join("nope"))
                .expect("a missing root is a cold start")
                .is_empty()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let locked = tmp.path().join("locked");
            std::fs::create_dir_all(locked.join("prod")).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read_dir(&locked).is_ok() {
                // Running as root: mode bits are not enforced.
                let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
                return;
            }
            let err = try_list_env_dirs(&locked);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                err.expect_err("an unreadable root must not read as empty")
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }
}
