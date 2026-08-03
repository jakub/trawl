// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What counts as an env directory on disk (ADR-0009).
//!
//! The single definition shared by the query planner (`source.rs`, which
//! globs `data/{env}/…`) and the compactor (`ingest::compaction`, which
//! walks both `wal/{env}/` and `data/{env}/`). Both must agree on the
//! set of envs, so the rule lives here once.

use std::path::{Path, PathBuf};

/// Enumerate env directories under `root`: immediate subdirectories whose
/// name passes the env charset and is not reserved (`wal/`, `scheduled/`).
/// Anything else (a stray file, a reserved directory) is skipped — env
/// names were validated at ingest, so the directory name IS the env value.
///
/// Sorted by name for deterministic ordering (glob order, compaction order).
pub(crate) fn list_env_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
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
    dirs
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
}
