// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The query scratch directory (ADR-0056), and the hold every directory
//! trawld keeps apart from its storage roots shares with it.
//!
//! `[server] query_scratch_dir` holds what trawld writes while it answers a
//! query: the query database's private spill directory, and beside it the
//! hot-buffer query snapshots, the Parquet export staging files and the
//! rows a captured result stages for its write. This module owns the names
//! trawld makes there, so the code that creates an entry and the code that
//! recognizes one left behind read one spelling.
//!
//! The spill directory follows ADR-0052: a fresh random name, mode 0700,
//! refused unless a no-follow handle shows it owned by this process's euid
//! with no group or other bit. `DuckDB` opens its spill files by predictable
//! names without `O_EXCL`, so a directory another user could enter would let
//! them plant or read one.
//!
//! [`hold_apart_from`] makes a directory trawld's for the life of the
//! process. It makes the directory 0700 if it is missing, holds its final
//! component no-follow as ADR-0052 holds a storage root, and refuses the
//! held directory if, by device and inode, it is one of trawld's storage
//! roots or sits inside or above one. It then takes an exclusive `flock` on
//! that handle. The query scratch directory and the results root
//! (ADR-0057) are both held this way; a [`RootRole`] says which one a
//! refusal names.
//!
//! [`ScratchLock::open_apart_from`] holds the scratch directory, and only
//! then clears what an earlier trawld left behind, such as after a SIGKILL
//! or an eviction. The clearing reads the whole tree before it removes
//! anything: one entry it does not recognize refuses the start with nothing
//! deleted. It never follows a link and never crosses a mount. The
//! `PostgreSQL` sole-writer lock does not authorize it: that lock guards one
//! app-state database, not this directory, and a lost lock can go unseen
//! for seconds. The results root is never cleared: what it holds outlives
//! a restart.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Prefix of a query database's private spill directory.
pub const SPILL_DIR_PREFIX: &str = "trawl-spill-";

/// Prefix and suffix of a hot-buffer query snapshot file.
pub const SNAPSHOT_FILE_PREFIX: &str = "trawl-snapshot-";
/// See [`SNAPSHOT_FILE_PREFIX`].
pub const SNAPSHOT_FILE_SUFFIX: &str = ".ndjson";

/// Prefix and suffix of a Parquet export staging file.
pub const EXPORT_FILE_PREFIX: &str = "trawl-export-";
/// See [`EXPORT_FILE_PREFIX`].
pub const EXPORT_FILE_SUFFIX: &str = ".parquet";

/// Prefix and suffix of the file a captured result stages its rows in for
/// `DuckDB` to copy to parquet (ADR-0057).
pub const STAGE_FILE_PREFIX: &str = "trawl-stage-";
/// See [`STAGE_FILE_PREFIX`].
pub const STAGE_FILE_SUFFIX: &str = ".ndjson";

/// The prefix `DuckDB` 1.5.5's `COPY ... TO` gives the file it writes before
/// renaming it onto the target (`use_tmp_file`, on by default for a local
/// file; `plan_copy_to_file.cpp`). An export killed mid-copy leaves one
/// beside its staging file.
const COPY_TMP_PREFIX: &str = "tmp_";

/// Length of the random part of every name trawld makes in the scratch
/// directory, pinned so the recognizers can demand exactly what the
/// creators make.
const RANDOM_LEN: usize = 6;

/// The setting an operator changes to move query spill, named in refusals
/// of the scratch directory and of a spill directory made in it.
pub(crate) const SCRATCH_SETTING: &str = "[server] query_scratch_dir";

/// Which directory [`hold_apart_from`] holds, as its refusals name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootRole {
    /// What the directory is, as a refusal names it: `query scratch
    /// directory`.
    pub noun: &'static str,
    /// The setting an operator changes to move it: `[server]
    /// query_scratch_dir`.
    pub setting: &'static str,
}

impl RootRole {
    /// The query scratch directory (ADR-0056).
    pub const QUERY_SCRATCH: Self = Self {
        noun: "query scratch directory",
        setting: SCRATCH_SETTING,
    };

    /// The results root, where captured results live (ADR-0057).
    pub const RESULTS: Self = Self {
        noun: "results root",
        setting: "[server] results_dir",
    };
}

/// The variable an operator changes to move spill of a database opened
/// without a scratch directory ([`crate::executor::Executor::new`]).
pub(crate) const TMPDIR_SETTING: &str = "TMPDIR";

/// The query scratch directory, by path.
///
/// It names the directory and makes the entries trawld keeps inside it. It
/// does not create, lock or clear the directory itself: [`ScratchLock`]
/// does, and hands out this name once it holds the lock.
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

    /// Create a hot-buffer query snapshot file, 0600, named so
    /// [`is_snapshot_file_name`] recognizes it.
    pub fn snapshot_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        named_file_in(&self.path, SNAPSHOT_FILE_PREFIX, SNAPSHOT_FILE_SUFFIX)
    }

    /// Create a Parquet export staging file, 0600, named so
    /// [`is_export_file_name`] recognizes it.
    pub fn export_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        named_file_in(&self.path, EXPORT_FILE_PREFIX, EXPORT_FILE_SUFFIX)
    }

    /// Create a captured result's row staging file, 0600, named so
    /// [`is_stage_file_name`] recognizes it.
    pub fn stage_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        stage_file_in(&self.path)
    }
}

/// Create a captured result's row staging file in `dir`, 0600, named as
/// [`QueryScratch::stage_file`] names one: for a database opened without a
/// scratch directory, whose private spill directory holds it instead.
pub(crate) fn stage_file_in(dir: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    named_file_in(dir, STAGE_FILE_PREFIX, STAGE_FILE_SUFFIX)
}

/// Create a 0600 file in `dir` named `prefix`, [`RANDOM_LEN`] ASCII letters
/// or digits, then `suffix`.
fn named_file_in(
    dir: &Path,
    prefix: &str,
    suffix: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .rand_bytes(RANDOM_LEN)
        .tempfile_in(dir)
}

/// Whether `name` is `prefix`, exactly [`RANDOM_LEN`] ASCII letters or
/// digits, then `suffix`.
fn is_made_name(name: &OsStr, prefix: &str, suffix: &str) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(prefix))
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(|random| {
            random.len() == RANDOM_LEN && random.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Whether `name` is one trawld gives a query database's spill directory:
/// [`SPILL_DIR_PREFIX`] and exactly six ASCII letters or digits.
#[must_use]
pub fn is_spill_dir_name(name: &OsStr) -> bool {
    is_made_name(name, SPILL_DIR_PREFIX, "")
}

/// Whether `name` is one [`QueryScratch::snapshot_file`] makes.
#[must_use]
pub fn is_snapshot_file_name(name: &OsStr) -> bool {
    is_made_name(name, SNAPSHOT_FILE_PREFIX, SNAPSHOT_FILE_SUFFIX)
}

/// Whether `name` is one [`QueryScratch::export_file`] makes, or the
/// `tmp_`-prefixed file `DuckDB`'s `COPY ... TO` writes before it renames
/// it onto that name.
#[must_use]
pub fn is_export_file_name(name: &OsStr) -> bool {
    let name = name
        .to_str()
        .map(|name| name.strip_prefix(COPY_TMP_PREFIX).unwrap_or(name));
    name.is_some_and(|name| is_made_name(OsStr::new(name), EXPORT_FILE_PREFIX, EXPORT_FILE_SUFFIX))
}

/// Whether `name` is one [`QueryScratch::stage_file`] makes.
#[must_use]
pub fn is_stage_file_name(name: &OsStr) -> bool {
    is_made_name(name, STAGE_FILE_PREFIX, STAGE_FILE_SUFFIX)
}

/// The size classes `DuckDB` 1.5.5 names a grouped spill file by: every
/// `TemporaryBufferSize` but `INVALID`, as `EnumUtil::ToString` prints it
/// (`enum_util.cpp`, `GetTemporaryBufferSizeValues`). `INVALID` is left
/// out because no file is ever made for it: `CreateTemporaryFileName`
/// asserts a valid identifier.
const SPILL_SIZE_CLASSES: [&str; 8] = [
    "S32K", "S64K", "S96K", "S128K", "S160K", "S192K", "S224K", "DEFAULT",
];

/// Whether `name` is one `DuckDB` 1.5.5 gives a file in a spill directory,
/// and nothing else: the preflight unlinks what this accepts, so a name
/// `DuckDB` cannot have made must refuse the start instead.
///
/// Two shapes, both from the bundled source. The grouped block files are
/// `duckdb_temp_storage_<size>-<n>.tmp` (`temporary_file_manager.cpp`,
/// `CreateTemporaryFileName`: `"duckdb_temp_storage_%s-%llu.tmp"`), where
/// `<size>` is one of [`SPILL_SIZE_CLASSES`] and `<n>` is the file index,
/// an `idx_t` (`uint64_t`). The files of buffers larger than a block are
/// `duckdb_temp_block-<id>.block` (`standard_buffer_manager.cpp`,
/// `GetTemporaryPath`: `"duckdb_temp_block-" + to_string(id) + ".block"`),
/// where `<id>` is a `block_id_t` (`int64_t`). The buffer manager starts
/// its temporary-id counter at `MAXIMUM_BLOCK`
/// (`storage_info.hpp`: `4611686018427388000`) and pre-increments it
/// before it names a block (`standard_buffer_manager.cpp`: `++temporary_id`
/// at both allocation sites), so the first id a file can carry is
/// `MAXIMUM_BLOCK + 1` and every id from `0` through `MAXIMUM_BLOCK` is
/// one `DuckDB` cannot have made. Each number is accepted only as the C++
/// library prints it: no sign, no leading zero, within its type's width
/// (and, for a block id, above that floor). A `DuckDB` upgrade has to
/// re-check both shapes, the classes and the floor.
#[must_use]
pub fn is_spill_file_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if let Some(rest) = name
        .strip_prefix("duckdb_temp_storage_")
        .and_then(|rest| rest.strip_suffix(".tmp"))
    {
        return rest.split_once('-').is_some_and(|(size, index)| {
            SPILL_SIZE_CLASSES.contains(&size) && is_printed_integer::<u64>(index)
        });
    }
    name.strip_prefix("duckdb_temp_block-")
        .and_then(|rest| rest.strip_suffix(".block"))
        .is_some_and(|id| {
            is_printed_integer::<i64>(id) && id.parse::<i64>().is_ok_and(|id| id > MAXIMUM_BLOCK)
        })
}

/// `DuckDB` 1.5.5's `MAXIMUM_BLOCK` (`storage_info.hpp`): the temporary
/// block-id counter starts here and is incremented before use, so a spill
/// block file's id is strictly above it.
const MAXIMUM_BLOCK: i64 = 4_611_686_018_427_388_000;

/// Whether `digits` is a non-negative integer of type `T` exactly as C++
/// prints one: ASCII digits only, no sign, no leading zero but `0` itself,
/// and a value `T` holds. Rust's `parse` accepts a `+` sign, so the first
/// byte is checked by hand.
fn is_printed_integer<T: std::str::FromStr>(digits: &str) -> bool {
    let canonical = match digits.as_bytes() {
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    canonical && digits.parse::<T>().is_ok()
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
    builder.rand_bytes(RANDOM_LEN);
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

/// A directory whose entries trawld names and removes on its own, which a
/// held directory ([`hold_apart_from`]) must not be, sit inside or sit
/// above: for the scratch directory (ADR-0056) the data root, the WAL
/// directory and the two repin staging roots, and for the results root
/// (ADR-0057) those and the scratch directory.
#[derive(Debug, Clone)]
pub struct OwnedRoot {
    /// What the root is, with the setting it comes from, such as `the data
    /// root ([data] path)`.
    pub what: &'static str,
    /// Its configured path. It need not exist yet, but it must carry no
    /// `..` component: a root that does not exist yet is placed by the
    /// nearest directory on its path that does, and a `..` after a missing
    /// component would name a directory other than the one trawld makes
    /// (`<base>/missing/../x` is placed at `<base>` but made at
    /// `<base>/x`). Config validation refuses such a spelling for every
    /// root trawld passes here.
    pub path: PathBuf,
}

/// How a held directory overlaps an [`OwnedRoot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlap {
    /// It is the root.
    Is,
    /// It sits inside the root.
    Inside,
    /// The root sits inside it, or would once trawld makes the root.
    Above,
}

impl std::fmt::Display for Overlap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Is => "is",
            Self::Inside => "is inside",
            Self::Above => "is above",
        })
    }
}

/// Why [`hold_apart_from`] or [`ScratchLock::open`] refused a directory.
/// Every message names the directory by its [`RootRole`], with the setting
/// the operator changes.
#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    /// The directory overlaps one of trawld's roots as the filesystem
    /// resolves both. A symlink, `..` or a relative path can hide that from
    /// the configured spellings.
    #[error(
        "the {} {} {overlap} {what} {}, as the filesystem resolves them; point {} at a \
         directory of its own",
        .role.noun,
        .path.display(),
        .root.display(),
        .role.setting
    )]
    Overlap {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
        /// How it overlaps the root.
        overlap: Overlap,
        /// What the root is ([`OwnedRoot::what`]).
        what: &'static str,
        /// The root's configured path.
        root: PathBuf,
    },
    /// The path does not end in a directory name: it is empty, `/`, or ends
    /// in `..`.
    #[error(
        "the {} {} does not end in a directory name; point {} at a directory of its own",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    BadPath {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
    },
    /// A filesystem call failed.
    #[error(
        "cannot {action} the {} {}: {source}; check {}",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    Io {
        /// Which directory was refused.
        role: RootRole,
        /// What trawld was doing, as a verb phrase.
        action: &'static str,
        /// The path it was doing it to.
        path: PathBuf,
        /// The failure.
        #[source]
        source: std::io::Error,
    },
    /// The directory cannot be written: a query-only node whose data root is
    /// mounted read-only keeps the default scratch directory beside it.
    #[error(
        "the {} {} is on a read-only filesystem; point {} at a writable directory",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    ReadOnly {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
    },
    /// The final component is a symbolic link, which trawld does not follow.
    #[error(
        "the {} {} is a symbolic link, which trawld does not follow; point {} at the directory \
         itself",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    Symlink {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
    },
    /// Something other than a directory is at the path.
    #[error(
        "the {} {} is not a directory; move it aside or point {} at a directory",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    NotDirectory {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
    },
    /// Another user owns the directory. trawld does not take over a
    /// directory it was not given, even when it runs as root.
    #[error(
        "the {} {} is owned by uid {owner}, not uid {euid}, the user trawld runs as; point {} at \
         a directory uid {euid} owns",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    ForeignOwner {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
        /// The directory's owner.
        owner: u32,
        /// The uid trawld runs as.
        euid: u32,
    },
    /// A group or other bit survived the change that removes them, as on a
    /// filesystem with fixed modes.
    #[error(
        "the {} {} keeps mode {mode:04o} after trawld removed its group and other bits, so \
         other users could read query data; point {} at a directory on a filesystem that keeps \
         Unix modes",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    NotPrivate {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
        /// The mode read back after the change.
        mode: u32,
    },
    /// Another process holds the lock, or another open in this one does.
    #[error(
        "the {} {} is locked by another trawld; stop that trawld, or point this one's {} at a \
         directory of its own",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    Locked {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
    },
    /// An entry trawld does not make, or one it cannot remove safely. The
    /// whole tree is read before anything is removed, so nothing was. Only
    /// the scratch directory is cleared, so only it refuses this way.
    #[error(
        "the {} {} holds {}, which {reason}; trawld removes only the entries it makes and has \
         removed nothing. Move the entry out, or point {} at a directory of its own",
        .role.noun,
        .path.display(),
        .entry.display(),
        .role.setting
    )]
    ForeignEntry {
        /// Which directory was refused.
        role: RootRole,
        /// The configured path.
        path: PathBuf,
        /// The entry.
        entry: PathBuf,
        /// Why it refuses the start, as a predicate.
        reason: String,
    },
}

/// What [`ScratchLock::open`] removed: the entries an earlier trawld left
/// behind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// Query database spill directories, each with its spill files.
    pub spill_dirs: usize,
    /// Hot-buffer query snapshot files.
    pub snapshots: usize,
    /// Parquet export staging files, `DuckDB`'s `tmp_` copies included.
    pub exports: usize,
    /// Captured results' row staging files.
    pub stages: usize,
}

/// A directory held for this process by [`hold_apart_from`]: made, checked
/// owner-only and locked, its contents left as they were.
///
/// The lock is an exclusive `flock` on the directory's own no-follow
/// handle, so it lives exactly as long as this value: dropping it closes
/// the handle and frees the lock, and so does the end of the process,
/// however it ends. `flock` belongs to the open file description, so a
/// second hold in the same process is refused like one in another process.
#[derive(Debug)]
pub struct HeldRoot {
    path: PathBuf,
    /// The handle the lock is held on.
    handle: std::fs::File,
}

impl HeldRoot {
    /// The held directory's path, as configured, with a trailing `/` or `.`
    /// dropped.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The no-follow handle the lock is held on. Reach the directory's
    /// entries through it, by `openat`, `renameat` and `unlinkat`, and
    /// `fsync` it after a rename, so no link in the configured path can
    /// redirect a call to another directory. Unlocking it gives the
    /// directory up to any other process.
    #[must_use]
    pub fn dir(&self) -> &std::fs::File {
        &self.handle
    }
}

/// Make, check and lock the directory at `path` for a trawld running as
/// `euid`, which must not be, sit inside or sit above any of `roots`.
/// Nothing in it is removed: the scratch directory's clearing is
/// [`ScratchLock::open_apart_from`]'s own.
///
/// A missing directory is made 0700, below parents made as needed, as the
/// epoch gate makes the data root, unless the directory that would hold it
/// is or sits inside a root. Its final component is then opened no-follow,
/// and everything after is judged and done through that handle. It refuses
/// a symlink, a non-directory, a directory that overlaps a root, a
/// directory another user owns, and one on a read-only filesystem. A group
/// or other bit is removed through the handle, as ADR-0052 closes a root,
/// and refused if it survives. Then it takes the lock, refusing if another
/// process holds it. Every refusal names the directory by `role`.
///
/// The overlap is judged by device and inode, never by spelling: the
/// handle's own directory and each one above it, reached by opening `..`,
/// against each root and each directory above it, or, for a root that does
/// not exist yet, above where it would be made. A symlink, a `..` and a
/// relative path in either path all resolve to the same pairs. Judging the
/// handle makes the answer hold for the directory that is locked, whatever
/// its path names by then.
pub fn hold_apart_from(
    path: &Path,
    roots: &[OwnedRoot],
    euid: u32,
    role: RootRole,
) -> Result<HeldRoot, ScratchError> {
    #[cfg(unix)]
    return lock::hold_apart_from(path, roots, euid, role);
    #[cfg(not(unix))]
    {
        let _ = (roots, euid);
        Err(ScratchError::Io {
            role,
            action: "lock",
            path: path.to_owned(),
            source: std::io::ErrorKind::Unsupported.into(),
        })
    }
}

/// The query scratch directory, locked for this process and cleared of
/// what an earlier trawld left behind.
///
/// The lock is [`HeldRoot`]'s: it lives exactly as long as this value.
#[derive(Debug)]
pub struct ScratchLock {
    scratch: QueryScratch,
    /// The held directory, whose handle carries the lock.
    _held: HeldRoot,
    reclaimed: Reclaimed,
}

impl ScratchLock {
    /// [`Self::open_apart_from`] with no root to keep apart from, for a
    /// directory that cannot overlap trawld's storage, such as a test's own
    /// temporary directory.
    pub fn open(path: &Path) -> Result<Self, ScratchError> {
        Self::open_apart_from(path, &[])
    }

    /// Hold the scratch directory at `path` apart from `roots`, as
    /// [`hold_apart_from`] holds a directory, then clear it (see the module
    /// documentation). Refusals name [`RootRole::QUERY_SCRATCH`].
    ///
    /// The lock comes before the clearing, so a directory another trawld
    /// holds, for any role, is refused with nothing removed.
    pub fn open_apart_from(path: &Path, roots: &[OwnedRoot]) -> Result<Self, ScratchError> {
        #[cfg(unix)]
        return Self::open_as(path, roots, rustix::process::geteuid().as_raw());
        #[cfg(not(unix))]
        return Err(ScratchError::Io {
            role: RootRole::QUERY_SCRATCH,
            action: "lock",
            path: path.to_owned(),
            source: std::io::ErrorKind::Unsupported.into(),
        });
    }

    /// [`Self::open_apart_from`] for a trawld running as `euid`.
    #[cfg(unix)]
    fn open_as(path: &Path, roots: &[OwnedRoot], euid: u32) -> Result<Self, ScratchError> {
        let held = lock::hold_apart_from(path, roots, euid, RootRole::QUERY_SCRATCH)?;
        let stale = lock::preflight(held.path(), held.dir(), euid)?;
        let reclaimed = lock::remove(held.path(), held.dir(), stale)?;
        Ok(Self {
            scratch: QueryScratch::new(held.path()),
            _held: held,
            reclaimed,
        })
    }

    /// The scratch directory this lock holds.
    #[must_use]
    pub fn scratch(&self) -> &QueryScratch {
        &self.scratch
    }

    /// What the open removed.
    #[must_use]
    pub fn reclaimed(&self) -> Reclaimed {
        self.reclaimed
    }
}

/// The filesystem work behind [`hold_apart_from`] and [`ScratchLock::open`].
/// Everything below the held directory is reached by descriptor, one
/// `openat` or `unlinkat` at a time, never by a path a link could redirect.
#[cfg(unix)]
mod lock {
    use std::ffi::{CStr, CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsFd as _, BorrowedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Component, Path, PathBuf};

    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    use super::{
        HeldRoot, Overlap, OwnedRoot, Reclaimed, RootRole, ScratchError, is_export_file_name,
        is_snapshot_file_name, is_spill_dir_name, is_spill_file_name, is_stage_file_name,
    };

    /// The role the clearing names: only the scratch directory is cleared.
    const SCRATCH: RootRole = RootRole::QUERY_SCRATCH;

    /// The group and other permission bits.
    const GROUP_OTHER: u32 = 0o077;

    /// Flags for a no-follow directory open.
    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    /// `path` rebuilt from its components, or `None` when it does not end
    /// in a directory name. Rebuilding drops a trailing `/` or `.`, which
    /// would turn a final symlink into an intermediate component that the
    /// no-follow open follows.
    pub(super) fn final_name(path: &Path) -> Option<PathBuf> {
        let components: Vec<Component<'_>> = path.components().collect();
        matches!(components.last(), Some(Component::Normal(_))).then(|| components.iter().collect())
    }

    /// [`super::hold_apart_from`].
    pub(super) fn hold_apart_from(
        path: &Path,
        roots: &[OwnedRoot],
        euid: u32,
        role: RootRole,
    ) -> Result<HeldRoot, ScratchError> {
        use std::fs::TryLockError;

        use rustix::fs::StatVfsMountFlags;

        let path = final_name(path).ok_or_else(|| ScratchError::BadPath {
            role,
            path: path.to_owned(),
        })?;
        refuse_making_inside(&path, roots, role)?;
        make(&path, role)?;
        let handle = hold(&path, role)?;
        refuse_overlap(&path, &handle, roots, role)?;
        let io = |action| {
            let path = path.clone();
            move |source: io::Error| ScratchError::Io {
                role,
                action,
                path,
                source,
            }
        };

        let stat = stat_handle(handle.as_fd()).map_err(io("inspect"))?;
        if stat.owner != euid {
            return Err(ScratchError::ForeignOwner {
                role,
                path,
                owner: stat.owner,
                euid,
            });
        }
        let read_only = rustix::fs::fstatvfs(&handle)
            .map_err(|e| io("inspect")(e.into()))?
            .f_flag
            .contains(StatVfsMountFlags::RDONLY);
        if read_only {
            return Err(ScratchError::ReadOnly { role, path });
        }
        if stat.mode & GROUP_OTHER != 0 {
            #[allow(
                clippy::unnecessary_cast,
                clippy::cast_possible_truncation,
                reason = "RawMode is u32 on Linux and u16 on macOS; `mode` holds only the 0o7777 bits"
            )]
            let private = (stat.mode & !GROUP_OTHER) as rustix::fs::RawMode;
            rustix::fs::fchmod(&handle, Mode::from_raw_mode(private))
                .map_err(|e| io("remove the group and other bits of")(e.into()))?;
            let mode = stat_handle(handle.as_fd()).map_err(io("inspect"))?.mode;
            if mode & GROUP_OTHER != 0 {
                return Err(ScratchError::NotPrivate { role, path, mode });
            }
        }

        match handle.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(ScratchError::Locked { role, path }),
            Err(TryLockError::Error(e)) => return Err(io("lock")(e)),
        }
        Ok(HeldRoot { path, handle })
    }

    /// Make the directory at `path` 0700 if nothing is there, and its
    /// parents as needed. Something already there is left for [`hold`] to
    /// judge.
    fn make(path: &Path, role: RootRole) -> Result<(), ScratchError> {
        use std::os::unix::fs::DirBuilderExt as _;
        let refuse = |action, source: io::Error| match source.kind() {
            io::ErrorKind::ReadOnlyFilesystem => ScratchError::ReadOnly {
                role,
                path: path.to_owned(),
            },
            _ => ScratchError::Io {
                role,
                action,
                path: path.to_owned(),
                source,
            },
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| refuse("make the parent of", e))?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(refuse("make", e)),
        }
    }

    /// Open the directory at `path` without following a final symlink.
    pub(super) fn hold(path: &Path, role: RootRole) -> Result<File, ScratchError> {
        match rustix::fs::open(path, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(File::from(fd)),
            // Linux answers a symlink with ENOTDIR here, not ELOOP: the open
            // has already refused it, and the lstat only names why.
            Err(Errno::LOOP | Errno::NOTDIR) => Err(
                if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) {
                    ScratchError::Symlink {
                        role,
                        path: path.to_owned(),
                    }
                } else {
                    ScratchError::NotDirectory {
                        role,
                        path: path.to_owned(),
                    }
                },
            ),
            Err(e) => Err(ScratchError::Io {
                role,
                action: "open",
                path: path.to_owned(),
                source: e.into(),
            }),
        }
    }

    /// A directory's device and inode. Every spelling of a directory, a
    /// symlink, a `..`, a relative path or a bind mount, leads to one pair.
    type Identity = (u64, u64);

    /// Flags for an open that only finds a directory, following links. On
    /// Linux `O_PATH` needs no read permission, only search on the way.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const LOOKUP_FLAGS: OFlags = OFlags::PATH.union(OFlags::DIRECTORY).union(OFlags::CLOEXEC);
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const LOOKUP_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC);

    #[allow(clippy::cast_sign_loss, clippy::unnecessary_cast)]
    fn identity(fd: BorrowedFd<'_>) -> io::Result<Identity> {
        let stat = rustix::fs::fstat(fd)?;
        Ok((stat.st_dev as u64, stat.st_ino as u64))
    }

    /// The identities of the directory `fd` holds and of each directory
    /// above it, nearest first, up to the process's root. Each step opens
    /// `..` from the step before, so the kernel names every parent.
    fn lineage(fd: BorrowedFd<'_>) -> io::Result<Vec<Identity>> {
        let mut lineage = vec![identity(fd)?];
        let mut dir = rustix::fs::openat(fd, c"..", LOOKUP_FLAGS, Mode::empty())?;
        loop {
            let id = identity(dir.as_fd())?;
            // The root is its own parent.
            if lineage.last() == Some(&id) {
                return Ok(lineage);
            }
            lineage.push(id);
            dir = rustix::fs::openat(&dir, c"..", LOOKUP_FLAGS, Mode::empty())?;
        }
    }

    /// `path`, or failing that the nearest directory on its path above it,
    /// opened for lookups, with whether it is `path` itself. `None` when
    /// nothing on the path opens.
    fn nearest(path: &Path) -> Option<(rustix::fd::OwnedFd, bool)> {
        let open = |dir: &Path| {
            let dir = if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            };
            rustix::fs::open(dir, LOOKUP_FLAGS, Mode::empty()).ok()
        };
        if let Some(fd) = open(path) {
            return Some((fd, true));
        }
        path.ancestors()
            .skip(1)
            .find_map(open)
            .map(|fd| (fd, false))
    }

    /// Where a root is, as far as the filesystem says.
    struct Located {
        /// The root itself, when it is a directory.
        at: Option<Identity>,
        /// The directories above it, or, for a root that is not there yet,
        /// the nearest one on its path that is and those above that: where
        /// trawld would make it.
        above: Vec<Identity>,
    }

    fn locate(root: &Path) -> io::Result<Located> {
        Ok(match nearest(root) {
            Some((fd, true)) => {
                let mut above = lineage(fd.as_fd())?;
                let at = above.remove(0);
                Located {
                    at: Some(at),
                    above,
                }
            }
            Some((fd, false)) => Located {
                at: None,
                above: lineage(fd.as_fd())?,
            },
            None => Located {
                at: None,
                above: Vec::new(),
            },
        })
    }

    /// How the directory `here` with the directories `above` it overlaps
    /// `root`. Without `here`, `above` is where a directory will be made,
    /// which can only land inside a root.
    fn overlap(here: Option<Identity>, above: &[Identity], root: &Located) -> Option<Overlap> {
        if here.is_some() && root.at == here {
            Some(Overlap::Is)
        } else if root.at.is_some_and(|at| above.contains(&at)) {
            Some(Overlap::Inside)
        } else if here.is_some_and(|here| root.above.contains(&here)) {
            Some(Overlap::Above)
        } else {
            None
        }
    }

    /// Refuse at the first of `roots` that `here` and `above` overlap.
    fn refuse_roots(
        path: &Path,
        here: Option<Identity>,
        above: &[Identity],
        roots: &[OwnedRoot],
        role: RootRole,
    ) -> Result<(), ScratchError> {
        for root in roots {
            let located = locate(&root.path).map_err(|source| ScratchError::Io {
                role,
                action: "compare trawld's storage roots with",
                path: path.to_owned(),
                source,
            })?;
            if let Some(overlap) = overlap(here, above, &located) {
                return Err(ScratchError::Overlap {
                    role,
                    path: path.to_owned(),
                    overlap,
                    what: root.what,
                    root: root.path.clone(),
                });
            }
        }
        Ok(())
    }

    /// Refuse before [`make`] runs when the directory that would hold
    /// `path`, its parent or the nearest directory above that exists, is a
    /// root or sits inside one: making `path` there would make directories
    /// inside the root.
    fn refuse_making_inside(
        path: &Path,
        roots: &[OwnedRoot],
        role: RootRole,
    ) -> Result<(), ScratchError> {
        if roots.is_empty() {
            return Ok(());
        }
        let Some((holder, _)) = path.parent().and_then(nearest) else {
            return Ok(());
        };
        let above = lineage(holder.as_fd()).map_err(|source| ScratchError::Io {
            role,
            action: "inspect the parents of",
            path: path.to_owned(),
            source,
        })?;
        refuse_roots(path, None, &above, roots, role)
    }

    /// Refuse the directory `handle` holds when it is one of `roots`, sits
    /// inside one, or sits above one or above where trawld would make one.
    fn refuse_overlap(
        path: &Path,
        handle: &File,
        roots: &[OwnedRoot],
        role: RootRole,
    ) -> Result<(), ScratchError> {
        if roots.is_empty() {
            return Ok(());
        }
        let lineage = lineage(handle.as_fd()).map_err(|source| ScratchError::Io {
            role,
            action: "inspect the parents of",
            path: path.to_owned(),
            source,
        })?;
        let (here, above) = lineage
            .split_first()
            .expect("a lineage starts at its directory");
        refuse_roots(path, Some(*here), above, roots, role)
    }

    /// Where an inode lives: its filesystem and, where the kernel says,
    /// its mount. A bind mount keeps the device, so the device alone
    /// cannot see one; Linux 5.8 and later name the mount.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Placement {
        device: u64,
        mount: Option<u64>,
    }

    impl Placement {
        /// Whether this placement can prove two inodes share a mount, as
        /// `owner_only` asks of the WAL directory: on Linux it must name its
        /// mount, since a bind mount keeps the device. Elsewhere the
        /// platform names no mount, and the device is all there is.
        const fn names_its_mount(self) -> bool {
            !cfg!(any(target_os = "linux", target_os = "android")) || self.mount.is_some()
        }
    }

    /// What one `stat` of an entry says, without following a symlink.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct EntryStat {
        kind: FileType,
        owner: u32,
        mode: u32,
        inode: u64,
        placement: Placement,
    }

    #[cfg(test)]
    impl EntryStat {
        /// This stat as a kernel before Linux 5.8 answers it, without the
        /// mount.
        pub(super) fn without_mount(self) -> Self {
            Self {
                placement: Placement {
                    mount: None,
                    ..self.placement
                },
                ..self
            }
        }
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "android")))]
    thread_local! {
        /// Whether `statx` answers `ENOSYS` on this thread, as Linux before
        /// 4.11 does.
        pub(super) static STATX_ANSWERS_NOSYS: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    /// The entry `name` in `dir`, or with `AtFlags::EMPTY_PATH` the inode
    /// `dir` holds. Linux before 4.11 has no `statx`; there a plain `stat`
    /// answers, which names no mount, and [`refusal`] refuses every entry
    /// as on a kernel before 5.8.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn statx(dir: BorrowedFd<'_>, name: &CStr, flags: AtFlags) -> io::Result<EntryStat> {
        use rustix::fs::StatxFlags;
        let flags = flags | AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
        let answer = rustix::fs::statx(
            dir,
            name,
            flags,
            StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
        );
        #[cfg(test)]
        let answer = if STATX_ANSWERS_NOSYS.get() {
            Err(Errno::NOSYS)
        } else {
            answer
        };
        let stat = match answer {
            Ok(stat) => stat,
            Err(Errno::NOSYS) => return Ok(from_stat(&rustix::fs::statat(dir, name, flags)?)),
            Err(e) => return Err(e.into()),
        };
        let mode = u32::from(stat.stx_mode);
        Ok(EntryStat {
            kind: FileType::from_raw_mode(mode),
            owner: stat.stx_uid,
            mode: mode & 0o7777,
            inode: stat.stx_ino,
            placement: Placement {
                device: rustix::fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
                mount: (stat.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stat.stx_mnt_id),
            },
        })
    }

    /// The entry `name` in `dir`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn stat_entry(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<EntryStat> {
        statx(dir, name, AtFlags::empty())
    }

    /// The inode `fd` holds.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn stat_handle(fd: BorrowedFd<'_>) -> io::Result<EntryStat> {
        statx(fd, c"", AtFlags::EMPTY_PATH)
    }

    /// What a plain `stat` says, which names no mount.
    fn from_stat(stat: &rustix::fs::Stat) -> EntryStat {
        #[allow(clippy::cast_sign_loss, clippy::unnecessary_cast)]
        EntryStat {
            kind: FileType::from_raw_mode(stat.st_mode as _),
            owner: stat.st_uid,
            mode: (stat.st_mode as u32) & 0o7777,
            inode: stat.st_ino as u64,
            placement: Placement {
                device: stat.st_dev as u64,
                mount: None,
            },
        }
    }

    /// The entry `name` in `dir`. This platform names no mount, so the
    /// device is all there is.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn stat_entry(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<EntryStat> {
        Ok(from_stat(&rustix::fs::statat(
            dir,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
        )?))
    }

    /// The inode `fd` holds.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn stat_handle(fd: BorrowedFd<'_>) -> io::Result<EntryStat> {
        Ok(from_stat(&rustix::fs::fstat(fd)?))
    }

    /// The entries of the directory `dir` holds, `.` and `..` left out,
    /// each with its `stat`. `path` names `dir` in errors.
    fn entries(dir: &File, path: &Path) -> Result<Vec<(CString, EntryStat)>, ScratchError> {
        let io = |source: io::Error| ScratchError::Io {
            role: SCRATCH,
            action: "read",
            path: path.to_owned(),
            source,
        };
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::read_from(dir).map_err(|e| io(e.into()))? {
            let entry = entry.map_err(|e| io(e.into()))?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            let stat = stat_entry(dir.as_fd(), name).map_err(io)?;
            out.push((name.to_owned(), stat));
        }
        Ok(out)
    }

    /// An entry an earlier trawld left behind, checked and due for removal.
    pub(super) enum Stale {
        /// A spill directory, held open, and the spill files in it.
        SpillDir {
            name: CString,
            dir: File,
            files: Vec<CString>,
        },
        /// A snapshot file.
        Snapshot(CString),
        /// An export staging file.
        Export(CString),
        /// A captured result's row staging file.
        Stage(CString),
    }

    /// Why `stat` refuses whatever its name: a symlink, another mount or a
    /// mount the kernel does not name, or another owner.
    fn refusal(stat: &EntryStat, home: Placement, euid: u32) -> Option<String> {
        if stat.kind == FileType::Symlink {
            Some("is a symbolic link, which trawld does not follow".to_owned())
        } else if !home.names_its_mount() || !stat.placement.names_its_mount() {
            Some(
                "may be on another mount, which trawld does not cross: this kernel does not \
                 name mounts (Linux 5.8 and later do)"
                    .to_owned(),
            )
        } else if stat.placement != home {
            Some("is on another mount, which trawld does not cross".to_owned())
        } else if stat.owner != euid {
            Some(format!(
                "is owned by uid {}, not uid {euid}, the user trawld runs as",
                stat.owner
            ))
        } else {
            None
        }
    }

    /// Read the whole tree under the locked directory `root` and return
    /// what to remove, or refuse at the first entry trawld did not make.
    /// Nothing is removed here, so a refusal leaves the tree as it was.
    ///
    /// trawld makes four kinds of entry. A spill directory is a directory
    /// named by [`is_spill_dir_name`] holding only regular files named by
    /// [`is_spill_file_name`]. A snapshot, an export or a stage is a regular
    /// file named by [`is_snapshot_file_name`], [`is_export_file_name`] or
    /// [`is_stage_file_name`]. Each
    /// is owned by `euid` and sits in the root's mount. Anything else
    /// refuses: another name, a symlink, a special file, a directory
    /// inside a spill directory, another owner, another mount. On Linux a
    /// kernel that does not name mounts, before 5.8, refuses every entry:
    /// a bind mount keeps the device, so the device alone cannot show an
    /// entry sits in the root's mount. An empty directory still opens.
    pub(super) fn preflight(
        path: &Path,
        root: &File,
        euid: u32,
    ) -> Result<Vec<Stale>, ScratchError> {
        preflight_seen(path, root, euid, |stat| stat)
    }

    /// [`preflight`], with every `stat` passed through `seen`: unchanged in
    /// production, and in tests a stand-in for a kernel that answers
    /// differently.
    pub(super) fn preflight_seen(
        path: &Path,
        root: &File,
        euid: u32,
        seen: impl Fn(EntryStat) -> EntryStat,
    ) -> Result<Vec<Stale>, ScratchError> {
        let home = seen(
            stat_handle(root.as_fd()).map_err(|source| ScratchError::Io {
                role: SCRATCH,
                action: "inspect",
                path: path.to_owned(),
                source,
            })?,
        )
        .placement;
        let mut stale = Vec::new();
        for (name, stat) in entries(root, path)? {
            let stat = seen(stat);
            let os_name = OsStr::from_bytes(name.to_bytes());
            let entry = path.join(os_name);
            let foreign = |entry: &Path, reason: String| ScratchError::ForeignEntry {
                role: SCRATCH,
                path: path.to_owned(),
                entry: entry.to_owned(),
                reason,
            };
            if let Some(reason) = refusal(&stat, home, euid) {
                return Err(foreign(&entry, reason));
            }
            let regular = stat.kind == FileType::RegularFile;
            if is_spill_dir_name(os_name) && stat.kind == FileType::Directory {
                let dir = match rustix::fs::openat(root, &name, DIR_FLAGS, Mode::empty()) {
                    Ok(fd) => File::from(fd),
                    Err(e) => {
                        return Err(ScratchError::Io {
                            role: SCRATCH,
                            action: "open",
                            path: entry,
                            source: e.into(),
                        });
                    }
                };
                let opened = seen(stat_handle(dir.as_fd()).map_err(|source| ScratchError::Io {
                    role: SCRATCH,
                    action: "inspect",
                    path: entry.clone(),
                    source,
                })?);
                if opened.inode != stat.inode || opened.placement != stat.placement {
                    return Err(foreign(&entry, "changed while trawld read it".to_owned()));
                }
                let mut files = Vec::new();
                for (child, child_stat) in entries(&dir, &entry)? {
                    let child_stat = seen(child_stat);
                    let child_name = OsStr::from_bytes(child.to_bytes());
                    let child_path = entry.join(child_name);
                    if let Some(reason) = refusal(&child_stat, home, euid) {
                        return Err(foreign(&child_path, reason));
                    }
                    if child_stat.kind != FileType::RegularFile || !is_spill_file_name(child_name) {
                        return Err(foreign(
                            &child_path,
                            "is not a spill file DuckDB makes".to_owned(),
                        ));
                    }
                    files.push(child);
                }
                stale.push(Stale::SpillDir { name, dir, files });
            } else if regular && is_snapshot_file_name(os_name) {
                stale.push(Stale::Snapshot(name));
            } else if regular && is_export_file_name(os_name) {
                stale.push(Stale::Export(name));
            } else if regular && is_stage_file_name(os_name) {
                stale.push(Stale::Stage(name));
            } else {
                return Err(foreign(&entry, "is not an entry trawld makes".to_owned()));
            }
        }
        Ok(stale)
    }

    /// Remove what [`preflight`] found, by name relative to the open
    /// directories, so no link is followed. An entry already gone counts
    /// as removed.
    pub(super) fn remove(
        path: &Path,
        root: &File,
        stale: Vec<Stale>,
    ) -> Result<Reclaimed, ScratchError> {
        let unlink =
            |dir: &File, dir_path: &Path, name: &CStr, flags: AtFlags| match rustix::fs::unlinkat(
                dir, name, flags,
            ) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(ScratchError::Io {
                    role: SCRATCH,
                    action: "remove an entry of",
                    path: dir_path.join(OsStr::from_bytes(name.to_bytes())),
                    source: e.into(),
                }),
            };
        let mut reclaimed = Reclaimed::default();
        for entry in stale {
            match entry {
                Stale::SpillDir { name, dir, files } => {
                    let dir_path = path.join(OsStr::from_bytes(name.to_bytes()));
                    for file in &files {
                        unlink(&dir, &dir_path, file, AtFlags::empty())?;
                    }
                    drop(dir);
                    unlink(root, path, &name, AtFlags::REMOVEDIR)?;
                    reclaimed.spill_dirs += 1;
                }
                Stale::Snapshot(name) => {
                    unlink(root, path, &name, AtFlags::empty())?;
                    reclaimed.snapshots += 1;
                }
                Stale::Export(name) => {
                    unlink(root, path, &name, AtFlags::empty())?;
                    reclaimed.exports += 1;
                }
                Stale::Stage(name) => {
                    unlink(root, path, &name, AtFlags::empty())?;
                    reclaimed.stages += 1;
                }
            }
        }
        Ok(reclaimed)
    }
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

    /// The recognizer accepts exactly what `DuckDB` 1.5.5 prints: one of
    /// its eight size-class names and a `uint64_t` file index, or an
    /// `int64_t` block id, each as the C++ library prints it, so a name
    /// `DuckDB` cannot have made is foreign however spill-like it looks.
    #[test]
    fn spill_file_names_are_recognized_exactly() {
        for name in [
            "duckdb_temp_storage_DEFAULT-0.tmp",
            "duckdb_temp_storage_S32K-12.tmp",
            "duckdb_temp_storage_S64K-0.tmp",
            "duckdb_temp_storage_S96K-0.tmp",
            "duckdb_temp_storage_S128K-0.tmp",
            "duckdb_temp_storage_S160K-0.tmp",
            "duckdb_temp_storage_S192K-0.tmp",
            "duckdb_temp_storage_S224K-3.tmp",
            "duckdb_temp_storage_DEFAULT-18446744073709551615.tmp",
            // The first id DuckDB can name is MAXIMUM_BLOCK + 1.
            "duckdb_temp_block-4611686018427388001.block",
            "duckdb_temp_block-4611686018427388160.block",
            "duckdb_temp_block-9223372036854775807.block",
        ] {
            assert!(is_spill_file_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "duckdb_temp_storage_DEFAULT-.tmp",
            "duckdb_temp_storage_-0.tmp",
            "duckdb_temp_storage_default-0.tmp",
            "duckdb_temp_storage_DEFAULT-0.tmp.x",
            "duckdb_temp_storage_DEFAULT-0-1.tmp",
            // Size classes DuckDB does not have, or never names a file by.
            "duckdb_temp_storage_NOTTRAWLDS-0.tmp",
            "duckdb_temp_storage_INVALID-0.tmp",
            "duckdb_temp_storage_S48K-0.tmp",
            "duckdb_temp_storage_S256K-0.tmp",
            "duckdb_temp_storage_S32K2-0.tmp",
            // Over the width of a uint64_t, or not how one is printed.
            "duckdb_temp_storage_DEFAULT-18446744073709551616.tmp",
            "duckdb_temp_storage_DEFAULT-00.tmp",
            "duckdb_temp_storage_DEFAULT-01.tmp",
            "duckdb_temp_storage_DEFAULT-+1.tmp",
            "duckdb_temp_block-.block",
            "duckdb_temp_block--1.block",
            "duckdb_temp_block-+1.block",
            "duckdb_temp_block-007.block",
            // At or below MAXIMUM_BLOCK: DuckDB counts up from it, and
            // names a file only after incrementing, so none of these is
            // a name it can have made.
            "duckdb_temp_block-0.block",
            "duckdb_temp_block-1.block",
            "duckdb_temp_block-4611686018427387999.block",
            "duckdb_temp_block-4611686018427388000.block",
            // Over the width of an int64_t.
            "duckdb_temp_block-9223372036854775808.block",
            "duckdb_temp_block-18446744073709551615.block",
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

    #[test]
    fn snapshot_export_and_stage_names_are_recognized_exactly() {
        let parent = tempfile::tempdir().expect("parent");
        let scratch = QueryScratch::new(parent.path());
        let snapshot = scratch.snapshot_file().expect("snapshot");
        let export = scratch.export_file().expect("export");
        let stage = scratch.stage_file().expect("stage");
        let snapshot_name = snapshot.path().file_name().expect("a final component");
        let export_name = export.path().file_name().expect("a final component");
        let stage_name = stage.path().file_name().expect("a final component");
        assert!(is_snapshot_file_name(snapshot_name), "{snapshot_name:?}");
        assert!(is_export_file_name(export_name), "{export_name:?}");
        assert!(is_stage_file_name(stage_name), "{stage_name:?}");
        let copy_tmp = format!("tmp_{}", export_name.to_str().expect("utf-8"));
        assert!(is_export_file_name(OsStr::new(&copy_tmp)), "{copy_tmp}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for file in [&snapshot, &export, &stage] {
                let mode = file
                    .as_file()
                    .metadata()
                    .expect("stat")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{}", file.path().display());
            }
        }

        for name in [
            "trawl-snapshot-.ndjson",
            "trawl-snapshot-abc12.ndjson",
            "trawl-snapshot-abc1234.ndjson",
            "trawl-snapshot-abc-12.ndjson",
            "trawl-snapshot-abc123.ndjson.tmp",
            "tmp_trawl-snapshot-abc123.ndjson",
            "trawl-export-abc123.parquet",
            "trawl-snapshot-abc123",
        ] {
            assert!(!is_snapshot_file_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "trawl-export-abc12.parquet",
            "trawl-export-abc123.parquet.tmp",
            "tmp_tmp_trawl-export-abc123.parquet",
            "tmptrawl-export-abc123.parquet",
            "trawl-export-abc123",
            "trawl-snapshot-abc123.ndjson",
        ] {
            assert!(!is_export_file_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "trawl-stage-.ndjson",
            "trawl-stage-abc12.ndjson",
            "trawl-stage-abc1234.ndjson",
            "trawl-stage-abc-12.ndjson",
            "trawl-stage-abc123.ndjson.tmp",
            "tmp_trawl-stage-abc123.ndjson",
            "trawl-stage-abc123.parquet",
            "trawl-snapshot-abc123.ndjson",
        ] {
            assert!(!is_stage_file_name(OsStr::new(name)), "{name}");
        }
    }

    /// Every refusal of the scratch directory, word for word: the boot
    /// error an operator reads, the startup tests and the docs quote these.
    #[test]
    #[allow(clippy::too_many_lines)] // one case per variant, kept in one table
    fn scratch_refusals_read_word_for_word() {
        let path = || PathBuf::from("/srv/data.query-scratch");
        let cases: [(ScratchError, &str); 10] = [
            (
                ScratchError::Overlap {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                    overlap: Overlap::Inside,
                    what: "the data root ([data] path)",
                    root: PathBuf::from("/srv/data"),
                },
                "the query scratch directory /srv/data.query-scratch is inside the data root \
                 ([data] path) /srv/data, as the filesystem resolves them; point [server] \
                 query_scratch_dir at a directory of its own",
            ),
            (
                ScratchError::BadPath {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                },
                "the query scratch directory /srv/data.query-scratch does not end in a directory \
                 name; point [server] query_scratch_dir at a directory of its own",
            ),
            (
                ScratchError::Io {
                    role: RootRole::QUERY_SCRATCH,
                    action: "open",
                    path: path(),
                    source: std::io::Error::other("boom"),
                },
                "cannot open the query scratch directory /srv/data.query-scratch: boom; check \
                 [server] query_scratch_dir",
            ),
            (
                ScratchError::ReadOnly {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                },
                "the query scratch directory /srv/data.query-scratch is on a read-only \
                 filesystem; point [server] query_scratch_dir at a writable directory",
            ),
            (
                ScratchError::Symlink {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                },
                "the query scratch directory /srv/data.query-scratch is a symbolic link, which \
                 trawld does not follow; point [server] query_scratch_dir at the directory itself",
            ),
            (
                ScratchError::NotDirectory {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                },
                "the query scratch directory /srv/data.query-scratch is not a directory; move it \
                 aside or point [server] query_scratch_dir at a directory",
            ),
            (
                ScratchError::ForeignOwner {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                    owner: 7,
                    euid: 8,
                },
                "the query scratch directory /srv/data.query-scratch is owned by uid 7, not uid \
                 8, the user trawld runs as; point [server] query_scratch_dir at a directory uid \
                 8 owns",
            ),
            (
                ScratchError::NotPrivate {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                    mode: 0o750,
                },
                "the query scratch directory /srv/data.query-scratch keeps mode 0750 after trawld \
                 removed its group and other bits, so other users could read query data; point \
                 [server] query_scratch_dir at a directory on a filesystem that keeps Unix modes",
            ),
            (
                ScratchError::Locked {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                },
                "the query scratch directory /srv/data.query-scratch is locked by another trawld; \
                 stop that trawld, or point this one's [server] query_scratch_dir at a directory \
                 of its own",
            ),
            (
                ScratchError::ForeignEntry {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                    entry: PathBuf::from("/srv/data.query-scratch/notes.txt"),
                    reason: "is not an entry trawld makes".to_owned(),
                },
                "the query scratch directory /srv/data.query-scratch holds \
                 /srv/data.query-scratch/notes.txt, which is not an entry trawld makes; trawld \
                 removes only the entries it makes and has removed nothing. Move the entry out, \
                 or point [server] query_scratch_dir at a directory of its own",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }

    /// Every entry under `root`, `root` included, with what it is: a
    /// directory, a file and its bytes, or a link and its target. A refused
    /// open must leave this unchanged.
    #[cfg(unix)]
    fn tree(root: &Path) -> Vec<(PathBuf, String)> {
        let meta = std::fs::symlink_metadata(root).expect("stat");
        let what = if meta.is_symlink() {
            format!(
                "link {}",
                std::fs::read_link(root).expect("readlink").display()
            )
        } else if meta.is_dir() {
            "dir".to_owned()
        } else {
            format!("file {:?}", std::fs::read(root).expect("read"))
        };
        let mut out = vec![(root.to_owned(), what)];
        if meta.is_dir() {
            let mut children: Vec<PathBuf> = std::fs::read_dir(root)
                .expect("list")
                .map(|entry| entry.expect("entry").path())
                .collect();
            children.sort();
            for child in children {
                out.extend(tree(&child));
            }
        }
        out
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::symlink_metadata(path).expect("stat").mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// Leave in `scratch` what a trawld killed mid-query leaves: a spill
    /// directory holding both kinds of `DuckDB` spill file, a snapshot, an
    /// export staging file and `DuckDB`'s `tmp_` copy of it, and a
    /// capture's row staging file. Returns the spill directory.
    #[cfg(unix)]
    fn plant_stale(scratch: &Path) -> PathBuf {
        let made = QueryScratch::new(scratch);
        let spill = made.private_spill_dir().expect("spill dir").keep();
        for name in [
            "duckdb_temp_storage_DEFAULT-0.tmp",
            "duckdb_temp_block-4611686018427388160.block",
        ] {
            std::fs::write(spill.join(name), b"spilled").expect("spill file");
        }
        made.snapshot_file()
            .expect("snapshot")
            .keep()
            .expect("keep");
        let (_, export) = made.export_file().expect("export").keep().expect("keep");
        let name = export.file_name().expect("name").to_str().expect("utf-8");
        std::fs::write(scratch.join(format!("tmp_{name}")), b"half a copy").expect("copy");
        made.stage_file().expect("stage").keep().expect("keep");
        spill
    }

    #[cfg(unix)]
    #[test]
    fn scratch_open_makes_a_missing_directory_private() {
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("absent/parents/data.query-scratch");
        let lock = ScratchLock::open(&path).expect("open");
        assert_eq!(lock.scratch().path(), path);
        assert_eq!(mode_of(&path), 0o700);
        assert_eq!(lock.reclaimed(), Reclaimed::default());
        // A trailing `/` names the same directory.
        drop(lock);
        let again = ScratchLock::open(&parent.path().join("absent/parents/data.query-scratch/"))
            .expect("reopen");
        assert_eq!(again.scratch().path(), path);
    }

    /// ADR-0052's close: a directory trawld owns loses its group and other
    /// bits through the held handle, and keeps its setgid bit.
    #[cfg(unix)]
    #[test]
    fn scratch_open_closes_an_open_directory() {
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("scratch");
        std::fs::create_dir(&path).expect("mkdir");
        set_mode(&path, 0o2755);
        let _lock = ScratchLock::open(&path).expect("open");
        assert_eq!(mode_of(&path), 0o2700);
    }

    /// `flock` belongs to the open file description, so a second open in
    /// this process is refused as one in another trawld is, and touches
    /// nothing; dropping the first frees the lock.
    #[cfg(unix)]
    #[test]
    fn scratch_lock_refuses_a_second_open_until_dropped() {
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("scratch");
        let first = ScratchLock::open(&path).expect("first");
        plant_stale(&path);
        let before = tree(&path);
        let error = ScratchLock::open(&path).expect_err("held");
        assert!(matches!(error, ScratchError::Locked { .. }), "{error}");
        let message = error.to_string();
        assert!(message.contains("locked by another trawld"), "{message}");
        assert!(message.contains("[server] query_scratch_dir"), "{message}");
        assert_eq!(
            tree(&path),
            before,
            "the refused open changed the directory"
        );
        drop(first);
        let second = ScratchLock::open(&path).expect("free once dropped");
        assert_eq!(second.reclaimed().spill_dirs, 1);
    }

    /// Hold `path` as the results root, for a trawld running as this test.
    #[cfg(unix)]
    fn hold_results(path: &Path, roots: &[OwnedRoot]) -> Result<HeldRoot, ScratchError> {
        hold_apart_from(
            path,
            roots,
            rustix::process::geteuid().as_raw(),
            RootRole::RESULTS,
        )
    }

    /// A refusal of the results root names it and the setting that moves
    /// it.
    #[cfg(unix)]
    fn assert_names_the_results_root(error: &ScratchError) {
        let message = error.to_string();
        assert!(message.contains("the results root "), "{message}");
        assert!(message.contains("[server] results_dir"), "{message}");
    }

    /// ADR-0057: the results root is held as the scratch directory is,
    /// made 0700 when missing and closed through its handle when open, but
    /// what it holds outlives the hold: names the scratch preflight would
    /// refuse and names it would reclaim both stay.
    #[cfg(unix)]
    #[test]
    fn a_held_root_keeps_what_it_holds() {
        let parent = tempfile::tempdir().expect("parent");
        let made = parent.path().join("absent/data.results");
        let held = hold_results(&made, &[]).expect("hold a missing root");
        assert_eq!(held.path(), made);
        assert_eq!(mode_of(&made), 0o700);
        assert!(held.dir().metadata().expect("stat").is_dir());
        drop(held);

        let path = parent.path().join("data.results");
        std::fs::create_dir(&path).expect("mkdir");
        plant_stale(&path);
        std::fs::write(
            path.join("0123456789abcdef0123456789abcdef.parquet"),
            b"result",
        )
        .expect("result");
        std::fs::write(path.join(".stage-a1B2c3.parquet"), b"staged").expect("stage");
        std::fs::create_dir(path.join("later")).expect("subdir");
        set_mode(&path, 0o2755);
        let before = tree(&path);

        let held = hold_results(&path.join(""), &[]).expect("hold");
        assert_eq!(held.path(), path, "a trailing `/` names the same directory");
        assert_eq!(tree(&path), before, "the hold removed or changed an entry");
        assert_eq!(mode_of(&path), 0o2700, "the hold closes the root");
    }

    /// The lock is the scratch directory's lock: a second hold of a held
    /// root refuses, in this process as in another, and so does opening it
    /// as the scratch directory, before anything in it is removed.
    #[cfg(unix)]
    #[test]
    fn a_second_hold_refuses_until_the_first_drops() {
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("data.results");
        let first = hold_results(&path, &[]).expect("first");
        plant_stale(&path);
        let before = tree(&path);

        let error = hold_results(&path, &[]).expect_err("held");
        assert!(
            matches!(error, ScratchError::Locked { role, .. } if role == RootRole::RESULTS),
            "{error}"
        );
        assert_names_the_results_root(&error);
        assert!(error.to_string().contains("locked by another trawld"));

        let error = ScratchLock::open(&path).expect_err("held as the results root");
        assert!(matches!(error, ScratchError::Locked { .. }), "{error}");
        assert_eq!(tree(&path), before, "a refused open removed an entry");

        drop(first);
        let again = hold_results(&path, &[]).expect("free once dropped");
        assert_eq!(tree(&path), before);
        drop(again);
    }

    /// The results root is kept apart from the scratch directory and the
    /// data root by device and inode, and the refusal names the results
    /// root and its setting.
    #[cfg(unix)]
    #[test]
    fn a_held_root_refuses_an_owned_root_by_its_role() {
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let data = base.join("data");
        let scratch = base.join("data.query-scratch");
        std::fs::create_dir(&data).expect("data");
        std::fs::create_dir(&scratch).expect("scratch");
        std::os::unix::fs::symlink(&scratch, base.join("alias")).expect("alias");
        std::os::unix::fs::symlink(base, base.join("up")).expect("up");
        let roots = [
            OwnedRoot {
                what: "the data root ([data] path)",
                path: data.clone(),
            },
            OwnedRoot {
                what: "the query scratch directory ([server] query_scratch_dir)",
                path: scratch.clone(),
            },
        ];
        let cases = [
            (base.join("alias/results"), Overlap::Inside, roots[1].what),
            (
                base.join("up/data.query-scratch"),
                Overlap::Is,
                roots[1].what,
            ),
            (data.join("results"), Overlap::Inside, roots[0].what),
            (base.to_owned(), Overlap::Above, roots[0].what),
        ];
        for (path, overlap, what) in cases {
            let before = tree(base);
            let error = hold_results(&path, &roots).expect_err("an overlap");
            assert!(
                matches!(&error, ScratchError::Overlap { role, overlap: o, what: w, .. }
                    if *role == RootRole::RESULTS && *o == overlap && *w == what),
                "{}: {error}",
                path.display()
            );
            assert_names_the_results_root(&error);
            assert!(
                error.to_string().starts_with(&format!(
                    "the results root {} {overlap} {what}",
                    path.display()
                )),
                "{error}"
            );
            assert_eq!(
                tree(base),
                before,
                "{}: the refusal changed the tree",
                path.display()
            );
        }
        hold_results(&base.join("data.results"), &roots).expect("a sibling holds");
    }

    /// The results root's final component is held no-follow: a symlink
    /// refuses, the link's target is left as it was, and a file, a path
    /// without a final name and another owner refuse too. Each refusal
    /// names the results root.
    #[cfg(unix)]
    #[test]
    fn a_held_root_refuses_a_symlink_and_names_its_role() {
        let euid = rustix::process::geteuid().as_raw();
        let parent = tempfile::tempdir().expect("parent");
        let target = parent.path().join("target");
        std::fs::create_dir(&target).expect("mkdir");
        set_mode(&target, 0o755);
        let link = parent.path().join("data.results");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        for path in [link.clone(), link.join("")] {
            let error = hold_results(&path, &[]).expect_err("a symlink");
            assert!(
                matches!(error, ScratchError::Symlink { role, .. } if role == RootRole::RESULTS),
                "{error}"
            );
            assert_names_the_results_root(&error);
        }
        assert_eq!(mode_of(&target), 0o755, "the link was followed");

        let file = parent.path().join("file");
        std::fs::write(&file, b"not a directory").expect("file");
        let error = hold_results(&file, &[]).expect_err("a file");
        assert!(
            matches!(error, ScratchError::NotDirectory { .. }),
            "{error}"
        );
        assert_names_the_results_root(&error);

        let error = hold_results(Path::new("results/.."), &[]).expect_err("no final name");
        assert!(matches!(error, ScratchError::BadPath { .. }), "{error}");
        assert_names_the_results_root(&error);

        let error = hold_apart_from(&target, &[], euid.wrapping_add(1), RootRole::RESULTS)
            .expect_err("another owner");
        assert!(
            matches!(error, ScratchError::ForeignOwner { .. }),
            "{error}"
        );
        assert_names_the_results_root(&error);
        assert_eq!(mode_of(&target), 0o755, "a foreign directory was changed");
    }

    #[cfg(unix)]
    #[test]
    fn scratch_open_reclaims_what_a_killed_trawld_left() {
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("scratch");
        std::fs::create_dir(&path).expect("mkdir");
        let spill = plant_stale(&path);
        plant_stale(&path);
        assert!(spill.is_dir());

        let lock = ScratchLock::open(&path).expect("open");
        assert_eq!(
            lock.reclaimed(),
            Reclaimed {
                spill_dirs: 2,
                snapshots: 2,
                exports: 4,
                stages: 2,
            }
        );
        assert_eq!(tree(&path), vec![(path.clone(), "dir".to_owned())]);
    }

    #[cfg(unix)]
    #[test]
    fn scratch_open_refuses_a_symlink_or_a_non_directory() {
        let parent = tempfile::tempdir().expect("parent");
        let target = parent.path().join("target");
        std::fs::create_dir(&target).expect("mkdir");
        plant_stale(&target);
        set_mode(&target, 0o755);
        let before = tree(&target);
        let link = parent.path().join("scratch");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        for path in [link.clone(), link.join("")] {
            let error = ScratchLock::open(&path).expect_err("a symlink");
            assert!(matches!(error, ScratchError::Symlink { .. }), "{error}");
        }
        assert_eq!(tree(&target), before, "the link was followed");
        assert_eq!(mode_of(&target), 0o755);

        let file = parent.path().join("file");
        std::fs::write(&file, b"not a directory").expect("file");
        let error = ScratchLock::open(&file).expect_err("a file");
        assert!(
            matches!(error, ScratchError::NotDirectory { .. }),
            "{error}"
        );
        assert_eq!(std::fs::read(&file).expect("read"), b"not a directory");

        for path in ["", "/", "scratch/.."] {
            let error = ScratchLock::open(Path::new(path)).expect_err("no final name");
            assert!(
                matches!(error, ScratchError::BadPath { .. }),
                "{path:?}: {error}"
            );
        }
    }

    /// The roots trawld derives from a data root and a WAL directory, as
    /// `trawl-server` lists them, with short names for the assertions.
    #[cfg(unix)]
    fn owned_roots(data: &Path, wal: &Path) -> Vec<OwnedRoot> {
        let sibling = |suffix: &str| {
            let mut name = data.file_name().expect("a data root name").to_owned();
            name.push(suffix);
            data.with_file_name(name)
        };
        vec![
            OwnedRoot {
                what: "data root",
                path: data.to_owned(),
            },
            OwnedRoot {
                what: "WAL directory",
                path: wal.to_owned(),
            },
            OwnedRoot {
                what: "repin shadow root",
                path: sibling(".repin-next"),
            },
            OwnedRoot {
                what: "repin aside root",
                path: sibling(".repin-aside"),
            },
        ]
    }

    /// `path` spelled relative to the working directory, through `..` up to
    /// `/`.
    #[cfg(unix)]
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().expect("cwd");
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.join(path.strip_prefix("/").expect("an absolute path"))
    }

    /// #304 F1: the scratch directory is judged against the roots by device
    /// and inode, so no spelling hides an overlap: a symlink in the
    /// scratch path or a root's path, a `..`, or a root spelled relative to
    /// the working directory. Each refusal changes nothing, a directory
    /// that would be made inside a root is not made, and a root that does
    /// not exist yet is refused below the scratch directory.
    #[cfg(unix)]
    #[test]
    fn scratch_open_refuses_an_owned_root_however_it_is_spelled() {
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let data = base.join("data");
        let wal = base.join("wal");
        let hour = data.join("prod/2026-10-17/00");
        std::fs::create_dir_all(&hour).expect("hour");
        std::fs::create_dir(&wal).expect("wal");
        std::fs::write(hour.join("trawl-export-abc123.parquet"), b"corpus").expect("corpus");
        std::fs::create_dir(base.join("s")).expect("s");
        std::fs::create_dir(base.join("s/data")).expect("s/data");
        std::fs::create_dir(base.join("s2")).expect("s2");
        std::os::unix::fs::symlink(&data, base.join("alias")).expect("alias");
        std::os::unix::fs::symlink(base, base.join("up")).expect("up");
        let roots = owned_roots(&data, &wal);

        let cases: [(&str, PathBuf, Vec<OwnedRoot>, Overlap, &str); 8] = [
            (
                "an hour directory through a link to the data root",
                base.join("alias/prod/2026-10-17/00"),
                roots.clone(),
                Overlap::Inside,
                "data root",
            ),
            (
                "the data root through a link above it",
                base.join("up/data"),
                roots.clone(),
                Overlap::Is,
                "data root",
            ),
            (
                "the WAL directory through a link above it",
                base.join("up/wal"),
                roots.clone(),
                Overlap::Is,
                "WAL directory",
            ),
            (
                "the data root's hour directory through `..` out of the WAL directory",
                base.join("wal/../data/prod/2026-10-17/00"),
                roots.clone(),
                Overlap::Inside,
                "data root",
            ),
            (
                "an hour directory below a data root spelled relative to the cwd",
                base.join("alias/prod/2026-10-17/00"),
                owned_roots(&relative_to_cwd(&data), &wal),
                Overlap::Inside,
                "data root",
            ),
            (
                "a directory that holds a data root spelled through a link",
                base.join("s"),
                owned_roots(&base.join("up/s/data"), &wal),
                Overlap::Above,
                "data root",
            ),
            (
                "a directory below which a missing data root would be made",
                base.join("s2"),
                owned_roots(&base.join("up/s2/data"), &wal),
                Overlap::Above,
                "data root",
            ),
            (
                "a missing directory that would be made inside the data root",
                base.join("alias/newenv/x"),
                roots.clone(),
                Overlap::Inside,
                "data root",
            ),
        ];
        for (case, scratch, roots, overlap, what) in cases {
            let before = tree(base);
            let error = ScratchLock::open_apart_from(&scratch, &roots).expect_err(case);
            assert!(
                matches!(&error, ScratchError::Overlap { overlap: o, what: w, .. } if *o == overlap && *w == what),
                "{case}: {error}"
            );
            let message = error.to_string();
            for needle in [
                format!("{} {overlap} {what}", scratch.display()),
                "[server] query_scratch_dir".to_owned(),
            ] {
                assert!(
                    message.contains(&needle),
                    "{case}: {needle:?} missing from {message}"
                );
            }
            assert_eq!(
                tree(base),
                before,
                "{case}: the refused open changed the tree"
            );
        }
        assert!(!data.join("newenv").exists());
    }

    /// Siblings and lookalikes beside the roots open, and the repin roots
    /// beside the data root, which do not exist, refuse nothing.
    #[cfg(unix)]
    #[test]
    fn scratch_open_accepts_the_roots_siblings() {
        let parent = tempfile::tempdir().expect("parent");
        let data = parent.path().join("data");
        let wal = data.join("wal");
        std::fs::create_dir_all(&wal).expect("wal");
        let roots = owned_roots(&data, &wal);
        for scratch in ["data.query-scratch", "dat", "data-scratch", "elsewhere"] {
            let scratch = parent.path().join(scratch);
            ScratchLock::open_apart_from(&scratch, &roots)
                .unwrap_or_else(|e| panic!("{}: {e}", scratch.display()));
        }
    }

    /// A directory another user owns refuses the start before anything
    /// changes, even one trawld could tighten; so does an entry inside
    /// another user owns. An unprivileged test cannot own either as
    /// another user, so the open runs as a uid that does not own them.
    #[cfg(unix)]
    #[test]
    fn scratch_open_refuses_another_owner() {
        let euid = rustix::process::geteuid().as_raw();
        let other = euid.wrapping_add(1);
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("scratch");
        std::fs::create_dir(&path).expect("mkdir");
        plant_stale(&path);
        set_mode(&path, 0o755);
        let before = tree(&path);

        let error = ScratchLock::open_as(&path, &[], other).expect_err("foreign");
        assert!(
            matches!(error, ScratchError::ForeignOwner { owner, euid: e, .. } if owner == euid && e == other),
            "{error}"
        );
        assert_eq!(mode_of(&path), 0o755, "a foreign directory was changed");
        assert_eq!(tree(&path), before);

        let handle = lock::hold(&path, RootRole::QUERY_SCRATCH).expect("hold");
        let Err(error) = lock::preflight(&path, &handle, other) else {
            panic!("an entry another uid owns passed the preflight");
        };
        let message = error.to_string();
        assert!(
            message.contains(&format!("is owned by uid {euid}, not uid {other}")),
            "{message}"
        );
        assert_eq!(tree(&path), before);
    }

    /// #304 F4: a kernel before Linux 5.8 names no mount, so a bind mount of
    /// a directory on the scratch directory's own filesystem looks like any
    /// directory there. With the mount unknown on both sides, the preflight
    /// refuses the first entry before anything is removed, rather than
    /// reading two unknown mounts as one. An empty directory has nothing to
    /// cross and passes.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn scratch_preflight_refuses_entries_when_the_kernel_names_no_mount() {
        let euid = rustix::process::geteuid().as_raw();
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("scratch");
        std::fs::create_dir(&path).expect("mkdir");
        set_mode(&path, 0o700);
        let handle = lock::hold(&path, RootRole::QUERY_SCRATCH).expect("hold");
        let empty = lock::preflight_seen(&path, &handle, euid, lock::EntryStat::without_mount)
            .expect("an empty directory");
        assert!(empty.is_empty());

        plant_stale(&path);
        let before = tree(&path);
        let handle = lock::hold(&path, RootRole::QUERY_SCRATCH).expect("hold");
        let Err(error) = lock::preflight_seen(&path, &handle, euid, lock::EntryStat::without_mount)
        else {
            panic!("an entry in an unknown mount passed the preflight");
        };
        assert!(
            matches!(error, ScratchError::ForeignEntry { .. }),
            "{error}"
        );
        let message = error.to_string();
        for needle in [
            "this kernel does not name mounts",
            "has removed nothing",
            "[server] query_scratch_dir",
        ] {
            assert!(
                message.contains(needle),
                "{needle:?} missing from {message}"
            );
        }
        assert_eq!(tree(&path), before);
    }

    /// #304 H3: Linux before 4.11 has no `statx` and answers `ENOSYS`. The
    /// open then reads a plain `stat`, which names no mount, so an empty
    /// scratch directory still opens and one with entries refuses, as F4
    /// rules for a kernel that does not name mounts.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn scratch_open_without_statx_opens_only_an_empty_directory() {
        let parent = tempfile::tempdir().expect("parent");
        let path = parent.path().join("scratch");
        lock::STATX_ANSWERS_NOSYS.set(true);
        let empty = ScratchLock::open(&path).expect("an empty directory opens without statx");
        assert_eq!(empty.reclaimed(), Reclaimed::default());
        drop(empty);

        plant_stale(&path);
        let before = tree(&path);
        let Err(error) = ScratchLock::open(&path) else {
            panic!("an entry passed the open on a kernel without statx");
        };
        assert!(
            matches!(error, ScratchError::ForeignEntry { .. }),
            "{error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("this kernel does not name mounts"),
            "{message}"
        );
        assert_eq!(tree(&path), before);
    }

    /// Plants one entry trawld does not make, given the scratch directory,
    /// a stale spill directory in it and a file outside it, and returns
    /// the entry.
    #[cfg(unix)]
    type Plant = fn(&Path, &Path, &Path) -> PathBuf;

    /// Each entry the preflight refuses: what it is, how to plant it, and
    /// the reason the refusal gives.
    #[cfg(unix)]
    fn foreign_entries() -> [(&'static str, Plant, &'static str); 10] {
        [
            (
                "an unknown file",
                |scratch, _, _| {
                    let entry = scratch.join("notes.txt");
                    std::fs::write(&entry, b"mine").expect("write");
                    entry
                },
                "is not an entry trawld makes",
            ),
            (
                "a symlink with a snapshot's name",
                |scratch, _, victim| {
                    let entry = scratch.join("trawl-snapshot-abc123.ndjson");
                    std::os::unix::fs::symlink(victim, &entry).expect("symlink");
                    entry
                },
                "is a symbolic link",
            ),
            (
                "a symlink with a spill directory's name",
                |scratch, _, victim| {
                    let entry = scratch.join("trawl-spill-abc123");
                    std::os::unix::fs::symlink(victim.parent().expect("dir"), &entry)
                        .expect("symlink");
                    entry
                },
                "is a symbolic link",
            ),
            (
                "a symlink inside a spill directory",
                |_, spill, victim| {
                    let entry = spill.join("duckdb_temp_block-7.block");
                    std::os::unix::fs::symlink(victim, &entry).expect("symlink");
                    entry
                },
                "is a symbolic link",
            ),
            (
                "a directory inside a spill directory",
                |_, spill, _| {
                    let entry = spill.join("duckdb_temp_storage_DEFAULT-9.tmp");
                    std::fs::create_dir(&entry).expect("mkdir");
                    entry
                },
                "is not a spill file DuckDB makes",
            ),
            (
                "an unknown file inside a spill directory",
                |_, spill, _| {
                    let entry = spill.join("core");
                    std::fs::write(&entry, b"dump").expect("write");
                    entry
                },
                "is not a spill file DuckDB makes",
            ),
            (
                "a spill-like file of a size class DuckDB does not have",
                |_, spill, _| {
                    let entry = spill.join("duckdb_temp_storage_NOTTRAWLDS-0.tmp");
                    std::fs::write(&entry, b"not DuckDB's").expect("write");
                    entry
                },
                "is not a spill file DuckDB makes",
            ),
            (
                "a spill-like file with an index wider than DuckDB's",
                |_, spill, _| {
                    let entry = spill.join("duckdb_temp_storage_DEFAULT-18446744073709551616.tmp");
                    std::fs::write(&entry, b"not DuckDB's").expect("write");
                    entry
                },
                "is not a spill file DuckDB makes",
            ),
            (
                "a directory with an export's name",
                |scratch, _, _| {
                    let entry = scratch.join("trawl-export-abc123.parquet");
                    std::fs::create_dir(&entry).expect("mkdir");
                    entry
                },
                "is not an entry trawld makes",
            ),
            (
                "a symlink with a capture stage's name",
                |scratch, _, victim| {
                    let entry = scratch.join("trawl-stage-abc123.ndjson");
                    std::os::unix::fs::symlink(victim, &entry).expect("symlink");
                    entry
                },
                "is a symbolic link",
            ),
        ]
    }

    /// The preflight reads the whole tree before anything is removed: each
    /// entry trawld does not make refuses the start by name, the stale
    /// entries it does make beside it stay, and no link is followed.
    #[cfg(unix)]
    #[test]
    fn scratch_preflight_refuses_and_removes_nothing() {
        let outside = tempfile::tempdir().expect("outside");
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"not trawld's").expect("victim");
        for (case, plant, reason) in foreign_entries() {
            let parent = tempfile::tempdir().expect("parent");
            let path = parent.path().join("scratch");
            std::fs::create_dir(&path).expect("mkdir");
            let spill = plant_stale(&path);
            let entry = plant(&path, &spill, &victim);
            let before = tree(&path);

            let error = ScratchLock::open(&path).expect_err(case);
            assert!(
                matches!(&error, ScratchError::ForeignEntry { entry: named, .. } if *named == entry),
                "{case}: {error}"
            );
            let message = error.to_string();
            for needle in [reason, "has removed nothing", "[server] query_scratch_dir"] {
                assert!(
                    message.contains(needle),
                    "{case}: {needle:?} missing from {message}"
                );
            }
            assert_eq!(
                tree(&path),
                before,
                "{case}: the refused open changed the tree"
            );
            assert_eq!(
                std::fs::read(&victim).expect("victim"),
                b"not trawld's",
                "{case}"
            );
            // The refusal released the lock with the handle.
            std::fs::remove_dir_all(&path).expect("clear");
            ScratchLock::open(&path).expect("free after a refusal");
        }
    }
}
