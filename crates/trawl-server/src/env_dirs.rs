// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The data root's layout (ADR-0009): `{env}/{date}/{HH}/{service}.parquet`,
//! with daily rollups at `{env}/{date}/{service}.parquet`.

use std::collections::BinaryHeap;
use std::path::{Component, Path, PathBuf};

use chrono::NaiveDate;

pub(crate) fn is_env_dir_name(name: &str) -> bool {
    trawl_config::is_valid_env_name(name) && !trawl_config::RESERVED_ENV_NAMES.contains(&name)
}

pub(crate) fn date_partition(env: &str, date: &str) -> Option<NaiveDate> {
    if !is_env_dir_name(env) {
        return None;
    }
    parse_date(date)
}

const DATE_FORMAT: &str = "%Y-%m-%d";

const PARQUET_EXTENSION: &str = ".parquet";

pub(crate) fn date_name(date: NaiveDate) -> String {
    date.format(DATE_FORMAT).to_string()
}

pub(crate) fn hour_name(hour: u8) -> String {
    debug_assert!(hour <= 23, "hour {hour} has no directory");
    format!("{hour:02}")
}

/// `chrono` alone also reads `2026-2-01`, `+026-01-01` and `2026-01- 1`, so
/// the parsed date must spell `name` back.
pub(crate) fn parse_date(name: &str) -> Option<NaiveDate> {
    if name.len() != 10 || !name.is_ascii() {
        return None;
    }
    let date = NaiveDate::parse_from_str(name, DATE_FORMAT).ok()?;
    (date_name(date) == name).then_some(date)
}

pub(crate) fn parse_hour(name: &str) -> Option<u8> {
    let &[tens, ones] = name.as_bytes() else {
        return None;
    };
    if !tens.is_ascii_digit() || !ones.is_ascii_digit() {
        return None;
    }
    let hour = (tens - b'0') * 10 + (ones - b'0');
    (hour <= 23).then_some(hour)
}

pub(crate) fn day_dir(env_root: &Path, date: NaiveDate) -> PathBuf {
    env_root.join(date_name(date))
}

pub(crate) fn hour_dir(env_root: &Path, date: NaiveDate, hour: u8) -> PathBuf {
    day_dir(env_root, date).join(hour_name(hour))
}

pub(crate) fn hourly_file(env_root: &Path, date: NaiveDate, hour: u8, service: &str) -> PathBuf {
    hour_dir(env_root, date, hour).join(format!("{service}{PARQUET_EXTENSION}"))
}

pub(crate) fn daily_file(day_dir: &Path, service: &str) -> PathBuf {
    day_dir.join(format!("{service}{PARQUET_EXTENSION}"))
}

pub(crate) struct LayoutPath {
    pub(crate) env: String,
    /// The hour directory, or `None` for a daily rollup.
    pub(crate) hour: Option<u8>,
    pub(crate) service: String,
    /// The instant this file's partition directory claims (see
    /// [`crate::ingest::compaction::ConformPolicy::StandingFile`]).
    pub(crate) instant: chrono::DateTime<chrono::Utc>,
}

/// Read `path` back as trawl's own storage layout, relative to the data root.
/// `None` = foreign, i.e. not ours. This is the whole safety gate on an
/// in-place, lossy, irreversible rewrite, so it is deliberately the strict
/// inverse of the write path rather than a loose shape match.
pub(crate) fn layout_path(data_root: &Path, path: &Path) -> Option<LayoutPath> {
    let rel = path.strip_prefix(data_root).ok()?;
    let mut parts: Vec<&str> = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    let (env, day, hour, file) = match parts.as_slice() {
        [env, day, file] => (*env, *day, None, *file),
        [env, day, hour, file] => (*env, *day, Some(*hour), *file),
        _ => return None,
    };
    if !is_env_dir_name(env) {
        return None;
    }
    let service = file.strip_suffix(PARQUET_EXTENSION)?;
    if !trawl_config::is_valid_service_name(service) {
        return None;
    }
    let day = parse_date(day)?;
    let hour = match hour {
        Some(hh) => Some(parse_hour(hh)?),
        None => None,
    };
    let instant = day
        .and_hms_opt(u32::from(hour.unwrap_or(0)), 0, 0)?
        .and_utc();
    Some(LayoutPath {
        env: env.to_owned(),
        hour,
        service: service.to_owned(),
        instant,
    })
}

/// Date-shaped, whether or not it names a calendar date (ADR-0036): deliberately
/// looser than [`parse_date`], so boot refuses a root holding `2026-13-45`
/// rather than reading it as a generic archive.
pub(crate) fn is_owned_residue_dir_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
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

/// A *missing* root is a legitimate cold start and yields an empty list. A
/// root that is, or is below, a symlink to nothing is not missing (see
/// [`is_absent`]). Any other `read_dir` failure is returned: it means the env
/// set is unknown, not empty (ADR-0008: no silent loss).
pub(crate) fn try_list_env_dirs(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    try_list_env_dirs_observed(root, || {})
}

/// The callback observes non-`NotFound` entry and metadata errors only.
/// Fatal root-listing errors are returned, without calling the observer.
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

fn env_entry(
    entry: std::io::Result<std::fs::DirEntry>,
    on_skipped_error: &mut impl FnMut(),
) -> Option<(String, PathBuf)> {
    read_env_entry(entry).unwrap_or_else(|_| {
        on_skipped_error();
        None
    })
}

enum EntryError {
    Listing(std::io::Error),
    Metadata(PathBuf, std::io::Error),
}

fn read_env_entry(
    entry: std::io::Result<std::fs::DirEntry>,
) -> Result<Option<(String, PathBuf)>, EntryError> {
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(EntryError::Listing(error)),
    };
    let path = entry.path();
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
/// empty. Anything that must not proceed on an unknown env set calls
/// [`try_list_env_dirs`] instead.
pub(crate) fn list_env_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    try_list_env_dirs(root).unwrap_or_else(|e| {
        warn_unlisted(root, &e);
        Vec::new()
    })
}

fn warn_unlisted(root: &Path, error: &std::io::Error) {
    tracing::debug!(
        dir = %root.display(),
        error = %error,
        "failed to list env directories, treating as empty"
    );
    tracing::warn!(
        dir = %root.display(),
        cause_kind = crate::error::CauseKind::of_io(error).as_str(),
        "failed to list env directories, treating as empty"
    );
}

pub(crate) fn list_env_names(root: &Path) -> Vec<String> {
    list_env_dirs(root)
        .into_iter()
        .map(|(name, _path)| name)
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FirstEnvNames {
    pub(crate) names: Vec<String>,
    pub(crate) total: usize,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FirstEnvNamesError {
    #[error("env listing cancelled")]
    Cancelled,
    #[error("failed to list env directories")]
    Root(#[source] std::io::Error),
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
    fn canonical_date_and_hour_matrix() {
        for name in ["2024-02-29", "2026-02-13", "1999-12-31", "0999-01-01"] {
            let date = parse_date(name).unwrap_or_else(|| panic!("{name} is a date"));
            assert_eq!(date_name(date), name);
        }
        for name in [
            "2026-13-45",
            "2026-02-30",
            "2023-02-29",
            "2026-2-01",
            "2026-02-1",
            "+026-01-01",
            "2026-0\u{661}-01",
            // Ten bytes, one of them a two-byte digit.
            "2026-\u{661}-01",
            "2026-01- 1",
            " 2026-01-01",
            "2026-01-01 ",
            "20260101",
            "wal",
            "",
        ] {
            assert_eq!(parse_date(name), None, "{name:?} is not a date name");
            assert!(date_partition("prod", name).is_none(), "{name:?}");
        }
        for (name, hour) in [("00", 0), ("07", 7), ("23", 23)] {
            assert_eq!(parse_hour(name), Some(hour));
            assert_eq!(hour_name(hour), name);
        }
        for name in [
            "24",
            "99",
            "7",
            "0x",
            "+1",
            "-1",
            " 7",
            "007",
            "\u{661}\u{661}",
            "",
        ] {
            assert_eq!(parse_hour(name), None, "{name:?} is not an hour name");
        }
    }

    #[test]
    fn layout_paths_round_trip() {
        let root = PathBuf::from("/var/lib/trawl/data");
        let env_root = root.join("prod");
        let date = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        for hour in [0, 9, 23] {
            let path = hourly_file(&env_root, date, hour, "api.v2");
            let layout = layout_path(&root, &path).unwrap();
            assert_eq!(
                (layout.env.as_str(), layout.hour, layout.service.as_str()),
                ("prod", Some(hour), "api.v2")
            );
            assert_eq!(
                layout.instant,
                date.and_hms_opt(u32::from(hour), 0, 0).unwrap().and_utc()
            );
            assert_eq!(
                path.parent(),
                Some(hour_dir(&env_root, date, hour).as_path())
            );
        }
        let path = daily_file(&day_dir(&env_root, date), "svc-a");
        assert_eq!(path, root.join("prod/2026-08-01/svc-a.parquet"));
        assert_eq!(path.parent(), Some(day_dir(&env_root, date).as_path()));
        let layout = layout_path(&root, &path).unwrap();
        assert_eq!(
            (layout.env.as_str(), layout.hour, layout.service.as_str()),
            ("prod", None, "svc-a")
        );
        assert_eq!(layout.instant, date.and_hms_opt(0, 0, 0).unwrap().and_utc());

        for rel in [
            "prod/2026-8-1/10/s.parquet",
            "prod/2026-08-01/+1/s.parquet",
            "prod/2026-08-01/24/s.parquet",
            "prod/2026-08-01/7/s.parquet",
            "prod/2026-13-45/s.parquet",
            "prod/2026-13-45/10/s.parquet",
            "prod/2026-01- 1/s.parquet",
        ] {
            assert!(
                layout_path(&root, &root.join(rel)).is_none(),
                "{rel} is not a canonical layout path"
            );
        }
    }

    #[test]
    fn date_shaped_env_stays_valid() {
        assert!(is_env_dir_name("2026-01-01"));
        assert_eq!(
            date_partition("2026-01-01", "2026-01-02"),
            NaiveDate::from_ymd_opt(2026, 1, 2)
        );
        let root = PathBuf::from("/var/lib/trawl/data");
        let date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let path = hourly_file(&root.join("2026-01-01"), date, 3, "svc");
        let layout = layout_path(&root, &path).unwrap();
        assert_eq!((layout.env.as_str(), layout.hour), ("2026-01-01", Some(3)));
    }

    #[test]
    fn layout_path_reads_the_hour_and_the_daily_rollup() {
        let root = PathBuf::from("/var/lib/trawl/data");
        let hourly = layout_path(&root, &root.join("prod/2026-08-01/10/api.v2.parquet")).unwrap();
        assert_eq!(hourly.service, "api.v2");
        assert_eq!(hourly.instant.to_rfc3339(), "2026-08-01T10:00:00+00:00");
        assert_eq!((hourly.env.as_str(), hourly.hour), ("prod", Some(10)));

        let daily = layout_path(&root, &root.join("prod/2026-08-01/svc-a.parquet")).unwrap();
        assert_eq!(daily.service, "svc-a");
        assert_eq!(daily.instant.to_rfc3339(), "2026-08-01T00:00:00+00:00");
        assert_eq!((daily.env.as_str(), daily.hour), ("prod", None));
    }

    #[test]
    fn layout_path_declines_everything_trawl_did_not_write() {
        let root = PathBuf::from("/var/lib/trawl/data");
        let foreign = [
            "svc-a.parquet",
            "prod/svc-a.parquet",
            "prod/2026-08-01/10/extra/svc-a.parquet",
            "backups/exports/10/svc-a.parquet",
            "prod/backup-2026-08-01/svc-a.parquet",
            "wal/2026-08-01/10/svc-a.parquet",
            "scheduled/2026-08-01/10/svc-a.parquet",
            "PROD/2026-08-01/10/svc-a.parquet",
            "prod/2026-08-01/10/Living Room AP.parquet",
            "prod/2026-08-01/99/svc-a.parquet",
            "prod/2026-08-01/1/svc-a.parquet",
            "prod/2026-13-01/10/svc-a.parquet",
        ];
        for rel in foreign {
            assert!(
                layout_path(&root, &root.join(rel)).is_none(),
                "{rel} is not a path trawl wrote and must not be adopted"
            );
        }
        assert!(
            layout_path(
                &root,
                &PathBuf::from("/srv/other/prod/2026-08-01/10/s.parquet")
            )
            .is_none()
        );
    }

    #[test]
    fn date_partition_needs_an_env_and_a_calendar_date() {
        let date = NaiveDate::from_ymd_opt(2026, 2, 13).unwrap();
        assert_eq!(date_partition("prod", "2026-02-13"), Some(date));
        assert_eq!(date_partition("wal", "2026-02-13"), None);
        assert_eq!(date_partition("scheduled", "2026-02-13"), None);
        assert_eq!(date_partition("Prod", "2026-02-13"), None);
        assert_eq!(date_partition("prod", "2026-02-30"), None);
        assert_eq!(date_partition("prod", "2026-2-13"), None);
        assert_eq!(date_partition("prod", "00"), None);
    }

    #[test]
    fn keeps_valid_envs_sorted_and_skips_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
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

    #[test]
    fn a_missing_path_under_a_directory_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(is_absent(&tmp.path().join("a")));
        assert!(is_absent(&tmp.path().join("a/b/c")));
    }

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

    #[cfg(unix)]
    #[test]
    fn a_path_below_a_symlink_loop_is_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle = tmp.path().join("cycle");
        std::os::unix::fs::symlink("cycle", &cycle).unwrap();
        assert!(!is_absent(&cycle.join("prod")));
        assert!(!is_absent(&cycle.join("prod/2024-01-19")));
    }

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

    #[test]
    fn a_path_with_a_parent_component_is_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        assert!(!is_absent(&tmp.path().join("real/../missing")));
        assert!(!is_absent(&tmp.path().join("missing/../other")));
    }

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
