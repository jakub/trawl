// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The doctor's one way to read a file: bounded in size and in time, and
//! never waiting on a FIFO.
//!
//! Opening some devices acts on them
//! (opening `/dev/watchdog` arms a host reset), so nothing the doctor has
//! not seen to be a regular file is ever opened for I/O.

use std::io::Read as _;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const READ_DEADLINE: Duration = Duration::from_secs(5);

pub mod cap {
    pub const CONFIG: u64 = 1024 * 1024;
    pub const EPOCH: u64 = 64;
    pub const CATALOG: u64 = 128;
    pub const REPIN: u64 = 64 * 1024;
    pub const PUBLICATION_MARKER: u64 = crate::ingest::publication_marker::MAX_MARKER_BYTES;
    pub const ROLLUP_MARKER: u64 = crate::ingest::rollup_marker::MAX_ROLLUP_MARKER_BYTES;
    pub const CERT: u64 = 1024 * 1024;
    pub const KEY: u64 = 1024 * 1024;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Links {
    Follow,
    NoFollow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFault {
    Missing,
    PermissionDenied,
    NotRegular,
    SymlinkLoop,
    TooLarge,
    Changed,
    Io,
    TimedOut,
}

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

/// A read that misses the deadline is left behind on its thread; the
/// doctor's runtime is shut down with a bounded wait, so it cannot hold the
/// process.
pub async fn read(path: PathBuf, max: u64, links: Links) -> Result<Vec<u8>, ReadFault> {
    let task = tokio::task::spawn_blocking(move || read_bounded(&path, max, links));
    match tokio::time::timeout(READ_DEADLINE, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(ReadFault::Io),
        Err(_) => Err(ReadFault::TimedOut),
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum Found {
    Regular(std::fs::File),
    Other(OwnedFd),
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindFault {
    Look(rustix::io::Errno),
    Stat,
    /// Opening the regular file for reading through `/proc/self/fd`
    /// failed. `ENOENT` here means `/proc` is not mounted, not that the
    /// file is gone.
    Reopen(rustix::io::Errno),
    Changed,
}

#[cfg(target_os = "linux")]
pub fn find_at(dir: BorrowedFd<'_>, path: &Path, links: Links) -> Result<Found, FindFault> {
    find_at_with(dir, path, links, reopen_through_proc)
}

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

/// A regular file comes back open for reading. Anything else comes back
/// as its `O_PATH` handle, which answers the decoder's `fstat` and nothing
/// else, so the decoder refuses it by type as it would have, and it was
/// never opened.
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

#[cfg(target_os = "linux")]
fn open_with(
    path: &Path,
    links: Links,
    reopen: impl FnOnce(BorrowedFd<'_>) -> rustix::io::Result<OwnedFd>,
) -> Result<std::fs::File, ReadFault> {
    use rustix::io::Errno;
    match find_at_with(rustix::fs::CWD, path, links, reopen) {
        Ok(Found::Regular(file)) => Ok(file),
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

#[cfg(not(target_os = "linux"))]
fn open(path: &Path, _links: Links) -> Result<std::fs::File, ReadFault> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(ReadFault::Missing),
        _ => Err(ReadFault::Io),
    }
}

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
        assert_eq!(
            read_in_thread(a.clone(), 64, Links::NoFollow),
            Err(ReadFault::NotRegular)
        );
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

    #[cfg(target_os = "linux")]
    fn open_recorded(path: PathBuf, links: Links) -> (Result<(), ReadFault>, bool) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let reached = Arc::new(AtomicBool::new(false));
        let reopen = recording_blocking_reopen(Arc::clone(&reached));
        let result = returns_promptly(move || open_with(&path, links, reopen).map(drop));
        (result, reached.load(Ordering::SeqCst))
    }

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
        let file = dir.path().join("EPOCH");
        std::fs::write(&file, b"3\n").unwrap();
        assert_eq!(open_recorded(file, Links::NoFollow), (Ok(()), true));
    }

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
