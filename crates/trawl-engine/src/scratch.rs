// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The query scratch directory (ADR-0056).
//!
//! `[server] query_scratch_dir` holds what trawld writes while it answers a
//! query: the query database's private spill directory, and beside it the
//! hot-buffer query snapshots and the Parquet export staging files. This
//! module owns the names trawld makes there, so the code that creates an
//! entry and the code that recognizes one left behind read one spelling.
//!
//! The spill directory follows ADR-0052: a fresh random name, mode 0700,
//! refused unless a no-follow handle shows it owned by this process's euid
//! with no group or other bit. `DuckDB` opens its spill files by predictable
//! names without `O_EXCL`, so a directory another user could enter would let
//! them plant or read one.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Prefix of a query database's private spill directory.
pub const SPILL_DIR_PREFIX: &str = "trawl-spill-";

/// Length of the random suffix after [`SPILL_DIR_PREFIX`], pinned so the
/// recognizer can demand exactly what the creator makes.
const SPILL_DIR_RANDOM_LEN: usize = 6;

/// The setting an operator changes to move query spill, named in refusals
/// of a spill directory made in the scratch directory.
pub(crate) const SCRATCH_SETTING: &str = "[server] query_scratch_dir";

/// The variable an operator changes to move spill of a database opened
/// without a scratch directory ([`crate::executor::Executor::new`]).
pub(crate) const TMPDIR_SETTING: &str = "TMPDIR";

/// The query scratch directory, by path.
///
/// It names the directory and makes the spill directories inside it. It
/// does not create, lock or clear the directory itself.
#[derive(Debug, Clone)]
pub struct QueryScratch {
    path: PathBuf,
}

impl QueryScratch {
    /// The scratch directory at `path`, which must already exist.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The scratch directory's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Make a query database's private spill directory inside the scratch
    /// directory.
    pub(crate) fn private_spill_dir(&self) -> std::io::Result<tempfile::TempDir> {
        private_dir_in(&self.path, SCRATCH_SETTING, |_| Ok(()))
    }
}

/// Whether `name` is one trawld gives a query database's spill directory:
/// [`SPILL_DIR_PREFIX`] and exactly six ASCII letters or digits.
#[must_use]
pub fn is_spill_dir_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(SPILL_DIR_PREFIX))
        .is_some_and(|suffix| {
            suffix.len() == SPILL_DIR_RANDOM_LEN
                && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Whether `name` is one `DuckDB` 1.5.5 gives a file in a spill directory.
///
/// Two shapes, both from the bundled source: the grouped block files
/// `duckdb_temp_storage_<size>-<n>.tmp`, where `<size>` is a
/// `TemporaryBufferSize` name such as `DEFAULT` or `S32K`
/// (`temporary_file_manager.cpp`), and the files of buffers larger than a
/// block, `duckdb_temp_block-<id>.block` (`standard_buffer_manager.cpp`).
/// A `DuckDB` upgrade has to re-check both.
#[must_use]
pub fn is_spill_file_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = name
        .strip_prefix("duckdb_temp_storage_")
        .and_then(|rest| rest.strip_suffix(".tmp"))
    {
        return rest.split_once('-').is_some_and(|(size, index)| {
            !size.is_empty()
                && size
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                && digits(index)
        });
    }
    name.strip_prefix("duckdb_temp_block-")
        .and_then(|rest| rest.strip_suffix(".block"))
        .is_some_and(digits)
}

/// Make a private spill directory in `parent`, run `after_create` on it
/// (tests stand in a filesystem that ignores the requested mode), then
/// check it. `setting` is what a refusal tells the operator to change.
///
/// The directory is asked for at 0700, and a filesystem can still answer
/// otherwise: a fixed-mode mount ignores the mode. So it is opened
/// no-follow and its owner and mode are read from that handle: it must
/// belong to this process's euid and carry no group or other bit, or it is
/// refused and removed.
pub(crate) fn private_dir_in(
    parent: &Path,
    setting: &str,
    after_create: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(SPILL_DIR_PREFIX);
    builder.rand_bytes(SPILL_DIR_RANDOM_LEN);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir_in(parent)?;
    after_create(dir.path())?;
    #[cfg(unix)]
    check_private(dir.path(), rustix::process::geteuid().as_raw(), setting)?;
    #[cfg(not(unix))]
    let _ = setting;
    Ok(dir)
}

/// Refuse the directory at `path` unless `euid` owns it and it has no group
/// or other bit, read through a handle opened without following a symlink.
#[cfg(unix)]
pub(crate) fn check_private(path: &Path, euid: u32, setting: &str) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    use rustix::fs::{Mode, OFlags};
    /// The group and other permission bits.
    const GROUP_OTHER: u32 = 0o077;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let handle = std::fs::File::from(rustix::fs::open(path, flags, Mode::empty())?);
    let meta = handle.metadata()?;
    let (owner, mode) = (meta.uid(), meta.mode() & 0o7777);
    if owner == euid && mode & GROUP_OTHER == 0 {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "the query spill directory {} is not private to uid {euid}, the user \
             trawld runs as (owner uid {}, mode {mode:04o}), so other users could \
             read spilled query state; point {setting} at a directory on a \
             filesystem that keeps Unix modes",
            path.display(),
            owner,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_dir_names_are_recognized_exactly() {
        let parent = tempfile::tempdir().expect("parent");
        let made = QueryScratch::new(parent.path())
            .private_spill_dir()
            .expect("spill dir");
        let name = made.path().file_name().expect("a final component");
        assert!(is_spill_dir_name(name), "{}", made.path().display());

        for name in ["trawl-spill-AbC123", "trawl-spill-000000"] {
            assert!(is_spill_dir_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "trawl-spill-",
            "trawl-spill-abc12",
            "trawl-spill-abc1234",
            "trawl-spill-abc-12",
            "trawl-spill-abc12.",
            "trawl-spill-../x1",
            "xtrawl-spill-abc123",
            "trawl-snapshot-abc123",
        ] {
            assert!(!is_spill_dir_name(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn spill_file_names_are_recognized_exactly() {
        for name in [
            "duckdb_temp_storage_DEFAULT-0.tmp",
            "duckdb_temp_storage_S32K-12.tmp",
            "duckdb_temp_storage_S224K-3.tmp",
            "duckdb_temp_block-4611686018427388160.block",
        ] {
            assert!(is_spill_file_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "duckdb_temp_storage_DEFAULT-.tmp",
            "duckdb_temp_storage_-0.tmp",
            "duckdb_temp_storage_default-0.tmp",
            "duckdb_temp_storage_DEFAULT-0.tmp.x",
            "duckdb_temp_storage_DEFAULT-0-1.tmp",
            "duckdb_temp_block-.block",
            "duckdb_temp_block--1.block",
            "duckdb_temp_block-1.tmp",
            "trawl-spill-abc123",
            "evil",
        ] {
            assert!(!is_spill_file_name(OsStr::new(name)), "{name}");
        }
    }

    /// The names a real spill writes are the ones the recognizer accepts:
    /// a whole-input window sort past a 64 MB memory limit spills into the
    /// private directory, which is read while the sort runs (`DuckDB`
    /// removes its files when the query ends).
    #[test]
    fn duckdb_spill_files_carry_the_recognized_names() {
        let parent = tempfile::tempdir().expect("parent");
        let spill = QueryScratch::new(parent.path())
            .private_spill_dir()
            .expect("spill dir");
        let conn = duckdb::Connection::open_in_memory().expect("duckdb");
        conn.execute_batch(&format!(
            "SET temp_directory='{}'; SET memory_limit='64MB'; SET threads=1;",
            spill.path().to_str().expect("utf-8")
        ))
        .expect("settings");
        let sort = std::thread::spawn(move || {
            conn.query_row(
                "SELECT max(rn) FROM (SELECT row_number() OVER (ORDER BY s) AS rn FROM \
                 (SELECT md5(i::VARCHAR) || md5((i * 7)::VARCHAR) AS s FROM range(2000000) t(i)))",
                [],
                |row| row.get::<_, i64>(0),
            )
        });
        let mut names = std::collections::BTreeSet::new();
        while !sort.is_finished() {
            for entry in std::fs::read_dir(spill.path()).expect("read spill dir") {
                names.insert(entry.expect("entry").file_name());
            }
        }
        assert_eq!(sort.join().expect("sort thread").expect("sort"), 2_000_000);
        assert!(!names.is_empty(), "the sort spilled nothing");
        for name in &names {
            assert!(is_spill_file_name(name), "{}", name.to_string_lossy());
        }
    }
}
