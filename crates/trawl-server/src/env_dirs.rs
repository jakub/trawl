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

use std::collections::BinaryHeap;
use std::path::{Component, Path, PathBuf};

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

/// Whether `path` is absent: there is no entry at the name, and the
/// nearest existing ancestor is a directory (a symlink to one counts).
/// Anything else, an error included, is not absent.
///
/// `read_dir` answers `NotFound` for a missing path, for a dangling
/// symlink at the path and for a path below one alike, so its `NotFound`
/// alone cannot tell a cold start from a link to nothing. The name is
/// rebuilt from its components, so a trailing `/` or `.` cannot make
/// `symlink_metadata` follow a final link, and must then fail with
/// `NotFound`; a `..` component answers false rather than being walked
/// lexically. Each missing ancestor must fail the same way, and the first
/// existing one must resolve to a directory.
///
/// It is an observation, not a guard for writes: an ancestor swapped
/// after the check needs a writer on the owner-only data root (ADR-0052),
/// and a path created after the check reads as present, so the caller
/// fails closed and the request can be retried.
pub(crate) fn is_absent(path: &Path) -> bool {
    if path.components().any(|part| part == Component::ParentDir) {
        return false;
    }
    let path: PathBuf = path.components().collect();
    if !matches!(
        std::fs::symlink_metadata(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ) {
        return false;
    }
    let mut missing = path.as_path();
    loop {
        let parent = match missing.parent() {
            Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
            Some(parent) => parent,
            None => return false,
        };
        if parent == missing {
            return false;
        }
        match crate::epoch::metadata_if_present::<std::convert::Infallible>(parent) {
            Ok(Some(metadata)) => return metadata.is_dir(),
            Ok(None) => missing = parent,
            Err(_) => return false,
        }
    }
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
/// yet — and yields an empty list. A root that is, or is below, a symlink
/// to nothing is not missing (see [`is_absent`]). Any other `read_dir`
/// failure (permissions, I/O, a file where the root should be) is returned:
/// it means the env set is unknown, not empty, and a caller that cannot
/// tell the two apart reports a clean run while nothing gets done
/// (ADR-0008: no silent loss).
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_absent(root) => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .filter_map(|entry| env_entry(entry, &mut on_skipped_error))
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// The env directory one entry of the root is, or `None` when it is not
/// one: the per-entry rule every listing here applies. `on_skipped_error`
/// observes the non-`NotFound` entry and metadata errors it skips.
fn env_entry(
    entry: std::io::Result<std::fs::DirEntry>,
    on_skipped_error: &mut impl FnMut(),
) -> Option<(String, PathBuf)> {
    read_env_entry(entry).unwrap_or_else(|_| {
        on_skipped_error();
        None
    })
}

/// Why [`read_env_entry`] could not tell whether an entry is an env.
enum EntryError {
    /// The root's listing failed reading the next entry.
    Listing(std::io::Error),
    /// The entry's metadata could not be read.
    Metadata(PathBuf, std::io::Error),
}

/// [`env_entry`]'s rule, returning the errors it skips. An entry that is
/// `NotFound` (it vanished between the listing and its metadata, or is a
/// dangling symlink) is not an env, not an error.
fn read_env_entry(
    entry: std::io::Result<std::fs::DirEntry>,
) -> Result<Option<(String, PathBuf)>, EntryError> {
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(EntryError::Listing(error)),
    };
    let path = entry.path();
    // Path::metadata follows symlinks, as the previous is_dir did.
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(EntryError::Metadata(path, error)),
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if !is_env_dir_name(name) {
        return Ok(None);
    }
    Ok(Some((name.to_owned(), path)))
}

/// Enumerate env directories under `root`, treating an unreadable root as
/// empty. For callers where "no envs" and "cannot tell" are the same
/// outcome — the query planner (a broader source is never incorrect) and
/// best-effort housekeeping sweeps. The failure is logged rather than
/// swallowed silently; anything that must not proceed on an unknown env set
/// calls [`try_list_env_dirs`] instead.
pub(crate) fn list_env_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    try_list_env_dirs(root).unwrap_or_else(|e| {
        warn_unlisted(root, &e);
        Vec::new()
    })
}

/// Log a root the listings that treat "cannot tell" as empty could not read.
fn warn_unlisted(root: &Path, error: &std::io::Error) {
    tracing::warn!(
        dir = %root.display(),
        error = %error,
        "failed to list env directories, treating as empty"
    );
}

/// Env names under `root`, as [`list_env_dirs`] but dropping the paths.
pub(crate) fn list_env_names(root: &Path) -> Vec<String> {
    list_env_dirs(root)
        .into_iter()
        .map(|(name, _path)| name)
        .collect()
}

/// The first env names under a root, in name order, and how many envs it
/// holds: [`first_env_names`]'s answer.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FirstEnvNames {
    /// The lexicographically first names, at most the cap asked for.
    pub(crate) names: Vec<String>,
    /// Every env under the root, the named ones included.
    pub(crate) total: usize,
}

/// Why [`first_env_names`] could not say which envs a root holds.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FirstEnvNamesError {
    /// The caller's `cancelled` returned true.
    #[error("env listing cancelled")]
    Cancelled,
    /// The root could not be listed, for a reason other than not existing.
    #[error("failed to list env directories")]
    Root(#[source] std::io::Error),
    /// One of the root's entries could not be read, for a reason other
    /// than having vanished.
    #[error("failed to read env directory entry {}", path.display())]
    Entry {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
}

/// The first `cap` names [`list_env_names`] would list under `root`, and
/// how many it would list, holding at most `cap + 1` names however many
/// envs the root holds.
///
/// One pass over the root's entries: each env name goes into a max-heap,
/// which drops its largest name once it holds more than `cap`, and what is
/// left is sorted at the end. `cancelled` is read before each entry, so a
/// cancelled walk stops at the next entry rather than finishing the root.
///
/// Unlike [`list_env_dirs`], an unreadable root is an error, not "no
/// envs": a caller that reports on the envs it found cannot vouch for a
/// set it could not read. A missing root is a cold start and holds none
/// (a dangling symlink in its place, or above it, is not missing, see
/// [`is_absent`]); an entry that vanished during the walk is skipped.
///
/// # Errors
/// [`FirstEnvNamesError::Cancelled`] once `cancelled` returns true,
/// [`FirstEnvNamesError::Root`] when the root's listing fails, a dangling
/// symlink root or a root below a symlink to nothing included, and
/// [`FirstEnvNamesError::Entry`] when an entry's metadata cannot be read.
pub(crate) fn first_env_names(
    root: &Path,
    cap: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<FirstEnvNames, FirstEnvNamesError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_absent(root) => {
            return Ok(FirstEnvNames::default());
        }
        Err(e) => return Err(FirstEnvNamesError::Root(e)),
    };
    let mut first = BinaryHeap::new();
    let mut total = 0_usize;
    for entry in entries {
        if cancelled() {
            return Err(FirstEnvNamesError::Cancelled);
        }
        let name = match read_env_entry(entry) {
            Ok(Some((name, _path))) => name,
            Ok(None) => continue,
            Err(EntryError::Listing(error)) => return Err(FirstEnvNamesError::Root(error)),
            Err(EntryError::Metadata(path, error)) => {
                return Err(FirstEnvNamesError::Entry { path, error });
            }
        };
        total += 1;
        first.push(name);
        if first.len() > cap {
            first.pop();
        }
    }
    Ok(FirstEnvNames {
        names: first.into_sorted_vec(),
        total,
    })
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

    fn numbered_envs(root: &Path, count: usize) -> Vec<String> {
        let names: Vec<String> = (0..count).map(|n| format!("e{n:04}")).collect();
        // Created in reverse, so name order is not creation order.
        for name in names.iter().rev() {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        names
    }

    /// Past the cap the walk keeps the first names in name order and still
    /// counts every env.
    #[test]
    fn first_env_names_keeps_the_first_cap_names_and_counts_every_env() {
        let tmp = tempfile::tempdir().unwrap();
        let names = numbered_envs(tmp.path(), 1500);
        let first = first_env_names(tmp.path(), 1024, &|| false).unwrap();
        assert_eq!(first.total, 1500);
        assert_eq!(first.names, names[..1024]);
        assert_eq!(first.names, list_env_names(tmp.path())[..1024]);
    }

    #[test]
    fn first_env_names_at_the_cap_lists_every_env() {
        let tmp = tempfile::tempdir().unwrap();
        let names = numbered_envs(tmp.path(), 1024);
        let first = first_env_names(tmp.path(), 1024, &|| false).unwrap();
        assert_eq!(first, FirstEnvNames { names, total: 1024 });
    }

    /// The listing's rule decides what is an env, for the names and for
    /// the count.
    #[test]
    fn first_env_names_skips_what_is_not_an_env() {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["prod", "lab", "wal", "scheduled", "Staging"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        std::fs::write(tmp.path().join("nginx.parquet"), b"not a dir").unwrap();
        let first = first_env_names(tmp.path(), 1024, &|| false).unwrap();
        assert_eq!(first.names, list_env_names(tmp.path()));
        assert_eq!(first.total, 2);
        let capped = first_env_names(tmp.path(), 1, &|| false).unwrap();
        assert_eq!(
            capped,
            FirstEnvNames {
                names: vec!["lab".to_owned()],
                total: 2
            }
        );
    }

    /// The walk reads `cancelled` before each entry and stops at the
    /// first read that says so, without finishing the root.
    #[test]
    fn first_env_names_stops_at_the_entry_cancelled_is_read_before() {
        let tmp = tempfile::tempdir().unwrap();
        numbered_envs(tmp.path(), 10);
        let reads = std::cell::Cell::new(0_usize);
        let cancelled = || {
            reads.set(reads.get() + 1);
            reads.get() == 4
        };
        let walked = first_env_names(tmp.path(), 1024, &cancelled);
        assert!(
            matches!(walked, Err(FirstEnvNamesError::Cancelled)),
            "{walked:?}"
        );
        assert_eq!(reads.get(), 4, "the walk stopped at the read");
    }

    /// A missing root is a cold start with no envs; a root that is not a
    /// directory cannot be listed and says so, where the planner's listing
    /// still reads it as empty.
    #[test]
    fn first_env_names_reads_a_missing_root_as_no_envs_and_refuses_an_unreadable_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        assert_eq!(
            first_env_names(&root, 1024, &|| false).unwrap(),
            FirstEnvNames::default()
        );
        std::fs::write(&root, b"not a directory").unwrap();
        let refused = first_env_names(&root, 1024, &|| false);
        assert!(
            matches!(&refused, Err(FirstEnvNamesError::Root(error))
                if error.kind() == std::io::ErrorKind::NotADirectory),
            "{refused:?}"
        );
        assert!(list_env_dirs(&root).is_empty());
    }

    /// A dangling symlink where the root should be is not a cold start:
    /// `read_dir` says `NotFound` for it as for a missing root, but something
    /// is there, so the env set is unknown rather than empty.
    #[cfg(unix)]
    #[test]
    fn first_env_names_refuses_a_dangling_root_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &root).unwrap();
        let refused = first_env_names(&root, 1024, &|| false);
        assert!(
            matches!(&refused, Err(FirstEnvNamesError::Root(error))
                if error.kind() == std::io::ErrorKind::NotFound),
            "{refused:?}"
        );
        assert!(is_absent(&tmp.path().join("nothing")));
        assert!(!is_absent(&root));
    }

    /// A root below a dangling symlink is not missing either: neither the
    /// root nor the link's target exists, but the link does.
    #[cfg(unix)]
    #[test]
    fn first_env_names_refuses_a_root_below_a_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &link).unwrap();
        let refused = first_env_names(&link.join("data"), 1024, &|| false);
        assert!(
            matches!(&refused, Err(FirstEnvNamesError::Root(error))
                if error.kind() == std::io::ErrorKind::NotFound),
            "{refused:?}"
        );
    }

    /// No entry at the name, however deep, under a real directory is
    /// absent: a cold start.
    #[test]
    fn a_missing_path_under_a_directory_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(is_absent(&tmp.path().join("a")));
        assert!(is_absent(&tmp.path().join("a/b/c")));
    }

    /// A relative path resolves its ancestors from the working directory,
    /// whose parent walk ends at `.`.
    #[test]
    fn a_missing_relative_path_is_absent() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("no-such-dir-{}-{nanos}", std::process::id());
        assert!(is_absent(Path::new(&name)));
        assert!(is_absent(&Path::new(&name).join("prod")));
    }

    /// Below a symlink to nothing, at any depth, the name is not absent:
    /// `lstat` says `NotFound` for it, but the nearest existing ancestor
    /// is the link, which resolves to nothing.
    #[cfg(unix)]
    #[test]
    fn a_path_below_a_dangling_symlink_is_not_absent() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        symlink(tmp.path().join("nowhere"), &link).unwrap();
        assert!(!is_absent(&link.join("prod")));
        assert!(!is_absent(&link.join("prod/2024-01-19")));

        let deeper = tmp.path().join("deeper");
        symlink(tmp.path().join("gone/deeper"), &deeper).unwrap();
        assert!(!is_absent(&deeper.join("prod")));
        assert!(!is_absent(&deeper.join("prod/2024-01-19")));
    }

    /// A trailing `/` or `.` would make `lstat` follow the final link;
    /// the name is rebuilt from its components, so the link is seen.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_spelled_with_a_trailing_slash_is_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &link).unwrap();
        let slash = PathBuf::from(format!("{}/", link.display()));
        let dot = PathBuf::from(format!("{}/.", link.display()));
        assert_eq!(
            std::fs::symlink_metadata(&slash).unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "lstat follows the link through the trailing slash"
        );
        assert!(!is_absent(&slash));
        assert!(!is_absent(&dot));
    }

    /// Below a symlink to a directory is below a directory.
    #[cfg(unix)]
    #[test]
    fn a_missing_path_below_a_symlinked_directory_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("real"), &link).unwrap();
        assert!(is_absent(&link.join("prod")));
        assert!(is_absent(&link.join("prod/2024-01-19")));
    }

    /// Below a regular file, or a symlink to one, nothing can be created:
    /// not absent.
    #[cfg(unix)]
    #[test]
    fn a_path_below_a_file_is_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(!is_absent(&file.join("prod")));
        assert!(!is_absent(&file.join("prod/2024-01-19")));
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(!is_absent(&link.join("prod")));
    }

    /// A symlink loop above the name cannot be resolved: not absent.
    #[cfg(unix)]
    #[test]
    fn a_path_below_a_symlink_loop_is_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle = tmp.path().join("cycle");
        std::os::unix::fs::symlink("cycle", &cycle).unwrap();
        assert!(!is_absent(&cycle.join("prod")));
        assert!(!is_absent(&cycle.join("prod/2024-01-19")));
    }

    /// Below a directory that cannot be searched, nothing can be told:
    /// not absent.
    #[cfg(unix)]
    #[test]
    fn a_path_below_an_unsearchable_directory_is_not_absent() {
        use std::os::unix::fs::PermissionsExt as _;

        if rustix::process::geteuid().is_root() {
            eprintln!("skipped: running as root, which mode 0o000 does not lock out");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let locked = tmp.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let shallow = is_absent(&locked.join("prod"));
        let deep = is_absent(&locked.join("prod/2024-01-19"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!shallow);
        assert!(!deep);
    }

    /// A `..` component is never walked lexically: not absent.
    #[test]
    fn a_path_with_a_parent_component_is_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        assert!(!is_absent(&tmp.path().join("real/../missing")));
        assert!(!is_absent(&tmp.path().join("missing/../other")));
    }

    /// The fallible listing reads a missing root under a directory as a
    /// cold start, and a root below a symlink to nothing as an error.
    #[cfg(unix)]
    #[test]
    fn try_list_refuses_a_root_below_a_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            try_list_env_dirs(&tmp.path().join("missing/data"))
                .expect("a missing root is a cold start")
                .is_empty()
        );
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &link).unwrap();
        for root in [link.clone(), link.join("data")] {
            let mut skipped_errors = 0;
            let refused = try_list_env_dirs_observed(&root, || skipped_errors += 1);
            assert_eq!(
                refused.expect_err("not a cold start").kind(),
                std::io::ErrorKind::NotFound,
                "{}",
                root.display()
            );
            assert_eq!(skipped_errors, 0);
        }
    }

    /// An entry whose metadata cannot be read fails the walk naming it; a
    /// dangling symlink has vanished as far as the walk can tell, and is
    /// skipped while the rest are counted.
    #[cfg(unix)]
    #[test]
    fn first_env_names_refuses_an_unreadable_entry_and_skips_a_dangling_one() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        numbered_envs(&root, 3);
        symlink(root.join("missing"), root.join("gone")).unwrap();
        assert_eq!(
            first_env_names(&root, 1024, &|| false).unwrap(),
            FirstEnvNames {
                names: vec!["e0000".into(), "e0001".into(), "e0002".into()],
                total: 3
            }
        );

        symlink("loop", root.join("loop")).unwrap();
        let refused = first_env_names(&root, 1024, &|| false);
        let Err(FirstEnvNamesError::Entry { path, error }) = &refused else {
            panic!("{refused:?}");
        };
        assert_eq!(path, &root.join("loop"));
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(list_env_names(&root), ["e0000", "e0001", "e0002"]);
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
