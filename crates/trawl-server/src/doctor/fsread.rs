// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The doctor's one way to read a file: bounded in size and in time, and
//! never waiting on a FIFO (#269, D15).
//!
//! An installation the doctor inspects may hold anything at a path it
//! reads: a FIFO nobody writes to, a symlink loop, a file of mode 000, a
//! device, or a multi-gigabyte file. Opening some devices acts on them
//! (opening `/dev/watchdog` arms a host reset), so nothing the doctor has
//! not seen to be a regular file is ever opened for I/O. [`find_at`] first
//! opens the path with `O_PATH | O_CLOEXEC`, which opens no file for I/O
//! and so neither blocks on a FIFO nor reaches a device's driver, and
//! `fstat`s that handle. Only a regular file is then opened for reading,
//! through `/proc/self/fd/<n>` with `O_RDONLY | O_NOCTTY | O_NONBLOCK |
//! O_CLOEXEC`, which reaches the object the handle holds, never whatever
//! the path names by then; a second `fstat` must see the same device and
//! inode. Where `/proc` is not mounted nothing is read.
//!
//! [`read_bounded`] then checks that the file is no larger than the cap
//! before it reads, reads at most one byte past the cap, and checks the
//! descriptor again afterwards. Every failure is a [`ReadFault`], never an
//! `io::Error`, so no OS text can reach the report.
//!
//! Two link policies:
//!
//! - [`Links::Follow`] for the paths an operator selected (the
//!   configuration, a certificate, a key). Those are often symlinks, as in
//!   a Kubernetes secret mount. A loop is [`ReadFault::SymlinkLoop`].
//! - [`Links::NoFollow`] for trawld's own markers under the data root. The
//!   `O_PATH` open adds `O_NOFOLLOW`, so a symlink at the last component
//!   is the link itself, never its target, and is [`ReadFault::NotRegular`].
//!
//! The doctor's reads of files that boot's own readers decode (publication
//! and rollup markers, the generated certificate and key) open through
//! [`find_at`] too, by [`open_for_decoder`] or directly.
//!
//! [`read`] runs the read on the blocking pool under [`READ_DEADLINE`], so a
//! hung filesystem costs the check its answer, not the run.

use std::io::Read as _;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The deadline for one file read, open to last byte.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);

/// The most bytes the doctor reads from each kind of file.
pub mod cap {
    /// `trawld.toml`. The shipped reference configuration is about 12 KiB.
    pub const CONFIG: u64 = 1024 * 1024;
    /// The data root's `EPOCH` marker: one small integer and a newline.
    pub const EPOCH: u64 = 64;
    /// The data root's `CATALOG` marker: one UUID and a newline.
    pub const CATALOG: u64 = 128;
    /// The data root's `REPIN` marker: a small JSON document.
    pub const REPIN: u64 = 64 * 1024;
    /// A publication marker, at the bound its own reader enforces.
    pub const PUBLICATION_MARKER: u64 = crate::ingest::publication_marker::MAX_MARKER_BYTES;
    /// A rollup marker, at the bound its own reader enforces.
    pub const ROLLUP_MARKER: u64 = crate::ingest::compaction::MAX_ROLLUP_MARKER_BYTES;
    /// A PEM certificate chain.
    pub const CERT: u64 = 1024 * 1024;
    /// A PEM private key.
    pub const KEY: u64 = 1024 * 1024;
}

/// Whether a symlink at the path's last component is followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Links {
    /// Follow it: a path the operator selected.
    Follow,
    /// Refuse it: one of trawld's own markers.
    NoFollow,
}

/// Why a bounded read returned no bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFault {
    /// Nothing exists at the path, or a component of it is not a directory.
    Missing,
    /// The running user may not open it.
    PermissionDenied,
    /// It is not a regular file: a directory, FIFO, socket, or device, or,
    /// under [`Links::NoFollow`], a symlink.
    NotRegular,
    /// Resolving it met too many symlinks, as a loop does.
    SymlinkLoop,
    /// It is larger than the cap.
    TooLarge,
    /// It changed while it was read, or the object opened for reading is
    /// not the one first looked at.
    Changed,
    /// Any other failure to open or read it, `/proc` not mounted among
    /// them.
    Io,
    /// The read did not finish within [`READ_DEADLINE`].
    TimedOut,
}

/// Read the regular file at `path`, at most `max` bytes, without blocking
/// on what is there.
///
/// # Errors
/// A [`ReadFault`] naming why nothing was read.
pub fn read_bounded(path: &Path, max: u64, links: Links) -> Result<Vec<u8>, ReadFault> {
    let file = open(path, links)?;
    let before = file.metadata().map_err(|_| ReadFault::Io)?;
    if !before.file_type().is_file() {
        return Err(ReadFault::NotRegular);
    }
    if before.len() > max {
        return Err(ReadFault::TooLarge);
    }
    let mut bytes = Vec::new();
    (&file)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ReadFault::Io)?;
    if bytes.len() as u64 > max {
        return Err(ReadFault::TooLarge);
    }
    let after = file.metadata().map_err(|_| ReadFault::Io)?;
    if bytes.len() as u64 != before.len() || !same_version(&before, &after) {
        return Err(ReadFault::Changed);
    }
    Ok(bytes)
}

/// [`read_bounded`] on the blocking pool, under [`READ_DEADLINE`].
///
/// A read that misses the deadline is left behind on its thread; the
/// doctor's runtime is shut down with a bounded wait, so it cannot hold the
/// process.
///
/// # Errors
/// As [`read_bounded`], and [`ReadFault::TimedOut`] past the deadline.
pub async fn read(path: PathBuf, max: u64, links: Links) -> Result<Vec<u8>, ReadFault> {
    let task = tokio::task::spawn_blocking(move || read_bounded(&path, max, links));
    match tokio::time::timeout(READ_DEADLINE, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(ReadFault::Io),
        Err(_) => Err(ReadFault::TimedOut),
    }
}

/// What [`find_at`] found at a path.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum Found {
    /// A regular file, open for reading: the object the `O_PATH` look
    /// found, reached through `/proc/self/fd`.
    Regular(std::fs::File),
    /// Anything else, as its `O_PATH` handle, which answers `fstat` and
    /// no I/O. It was never opened for reading.
    Other(OwnedFd),
}

/// Why [`find_at`] found nothing it could hand back.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindFault {
    /// The `O_PATH` open failed.
    Look(rustix::io::Errno),
    /// An `fstat` failed.
    Stat,
    /// Opening the regular file for reading through `/proc/self/fd`
    /// failed. `ENOENT` here means `/proc` is not mounted, not that the
    /// file is gone.
    Reopen(rustix::io::Errno),
    /// The object opened for reading is not the one looked at.
    Changed,
}

/// Look at `path`, relative to `dir` when it is relative, with an `O_PATH`
/// open (`O_NOFOLLOW` under [`Links::NoFollow`]), and open it for reading
/// only when that handle is a regular file.
///
/// # Errors
/// A [`FindFault`] naming the step that failed.
#[cfg(target_os = "linux")]
pub fn find_at(dir: BorrowedFd<'_>, path: &Path, links: Links) -> Result<Found, FindFault> {
    find_at_with(dir, path, links, reopen_through_proc)
}

/// [`find_at`], reopening a regular file with `reopen`, which tests replace.
#[cfg(target_os = "linux")]
fn find_at_with(
    dir: BorrowedFd<'_>,
    path: &Path,
    links: Links,
    reopen: impl FnOnce(BorrowedFd<'_>) -> rustix::io::Result<OwnedFd>,
) -> Result<Found, FindFault> {
    use std::os::fd::AsFd as _;

    use rustix::fs::{FileType, Mode, OFlags};
    let mut flags = OFlags::PATH | OFlags::CLOEXEC;
    if links == Links::NoFollow {
        flags |= OFlags::NOFOLLOW;
    }
    let handle = rustix::fs::openat(dir, path, flags, Mode::empty()).map_err(FindFault::Look)?;
    let seen = rustix::fs::fstat(&handle).map_err(|_| FindFault::Stat)?;
    if FileType::from_raw_mode(seen.st_mode) != FileType::RegularFile {
        return Ok(Found::Other(handle));
    }
    let file = reopen(handle.as_fd()).map_err(FindFault::Reopen)?;
    let opened = rustix::fs::fstat(&file).map_err(|_| FindFault::Stat)?;
    if (opened.st_dev, opened.st_ino) != (seen.st_dev, seen.st_ino) {
        return Err(FindFault::Changed);
    }
    Ok(Found::Regular(std::fs::File::from(file)))
}

/// Open the object `handle` holds for reading, through its
/// `/proc/self/fd` entry: the kernel resolves that to the object, not to
/// whatever its path names now.
#[cfg(target_os = "linux")]
fn reopen_through_proc(handle: BorrowedFd<'_>) -> rustix::io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        format!("/proc/self/fd/{}", handle.as_raw_fd()),
        OFlags::RDONLY | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// An opener for a decoder boot shares with the doctor, such as
/// [`crate::ingest::publication_marker::census`]: the file at `path`,
/// never following a symlink at its last component, through [`find_at`].
///
/// A regular file comes back open for reading. Anything else comes back
/// as its `O_PATH` handle, which answers the decoder's `fstat` and nothing
/// else, so the decoder refuses it by type as it would have, and it was
/// never opened.
///
/// # Errors
/// The `errno` of the step that failed, so a decoder tells a missing file
/// from one it may not read, as it does for its own opener.
#[cfg(target_os = "linux")]
pub fn open_for_decoder(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::io::Errno;
    match find_at(rustix::fs::CWD, path, Links::NoFollow) {
        Ok(Found::Regular(file)) => Ok(file),
        Ok(Found::Other(handle)) => Ok(std::fs::File::from(handle)),
        // `/proc` not mounted is not a missing marker.
        Err(FindFault::Reopen(Errno::NOENT)) => Err(std::io::Error::other("not reopened")),
        Err(FindFault::Look(errno) | FindFault::Reopen(errno)) => Err(errno.into()),
        Err(FindFault::Stat | FindFault::Changed) => Err(std::io::Error::other("not read")),
    }
}

/// Off Linux there is no `O_PATH` open, so nothing is opened.
#[cfg(not(target_os = "linux"))]
pub fn open_for_decoder(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::symlink_metadata(path)?;
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the doctor opens files only on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn open(path: &Path, links: Links) -> Result<std::fs::File, ReadFault> {
    open_with(path, links, reopen_through_proc)
}

/// [`open`], reopening with `reopen`, which tests replace.
#[cfg(target_os = "linux")]
fn open_with(
    path: &Path,
    links: Links,
    reopen: impl FnOnce(BorrowedFd<'_>) -> rustix::io::Result<OwnedFd>,
) -> Result<std::fs::File, ReadFault> {
    use rustix::io::Errno;
    match find_at_with(rustix::fs::CWD, path, links, reopen) {
        Ok(Found::Regular(file)) => Ok(file),
        // A directory, FIFO, socket or device, or under `NoFollow` a
        // symlink: seen through its `O_PATH` handle, never opened.
        Ok(Found::Other(_)) => Err(ReadFault::NotRegular),
        Err(FindFault::Look(Errno::NOENT | Errno::NOTDIR)) => Err(ReadFault::Missing),
        // A directory above it the running user may not search refuses the
        // look; mode 000 passes the `O_PATH` look and refuses the read.
        Err(
            FindFault::Look(Errno::ACCESS | Errno::PERM)
            | FindFault::Reopen(Errno::ACCESS | Errno::PERM),
        ) => Err(ReadFault::PermissionDenied),
        // `O_PATH | O_NOFOLLOW` opens a symlink at the last component as
        // itself, so ELOOP is a loop under either policy.
        Err(FindFault::Look(Errno::LOOP)) => Err(ReadFault::SymlinkLoop),
        Err(FindFault::Changed) => Err(ReadFault::Changed),
        Err(FindFault::Look(_) | FindFault::Reopen(_) | FindFault::Stat) => Err(ReadFault::Io),
    }
}

/// Off Linux there is no `O_PATH` open, so nothing is read.
#[cfg(not(target_os = "linux"))]
fn open(path: &Path, _links: Links) -> Result<std::fs::File, ReadFault> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(ReadFault::Missing),
        _ => Err(ReadFault::Io),
    }
}

/// Whether two `fstat`s of one descriptor saw the same version of the file.
#[cfg(unix)]
fn same_version(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(not(unix))]
fn same_version(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.len() == after.len() && before.modified().ok() == after.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::no_follow::test_support::returns_promptly;

    fn read_in_thread(path: PathBuf, max: u64, links: Links) -> Result<Vec<u8>, ReadFault> {
        returns_promptly(move || read_bounded(&path, max, links))
    }

    #[test]
    fn a_regular_file_within_the_cap_reads_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("EPOCH");
        std::fs::write(&path, b"3\n").unwrap();
        for links in [Links::Follow, Links::NoFollow] {
            assert_eq!(read_in_thread(path.clone(), 2, links), Ok(b"3\n".to_vec()));
        }
        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(read_in_thread(empty, 0, Links::Follow), Ok(Vec::new()));
    }

    #[test]
    fn a_missing_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        for path in [dir.path().join("absent"), file.join("below-a-file")] {
            for links in [Links::Follow, Links::NoFollow] {
                assert_eq!(
                    read_in_thread(path.clone(), 64, links),
                    Err(ReadFault::Missing),
                    "{}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn an_oversize_file_is_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CATALOG");
        std::fs::write(&path, vec![b'a'; 129]).unwrap();
        assert_eq!(
            read_in_thread(path.clone(), 128, Links::Follow),
            Err(ReadFault::TooLarge)
        );
        assert_eq!(read_in_thread(path, 129, Links::Follow).unwrap().len(), 129);
    }

    #[test]
    fn a_directory_is_not_regular() {
        let dir = tempfile::tempdir().unwrap();
        for links in [Links::Follow, Links::NoFollow] {
            assert_eq!(
                read_in_thread(dir.path().to_owned(), 64, links),
                Err(ReadFault::NotRegular)
            );
        }
    }

    /// A FIFO nobody writes to: a plain open would wait forever. The read
    /// returns at once, under either link policy.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_returns_promptly_as_not_regular() {
        use crate::ingest::no_follow::test_support::make_fifo;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("trawld.toml");
        make_fifo(&fifo);
        for links in [Links::Follow, Links::NoFollow] {
            assert_eq!(
                read_in_thread(fifo.clone(), cap::CONFIG, links),
                Err(ReadFault::NotRegular)
            );
        }
        // Reached through a symlink, as an operator path may be.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        assert_eq!(
            read_in_thread(link, cap::CONFIG, Links::Follow),
            Err(ReadFault::NotRegular)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_returns_promptly_as_a_loop() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert_eq!(
            read_in_thread(a.clone(), 64, Links::Follow),
            Err(ReadFault::SymlinkLoop)
        );
        // Under no-follow the last component is itself a symlink.
        assert_eq!(
            read_in_thread(a.clone(), 64, Links::NoFollow),
            Err(ReadFault::NotRegular)
        );
        // A loop earlier in the path is a loop under either policy.
        for links in [Links::Follow, Links::NoFollow] {
            assert_eq!(
                read_in_thread(a.join("EPOCH"), 64, links),
                Err(ReadFault::SymlinkLoop)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_followed_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cert.pem");
        std::fs::write(&target, b"pem").unwrap();
        let link = dir.path().join("tls.crt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            read_in_thread(link.clone(), cap::CERT, Links::Follow),
            Ok(b"pem".to_vec())
        );
        assert_eq!(
            read_in_thread(link, cap::CERT, Links::NoFollow),
            Err(ReadFault::NotRegular)
        );
    }

    /// Mode 000 refuses the open to an unprivileged user. A privileged
    /// user (root, or one holding `CAP_DAC_OVERRIDE`) reads it anyway; the
    /// test asserts whichever the running user is, never neither.
    #[cfg(unix)]
    #[test]
    fn mode_000_is_permission_denied() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.pem");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let privileged = std::fs::File::open(&path).is_ok();
        let result = read_in_thread(path.clone(), cap::KEY, Links::Follow);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        if privileged {
            assert_eq!(result, Ok(b"secret".to_vec()));
        } else {
            assert_eq!(result, Err(ReadFault::PermissionDenied));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_device_is_not_regular() {
        assert_eq!(
            read_in_thread(PathBuf::from("/dev/zero"), 64, Links::Follow),
            Err(ReadFault::NotRegular)
        );
    }

    /// A reopen that records that it ran, then opens the handle's object
    /// for reading as a plain open would: without `O_NONBLOCK`, so on a
    /// FIFO with no writer it never returns.
    #[cfg(target_os = "linux")]
    fn recording_blocking_reopen(
        reached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> impl FnOnce(BorrowedFd<'_>) -> rustix::io::Result<OwnedFd> {
        move |handle| {
            reached.store(true, std::sync::atomic::Ordering::SeqCst);
            rustix::fs::open(
                format!("/proc/self/fd/{}", handle.as_raw_fd()),
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
        }
    }

    /// Run [`open_with`] on `path` with [`recording_blocking_reopen`], on
    /// its own thread, and return its result and whether the reopen ran.
    #[cfg(target_os = "linux")]
    fn open_recorded(path: PathBuf, links: Links) -> (Result<(), ReadFault>, bool) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let reached = Arc::new(AtomicBool::new(false));
        let reopen = recording_blocking_reopen(Arc::clone(&reached));
        let result = returns_promptly(move || open_with(&path, links, reopen).map(drop));
        (result, reached.load(Ordering::SeqCst))
    }

    /// A character device is refused on its `O_PATH` handle's type: the
    /// open that would reach its driver never runs. The one descriptor the
    /// doctor ever held on it is that `O_PATH` handle.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_device_is_refused_before_it_is_opened() {
        for device in ["/dev/null", "/dev/zero"] {
            for links in [Links::Follow, Links::NoFollow] {
                assert_eq!(
                    open_recorded(PathBuf::from(device), links),
                    (Err(ReadFault::NotRegular), false),
                    "{device}"
                );
                let Ok(Found::Other(handle)) = find_at(rustix::fs::CWD, Path::new(device), links)
                else {
                    panic!("{device} is not a regular file");
                };
                let flags = rustix::fs::fcntl_getfl(&handle).unwrap();
                assert!(
                    flags.contains(rustix::fs::OFlags::PATH),
                    "{device}: {flags:?}"
                );
            }
        }
    }

    /// A FIFO with no writer returns at once even though the `O_PATH` step
    /// has no `O_NONBLOCK` and the reopen here blocks: only a regular file
    /// is ever reopened.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_is_refused_before_it_is_opened() {
        use crate::ingest::no_follow::test_support::make_fifo;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("REPIN");
        make_fifo(&fifo);
        for links in [Links::Follow, Links::NoFollow] {
            assert_eq!(
                open_recorded(fifo.clone(), links),
                (Err(ReadFault::NotRegular), false)
            );
        }
        // The same reopen does reach a regular file.
        let file = dir.path().join("EPOCH");
        std::fs::write(&file, b"3\n").unwrap();
        assert_eq!(open_recorded(file, Links::NoFollow), (Ok(()), true));
    }

    /// Under `NoFollow` a symlink is its own `O_PATH` handle: refused
    /// without opening its target, even a regular file.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_symlink_under_no_follow_is_refused_before_its_target_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("EPOCH.real");
        std::fs::write(&target, b"3\n").unwrap();
        let link = dir.path().join("EPOCH");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            open_recorded(link.clone(), Links::NoFollow),
            (Err(ReadFault::NotRegular), false)
        );
        assert_eq!(open_recorded(link, Links::Follow), (Ok(()), true));
        let to_device = dir.path().join("to-device");
        std::os::unix::fs::symlink("/dev/null", &to_device).unwrap();
        for links in [Links::Follow, Links::NoFollow] {
            assert_eq!(
                open_recorded(to_device.clone(), links),
                (Err(ReadFault::NotRegular), false)
            );
        }
    }

    /// The object opened for reading must be the one looked at: a reopen
    /// that reaches another file is `Changed`, and a reopen that fails as
    /// an unmounted `/proc` does is `Io`, not `Missing`.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_reopened_object_must_be_the_one_looked_at() {
        use rustix::fs::{Mode, OFlags};
        let dir = tempfile::tempdir().unwrap();
        let looked = dir.path().join("CATALOG");
        let other = dir.path().join("other");
        std::fs::write(&looked, b"a\n").unwrap();
        std::fs::write(&other, b"a\n").unwrap();
        let swapped = {
            let other = other.clone();
            move |_: BorrowedFd<'_>| {
                rustix::fs::open(&other, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            }
        };
        assert_eq!(
            open_with(&looked, Links::NoFollow, swapped).map(drop),
            Err(ReadFault::Changed)
        );
        let no_proc = |_: BorrowedFd<'_>| Err(rustix::io::Errno::NOENT);
        assert_eq!(
            open_with(&looked, Links::NoFollow, no_proc).map(drop),
            Err(ReadFault::Io)
        );
    }

    /// The decoders' opener hands back a regular file open for reading,
    /// and anything else as a handle that answers `fstat` only, so the
    /// decoder refuses it by type without it ever being opened.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_decoder_opener_opens_only_a_regular_file() {
        use std::io::Read as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("marker");
        std::fs::write(&file, b"body").unwrap();
        let mut body = String::new();
        open_for_decoder(&file)
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "body");

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        for path in [PathBuf::from("/dev/null"), link, dir.path().to_owned()] {
            let mut handle = open_for_decoder(&path).unwrap();
            assert!(!handle.metadata().unwrap().is_file(), "{}", path.display());
            assert!(handle.read(&mut [0; 8]).is_err(), "{}", path.display());
        }
        let absent = open_for_decoder(&dir.path().join("absent")).unwrap_err();
        assert_eq!(absent.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn the_async_read_returns_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("EPOCH");
        std::fs::write(&path, b"3").unwrap();
        assert_eq!(
            read(path, cap::EPOCH, Links::NoFollow).await,
            Ok(b"3".to_vec())
        );
    }
}
