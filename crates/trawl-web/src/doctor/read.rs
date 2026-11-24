// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The key and pin readers expand `~` on the reading thread, under the read
//! deadline: with `HOME` unset or empty that expansion asks the user
//! database, which can stall.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{expand_tilde, read_capped_file, read_capped_secret};

pub const READ_DEADLINE: Duration = Duration::from_secs(5);

pub mod cap {
    pub const CONFIG: u64 = 1024 * 1024;
    pub const KEY: u64 = fleet_auth::KEY_LEN as u64 + 1;
    pub const CA: u64 = crate::config::MAX_PIN_FILE_BYTES;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFault {
    Missing,
    PermissionDenied,
    NotRegular,
    TooLarge,
    SymlinkLoop,
    TimedOut,
    Io,
}

impl ReadFault {
    fn of(error: &std::io::Error) -> Self {
        #[cfg(unix)]
        match error.raw_os_error() {
            Some(libc::ELOOP) => return Self::SymlinkLoop,
            // A Unix socket, which an `O_NONBLOCK` open refuses with this
            // code rather than a kind.
            Some(libc::ENXIO) => return Self::NotRegular,
            _ => {}
        }
        match error.kind() {
            ErrorKind::NotFound => Self::Missing,
            ErrorKind::NotADirectory | ErrorKind::InvalidInput => Self::NotRegular,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::FileTooLarge => Self::TooLarge,
            _ => Self::Io,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinFault {
    Empty,
    Read(ReadFault),
}

/// A read that misses the deadline is left behind on its thread; the
/// doctor's runtime shuts down with a bounded wait, so it cannot hold the
/// run. Not for a key: its bytes would cross to the async side, and a
/// late read's would be dropped unscrubbed. Use [`secret_len`].
pub async fn read(path: PathBuf, cap: u64) -> Result<Vec<u8>, ReadFault> {
    within_deadline(move || read_capped_file(&path, cap)).await
}

pub async fn read_pin(path: PathBuf) -> Result<Vec<u8>, PinFault> {
    read_pin_expanded_by(path, expand_tilde).await
}

async fn read_pin_expanded_by(
    path: PathBuf,
    expand: impl FnOnce(&Path) -> PathBuf + Send + 'static,
) -> Result<Vec<u8>, PinFault> {
    let read = within_deadline(move || {
        let expanded = expand(&path);
        if expanded.as_os_str().is_empty() {
            return Ok(None);
        }
        read_capped_file(&expanded, cap::CA).map(Some)
    })
    .await;
    match read {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(PinFault::Empty),
        Err(fault) => Err(PinFault::Read(fault)),
    }
}

pub async fn secret_len(path: PathBuf, cap: u64) -> Result<usize, ReadFault> {
    within_deadline(move || read_capped_secret(&expand_tilde(&path), cap).map(|bytes| bytes.len()))
        .await
}

async fn within_deadline<T: Send + 'static>(
    read: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, ReadFault> {
    let task = tokio::task::spawn_blocking(read);
    match tokio::time::timeout(READ_DEADLINE, task).await {
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(error))) => Err(ReadFault::of(&error)),
        Ok(Err(_)) => Err(ReadFault::Io),
        Err(_) => Err(ReadFault::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Err(ReadFault::NotRegular)
        );
        assert_eq!(
            read(dir.path().join("ok/"), 10).await,
            Err(ReadFault::NotRegular)
        );
        let _socket = std::os::unix::net::UnixListener::bind(path("sock")).unwrap();
        assert_eq!(read(path("sock"), 10).await, Err(ReadFault::NotRegular));
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

    #[tokio::test]
    async fn an_empty_expanded_pin_is_refused() {
        assert_eq!(
            read_pin_expanded_by(PathBuf::from("~"), |_| PathBuf::new()).await,
            Err(PinFault::Empty)
        );
        assert_eq!(read_pin(PathBuf::new()).await, Err(PinFault::Empty));

        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("ca.pem");
        std::fs::write(&pem, b"pem").unwrap();
        assert_eq!(
            read_pin_expanded_by(PathBuf::from("~/ca.pem"), move |_| pem).await,
            Ok(b"pem".to_vec())
        );
        assert_eq!(
            read_pin(dir.path().join("absent.pem")).await,
            Err(PinFault::Read(ReadFault::Missing))
        );
    }

    /// A key read gives the async side a length or a fault, never bytes:
    /// the signature is the assertion.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_secret_read_returns_only_its_length() {
        fn only_a_length<F: Future<Output = Result<usize, ReadFault>>>(read: F) -> F {
            read
        }

        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        let key = fleet_auth::KEY_LEN;

        for len in [0, key - 1, key, key + 1] {
            std::fs::write(path("key"), vec![7; len]).unwrap();
            assert_eq!(
                only_a_length(secret_len(path("key"), cap::KEY)).await,
                Ok(len)
            );
        }
        std::fs::write(path("key"), vec![7; key + 2]).unwrap();
        assert_eq!(
            secret_len(path("key"), cap::KEY).await,
            Err(ReadFault::TooLarge)
        );
        assert_eq!(
            secret_len(path("absent"), cap::KEY).await,
            Err(ReadFault::Missing)
        );
        assert_eq!(
            secret_len(dir.path().to_owned(), cap::KEY).await,
            Err(ReadFault::NotRegular)
        );
        let fifo = path("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
        assert_eq!(secret_len(fifo, cap::KEY).await, Err(ReadFault::NotRegular));
    }
}
