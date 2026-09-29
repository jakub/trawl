// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The doctor's one way to read a file: bounded in size and in time, and
//! never waiting on a FIFO (#269, D15).
//!
//! An installation the doctor inspects may hold anything at a path it
//! reads: a FIFO nobody writes to, a symlink loop, a file of mode 000, a
//! device, or a multi-gigabyte file. [`read_bounded`] opens with
//! `O_NONBLOCK | O_NOCTTY | O_CLOEXEC`, so opening a FIFO or a terminal
//! returns at once, then checks on the open descriptor that the file is a
//! regular file no larger than the cap before it reads, reads at most one
//! byte past the cap, and checks the descriptor again afterwards. Every
//! failure is a [`ReadFault`], never an `io::Error`, so no OS text can
//! reach the report.
//!
//! Two link policies:
//!
//! - [`Links::Follow`] for the paths an operator selected (the
//!   configuration, a certificate, a key). Those are often symlinks, as in
//!   a Kubernetes secret mount. A loop is [`ReadFault::SymlinkLoop`].
//! - [`Links::NoFollow`] for trawld's own markers under the data root,
//!   opened the way the WAL reader opens them
//!   ([`crate::ingest::no_follow::open`]). A symlink at the last component
//!   is [`ReadFault::NotRegular`].
//!
//! [`read`] runs the read on the blocking pool under [`READ_DEADLINE`], so a
//! hung filesystem costs the check its answer, not the run.

use std::io::Read as _;
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
    /// It changed while it was read.
    Changed,
    /// Any other failure to open or read it.
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

#[cfg(unix)]
fn open(path: &Path, links: Links) -> Result<std::fs::File, ReadFault> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    let opened = match links {
        Links::Follow => rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(std::fs::File::from)
        .map_err(|errno| std::io::Error::from_raw_os_error(errno.raw_os_error())),
        Links::NoFollow => crate::ingest::no_follow::open(path),
    };
    opened.map_err(|error| {
        let errno = error.raw_os_error().map(Errno::from_raw_os_error);
        match errno {
            Some(Errno::NOENT | Errno::NOTDIR) => ReadFault::Missing,
            Some(Errno::ACCESS | Errno::PERM) => ReadFault::PermissionDenied,
            // A socket file refuses `open` with ENXIO.
            Some(Errno::NXIO) => ReadFault::NotRegular,
            // Under O_NOFOLLOW, ELOOP is also what a symlink at the last
            // component gives. lstat tells the two apart without opening.
            Some(Errno::LOOP)
                if links == Links::NoFollow
                    && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()) =>
            {
                ReadFault::NotRegular
            }
            Some(Errno::LOOP) => ReadFault::SymlinkLoop,
            _ => ReadFault::Io,
        }
    })
}

/// Off Unix there is no non-blocking open, so nothing is read.
#[cfg(not(unix))]
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
