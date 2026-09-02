// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What counts as an env directory on disk (ADR-0009).
//!
//! The single definition, shared by the query planner (`source.rs`, which
//! globs `data/{env}/…`), the compactor (`ingest::compaction`, which walks
//! both `wal/{env}/` and `data/{env}/`) and the repin engine. They must all
//! agree on the set of envs, so the rule lives here once.

use std::path::{Path, PathBuf};

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
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_owned();
            if !trawl_config::is_valid_env_name(&name)
                || trawl_config::RESERVED_ENV_NAMES.contains(&name.as_str())
            {
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
    }

    #[test]
    fn missing_root_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list_env_dirs(&tmp.path().join("nope")).is_empty());
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
