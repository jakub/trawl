// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How `trawl-web --doctor` reads a file: the configuration, the cookie
//! key and the pinned CA (D6).
//!
//! [`read`] runs [`crate::config::read_capped_file`], the reader startup
//! uses for the pin, on the blocking pool under [`READ_DEADLINE`]. The open
//! never waits on a FIFO, the type and size checks run on the opened
//! handle, and the read stops one byte past the cap. The error comes back
//! as a [`ReadFault`], a kind with no text, so no OS message can reach a
//! row.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::read_capped_file;

/// How long one read may take. A hung network volume costs the check its
/// answer, not the run.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);

/// The most bytes the doctor reads from each kind of file.
pub mod cap {
    /// The `--config` file.
    pub const CONFIG: u64 = 1024 * 1024;
    /// The `cookie_secret_path` file: one byte past a key, so a longer file
    /// is told apart from a key without reading it whole.
    pub const KEY: u64 = fleet_auth::KEY_LEN as u64 + 1;
    /// The pinned CA file, as startup caps it.
    pub const CA: u64 = crate::config::MAX_PIN_FILE_BYTES;
}

/// Why a file was not read. No variant carries text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFault {
    /// Nothing exists at the path, or a component of it is not a directory.
    Missing,
    /// The running user may not open it.
    PermissionDenied,
    /// It is not a regular file: a directory, FIFO, socket or device.
    NotRegular,
    /// It is larger than the cap.
    TooLarge,
    /// Resolving it met too many symlinks, as a loop does.
    SymlinkLoop,
    /// The read did not finish within [`READ_DEADLINE`].
    TimedOut,
    /// Any other failure to open or read it.
    Io,
}

impl ReadFault {
    /// The fault an error from [`read_capped_file`] stands for.
    fn of(error: &std::io::Error) -> Self {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return Self::SymlinkLoop;
        }
        match error.kind() {
            ErrorKind::NotFound | ErrorKind::NotADirectory => Self::Missing,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::InvalidInput => Self::NotRegular,
            ErrorKind::FileTooLarge => Self::TooLarge,
            _ => Self::Io,
        }
    }
}

/// Read the regular file at `path`, at most `cap` bytes, on the blocking
/// pool under [`READ_DEADLINE`].
///
/// A read that misses the deadline is left behind on its thread; the
/// doctor's runtime shuts down with a bounded wait, so it cannot hold the
/// run. A key read's bytes are the caller's to wrap in `Zeroizing` at
/// once.
///
/// # Errors
/// A [`ReadFault`] naming why nothing was read.
pub async fn read(path: PathBuf, cap: u64) -> Result<Vec<u8>, ReadFault> {
    let task = tokio::task::spawn_blocking(move || read_capped_file(&path, cap));
    match tokio::time::timeout(READ_DEADLINE, task).await {
        Ok(Ok(Ok(bytes))) => Ok(bytes),
        Ok(Ok(Err(error))) => Err(ReadFault::of(&error)),
        Ok(Err(_)) => Err(ReadFault::Io),
        Err(_) => Err(ReadFault::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind of file the doctor may meet at an operator's path maps to
    /// its own fault, and a FIFO is refused at once rather than waited on.
    #[cfg(unix)]
    #[tokio::test]
    async fn each_file_kind_maps_to_its_fault() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);

        std::fs::write(path("ok"), b"0123456789").unwrap();
        assert_eq!(read(path("ok"), 10).await, Ok(b"0123456789".to_vec()));
        assert_eq!(read(path("ok"), 9).await, Err(ReadFault::TooLarge));

        assert_eq!(read(path("absent"), 10).await, Err(ReadFault::Missing));
        assert_eq!(
            read(path("ok").join("below"), 10).await,
            Err(ReadFault::Missing)
        );
        assert_eq!(
            read(dir.path().to_owned(), 10).await,
            Err(ReadFault::NotRegular)
        );

        symlink(path("loop-a"), path("loop-b")).unwrap();
        symlink(path("loop-b"), path("loop-a")).unwrap();
        assert_eq!(read(path("loop-a"), 10).await, Err(ReadFault::SymlinkLoop));

        let fifo = path("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
        let started = std::time::Instant::now();
        assert_eq!(read(fifo, 10).await, Err(ReadFault::NotRegular));
        assert!(started.elapsed() < READ_DEADLINE, "the FIFO was waited on");

        // Root reads a mode-000 file, so only a non-root run can see the
        // refusal.
        if rustix::process::geteuid().as_raw() != 0 {
            std::fs::write(path("closed"), b"x").unwrap();
            std::fs::set_permissions(path("closed"), std::fs::Permissions::from_mode(0o000))
                .unwrap();
            assert_eq!(
                read(path("closed"), 10).await,
                Err(ReadFault::PermissionDenied)
            );
        }
    }
}
