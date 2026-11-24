// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The query scratch directory (ADR-0056), and the hold every directory
//! trawld keeps apart from its storage roots shares with it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod mounts;

pub const SPILL_DIR_PREFIX: &str = "trawl-spill-";

pub const SNAPSHOT_FILE_PREFIX: &str = "trawl-snapshot-";
pub const SNAPSHOT_FILE_SUFFIX: &str = ".ndjson";

pub const EXPORT_FILE_PREFIX: &str = "trawl-export-";
pub const EXPORT_FILE_SUFFIX: &str = ".parquet";

pub const STAGE_FILE_PREFIX: &str = "trawl-stage-";
pub const STAGE_FILE_SUFFIX: &str = ".ndjson";

/// The prefix `DuckDB` 1.5.5's `COPY ... TO` gives the file it writes before
/// renaming it onto the target (`use_tmp_file`, on by default for a local
/// file; `plan_copy_to_file.cpp`). An export killed mid-copy leaves one
/// beside its staging file.
const COPY_TMP_PREFIX: &str = "tmp_";

const RANDOM_LEN: usize = 6;

pub(crate) const SCRATCH_SETTING: &str = "[server] query_scratch_dir";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootRole {
    pub noun: &'static str,
    pub setting: &'static str,
}

impl RootRole {
    pub const QUERY_SCRATCH: Self = Self {
        noun: "query scratch directory",
        setting: SCRATCH_SETTING,
    };

    pub const RESULTS: Self = Self {
        noun: "results root",
        setting: "[server] results_dir",
    };
}

pub(crate) const TMPDIR_SETTING: &str = "TMPDIR";

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

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn private_spill_dir(&self) -> std::io::Result<tempfile::TempDir> {
        private_dir_in(&self.path, SCRATCH_SETTING, |_| Ok(()))
    }

    pub fn snapshot_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        named_file_in(&self.path, SNAPSHOT_FILE_PREFIX, SNAPSHOT_FILE_SUFFIX)
    }

    pub fn export_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        named_file_in(&self.path, EXPORT_FILE_PREFIX, EXPORT_FILE_SUFFIX)
    }

    pub fn stage_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        stage_file_in(&self.path)
    }
}

pub(crate) fn stage_file_in(dir: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    named_file_in(dir, STAGE_FILE_PREFIX, STAGE_FILE_SUFFIX)
}

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

fn is_made_name(name: &OsStr, prefix: &str, suffix: &str) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(prefix))
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(|random| {
            random.len() == RANDOM_LEN && random.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

#[must_use]
pub fn is_spill_dir_name(name: &OsStr) -> bool {
    is_made_name(name, SPILL_DIR_PREFIX, "")
}

#[must_use]
pub fn is_snapshot_file_name(name: &OsStr) -> bool {
    is_made_name(name, SNAPSHOT_FILE_PREFIX, SNAPSHOT_FILE_SUFFIX)
}

#[must_use]
pub fn is_export_file_name(name: &OsStr) -> bool {
    let name = name
        .to_str()
        .map(|name| name.strip_prefix(COPY_TMP_PREFIX).unwrap_or(name));
    name.is_some_and(|name| is_made_name(OsStr::new(name), EXPORT_FILE_PREFIX, EXPORT_FILE_SUFFIX))
}

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
/// The grouped block files are `duckdb_temp_storage_<size>-<n>.tmp`
/// (`temporary_file_manager.cpp`, `CreateTemporaryFileName`). The files
/// of buffers larger than a block are `duckdb_temp_block-<id>.block`
/// (`standard_buffer_manager.cpp`, `GetTemporaryPath`). A `DuckDB`
/// upgrade has to re-check both shapes, the classes and the floor.
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
/// check it.
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

#[cfg(unix)]
pub(crate) fn check_private(path: &Path, euid: u32, setting: &str) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    use rustix::fs::{Mode, OFlags};
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

#[derive(Debug, Clone)]
pub struct OwnedRoot {
    pub what: &'static str,
    /// Its configured path. It need not exist yet, but it must carry no
    /// `..` component: a `..` after a missing component would name a
    /// directory other than the one trawld makes (`<base>/missing/../x` is
    /// placed at `<base>` but made at `<base>/x`).
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlap {
    Is,
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

#[derive(Debug, thiserror::Error)]
pub enum ProofFault {
    #[error("this kernel does not name mounts (Linux 5.8 and later do)")]
    NoMountId,
    #[error("cannot read /proc/self/mountinfo: {0}")]
    TableUnreadable(#[source] std::io::Error),
    #[error("/proc/self/mountinfo is not on procfs")]
    TableNotProcfs,
    #[error("/proc/self/mountinfo is larger than 64 MiB")]
    TableTooLarge,
    #[error("/proc/self/mountinfo has no record of mount {0}")]
    NoRecord(u64),
    #[error("/proc/self/mountinfo has more than one record of mount {0}")]
    DuplicateRecord(u64),
    #[error("the record of mount {id} in /proc/self/mountinfo is malformed ({why})")]
    Malformed { id: u64, why: &'static str },
    #[error("cannot read a directory's path from /proc/self/fd: {0}")]
    PathUnreadable(#[source] std::io::Error),
    #[error(
        "the directory at {}, as /proc/self/fd names it, is not the one trawld opened",
        .path.display()
    )]
    PathMoved { path: PathBuf },
    #[error(
        "the directory {} is not below the mount point {} of its mount",
        .path.display(),
        .mount_point.display()
    )]
    OutsideMount { path: PathBuf, mount_point: PathBuf },
    #[error(
        "the components {} still to be made include . or .., which cannot be placed before \
         they exist",
        .tail.display()
    )]
    DotInTail { tail: PathBuf },
}

fn through_clause(through: Option<&Path>) -> String {
    through.map_or_else(String::new, |mount_point| {
        format!(" through the mount at {}", mount_point.display())
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    #[error(
        "the {} {} {overlap} {what} {}{}, as the filesystem resolves them; point {} at a \
         directory of its own",
        .role.noun,
        .path.display(),
        .root.display(),
        through_clause(.through.as_deref()),
        .role.setting
    )]
    Overlap {
        role: RootRole,
        path: PathBuf,
        overlap: Overlap,
        what: &'static str,
        root: PathBuf,
        /// Boxed to keep the error under clippy's `result_large_err`
        /// threshold.
        through: Option<Box<Path>>,
    },
    #[error(
        "cannot prove the {} {} apart from trawld's storage roots: {fault}; comparing \
         directories in different mounts needs Linux 5.8 or later, statx allowed and a readable \
         procfs; check {}",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    Unproven {
        role: RootRole,
        path: PathBuf,
        #[source]
        fault: ProofFault,
    },
    #[error(
        "cannot look up {what} {} to prove the {} {} apart from it: {source}; give the service \
         user search access to every directory on that path, or check {}",
        .root.display(),
        .role.noun,
        .path.display(),
        .role.setting
    )]
    RootUnreachable {
        role: RootRole,
        path: PathBuf,
        what: &'static str,
        root: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the {} {} does not end in a directory name; point {} at a directory of its own",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    BadPath { role: RootRole, path: PathBuf },
    #[error(
        "cannot {action} the {} {}: {source}; check {}",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    Io {
        role: RootRole,
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the {} {} is on a read-only filesystem; point {} at a writable directory",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    ReadOnly { role: RootRole, path: PathBuf },
    #[error(
        "the {} {} is a symbolic link, which trawld does not follow; point {} at the directory \
         itself",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    Symlink { role: RootRole, path: PathBuf },
    #[error(
        "the {} {} is not a directory; move it aside or point {} at a directory",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    NotDirectory { role: RootRole, path: PathBuf },
    #[error(
        "the {} {} is owned by uid {owner}, not uid {euid}, the user trawld runs as; point {} at \
         a directory uid {euid} owns",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    ForeignOwner {
        role: RootRole,
        path: PathBuf,
        owner: u32,
        euid: u32,
    },
    #[error(
        "the {} {} keeps mode {mode:04o} after trawld removed its group and other bits, so \
         other users could read query data; point {} at a directory on a filesystem that keeps \
         Unix modes",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    NotPrivate {
        role: RootRole,
        path: PathBuf,
        mode: u32,
    },
    #[error(
        "the {} {} is locked by another trawld; stop that trawld, or point this one's {} at a \
         directory of its own",
        .role.noun,
        .path.display(),
        .role.setting
    )]
    Locked { role: RootRole, path: PathBuf },
    #[error(
        "the {} {} holds {}, which {reason}; trawld removes only the entries it makes and has \
         removed nothing. Move the entry out, or point {} at a directory of its own",
        .role.noun,
        .path.display(),
        .entry.display(),
        .role.setting
    )]
    ForeignEntry {
        role: RootRole,
        path: PathBuf,
        entry: PathBuf,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reclaimed {
    pub spill_dirs: usize,
    pub snapshots: usize,
    pub exports: usize,
    pub stages: usize,
}

/// The lock is an exclusive `flock` on the directory's own no-follow
/// handle, so it lives exactly as long as this value. `flock` belongs
/// to the open file description, so a second hold in the same process
/// is refused like one in another process.
#[derive(Debug)]
pub struct HeldRoot {
    path: PathBuf,
    handle: std::fs::File,
}

impl HeldRoot {
    /// The held directory's path, as configured, with a trailing `/` or `.`
    /// dropped.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reach the directory's entries through it, by `openat`, `renameat`
    /// and `unlinkat`, and `fsync` it after a rename, so no link in the
    /// configured path can redirect a call to another directory.
    #[must_use]
    pub fn dir(&self) -> &std::fs::File {
        &self.handle
    }
}

/// Nothing in it is removed: the scratch directory's clearing is
/// [`ScratchLock::open_apart_from`]'s own.
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

#[derive(Debug)]
pub struct ScratchLock {
    scratch: QueryScratch,
    _held: HeldRoot,
    reclaimed: Reclaimed,
}

impl ScratchLock {
    pub fn open(path: &Path) -> Result<Self, ScratchError> {
        Self::open_apart_from(path, &[])
    }

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

    #[must_use]
    pub fn scratch(&self) -> &QueryScratch {
        &self.scratch
    }

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
    #[cfg(target_os = "linux")]
    use super::{ProofFault, mounts};

    const SCRATCH: RootRole = RootRole::QUERY_SCRATCH;

    const GROUP_OTHER: u32 = 0o077;

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

    struct Nearest {
        /// The directory, held open: as long as it is, its mount cannot be
        /// unmounted and its mount ID cannot be given to another mount.
        fd: rustix::fd::OwnedFd,
        missing: PathBuf,
    }

    /// Only a component that does not exist yet (`ENOENT`) or is not a
    /// directory (`ENOTDIR`) moves the lookup to the parent. Any other
    /// failure ends it with that error: such a path may name a directory
    /// that exists, and placing it by a directory above would miss what
    /// sits inside it.
    fn nearest(path: &Path) -> io::Result<Nearest> {
        let mut absent = None;
        for ancestor in path.ancestors() {
            let dir = if ancestor.as_os_str().is_empty() {
                Path::new(".")
            } else {
                ancestor
            };
            match rustix::fs::open(dir, LOOKUP_FLAGS, Mode::empty()) {
                Ok(fd) => {
                    let missing = path
                        .strip_prefix(ancestor)
                        .expect("an ancestor is a prefix of its path")
                        .to_owned();
                    return Ok(Nearest { fd, missing });
                }
                Err(errno @ (Errno::NOENT | Errno::NOTDIR)) => absent = Some(errno),
                Err(errno) => return Err(errno.into()),
            }
        }
        Err(absent.expect("a path is its own first ancestor").into())
    }

    struct Located {
        at: Option<Identity>,
        /// The directories above it, or, for a root that is not there yet,
        /// the nearest one on its path that is and those above that: where
        /// trawld would make it.
        above: Vec<Identity>,
        /// The directory the lineage was read from, kept open until the
        /// comparison is over, with the components below it that could not
        /// be opened.
        #[cfg_attr(
            not(target_os = "linux"),
            allow(dead_code, reason = "only the Linux mount comparison reads it")
        )]
        nearest: Nearest,
    }

    fn locate(nearest: Nearest) -> io::Result<Located> {
        let mut above = lineage(nearest.fd.as_fd())?;
        let at = nearest
            .missing
            .as_os_str()
            .is_empty()
            .then(|| above.remove(0));
        Ok(Located { at, above, nearest })
    }

    /// Without `here`, `above` is where the directory would be made, which
    /// can only land inside a root.
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

    struct Held<'a> {
        /// The directory's handle, open for the whole comparison.
        #[cfg_attr(
            not(target_os = "linux"),
            allow(dead_code, reason = "only the Linux mount comparison reads it")
        )]
        fd: BorrowedFd<'a>,
        here: Option<Identity>,
        above: &'a [Identity],
        #[cfg_attr(
            not(target_os = "linux"),
            allow(dead_code, reason = "only the Linux mount comparison reads it")
        )]
        tail: &'a Path,
    }

    /// Every root's handle stays open until both are done.
    fn refuse_roots(
        path: &Path,
        held: &Held<'_>,
        roots: &[OwnedRoot],
        role: RootRole,
    ) -> Result<(), ScratchError> {
        let mut located = Vec::with_capacity(roots.len());
        for root in roots {
            let nearest = nearest(&root.path).map_err(|source| ScratchError::RootUnreachable {
                role,
                path: path.to_owned(),
                what: root.what,
                root: root.path.clone(),
                source,
            })?;
            let at = locate(nearest).map_err(|source| ScratchError::Io {
                role,
                action: "compare trawld's storage roots with",
                path: path.to_owned(),
                source,
            })?;
            if let Some(overlap) = overlap(held.here, held.above, &at) {
                return Err(ScratchError::Overlap {
                    role,
                    path: path.to_owned(),
                    overlap,
                    what: root.what,
                    root: root.path.clone(),
                    through: None,
                });
            }
            located.push(at);
        }
        #[cfg(target_os = "linux")]
        refuse_aliases(path, held, roots, &located, role)?;
        // The roots' handles close only here, after the mount comparison.
        drop(located);
        Ok(())
    }

    #[cfg(all(test, target_os = "linux"))]
    thread_local! {
        /// Mount IDs to answer for the listed directories, by identity, in
        /// place of the kernel's, so a plain directory can stand in another
        /// mount and reach the mount table without a mount namespace.
        pub(super) static MOUNT_OVERRIDES: std::cell::RefCell<Vec<(Identity, u64)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    #[cfg(target_os = "linux")]
    pub(super) fn mount_of(fd: BorrowedFd<'_>) -> io::Result<Option<u64>> {
        let mount = stat_handle(fd)?.placement.mount;
        #[cfg(test)]
        {
            let id = identity(fd)?;
            let over = MOUNT_OVERRIDES.with_borrow(|overrides| {
                overrides
                    .iter()
                    .find(|(listed, _)| *listed == id)
                    .map(|(_, mount)| *mount)
            });
            if over.is_some() {
                return Ok(over);
            }
        }
        Ok(mount)
    }

    /// Its path is read from `/proc/self/fd`, then that path is opened and
    /// its identity compared with `fd`'s, so a path the kernel marks
    /// deleted, one renamed since, or one spelled from outside this
    /// process's root cannot stand for the directory.
    #[cfg(target_os = "linux")]
    pub(super) fn anchor(
        fd: BorrowedFd<'_>,
        mount: u64,
        tail: &Path,
    ) -> Result<mounts::Anchor, ProofFault> {
        use std::os::fd::AsRawFd as _;
        if !tail
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(ProofFault::DotInTail {
                tail: tail.to_owned(),
            });
        }
        let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            .map_err(ProofFault::PathUnreadable)?;
        let moved = || ProofFault::PathMoved { path: path.clone() };
        let again = rustix::fs::open(&path, LOOKUP_FLAGS, Mode::empty()).map_err(|_| moved())?;
        let same = identity(again.as_fd()).map_err(ProofFault::PathUnreadable)?
            == identity(fd).map_err(ProofFault::PathUnreadable)?;
        if !same {
            return Err(moved());
        }
        Ok(mounts::Anchor {
            mount,
            path,
            tail: tail.to_owned(),
        })
    }

    #[cfg(target_os = "linux")]
    fn refuse_aliases(
        path: &Path,
        held: &Held<'_>,
        roots: &[OwnedRoot],
        located: &[Located],
        role: RootRole,
    ) -> Result<(), ScratchError> {
        let unproven = |fault| ScratchError::Unproven {
            role,
            path: path.to_owned(),
            fault,
        };
        let io = |source| ScratchError::Io {
            role,
            action: "compare trawld's storage roots with",
            path: path.to_owned(),
            source,
        };
        let mount = |fd| match mount_of(fd) {
            Ok(Some(mount)) => Ok(mount),
            Ok(None) => Err(unproven(ProofFault::NoMountId)),
            Err(e) => Err(io(e)),
        };
        let held_mount = mount(held.fd)?;
        let mut held_anchor = None;
        let mut table = None;
        for (root, located) in roots.iter().zip(located) {
            let nearest = &located.nearest;
            let root_mount = mount(nearest.fd.as_fd())?;
            if root_mount == held_mount {
                continue;
            }
            let table = match &table {
                Some(table) => table,
                None => table.insert(mounts::MountTable::read().map_err(unproven)?),
            };
            let held_anchor = match &held_anchor {
                Some(anchor) => anchor,
                None => {
                    held_anchor.insert(anchor(held.fd, held_mount, held.tail).map_err(unproven)?)
                }
            };
            let root_anchor =
                anchor(nearest.fd.as_fd(), root_mount, &nearest.missing).map_err(unproven)?;
            if let Some((overlap, through)) =
                mounts::alias(held_anchor, &root_anchor, table).map_err(unproven)?
            {
                return Err(ScratchError::Overlap {
                    role,
                    path: path.to_owned(),
                    overlap,
                    what: root.what,
                    root: root.path.clone(),
                    through: Some(through.into()),
                });
            }
        }
        Ok(())
    }

    /// A directory already at `path` is compared by the mount table as
    /// itself, not as its parent plus its name, which for a mount point
    /// names the directory the mount covers.
    fn refuse_making_inside(
        path: &Path,
        roots: &[OwnedRoot],
        role: RootRole,
    ) -> Result<(), ScratchError> {
        if roots.is_empty() {
            return Ok(());
        }
        let inspect = |source| ScratchError::Io {
            role,
            action: "inspect the parents of",
            path: path.to_owned(),
            source,
        };
        let parent = path.parent().expect("final_name keeps a directory name");
        let holder = nearest(parent).map_err(inspect)?;
        let above = lineage(holder.fd.as_fd()).map_err(inspect)?;
        let name = path.file_name().expect("final_name keeps a directory name");
        let existing = if holder.missing.as_os_str().is_empty() {
            let flags = LOOKUP_FLAGS | OFlags::NOFOLLOW;
            match rustix::fs::openat(&holder.fd, name, flags, Mode::empty()) {
                Ok(fd) => Some(fd),
                // Not there yet: placed by the parent, with `name` to make.
                // A symlink or a non-directory is placed the same way and
                // not refused here: `hold` refuses it in its own words.
                Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => None,
                Err(errno) => return Err(inspect(errno.into())),
            }
        } else {
            None
        };
        let (fd, tail) = match &existing {
            Some(dir) => (dir.as_fd(), PathBuf::new()),
            None => (holder.fd.as_fd(), holder.missing.join(name)),
        };
        let held = Held {
            fd,
            here: None,
            above: &above,
            tail: &tail,
        };
        refuse_roots(path, &held, roots, role)
    }

    pub(super) fn refuse_overlap(
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
        let held = Held {
            fd: handle.as_fd(),
            here: Some(*here),
            above,
            tail: Path::new(""),
        };
        refuse_roots(path, &held, roots, role)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Placement {
        device: u64,
        mount: Option<u64>,
    }

    impl Placement {
        /// On Linux it must name its mount, since a bind mount keeps the
        /// device. Elsewhere the platform names no mount, and the device is all
        /// there is.
        const fn names_its_mount(self) -> bool {
            !cfg!(any(target_os = "linux", target_os = "android")) || self.mount.is_some()
        }
    }

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
        pub(super) static STATX_ANSWERS_NOSYS: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    /// Linux before 4.11 has no `statx`; there a plain `stat` answers,
    /// which names no mount, and [`refusal`] refuses every entry as on a
    /// kernel before 5.8.
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn stat_entry(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<EntryStat> {
        statx(dir, name, AtFlags::empty())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn stat_handle(fd: BorrowedFd<'_>) -> io::Result<EntryStat> {
        statx(fd, c"", AtFlags::EMPTY_PATH)
    }

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

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn stat_entry(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<EntryStat> {
        Ok(from_stat(&rustix::fs::statat(
            dir,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
        )?))
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn stat_handle(fd: BorrowedFd<'_>) -> io::Result<EntryStat> {
        Ok(from_stat(&rustix::fs::fstat(fd)?))
    }

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

    pub(super) enum Stale {
        SpillDir {
            name: CString,
            dir: File,
            files: Vec<CString>,
        },
        Snapshot(CString),
        Export(CString),
        Stage(CString),
    }

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

    pub(super) fn preflight(
        path: &Path,
        root: &File,
        euid: u32,
    ) -> Result<Vec<Stale>, ScratchError> {
        preflight_seen(path, root, euid, |stat| stat)
    }

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
            "duckdb_temp_storage_NOTTRAWLDS-0.tmp",
            "duckdb_temp_storage_INVALID-0.tmp",
            "duckdb_temp_storage_S48K-0.tmp",
            "duckdb_temp_storage_S256K-0.tmp",
            "duckdb_temp_storage_S32K2-0.tmp",
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
            "duckdb_temp_block-9223372036854775808.block",
            "duckdb_temp_block-18446744073709551615.block",
            "duckdb_temp_block-1.tmp",
            "trawl-spill-abc123",
            "evil",
        ] {
            assert!(!is_spill_file_name(OsStr::new(name)), "{name}");
        }
    }

    /// The private directory is read while the sort runs (`DuckDB` removes
    /// its files when the query ends).
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
    #[allow(clippy::too_many_lines)]
    fn scratch_refusals_read_word_for_word() {
        let path = || PathBuf::from("/srv/data.query-scratch");
        let unproven = |fault| ScratchError::Unproven {
            role: RootRole::QUERY_SCRATCH,
            path: path(),
            fault,
        };
        let cases: [(ScratchError, &str); 23] = [
            (
                ScratchError::Overlap {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                    overlap: Overlap::Inside,
                    what: "the data root ([data] path)",
                    root: PathBuf::from("/srv/data"),
                    through: None,
                },
                "the query scratch directory /srv/data.query-scratch is inside the data root \
                 ([data] path) /srv/data, as the filesystem resolves them; point [server] \
                 query_scratch_dir at a directory of its own",
            ),
            (
                ScratchError::Overlap {
                    role: RootRole::RESULTS,
                    path: PathBuf::from("/srv/results"),
                    overlap: Overlap::Inside,
                    what: "the data root ([data] path)",
                    root: PathBuf::from("/srv/data"),
                    through: Some(Path::new("/srv/results").into()),
                },
                "the results root /srv/results is inside the data root ([data] path) /srv/data \
                 through the mount at /srv/results, as the filesystem resolves them; point \
                 [server] results_dir at a directory of its own",
            ),
            (
                ScratchError::RootUnreachable {
                    role: RootRole::QUERY_SCRATCH,
                    path: path(),
                    what: "the data root ([data] path)",
                    root: PathBuf::from("/srv/data"),
                    source: std::io::Error::other("boom"),
                },
                "cannot look up the data root ([data] path) /srv/data to prove the query scratch \
                 directory /srv/data.query-scratch apart from it: boom; give the service user \
                 search access to every directory on that path, or check [server] \
                 query_scratch_dir",
            ),
            (
                unproven(ProofFault::NoMountId),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: this kernel does not name mounts (Linux 5.8 and later \
                 do); comparing directories in different mounts needs Linux 5.8 or later, statx \
                 allowed and a readable procfs; check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::TableUnreadable(std::io::Error::other("boom"))),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: cannot read /proc/self/mountinfo: boom; comparing \
                 directories in different mounts needs Linux 5.8 or later, statx allowed and a \
                 readable procfs; check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::TableNotProcfs),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: /proc/self/mountinfo is not on procfs; comparing \
                 directories in different mounts needs Linux 5.8 or later, statx allowed and a \
                 readable procfs; check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::TableTooLarge),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: /proc/self/mountinfo is larger than 64 MiB; comparing \
                 directories in different mounts needs Linux 5.8 or later, statx allowed and a \
                 readable procfs; check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::NoRecord(31)),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: /proc/self/mountinfo has no record of mount 31; \
                 comparing directories in different mounts needs Linux 5.8 or later, statx \
                 allowed and a readable procfs; check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::DuplicateRecord(31)),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: /proc/self/mountinfo has more than one record of mount \
                 31; comparing directories in different mounts needs Linux 5.8 or later, statx \
                 allowed and a readable procfs; check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::Malformed {
                    id: 31,
                    why: "no lone - separates its optional fields",
                }),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: the record of mount 31 in /proc/self/mountinfo is \
                 malformed (no lone - separates its optional fields); comparing directories in \
                 different mounts needs Linux 5.8 or later, statx allowed and a readable procfs; \
                 check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::PathUnreadable(std::io::Error::other("boom"))),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: cannot read a directory's path from /proc/self/fd: \
                 boom; comparing directories in different mounts needs Linux 5.8 or later, statx \
                 allowed and a readable procfs; check [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::PathMoved {
                    path: PathBuf::from("/srv/data (deleted)"),
                }),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: the directory at /srv/data (deleted), as /proc/self/fd \
                 names it, is not the one trawld opened; comparing directories in different \
                 mounts needs Linux 5.8 or later, statx allowed and a readable procfs; check \
                 [server] query_scratch_dir",
            ),
            (
                unproven(ProofFault::OutsideMount {
                    path: PathBuf::from("/srv/data"),
                    mount_point: PathBuf::from("/mnt/data"),
                }),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: the directory /srv/data is not below the mount point \
                 /mnt/data of its mount; comparing directories in different mounts needs Linux \
                 5.8 or later, statx allowed and a readable procfs; check [server] \
                 query_scratch_dir",
            ),
            (
                unproven(ProofFault::DotInTail {
                    tail: PathBuf::from("missing/../x"),
                }),
                "cannot prove the query scratch directory /srv/data.query-scratch apart from \
                 trawld's storage roots: the components missing/../x still to be made include . \
                 or .., which cannot be placed before they exist; comparing directories in \
                 different mounts needs Linux 5.8 or later, statx allowed and a readable procfs; \
                 check [server] query_scratch_dir",
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
        drop(lock);
        let again = ScratchLock::open(&parent.path().join("absent/parents/data.query-scratch/"))
            .expect("reopen");
        assert_eq!(again.scratch().path(), path);
    }

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

    #[cfg(unix)]
    fn hold_results(path: &Path, roots: &[OwnedRoot]) -> Result<HeldRoot, ScratchError> {
        hold_apart_from(
            path,
            roots,
            rustix::process::geteuid().as_raw(),
            RootRole::RESULTS,
        )
    }

    #[cfg(unix)]
    fn assert_names_the_results_root(error: &ScratchError) {
        let message = error.to_string();
        assert!(message.contains("the results root "), "{message}");
        assert!(message.contains("[server] results_dir"), "{message}");
    }

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
        let data = parent.path().join("data");
        std::fs::create_dir(&data).expect("data");
        let roots = owned_roots(&data, &data.join("wal"));
        for roots in [&[][..], &roots[..]] {
            for path in [link.clone(), link.join("")] {
                let error = hold_results(&path, roots).expect_err("a symlink");
                assert!(
                    matches!(error, ScratchError::Symlink { role, .. } if role == RootRole::RESULTS),
                    "{error}"
                );
                assert_names_the_results_root(&error);
            }
        }
        assert_eq!(mode_of(&target), 0o755, "the link was followed");

        let file = parent.path().join("file");
        std::fs::write(&file, b"not a directory").expect("file");
        for roots in [&[][..], &roots[..]] {
            let error = hold_results(&file, roots).expect_err("a file");
            assert!(
                matches!(error, ScratchError::NotDirectory { .. }),
                "{error}"
            );
            assert_names_the_results_root(&error);
        }

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

    #[cfg(unix)]
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().expect("cwd");
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.join(path.strip_prefix("/").expect("an absolute path"))
    }

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

    /// Read as missing, the root would be placed at the nearest directory
    /// above it that opens: a results root at `hour` would hold, and a
    /// scratch directory at `hour` would be cleared.
    #[cfg(unix)]
    fn assert_lookup_refuses(
        protected: &Path,
        roots: &[OwnedRoot],
        hour: &Path,
        held: &Path,
        held_roots: &[OwnedRoot],
        errno: rustix::io::Errno,
    ) {
        let data = &roots[0].path;
        let is_errno =
            |source: &std::io::Error| source.raw_os_error() == Some(errno.raw_os_error());
        let before = tree(protected);
        let assert_unreachable = |error: &ScratchError, role: RootRole| {
            assert!(
                matches!(error, ScratchError::RootUnreachable {
                    role: r,
                    path,
                    what: "data root",
                    root,
                    source,
                } if *r == role && path == hour && root == data && is_errno(source)),
                "{error}"
            );
            let message = error.to_string();
            let start = format!(
                "cannot look up data root {} to prove the {} {} apart from it: ",
                data.display(),
                role.noun,
                hour.display()
            );
            assert!(
                message.starts_with(&start),
                "{start:?} does not start {message}"
            );
            let end = format!(
                "; give the service user search access to every directory on that path, or check \
                 {}",
                role.setting
            );
            assert!(message.ends_with(&end), "{end:?} does not end {message}");
            assert!(
                !message.contains("Linux 5.8"),
                "a lookup failure is not the mount comparison's: {message}"
            );
        };

        let error = hold_results(hour, roots).expect_err("inside a root that fails to look up");
        assert_unreachable(&error, RootRole::RESULTS);
        let error = ScratchLock::open_apart_from(hour, roots)
            .expect_err("inside a root that fails to look up");
        assert_unreachable(&error, RootRole::QUERY_SCRATCH);
        assert_eq!(tree(protected), before, "a refusal changed the data root");

        let error =
            hold_results(held, held_roots).expect_err("a held directory that fails to look up");
        assert!(
            matches!(&error, ScratchError::Io { role, action: "inspect the parents of", source, .. }
                if *role == RootRole::RESULTS && is_errno(source)),
            "{error}"
        );
        assert_names_the_results_root(&error);
        assert_eq!(tree(protected), before, "the refusal made something");
    }

    /// The data root is spelled through a link that points at itself, so
    /// its lookup fails with ELOOP for every uid, root included.
    #[cfg(unix)]
    #[test]
    fn a_root_behind_a_symlink_loop_refuses() {
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let real = base.join("real");
        let hour = real.join("prod/2026-10-17/00");
        std::fs::create_dir_all(&hour).expect("hour");
        std::fs::write(hour.join("trawl-export-abc123.parquet"), b"corpus").expect("corpus");
        let looped = base.join("loop");
        std::os::unix::fs::symlink("loop", &looped).expect("loop");
        assert_lookup_refuses(
            base,
            &owned_roots(&looped.join("data"), &base.join("wal")),
            &hour,
            &looped.join("results"),
            &owned_roots(&real, &base.join("wal")),
            rustix::io::Errno::LOOP,
        );
    }

    /// The data root is spelled through a link whose target walks `..` out
    /// of a directory this uid may not search, so its lookup fails with
    /// EACCES while the directory itself stays in reach.
    #[cfg(unix)]
    #[test]
    fn a_root_that_cannot_be_looked_up_refuses() {
        /// Gives the unsearchable directory its bits back however the test
        /// ends, so the temporary directory can be removed.
        struct Unlock<'a>(&'a Path);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
            }
        }

        if rustix::process::geteuid().is_root() {
            eprintln!(
                "skipped: root searches a directory of mode 0o000, so EACCES cannot be provoked; \
                 a_root_behind_a_symlink_loop_refuses covers the refusal"
            );
            return;
        }
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let real = base.join("real");
        let hour = real.join("prod/2026-10-17/00");
        std::fs::create_dir_all(&hour).expect("hour");
        std::fs::write(hour.join("trawl-export-abc123.parquet"), b"corpus").expect("corpus");
        let locked = base.join("locked");
        std::fs::create_dir(&locked).expect("locked");
        let data = base.join("data");
        std::os::unix::fs::symlink("locked/../real", &data).expect("data");
        set_mode(&locked, 0o000);
        let _unlock = Unlock(&locked);
        assert_lookup_refuses(
            &real,
            &owned_roots(&data, &base.join("wal")),
            &hour,
            &data.join("prod/results"),
            &owned_roots(&real, &base.join("wal")),
            rustix::io::Errno::ACCESS,
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_held_name_that_fails_to_look_up_refuses() {
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let data = base.join("data");
        std::fs::create_dir(&data).expect("data");
        let roots = owned_roots(&data, &data.join("wal"));
        let before = tree(base);
        let error = hold_results(&base.join("r".repeat(256)), &roots).expect_err("a name too long");
        let too_long = rustix::io::Errno::NAMETOOLONG.raw_os_error();
        assert!(
            matches!(&error, ScratchError::Io { role, action: "inspect the parents of", source, .. }
                if *role == RootRole::RESULTS && source.raw_os_error() == Some(too_long)),
            "{error}"
        );
        assert_names_the_results_root(&error);
        assert_eq!(tree(base), before, "the refusal made something");
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
            std::fs::remove_dir_all(&path).expect("clear");
            ScratchLock::open(&path).expect("free after a refusal");
        }
    }

    #[cfg(target_os = "linux")]
    fn assert_unproven(error: &ScratchError, needle: &str) {
        assert!(
            matches!(error, ScratchError::Unproven { role, .. } if *role == RootRole::RESULTS),
            "{error}"
        );
        let message = error.to_string();
        for needle in [
            "cannot prove the results root ",
            needle,
            "needs Linux 5.8 or later, statx allowed and a readable procfs",
            "[server] results_dir",
        ] {
            assert!(
                message.contains(needle),
                "{needle:?} missing from {message}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    struct Placed {
        mount: u64,
        filesystem: Vec<u8>,
        path: PathBuf,
        within: PathBuf,
    }

    #[cfg(target_os = "linux")]
    fn placed(dir: &Path) -> Placed {
        use std::os::fd::AsFd as _;

        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = rustix::fs::open(dir, flags, Mode::empty()).expect("open");
        let mount = lock::mount_of(fd.as_fd())
            .expect("statx")
            .expect("this kernel names mounts");
        let anchor = lock::anchor(fd.as_fd(), mount, Path::new("")).expect("anchor");
        let table = mounts::MountTable::read().expect("the kernel's table");
        let record = table.record(mount).expect("the mount's record");
        let within = mounts::within_filesystem(&anchor, &record).expect("within the mount");
        Placed {
            mount,
            filesystem: record.filesystem,
            path: anchor.path,
            within,
        }
    }

    #[cfg(target_os = "linux")]
    fn stand_in_mount(path: &Path, mount: u64) {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::metadata(path).expect("stat");
        lock::MOUNT_OVERRIDES.set(vec![((meta.dev(), meta.ino()), mount)]);
    }

    #[cfg(target_os = "linux")]
    fn mount_line(id: u64, filesystem: &[u8], root: &Path, mount_point: &Path) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt as _;
        [
            id.to_string().as_bytes(),
            b" 1 ",
            filesystem,
            b" ",
            root.as_os_str().as_bytes(),
            b" ",
            mount_point.as_os_str().as_bytes(),
            b" rw,relatime shared:1 - tmpfs tmpfs rw\n",
        ]
        .concat()
    }

    /// The kernel's own table, which every injected table starts from so
    /// the held root's mount has its record.
    #[cfg(target_os = "linux")]
    fn kernel_table() -> Vec<u8> {
        std::fs::read(mounts::MOUNTINFO).expect("read the table")
    }

    /// The mount ID a directory stands in when a test puts it in another
    /// mount: one the kernel cannot give out, since the IDs `statx` names
    /// come from an allocator of non-negative 32-bit integers.
    #[cfg(target_os = "linux")]
    const OTHER_MOUNT: u64 = 1 << 32;

    #[cfg(target_os = "linux")]
    #[test]
    fn a_hold_in_one_mount_reads_no_mount_table() {
        let parent = tempfile::tempdir().expect("parent");
        let data = parent.path().join("data");
        let wal = parent.path().join("wal");
        std::fs::create_dir(&data).expect("data");
        std::fs::create_dir(&wal).expect("wal");
        let roots = owned_roots(&data, &wal);
        mounts::set_table_source(mounts::TableSource::Unreachable);
        let held = hold_results(&parent.path().join("results"), &roots)
            .expect("siblings in one mount hold without the table");
        drop(held);
        let error = hold_results(&data.join("results"), &roots).expect_err("inside the data root");
        assert!(
            matches!(&error, ScratchError::Overlap { through: None, .. }),
            "the walk over parents refuses on its own: {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_walk_overlap_refuses_before_any_mount_is_compared() {
        let parent = tempfile::tempdir().expect("parent");
        let data = parent.path().join("data");
        let wal = parent.path().join("wal");
        std::fs::create_dir(&data).expect("data");
        std::fs::create_dir(&wal).expect("wal");
        let roots = owned_roots(&data, &wal);
        stand_in_mount(&data, OTHER_MOUNT);
        mounts::set_table_source(mounts::TableSource::Unreachable);
        let error = hold_results(&wal.join("results"), &roots).expect_err("inside the WAL");
        assert!(
            matches!(
                &error,
                ScratchError::Overlap {
                    overlap: Overlap::Inside,
                    what: "WAL directory",
                    through: None,
                    ..
                }
            ),
            "{error}"
        );
        assert!(!wal.join("results").exists(), "the refusal made the root");
        mounts::set_table_source(mounts::TableSource::Proc);
        lock::MOUNT_OVERRIDES.set(Vec::new());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_hold_refuses_when_the_kernel_names_no_mount() {
        let parent = tempfile::tempdir().expect("parent");
        let data = parent.path().join("data");
        std::fs::create_dir(&data).expect("data");
        let results = parent.path().join("results");
        let roots = owned_roots(&data, &data.join("wal"));
        lock::STATX_ANSWERS_NOSYS.set(true);
        let before = tree(parent.path());
        let error = hold_results(&results, &roots).expect_err("no mount ID");
        assert!(
            matches!(
                &error,
                ScratchError::Unproven {
                    fault: ProofFault::NoMountId,
                    ..
                }
            ),
            "{error}"
        );
        assert_unproven(
            &error,
            "this kernel does not name mounts (Linux 5.8 and later do)",
        );
        assert_eq!(tree(parent.path()), before, "the refusal made something");
        hold_results(&results, &[]).expect("nothing to compare needs no mount ID");
    }

    /// How a test stands the mount table in for the kernel's, given the
    /// test's directory and where it and the data root are placed.
    #[cfg(target_os = "linux")]
    type Stand = fn(&Path, &Placed, &Placed) -> mounts::TableSource;

    /// One way the comparison cannot be made: the case, how the table is
    /// stood in, the fault to expect, and the words the refusal gives.
    #[cfg(target_os = "linux")]
    type UnprovenCase = (&'static str, Stand, fn(&ProofFault) -> bool, &'static str);

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_hold_refuses_what_it_cannot_prove() {
        let cases: [UnprovenCase; 7] = [
            (
                "the table unreadable",
                |_, _, _| mounts::TableSource::Path(PathBuf::from("/proc/self/no-such-table")),
                |fault| matches!(fault, ProofFault::TableUnreadable(_)),
                "cannot read /proc/self/mountinfo: ",
            ),
            (
                "the table not on procfs",
                |base, _, _| {
                    let copy = base.join("mountinfo");
                    std::fs::write(&copy, kernel_table()).expect("copy");
                    mounts::TableSource::Path(copy)
                },
                |fault| matches!(fault, ProofFault::TableNotProcfs),
                "/proc/self/mountinfo is not on procfs",
            ),
            (
                "the table over 64 MiB",
                |_, _, _| mounts::TableSource::too_large(),
                |fault| matches!(fault, ProofFault::TableTooLarge),
                "/proc/self/mountinfo is larger than 64 MiB",
            ),
            (
                "no record of the root's mount",
                |_, _, _| mounts::TableSource::bytes(kernel_table()),
                |fault| matches!(fault, ProofFault::NoRecord(OTHER_MOUNT)),
                "/proc/self/mountinfo has no record of mount 4294967296",
            ),
            (
                "two records of the root's mount",
                |_, base, data| {
                    let line = mount_line(OTHER_MOUNT, &base.filesystem, &base.within, &data.path);
                    mounts::TableSource::bytes([kernel_table(), line.clone(), line].concat())
                },
                |fault| matches!(fault, ProofFault::DuplicateRecord(OTHER_MOUNT)),
                "/proc/self/mountinfo has more than one record of mount 4294967296",
            ),
            (
                "a malformed record of the root's mount",
                |_, _, _| {
                    let line =
                        b"4294967296 1 0:29 / /x rw,relatime shared:1 no separator\n".to_vec();
                    mounts::TableSource::bytes([kernel_table(), line].concat())
                },
                |fault| {
                    matches!(
                        fault,
                        ProofFault::Malformed {
                            id: OTHER_MOUNT,
                            why: "no lone - separates its optional fields"
                        }
                    )
                },
                "the record of mount 4294967296 in /proc/self/mountinfo is malformed (no lone - \
                 separates its optional fields)",
            ),
            (
                "a record whose mount point is not above the root",
                |_, base, _| {
                    let line = mount_line(
                        OTHER_MOUNT,
                        &base.filesystem,
                        &base.within,
                        Path::new("/nowhere"),
                    );
                    mounts::TableSource::bytes([kernel_table(), line].concat())
                },
                |fault| {
                    matches!(fault, ProofFault::OutsideMount { mount_point, .. }
                        if mount_point == Path::new("/nowhere"))
                },
                "is not below the mount point /nowhere of its mount",
            ),
        ];
        for (case, stand, is_fault, needle) in cases {
            let parent = tempfile::tempdir().expect("parent");
            let base = parent.path();
            let data = base.join("data");
            std::fs::create_dir(&data).expect("data");
            let roots = owned_roots(&data, &data.join("wal"));
            let results = base.join("results");
            let source = stand(base, &placed(base), &placed(&data));
            let before = tree(base);
            stand_in_mount(&data, OTHER_MOUNT);
            mounts::set_table_source(source);

            let error = hold_results(&results, &roots).expect_err(case);
            assert!(
                matches!(&error, ScratchError::Unproven { fault, .. } if is_fault(fault)),
                "{case}: {error}"
            );
            assert_unproven(&error, needle);
            assert_eq!(tree(base), before, "{case}: the refusal made something");

            mounts::set_table_source(mounts::TableSource::Proc);
            lock::MOUNT_OVERRIDES.set(Vec::new());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_bind_alias_in_another_mount_refuses_through_its_mount_point() {
        use std::os::unix::fs::MetadataExt as _;
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let base_placed = placed(base);
        let other_device = Path::new("/proc/self");
        let data_placed = placed(other_device);
        assert_ne!(
            std::fs::metadata(base).expect("stat").dev(),
            std::fs::metadata(other_device).expect("stat").dev(),
            "the data root must be on another device"
        );
        let roots = owned_roots(other_device, &base.join("wal"));
        std::fs::create_dir(base.join("existing")).expect("existing");

        let inside = mount_line(
            OTHER_MOUNT,
            &base_placed.filesystem,
            &base_placed.within,
            &data_placed.path,
        );
        for results in [
            base.join("existing"),
            base.join("results"),
            base.join("absent/results"),
        ] {
            let before = tree(base);
            stand_in_mount(other_device, OTHER_MOUNT);
            mounts::set_table_source(mounts::TableSource::bytes(
                [kernel_table(), inside.clone()].concat(),
            ));
            let error = hold_results(&results, &roots).expect_err("inside the data root");
            assert!(
                matches!(
                    &error,
                    ScratchError::Overlap { role, overlap: Overlap::Inside, what: "data root", through: Some(through), .. }
                        if *role == RootRole::RESULTS && **through == *data_placed.path
                ),
                "{}: {error}",
                results.display()
            );
            assert_names_the_results_root(&error);
            assert!(
                error.to_string().starts_with(&format!(
                    "the results root {} is inside data root {} through the mount at {}, as \
                     the filesystem resolves them",
                    results.display(),
                    other_device.display(),
                    data_placed.path.display()
                )),
                "{error}"
            );
            assert_eq!(
                tree(base),
                before,
                "{}: the refusal made something",
                results.display()
            );
        }

        let existing = base.join("existing");
        let handle = lock::hold(&existing, RootRole::RESULTS).expect("hold");
        let error = lock::refuse_overlap(&existing, &handle, &roots, RootRole::RESULTS)
            .expect_err("inside the data root, by the held handle");
        assert!(
            matches!(
                &error,
                ScratchError::Overlap { overlap: Overlap::Inside, through: Some(through), .. }
                    if **through == *data_placed.path
            ),
            "{error}"
        );
        drop(handle);

        let elsewhere = mount_line(
            OTHER_MOUNT,
            b"0:65535",
            &base_placed.within,
            &data_placed.path,
        );
        mounts::set_table_source(mounts::TableSource::bytes(
            [kernel_table(), elsewhere].concat(),
        ));
        hold_results(&base.join("results"), &roots).expect("another filesystem cannot alias");
        mounts::set_table_source(mounts::TableSource::Proc);
        lock::MOUNT_OVERRIDES.set(Vec::new());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_root_bound_from_inside_the_held_directory_refuses_as_above() {
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let base_placed = placed(base);
        let results = base.join("results");
        std::fs::create_dir(&results).expect("results");
        let data = base.join("alias");
        std::fs::create_dir(&data).expect("alias");
        let data_placed = placed(&data);
        let roots = owned_roots(&data, &base.join("wal"));
        let above = mount_line(
            OTHER_MOUNT,
            &base_placed.filesystem,
            &base_placed.within.join("results/prod"),
            &data_placed.path,
        );
        stand_in_mount(&data, OTHER_MOUNT);
        mounts::set_table_source(mounts::TableSource::bytes([kernel_table(), above].concat()));
        let before = tree(base);
        let error = hold_results(&results, &roots).expect_err("above the data root");
        assert!(
            matches!(
                &error,
                ScratchError::Overlap { overlap: Overlap::Above, through: Some(through), .. }
                    if **through == *data_placed.path
            ),
            "{error}"
        );
        assert!(
            error.to_string().starts_with(&format!(
                "the results root {} is above data root {} through the mount at {}, as the \
                 filesystem resolves them",
                results.display(),
                data.display(),
                data_placed.path.display()
            )),
            "{error}"
        );
        assert_eq!(tree(base), before, "the refusal changed the tree");
        mounts::set_table_source(mounts::TableSource::Proc);
        lock::MOUNT_OVERRIDES.set(Vec::new());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_that_cannot_be_placed_refuses() {
        use std::os::fd::AsFd as _;

        use rustix::fs::{Mode, OFlags};
        let parent = tempfile::tempdir().expect("parent");
        let base = parent.path();
        let base_placed = placed(base);
        let data = base.join("data");
        std::fs::create_dir(&data).expect("data");
        let data_placed = placed(&data);
        let roots = owned_roots(&data, &base.join("wal"));
        stand_in_mount(&data, OTHER_MOUNT);
        mounts::set_table_source(mounts::TableSource::bytes(
            [
                kernel_table(),
                mount_line(
                    OTHER_MOUNT,
                    &base_placed.filesystem,
                    &base_placed.within,
                    &data_placed.path,
                ),
            ]
            .concat(),
        ));
        let before = tree(base);
        let error =
            hold_results(&base.join("missing/../results"), &roots).expect_err("a `..` to be made");
        assert!(
            matches!(&error, ScratchError::Unproven { fault: ProofFault::DotInTail { tail }, .. }
                if tail == Path::new("missing/../results")),
            "{error}"
        );
        assert_unproven(
            &error,
            "the components missing/../results still to be made include . or ..",
        );
        assert_eq!(tree(base), before, "the refusal made something");
        mounts::set_table_source(mounts::TableSource::Proc);
        lock::MOUNT_OVERRIDES.set(Vec::new());

        let gone = base.join("gone");
        std::fs::create_dir(&gone).expect("gone");
        let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = rustix::fs::open(&gone, flags, Mode::empty()).expect("open");
        std::fs::remove_dir(&gone).expect("rmdir");
        let fault =
            lock::anchor(fd.as_fd(), base_placed.mount, Path::new("")).expect_err("removed");
        assert!(
            matches!(&fault, ProofFault::PathMoved { path } if path.to_string_lossy().ends_with("/gone (deleted)")),
            "{fault}"
        );
    }
}
