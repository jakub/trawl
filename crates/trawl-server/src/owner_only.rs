// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's storage roots are owner-only (ADR-0052).
//!
//! trawld sets its umask to 077 before anything else runs, so whatever it
//! creates under its roots is owner-only. An installation started under an
//! older trawld, or a root another tool made, can still carry group or other
//! bits. Boot closes each root to its owner before it reads or writes the
//! corpus, and refuses to start when it cannot. A refused root does not
//! stop the others closing. Closing the root is enough:
//! an older 0644 file beneath a 0700 root cannot be reached by another user,
//! so nothing beneath a root is ever chmodded. A recursive repair would race
//! the writers and follow links planted under a directory trawld can write.
//!
//! The roots are those [`protected_roots`] lists: the data root on every
//! node, the WAL directory on an ingest node unless the data root provably
//! covers it, and the repin shadow and aside siblings when they exist. Boot
//! and `trawld --doctor` walk the same list. A WAL directory is covered only
//! when [`data_root_covers_wal`] reaches it from the data root by
//! descriptor, through real directories in the data root's mount; a name
//! under the data root is not enough, since a symlink or a mount there leads
//! out of it.
//!
//! A root is opened no-follow as a directory, and its owner and mode are read
//! from that handle. The chmod goes through the same handle, so the stat and
//! the change cannot land on two different inodes. Only the final component
//! of a configured path is held this way. Its ancestors are configuration
//! the operator trusts, as ADR-0041 treats the storage roots themselves.
//!
//! A symlink as the final component refuses, and so does a WAL directory or
//! repin sibling that is not a directory; a data root that is not one is
//! the epoch gate's to refuse. So does a root that another
//! user owns, even when trawld runs as root and could change it: trawld does
//! not take over a directory it was not given, the rule `tls.rs` applies to
//! its generated directories. A root the filesystem will not change refuses,
//! and so does one whose change the filesystem accepts and then ignores,
//! as a CIFS or FUSE mount with fixed modes does.
//!
//! A repin creates its shadow and aside roots while trawld serves, after
//! boot's close. [`close_created`] closes each as soon as it exists, before
//! anything is linked, written or renamed into it, and a refusal stops the
//! repin.
//!
//! [`close`] and [`close_created`] are the only functions here that change
//! anything, and the doctor calls neither. The doctor calls [`predict`], which also reads whether a root that needs
//! closing sits on a read-only mount. That is a prediction: boot's chmod is
//! authoritative, and an immutable attribute or a security module can still
//! refuse a root the doctor expected to close.

use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags, StatVfsMountFlags};
use rustix::io::Errno;

use crate::repin::marker::{aside_root, shadow_root};

/// The group and other permission bits, which an owner-only root lacks.
const GROUP_OTHER: u32 = 0o077;

/// The permission bits of a mode, setuid, setgid and sticky included.
const PERMISSION_BITS: u32 = 0o7777;

/// Which of trawld's storage roots a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// `[data] path`.
    DataRoot,
    /// `[ingest] wal_dir`, when it lies outside the data root.
    WalDir,
    /// The repin shadow sibling of the data root, `<data>.repin-next`.
    RepinShadow,
    /// The repin aside sibling of the data root, `<data>.repin-aside`.
    RepinAside,
}

impl RootKind {
    /// Whether something other than a directory at this root is left for
    /// the epoch gate, which refuses it with its own message. Only the data
    /// root's is; any other root that is not a directory is refused here.
    #[must_use]
    pub const fn epoch_gate_judges_non_directory(self) -> bool {
        matches!(self, Self::DataRoot)
    }

    /// The setting that names this root, or that the root is derived from.
    const fn setting(self) -> &'static str {
        match self {
            Self::DataRoot | Self::RepinShadow | Self::RepinAside => "[data] path",
            Self::WalDir => "[ingest] wal_dir",
        }
    }
}

impl fmt::Display for RootKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DataRoot => "data root",
            Self::WalDir => "WAL directory",
            Self::RepinShadow => "repin shadow root",
            Self::RepinAside => "repin aside root",
        })
    }
}

/// One root trawld keeps owner-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedRoot {
    /// Which root it is.
    pub kind: RootKind,
    /// Its configured path.
    pub path: PathBuf,
}

/// The roots trawld keeps owner-only, in the order boot closes them.
///
/// The data root is always listed. On an ingest node the WAL directory is
/// listed too, unless [`data_root_covers_wal`] proves the data root covers
/// it. The repin siblings are listed only when something exists at their
/// path: their absence is the normal state, and nothing here creates them.
#[must_use]
pub fn protected_roots(
    data_root: &Path,
    wal_dir: &Path,
    ingest_enabled: bool,
) -> Vec<ProtectedRoot> {
    let mut roots = vec![ProtectedRoot {
        kind: RootKind::DataRoot,
        path: data_root.to_owned(),
    }];
    if ingest_enabled && !data_root_covers_wal(data_root, wal_dir) {
        roots.push(ProtectedRoot {
            kind: RootKind::WalDir,
            path: wal_dir.to_owned(),
        });
    }
    for (kind, path) in [
        (RootKind::RepinShadow, shadow_root(data_root)),
        (RootKind::RepinAside, aside_root(data_root)),
    ] {
        // Present unless the lookup says it is absent. Any other error is
        // listed, so the close reports it rather than skipping the root.
        if !matches!(std::fs::symlink_metadata(&path), Err(e) if e.kind() == io::ErrorKind::NotFound)
        {
            roots.push(ProtectedRoot { kind, path });
        }
    }
    roots
}

/// Whether closing `data_root` closes `wal_dir` too, so the WAL directory
/// needs no closing of its own.
///
/// A name under the data root proves nothing: `data/wal` may be a symlink
/// out of it, or a mount whose directory other users reach by another path.
/// So the WAL path must be a plain descendant of the data root, compared
/// component by component with no `..` in either path, and then be reached
/// by descriptor. The walk opens the data root no-follow and each component
/// below it with `openat(O_DIRECTORY | O_NOFOLLOW)` from the directory
/// before, and every directory on the way must sit on the data root's
/// filesystem and, on Linux, in its mount. A symlink, a non-directory, a
/// mount boundary, a Linux kernel that does not name the mount, or any
/// failure ends the walk unproven, and the WAL
/// directory is then a root of its own: held no-follow at its final
/// component and closed or refused like any other. Closing a directory the
/// root already covered costs nothing; leaving one open would expose it.
///
/// A component that does not exist yet, below directories the walk reached,
/// is covered: boot creates it there, inside the closed root, and checks
/// the roots again once it exists. A data root that cannot be walked, such
/// as one still absent, proves nothing.
#[must_use]
pub fn data_root_covers_wal(data_root: &Path, wal_dir: &Path) -> bool {
    covers_wal_with(data_root, wal_dir, Placement::of)
}

/// [`data_root_covers_wal`], reading each directory's placement with
/// `placement`: [`Placement::of`] in production, and a stand-in in tests
/// for a kernel that answers differently.
fn covers_wal_with(
    data_root: &Path,
    wal_dir: &Path,
    placement: impl Fn(BorrowedFd<'_>) -> io::Result<Placement>,
) -> bool {
    let climbs = |path: &Path| path.components().any(|c| c == Component::ParentDir);
    if climbs(data_root) || climbs(wal_dir) {
        return false;
    }
    let Ok(below) = wal_dir.strip_prefix(data_root) else {
        return false;
    };
    let Ok(Observation::Directory(root)) = observe(data_root) else {
        return false;
    };
    let Ok(home) = placement(root.handle.as_fd()) else {
        return false;
    };
    if !home.names_its_mount() {
        return false;
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = root.handle;
    for component in below.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        dir = match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
            Ok(next) if placement(next.as_fd()).is_ok_and(|at| at == home) => File::from(next),
            Err(Errno::NOENT) => return true,
            Ok(_) | Err(_) => return false,
        };
    }
    true
}

/// Where a directory lives: its filesystem and, where the kernel says, its
/// mount. Two directories on one filesystem can sit in different mounts, as
/// a bind mount does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    device: u64,
    mount: Option<u64>,
}

impl Placement {
    /// Whether this placement can prove two directories share a mount. On
    /// Linux it must name its mount: a bind mount keeps the device, so the
    /// device alone cannot tell a directory in the data root's mount from a
    /// mount of another directory on the same filesystem. Elsewhere the
    /// platform names no mount, and the device is all there is.
    const fn names_its_mount(self) -> bool {
        !cfg!(any(target_os = "linux", target_os = "android")) || self.mount.is_some()
    }

    /// The placement of the directory `fd` holds. The mount is read with
    /// `statx(STATX_MNT_ID)`, which a kernel before 5.8 does not answer;
    /// the mount is then unknown, and containment unproven.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn of(fd: BorrowedFd<'_>) -> io::Result<Self> {
        use rustix::fs::{AtFlags, StatxFlags};
        let stat = rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
        Ok(Self {
            device: rustix::fs::makedev(stat.stx_dev_major, stat.stx_dev_minor),
            mount: (stat.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stat.stx_mnt_id),
        })
    }

    /// The placement of the directory `fd` holds: its device alone, since
    /// this platform names no mount.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn of(fd: BorrowedFd<'_>) -> io::Result<Self> {
        Ok(Self {
            device: rustix::fs::fstat(fd)?.st_dev as u64,
            mount: None,
        })
    }
}

/// What is at a root's path, read without following a final symlink and
/// without changing anything.
#[derive(Debug)]
pub enum Observation {
    /// Nothing is there. Boot creates a missing root later, under its umask.
    Absent,
    /// Something other than a directory or a symlink is there. At the data
    /// root the epoch gate refuses it with its own message; any other root
    /// is refused here (see [`RootKind::epoch_gate_judges_non_directory`]).
    NotDirectory,
    /// The final component is a symbolic link.
    Symlink,
    /// The path does not end in a directory name: it is empty, `/`, or ends
    /// in `..`.
    BadPath,
    /// A directory, held open.
    Directory(Directory),
}

/// A root directory held open no-follow, with the owner and mode read from
/// the handle.
#[derive(Debug)]
pub struct Directory {
    handle: File,
    owner: u32,
    mode: u32,
}

impl Directory {
    /// The directory's owner uid.
    #[must_use]
    pub const fn owner(&self) -> u32 {
        self.owner
    }

    /// The directory's permission bits, setuid, setgid and sticky included.
    #[must_use]
    pub const fn mode(&self) -> u32 {
        self.mode
    }

    /// Whether the directory sits on a filesystem mounted read-only.
    ///
    /// # Errors
    /// The failed `fstatvfs`.
    pub fn read_only(&self) -> io::Result<bool> {
        let stats = rustix::fs::fstatvfs(&self.handle)?;
        Ok(stats.f_flag.contains(StatVfsMountFlags::RDONLY))
    }
}

/// Look at the root at `path` without following a final symlink.
///
/// The path is rebuilt from its components first, so a trailing `/` or `.`
/// cannot turn a final symlink into an intermediate component that the open
/// would follow.
///
/// # Errors
/// An open or `fstat` that failed for any reason but absence, a symlink or
/// a non-directory, such as a root trawld may not search.
pub fn observe(path: &Path) -> io::Result<Observation> {
    let components: Vec<Component<'_>> = path.components().collect();
    if !matches!(components.last(), Some(Component::Normal(_))) {
        return Ok(Observation::BadPath);
    }
    let path: PathBuf = components.iter().collect();

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let handle = match rustix::fs::open(&path, flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(Observation::Absent),
        // Linux answers a symlink with ENOTDIR here, not ELOOP: the open has
        // already refused it, and the lstat only names why.
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return Ok(
                if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
                    Observation::Symlink
                } else {
                    Observation::NotDirectory
                },
            );
        }
        Err(e) => return Err(e.into()),
    };
    let meta = handle.metadata()?;
    Ok(Observation::Directory(Directory {
        owner: meta.uid(),
        mode: meta.mode() & PERMISSION_BITS,
        handle,
    }))
}

/// The verdict on a directory owned by `owner` with permission bits `mode`,
/// for a trawld running as `euid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Owned by trawld, with no group or other bit.
    OwnerOnly,
    /// Owned by trawld, with a group or other bit to remove.
    WillTighten,
    /// Owned by another user. Refused whatever its mode, even for euid 0.
    ForeignOwner {
        /// The directory's owner.
        owner: u32,
        /// The uid trawld runs as.
        euid: u32,
    },
}

/// Judge a directory from its owner and mode alone.
#[must_use]
pub const fn judge(owner: u32, mode: u32, euid: u32) -> Verdict {
    if owner != euid {
        Verdict::ForeignOwner { owner, euid }
    } else if mode & GROUP_OTHER == 0 {
        Verdict::OwnerOnly
    } else {
        Verdict::WillTighten
    }
}

/// Why a root cannot be kept owner-only, as the doctor predicts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The final component is a symbolic link.
    Symlink,
    /// The path does not end in a directory name.
    BadPath,
    /// Another user owns it.
    ForeignOwner {
        /// The directory's owner.
        owner: u32,
        /// The uid trawld would run as.
        euid: u32,
    },
    /// It needs closing and sits on a read-only mount.
    ReadOnlyFilesystem,
}

/// What boot would do with a root, predicted without changing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prediction {
    /// Nothing is there yet.
    Absent,
    /// Not a directory. At the data root that is the epoch gate's verdict;
    /// boot refuses any other root that is not a directory (see
    /// [`RootKind::epoch_gate_judges_non_directory`]).
    NotDirectory,
    /// Already owner-only.
    OwnerOnly,
    /// Boot would close it.
    WillTighten,
    /// Boot would refuse to start.
    Refused(Refusal),
}

/// Predict, read-only, what boot's [`close`] would do with the root at
/// `path` for a trawld running as `euid`. This is the doctor's half: it
/// opens and stats, and changes nothing.
///
/// # Errors
/// What [`observe`] or [`Directory::read_only`] returns.
pub fn predict(path: &Path, euid: u32) -> io::Result<Prediction> {
    let dir = match observe(path)? {
        Observation::Absent => return Ok(Prediction::Absent),
        Observation::NotDirectory => return Ok(Prediction::NotDirectory),
        Observation::Symlink => return Ok(Prediction::Refused(Refusal::Symlink)),
        Observation::BadPath => return Ok(Prediction::Refused(Refusal::BadPath)),
        Observation::Directory(dir) => dir,
    };
    Ok(match judge(dir.owner, dir.mode, euid) {
        Verdict::OwnerOnly => Prediction::OwnerOnly,
        Verdict::ForeignOwner { owner, euid } => {
            Prediction::Refused(Refusal::ForeignOwner { owner, euid })
        }
        Verdict::WillTighten if dir.read_only()? => {
            Prediction::Refused(Refusal::ReadOnlyFilesystem)
        }
        Verdict::WillTighten => Prediction::WillTighten,
    })
}

/// What boot's [`close`] did with one root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// Nothing was there; boot creates it later, under its umask.
    Absent,
    /// The data root is not a directory; left for the epoch gate to refuse.
    NotDirectory,
    /// Already owner-only; unchanged.
    OwnerOnly,
    /// Group and other bits removed.
    Tightened {
        /// The permission bits before.
        from: u32,
        /// The permission bits after, read back from the handle.
        to: u32,
    },
}

/// Change the mode of the directory `fd` holds: the chmod boot passes to
/// [`close`].
///
/// # Errors
/// The failed `fchmod`.
pub fn fchmod(fd: BorrowedFd<'_>, mode: Mode) -> io::Result<()> {
    rustix::fs::fchmod(fd, mode).map_err(io::Error::from)
}

/// Close every root in `roots` to its owner, for a trawld running as `euid`.
/// Boot only: the doctor calls [`predict`].
///
/// Each root is judged and, when it can be, closed, whatever another root's
/// verdict: a refused WAL directory or repin sibling never keeps the data
/// root open, nor does a chmod that failed on one root stop the next. A
/// root that is refused for its type, its owner or a failed open is left as
/// it was. A root that needs closing gets
/// `mode & !0o077` through `chmod` on its open handle, which keeps the
/// owner's bits as they are and keeps setgid and sticky (a Kubernetes
/// `fsGroup` sets setgid). The mode is read back afterwards, and a group or
/// other bit that survived refuses. Every root it closes is logged as it
/// closes, refusal or not. The result is in `roots` order.
///
/// # Errors
/// The first root in `roots` order that cannot be kept owner-only, as an
/// [`OwnerOnlyError`], once every other root has been closed.
pub fn close<F>(
    roots: &[ProtectedRoot],
    euid: u32,
    mut chmod: F,
) -> Result<Vec<Closed>, OwnerOnlyError>
where
    F: FnMut(BorrowedFd<'_>, Mode) -> io::Result<()>,
{
    let mut closed = Vec::with_capacity(roots.len());
    let mut refusal = None;
    for root in roots {
        match close_one(root, euid, &mut chmod) {
            Ok(outcome) => {
                log_closed(root, outcome);
                closed.push(outcome);
            }
            Err(error) => {
                refusal.get_or_insert(error);
            }
        }
    }
    refusal.map_or(Ok(closed), Err)
}

/// Judge one root and close it if it needs closing.
fn close_one<F>(root: &ProtectedRoot, euid: u32, chmod: &mut F) -> Result<Closed, OwnerOnlyError>
where
    F: FnMut(BorrowedFd<'_>, Mode) -> io::Result<()>,
{
    let refuse = |dir: Option<&Directory>, cause| OwnerOnlyError::new(root, dir, euid, cause);
    match observe(&root.path) {
        Err(e) => Err(refuse(None, Cause::Inspect(e))),
        Ok(Observation::Absent) => Ok(Closed::Absent),
        Ok(Observation::NotDirectory) if root.kind.epoch_gate_judges_non_directory() => {
            Ok(Closed::NotDirectory)
        }
        Ok(Observation::NotDirectory) => Err(refuse(None, Cause::NotDirectory)),
        Ok(Observation::Symlink) => Err(refuse(None, Cause::Symlink)),
        Ok(Observation::BadPath) => Err(refuse(None, Cause::BadPath)),
        Ok(Observation::Directory(dir)) => match judge(dir.owner, dir.mode, euid) {
            Verdict::OwnerOnly => Ok(Closed::OwnerOnly),
            Verdict::WillTighten => tighten(root, &dir, euid, chmod),
            Verdict::ForeignOwner { .. } => Err(refuse(Some(&dir), Cause::ForeignOwner)),
        },
    }
}

/// Close a root trawld has just created, for a trawld running as its own
/// euid. [`close`] logs it if it had to be tightened.
///
/// A repin calls this right after it creates its shadow or aside root,
/// before anything enters it. The umask makes a new directory owner-only
/// unless a fixed-mode mount or a default ACL on the parent overrides it,
/// so the new root goes through the same close as one boot found. A root
/// that is gone again, or is no longer a directory, refuses too: something
/// replaced it after its creation.
///
/// # Errors
/// The root cannot be kept owner-only, as an [`OwnerOnlyError`].
pub fn close_created(kind: RootKind, path: &Path) -> Result<(), OwnerOnlyError> {
    let root = ProtectedRoot {
        kind,
        path: path.to_owned(),
    };
    let euid = rustix::process::geteuid().as_raw();
    let closed = close(std::slice::from_ref(&root), euid, fchmod)?;
    match closed.as_slice() {
        [Closed::Absent] => Err(OwnerOnlyError::new(
            &root,
            None,
            euid,
            Cause::Inspect(Errno::NOENT.into()),
        )),
        [Closed::NotDirectory] => Err(OwnerOnlyError::new(&root, None, euid, Cause::NotDirectory)),
        [_] => Ok(()),
        _ => unreachable!("one root closed, one result"),
    }
}

/// Log a root [`close`] had to tighten, so the operator learns it was open.
fn log_closed(root: &ProtectedRoot, closed: Closed) {
    if let Closed::Tightened { from, to } = closed {
        tracing::warn!(
            event_type = "storage_root_closed",
            root = %root.kind,
            path = %root.path.display(),
            from = format_args!("{from:04o}"),
            to = format_args!("{to:04o}"),
            "storage root was readable by other users; closed it to its owner \
             (files beneath it keep their modes and are unreachable to others)"
        );
    }
}

/// Remove `dir`'s group and other bits through its handle, then read the
/// mode back and refuse if any survived.
fn tighten<F>(
    root: &ProtectedRoot,
    dir: &Directory,
    euid: u32,
    chmod: &mut F,
) -> Result<Closed, OwnerOnlyError>
where
    F: FnMut(BorrowedFd<'_>, Mode) -> io::Result<()>,
{
    let refuse = |cause| OwnerOnlyError::new(root, Some(dir), euid, cause);
    chmod(dir.handle.as_fd(), permission_mode(dir.mode & !GROUP_OTHER)).map_err(|e| {
        refuse(match Errno::from_io_error(&e) {
            Some(Errno::PERM | Errno::ACCESS) => Cause::TightenDenied(e),
            Some(Errno::ROFS) => Cause::ReadOnlyFilesystem(e),
            _ => Cause::TightenFailed(e),
        })
    })?;
    let after = dir
        .handle
        .metadata()
        .map_err(|e| refuse(Cause::Inspect(e)))?
        .mode()
        & PERMISSION_BITS;
    if after & GROUP_OTHER != 0 {
        return Err(refuse(Cause::Ineffective { after }));
    }
    Ok(Closed::Tightened {
        from: dir.mode,
        to: after,
    })
}

/// `bits` as a rustix [`Mode`].
#[allow(
    clippy::unnecessary_cast,
    clippy::cast_possible_truncation,
    reason = "RawMode is u32 on Linux and u16 on macOS; permission bits fit both"
)]
const fn permission_mode(bits: u32) -> Mode {
    Mode::from_raw_mode(bits as rustix::fs::RawMode)
}

/// Why boot refused a root.
#[derive(Debug)]
pub enum Cause {
    /// The final component is a symbolic link.
    Symlink,
    /// The path does not end in a directory name.
    BadPath,
    /// Something other than a directory is there.
    NotDirectory,
    /// Another user owns it.
    ForeignOwner,
    /// It could not be opened or stat'ed.
    Inspect(io::Error),
    /// The chmod was refused for permission (EPERM or EACCES).
    TightenDenied(io::Error),
    /// The chmod was refused because the filesystem is read-only (EROFS).
    ReadOnlyFilesystem(io::Error),
    /// The chmod failed for another reason.
    TightenFailed(io::Error),
    /// The chmod reported success, and a group or other bit survived it.
    Ineffective {
        /// The permission bits read back after the chmod.
        after: u32,
    },
}

/// A storage root boot could not keep owner-only. trawld refuses to start.
///
/// The message names the root, its path, its owner and mode when a handle
/// was obtained, the uid trawld runs as, the cause and a fix. It is for the
/// boot log and stderr; the doctor's rows use fixed sentences instead.
#[derive(Debug)]
pub struct OwnerOnlyError {
    /// Which root.
    pub kind: RootKind,
    /// Its configured path.
    pub path: PathBuf,
    /// Its owner uid, when a handle was obtained.
    pub owner: Option<u32>,
    /// Its permission bits, when a handle was obtained.
    pub mode: Option<u32>,
    /// The uid trawld runs as.
    pub euid: u32,
    /// Why it was refused.
    pub cause: Cause,
}

impl OwnerOnlyError {
    fn new(root: &ProtectedRoot, dir: Option<&Directory>, euid: u32, cause: Cause) -> Self {
        Self {
            kind: root.kind,
            path: root.path.clone(),
            owner: dir.map(|d| d.owner),
            mode: dir.map(|d| d.mode),
            euid,
            cause,
        }
    }

    fn fix(&self) -> String {
        let path = self.path.display();
        let euid = self.euid;
        match &self.cause {
            Cause::Symlink => match self.kind {
                RootKind::DataRoot | RootKind::WalDir => format!(
                    "point {} at the directory itself, not a symlink",
                    self.kind.setting()
                ),
                RootKind::RepinShadow | RootKind::RepinAside => {
                    format!("replace {path} with the directory it links to, or remove it")
                }
            },
            Cause::BadPath => format!("set {} to the directory itself", self.kind.setting()),
            Cause::NotDirectory => match self.kind {
                RootKind::DataRoot => format!("point {} at a directory", self.kind.setting()),
                RootKind::WalDir => format!(
                    "point [ingest] wal_dir at a directory, or move {path} aside so trawld can \
                     create it"
                ),
                RootKind::RepinShadow | RootKind::RepinAside => {
                    format!("move {path} aside; trawld creates it when a repin runs")
                }
            },
            Cause::ForeignOwner => format!(
                "chown {path} to uid {euid}, the user that runs trawld, or run trawld as \
                 its owner"
            ),
            Cause::Inspect(_) => format!(
                "make {path} a directory that uid {euid}, the user that runs trawld, owns \
                 and can read"
            ),
            Cause::TightenDenied(_) | Cause::TightenFailed(_) => format!(
                "run `chmod 0700 {path}` as its owner or root, after clearing anything that \
                 blocks it, such as an immutable attribute"
            ),
            Cause::ReadOnlyFilesystem(_) => format!(
                "mount it read-write, or run `chmod 0700 {path}` before mounting it read-only"
            ),
            Cause::Ineffective { .. } => "mount it with owner-only modes (for example \
                 `dir_mode=0700` on CIFS), or move it to a filesystem that keeps Unix modes"
                .to_owned(),
        }
    }
}

impl fmt::Display for OwnerOnlyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} (owner ", self.kind, self.path.display())?;
        match self.owner {
            Some(owner) => write!(f, "uid {owner}")?,
            None => f.write_str("unknown")?,
        }
        f.write_str(", mode ")?;
        match self.mode {
            Some(mode) => write!(f, "{mode:04o}")?,
            None => f.write_str("unknown")?,
        }
        write!(
            f,
            ") cannot be kept owner-only for trawld, which runs as uid {}: ",
            self.euid
        )?;
        match &self.cause {
            Cause::Symlink => f.write_str(
                "it is a symbolic link, and trawld does not follow one at a storage root",
            ),
            Cause::BadPath => f.write_str("the configured path does not end in a directory name"),
            Cause::NotDirectory => f.write_str("it is not a directory"),
            Cause::ForeignOwner => f.write_str(
                "another user owns it, and trawld does not take over a directory it does not own",
            ),
            Cause::Inspect(e) => {
                write!(f, "it could not be opened to read its owner and mode ({e})")
            }
            Cause::TightenDenied(e) => write!(
                f,
                "the filesystem refused to remove its group and other permissions ({e})"
            ),
            Cause::ReadOnlyFilesystem(e) => write!(
                f,
                "it is on a read-only filesystem, so its group and other permissions cannot be \
                 removed ({e})"
            ),
            Cause::TightenFailed(e) => {
                write!(f, "removing its group and other permissions failed ({e})")
            }
            Cause::Ineffective { after } => write!(
                f,
                "the filesystem accepted the change and still reports mode {after:04o}"
            ),
        }?;
        write!(
            f,
            ". Refusing to start, because stored logs must not be readable by other local \
             users. Fix: {}.",
            self.fix()
        )
    }
}

impl std::error::Error for OwnerOnlyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            Cause::Inspect(e)
            | Cause::TightenDenied(e)
            | Cause::ReadOnlyFilesystem(e)
            | Cause::TightenFailed(e) => Some(e),
            Cause::Symlink
            | Cause::BadPath
            | Cause::NotDirectory
            | Cause::ForeignOwner
            | Cause::Ineffective { .. } => None,
        }
    }
}

/// Give `dir` a default ACL of `u::rwx,g::r-x,o::r-x`, so a directory
/// created in it is 0755 whatever the umask: the real-filesystem stand-in
/// for a mount whose modes trawld's umask does not decide.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn inherit_0755(dir: &Path) {
    const USER_OBJ: u16 = 0x01;
    const GROUP_OBJ: u16 = 0x04;
    const OTHER: u16 = 0x20;
    let mut acl = 2_u32.to_le_bytes().to_vec();
    for (tag, perm) in [(USER_OBJ, 7_u16), (GROUP_OBJ, 5), (OTHER, 5)] {
        acl.extend(tag.to_le_bytes());
        acl.extend(perm.to_le_bytes());
        acl.extend(u32::MAX.to_le_bytes());
    }
    rustix::fs::setxattr(
        dir,
        "system.posix_acl_default",
        &acl,
        rustix::fs::XattrFlags::empty(),
    )
    .expect("the test filesystem must support default ACLs");
    let probe = dir.join("probe");
    std::fs::create_dir(&probe).unwrap();
    let mode = std::fs::symlink_metadata(&probe).unwrap().mode() & PERMISSION_BITS;
    assert_eq!(mode, 0o755, "the default ACL decides the mode");
    std::fs::remove_dir(&probe).unwrap();
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn euid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & PERMISSION_BITS
    }

    fn data_root(path: &Path) -> Vec<ProtectedRoot> {
        vec![ProtectedRoot {
            kind: RootKind::DataRoot,
            path: path.to_owned(),
        }]
    }

    /// A chmod that must never be reached.
    fn no_chmod(_: BorrowedFd<'_>, _: Mode) -> io::Result<()> {
        panic!("chmod reached")
    }

    #[test]
    fn judge_matrix() {
        let me = 1000;
        for (mode, want) in [
            (0o700, Verdict::OwnerOnly),
            (0o500, Verdict::OwnerOnly),
            (0o000, Verdict::OwnerOnly),
            (0o2700, Verdict::OwnerOnly),
            (0o755, Verdict::WillTighten),
            (0o750, Verdict::WillTighten),
            (0o701, Verdict::WillTighten),
            (0o710, Verdict::WillTighten),
            (0o2770, Verdict::WillTighten),
            (0o1777, Verdict::WillTighten),
        ] {
            assert_eq!(judge(me, mode, me), want, "mode {mode:04o}");
        }
        // Another owner refuses whatever the mode, and whatever trawld's uid,
        // root included.
        for (owner, euid, mode) in [
            (0, me, 0o700),
            (0, me, 0o755),
            (me, 0, 0o700),
            (7, 8, 0o750),
        ] {
            assert_eq!(
                judge(owner, mode, euid),
                Verdict::ForeignOwner { owner, euid },
                "owner {owner} euid {euid} mode {mode:04o}"
            );
        }
    }

    #[test]
    fn close_tightens_the_root_and_nothing_beneath_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("old.parquet"), b"stored").unwrap();
        std::fs::create_dir(root.join("prod")).unwrap();
        set_mode(&root.join("old.parquet"), 0o644);
        set_mode(&root.join("prod"), 0o755);
        set_mode(&root, 0o755);
        let inode = std::fs::metadata(&root).unwrap().ino();

        let closed = close(&data_root(&root), euid(), fchmod).unwrap();

        assert_eq!(
            closed,
            vec![Closed::Tightened {
                from: 0o755,
                to: 0o700
            }]
        );
        assert_eq!(mode_of(&root), 0o700);
        assert_eq!(std::fs::metadata(&root).unwrap().ino(), inode);
        assert_eq!(mode_of(&root.join("old.parquet")), 0o644);
        assert_eq!(mode_of(&root.join("prod")), 0o755);
    }

    #[test]
    fn close_keeps_owner_bits_and_setgid() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        // The owner's bits stay as they are: a 0570 root does not gain the
        // owner write bit it lacked.
        set_mode(&root, 0o570);
        close(&data_root(&root), euid(), fchmod).unwrap();
        assert_eq!(mode_of(&root), 0o500);

        // A Kubernetes fsGroup root is setgid; the chmod asks to keep it.
        set_mode(&root, 0o2775);
        let before = mode_of(&root);
        let asked = Cell::new(None);
        close(&data_root(&root), euid(), |fd, mode| {
            asked.set(Some(mode));
            fchmod(fd, mode)
        })
        .unwrap();
        assert_eq!(asked.get(), Some(permission_mode(before & !GROUP_OTHER)));
        assert_eq!(mode_of(&root), before & !GROUP_OTHER);
    }

    #[test]
    fn an_owner_only_root_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        assert_eq!(
            close(&data_root(&root), euid(), no_chmod).unwrap(),
            vec![Closed::OwnerOnly]
        );
    }

    #[test]
    fn an_absent_root_is_left_for_boot_to_create() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        assert_eq!(
            close(&data_root(&root), euid(), no_chmod).unwrap(),
            vec![Closed::Absent]
        );
        assert!(!root.exists(), "closing creates nothing");
    }

    #[test]
    fn a_non_directory_is_left_to_the_epoch_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::write(&root, b"not a directory").unwrap();
        set_mode(&root, 0o644);
        assert_eq!(
            close(&data_root(&root), euid(), no_chmod).unwrap(),
            vec![Closed::NotDirectory]
        );
        assert_eq!(mode_of(&root), 0o644);
    }

    /// A WAL directory or repin sibling that is not a directory refuses,
    /// without a chmod, and is left as it was: only the data root's
    /// non-directory is the epoch gate's to refuse.
    #[test]
    fn a_non_directory_wal_or_repin_sibling_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        set_mode(&file, 0o644);
        for kind in [
            RootKind::WalDir,
            RootKind::RepinShadow,
            RootKind::RepinAside,
        ] {
            let roots = [ProtectedRoot {
                kind,
                path: file.clone(),
            }];
            let error = close(&roots, euid(), no_chmod).unwrap_err();
            assert!(matches!(error.cause, Cause::NotDirectory), "{error}");
            let text = error.to_string();
            assert!(text.contains("it is not a directory"), "{text}");
            assert!(
                text.contains(&format!("move {} aside", file.display())),
                "{text}"
            );
        }
        assert_eq!(mode_of(&file), 0o644);
        assert!(!RootKind::WalDir.epoch_gate_judges_non_directory());
        assert!(RootKind::DataRoot.epoch_gate_judges_non_directory());
    }

    #[test]
    fn a_symlinked_root_refuses_in_every_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real");
        std::fs::create_dir(&target).unwrap();
        set_mode(&target, 0o755);
        let link = tmp.path().join("data");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let spellings = [
            link.clone(),
            PathBuf::from(format!("{}/", link.display())),
            link.join("."),
            PathBuf::from(format!("{}/./", link.display())),
        ];
        for spelling in spellings {
            let error = close(&data_root(&spelling), euid(), no_chmod).unwrap_err();
            assert!(
                matches!(error.cause, Cause::Symlink),
                "{}: {error}",
                spelling.display()
            );
            assert_eq!(error.owner, None);
            assert_eq!(
                predict(&spelling, euid()).unwrap(),
                Prediction::Refused(Refusal::Symlink),
                "{}",
                spelling.display()
            );
        }
        assert_eq!(mode_of(&target), 0o755, "the link target is untouched");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }

    #[test]
    fn a_dangling_symlink_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("data");
        std::os::unix::fs::symlink(tmp.path().join("missing"), &link).unwrap();
        let error = close(&data_root(&link), euid(), no_chmod).unwrap_err();
        assert!(matches!(error.cause, Cause::Symlink), "{error}");
        assert!(!tmp.path().join("missing").exists());
    }

    #[test]
    fn a_path_without_a_final_name_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        for path in [PathBuf::from("/"), tmp.path().join(".."), PathBuf::new()] {
            let error = close(&data_root(&path), euid(), no_chmod).unwrap_err();
            assert!(
                matches!(error.cause, Cause::BadPath),
                "{}: {error}",
                path.display()
            );
            assert_eq!(
                predict(&path, euid()).unwrap(),
                Prediction::Refused(Refusal::BadPath)
            );
        }
    }

    #[test]
    fn a_foreign_owner_refuses_without_a_chmod() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        set_mode(&root, 0o755);
        // A synthetic euid stands in for a root another user owns: the
        // directory's real owner is then not the uid trawld "runs as".
        let other = euid().wrapping_add(1);
        let error = close(&data_root(&root), other, no_chmod).unwrap_err();
        assert!(matches!(error.cause, Cause::ForeignOwner), "{error}");
        assert_eq!(error.owner, Some(euid()));
        assert_eq!(error.mode, Some(0o755));
        assert_eq!(mode_of(&root), 0o755);
        assert_eq!(
            predict(&root, other).unwrap(),
            Prediction::Refused(Refusal::ForeignOwner {
                owner: euid(),
                euid: other
            })
        );
    }

    #[test]
    fn a_refused_chmod_names_its_cause() {
        for (errno, want) in [
            (Errno::PERM, "refused to remove"),
            (Errno::ACCESS, "refused to remove"),
            (Errno::ROFS, "read-only filesystem"),
            (Errno::IO, "removing its group and other permissions failed"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("data");
            std::fs::create_dir(&root).unwrap();
            set_mode(&root, 0o755);
            let error = close(&data_root(&root), euid(), |_, _| Err(errno.into())).unwrap_err();
            match (errno, &error.cause) {
                (Errno::PERM | Errno::ACCESS, Cause::TightenDenied(_))
                | (Errno::ROFS, Cause::ReadOnlyFilesystem(_))
                | (Errno::IO, Cause::TightenFailed(_)) => {}
                _ => panic!("{errno:?} mapped to {:?}", error.cause),
            }
            let text = error.to_string();
            assert!(text.contains(want), "{text}");
            assert!(std::error::Error::source(&error).is_some());
            assert_eq!(mode_of(&root), 0o755);
        }
    }

    #[test]
    fn a_chmod_that_changes_nothing_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        set_mode(&root, 0o755);
        let error = close(&data_root(&root), euid(), |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(error.cause, Cause::Ineffective { after: 0o755 }),
            "{error}"
        );
        assert!(
            error.to_string().contains("still reports mode 0755"),
            "{error}"
        );
    }

    /// A refused root keeps no other root open: every root that can be
    /// closed is closed, and the first refusal in `roots` order is returned
    /// afterwards. A data root left 0755 behind a refused WAL directory or
    /// repin sibling would stay open to other users across every refused
    /// restart.
    #[test]
    fn a_refused_root_leaves_every_closable_root_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        set_mode(&data, 0o755);
        let real = tmp.path().join("real-wal");
        std::fs::create_dir(&real).unwrap();
        set_mode(&real, 0o755);
        let wal = tmp.path().join("wal");
        std::os::unix::fs::symlink(&real, &wal).unwrap();
        let shadow = tmp.path().join("data.repin-next");
        std::fs::write(&shadow, b"not a directory").unwrap();
        set_mode(&shadow, 0o644);
        let aside = tmp.path().join("data.repin-aside");
        std::fs::create_dir(&aside).unwrap();
        set_mode(&aside, 0o755);
        let roots = [
            (RootKind::DataRoot, &data),
            (RootKind::WalDir, &wal),
            (RootKind::RepinShadow, &shadow),
            (RootKind::RepinAside, &aside),
        ]
        .map(|(kind, path)| ProtectedRoot {
            kind,
            path: path.clone(),
        });

        let error = close(&roots, euid(), fchmod).unwrap_err();

        assert_eq!(error.kind, RootKind::WalDir, "the first refusal: {error}");
        assert!(matches!(error.cause, Cause::Symlink), "{error}");
        assert_eq!(mode_of(&data), 0o700, "the data root closes anyway");
        assert_eq!(mode_of(&aside), 0o700, "a root after the refusals closes");
        assert_eq!(mode_of(&real), 0o755, "the link target is untouched");
        assert_eq!(mode_of(&shadow), 0o644, "the refused file is untouched");
    }

    /// A chmod that fails on one root does not stop the next from closing,
    /// and the failure is still the refusal.
    #[test]
    fn a_failed_chmod_does_not_stop_the_other_roots_closing() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let wal = tmp.path().join("wal");
        for dir in [&data, &wal] {
            std::fs::create_dir(dir).unwrap();
            set_mode(dir, 0o755);
        }
        let roots = [(RootKind::DataRoot, &data), (RootKind::WalDir, &wal)].map(|(kind, path)| {
            ProtectedRoot {
                kind,
                path: path.clone(),
            }
        });
        let calls = Cell::new(0);

        let error = close(&roots, euid(), |fd, mode| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(Errno::PERM.into())
            } else {
                fchmod(fd, mode)
            }
        })
        .unwrap_err();

        assert_eq!(error.kind, RootKind::DataRoot, "{error}");
        assert!(matches!(error.cause, Cause::TightenDenied(_)), "{error}");
        assert_eq!(mode_of(&data), 0o755);
        assert_eq!(mode_of(&wal), 0o700, "the WAL directory closes anyway");
    }

    #[test]
    fn the_error_names_path_owner_mode_uid_and_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        set_mode(&root, 0o755);
        let other = euid().wrapping_add(1);
        let text = close(&data_root(&root), other, no_chmod)
            .unwrap_err()
            .to_string();
        for needle in [
            format!("data root {}", root.display()),
            format!("owner uid {}", euid()),
            "mode 0755".to_owned(),
            format!("runs as uid {other}"),
            "another user owns it".to_owned(),
            "Refusing to start".to_owned(),
            format!("Fix: chown {} to uid {other}", root.display()),
        ] {
            assert!(text.contains(&needle), "{needle:?} missing from {text}");
        }

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let wal = [ProtectedRoot {
            kind: RootKind::WalDir,
            path: link.clone(),
        }];
        let text = close(&wal, euid(), no_chmod).unwrap_err().to_string();
        for needle in [
            format!("WAL directory {}", link.display()),
            "owner unknown, mode unknown".to_owned(),
            "symbolic link".to_owned(),
            "Fix: point [ingest] wal_dir at the directory itself, not a symlink.".to_owned(),
        ] {
            assert!(text.contains(&needle), "{needle:?} missing from {text}");
        }
    }

    #[test]
    fn predict_reads_and_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        assert_eq!(predict(&root, euid()).unwrap(), Prediction::Absent);
        std::fs::create_dir(&root).unwrap();
        set_mode(&root, 0o755);
        assert_eq!(predict(&root, euid()).unwrap(), Prediction::WillTighten);
        assert_eq!(mode_of(&root), 0o755);
        set_mode(&root, 0o700);
        assert_eq!(predict(&root, euid()).unwrap(), Prediction::OwnerOnly);
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(predict(&file, euid()).unwrap(), Prediction::NotDirectory);
    }

    #[test]
    fn a_wal_is_covered_only_as_a_plain_descendant() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(tmp.path().join("x")).unwrap();
        let at = |rest: &str| PathBuf::from(format!("{}/{rest}", tmp.path().display()));
        for (wal, covered) in [
            ("data/wal", true),
            ("data/nested/wal", true),
            ("data/", true),
            ("wal", false),
            ("data2/wal", false),
            ("data-wal", false),
            ("data/../wal", false),
            ("data/wal/../../wal", false),
        ] {
            assert_eq!(data_root_covers_wal(&data, &at(wal)), covered, "{wal}");
        }
        assert!(!data_root_covers_wal(
            &at("x/../data"),
            &at("x/../data/wal")
        ));
    }

    /// A kernel that does not answer `STATX_MNT_ID` (before 5.8) leaves a
    /// bind mount indistinguishable from a plain directory by device, so a
    /// WAL directory really nested in the data root is unproven there and
    /// gets closed on its own.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn an_unknown_mount_id_leaves_wal_containment_unproven() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(data.join("spool/wal")).unwrap();
        let wal = data.join("spool/wal");
        assert!(covers_wal_with(&data, &wal, Placement::of), "a real kernel");

        let without_mount_id =
            |fd: BorrowedFd<'_>| Placement::of(fd).map(|at| Placement { mount: None, ..at });
        assert!(!covers_wal_with(&data, &wal, without_mount_id));
        assert!(!covers_wal_with(
            &data,
            &data.join("absent"),
            without_mount_id
        ));
    }

    /// The kinds and paths `protected_roots` lists for an ingest node.
    fn listed(data: &Path, wal: &Path) -> Vec<(RootKind, PathBuf)> {
        protected_roots(data, wal, true)
            .into_iter()
            .map(|r| (r.kind, r.path))
            .collect()
    }

    #[test]
    fn a_wal_under_the_root_by_name_is_covered_only_when_reached_by_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("wal")).unwrap();
        set_mode(&elsewhere.join("wal"), 0o755);
        let only_root = vec![(RootKind::DataRoot, data.clone())];

        // A real directory nested in the root is the root's, and is never
        // chmodded (AC3), however deep, and while it is still absent.
        std::fs::create_dir_all(data.join("spool/wal")).unwrap();
        set_mode(&data.join("spool/wal"), 0o755);
        for wal in [
            data.join("spool/wal"),
            data.join("spool/absent"),
            data.join("absent/wal"),
        ] {
            assert_eq!(listed(&data, &wal), only_root, "{}", wal.display());
        }
        let closed = close(
            &protected_roots(&data, &data.join("spool/wal"), true),
            euid(),
            fchmod,
        );
        assert!(closed.is_ok());
        assert_eq!(mode_of(&data.join("spool/wal")), 0o755);

        // The WAL directory itself is a symlink out of the root: it is its
        // own root, and the close refuses it without following it.
        let link = data.join("wal");
        std::os::unix::fs::symlink(elsewhere.join("wal"), &link).unwrap();
        let wal_root = |path: &Path| {
            vec![
                (RootKind::DataRoot, data.clone()),
                (RootKind::WalDir, path.to_owned()),
            ]
        };
        assert_eq!(listed(&data, &link), wal_root(&link));
        let error = close(&protected_roots(&data, &link, true), euid(), no_chmod).unwrap_err();
        assert_eq!(error.kind, RootKind::WalDir);
        assert!(matches!(error.cause, Cause::Symlink), "{error}");

        // A directory on the way is a symlink out of the root: the WAL
        // directory past it is its own root, and the close tightens it.
        let hop = data.join("hop");
        std::os::unix::fs::symlink(&elsewhere, &hop).unwrap();
        let through = hop.join("wal");
        assert_eq!(listed(&data, &through), wal_root(&through));
        assert_eq!(
            listed(&data, &hop.join("absent")),
            wal_root(&hop.join("absent"))
        );
        close(&protected_roots(&data, &through, true), euid(), fchmod).unwrap();
        assert_eq!(
            mode_of(&elsewhere.join("wal")),
            0o700,
            "the escape is closed"
        );
        assert!(std::fs::symlink_metadata(&hop).unwrap().is_symlink());

        // A non-directory on the way, a `..`, or a data root that cannot be
        // walked (absent, or a symlink) proves nothing: the WAL directory
        // is listed on its own.
        std::fs::write(data.join("file"), b"").unwrap();
        for wal in [
            data.join("file/wal"),
            data.join("../elsewhere/wal"),
            data.join("spool/../wal"),
        ] {
            assert_eq!(listed(&data, &wal), wal_root(&wal), "{}", wal.display());
        }
        let absent = tmp.path().join("absent-data");
        assert_eq!(
            listed(&absent, &absent.join("wal")),
            vec![
                (RootKind::DataRoot, absent.clone()),
                (RootKind::WalDir, absent.join("wal"))
            ]
        );
        let linked = tmp.path().join("linked-data");
        std::os::unix::fs::symlink(&data, &linked).unwrap();
        assert_eq!(
            listed(&linked, &linked.join("spool/wal")),
            vec![
                (RootKind::DataRoot, linked.clone()),
                (RootKind::WalDir, linked.join("spool/wal"))
            ]
        );
    }

    #[test]
    fn protected_roots_lists_what_boot_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let nested = data.join("wal");
        let outside = tmp.path().join("wal");
        let kinds = |roots: Vec<ProtectedRoot>| -> Vec<(RootKind, PathBuf)> {
            roots.into_iter().map(|r| (r.kind, r.path)).collect()
        };

        assert_eq!(
            kinds(protected_roots(&data, &nested, true)),
            vec![(RootKind::DataRoot, data.clone())]
        );
        assert_eq!(
            kinds(protected_roots(&data, &outside, true)),
            vec![
                (RootKind::DataRoot, data.clone()),
                (RootKind::WalDir, outside.clone())
            ]
        );
        assert_eq!(
            kinds(protected_roots(&data, &outside, false)),
            vec![(RootKind::DataRoot, data.clone())],
            "a query-only node has no WAL to close"
        );

        // The repin siblings are listed only when something is there, and
        // a dangling symlink counts, so the close refuses it.
        let shadow = tmp.path().join("data.repin-next");
        let aside = tmp.path().join("data.repin-aside");
        std::fs::create_dir(&shadow).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), &aside).unwrap();
        assert_eq!(
            kinds(protected_roots(&data, &nested, false)),
            vec![
                (RootKind::DataRoot, data.clone()),
                (RootKind::RepinShadow, shadow),
                (RootKind::RepinAside, aside),
            ]
        );
    }
}
