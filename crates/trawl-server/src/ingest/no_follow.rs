// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opening a file that someone else can swap: never follow a symlink at
//! the last component, and never wait on a FIFO.
//!
//! Hydration reads WAL files at boot, and the WAL writer reads the
//! publication marker on every write. A check with `lstat` before a plain
//! open leaves a gap: a FIFO swapped in there blocks the open until a
//! writer comes, and a symlink swapped in is followed. [`open`] closes the
//! gap on every Unix host, Linux and macOS alike. The caller checks the
//! file type with `fstat` on the descriptor it returns, never on the path.

use std::fs::File;
use std::io;
use std::path::Path;

/// Open `path` read-only with `O_NOFOLLOW`, and `O_NONBLOCK` so that
/// opening a FIFO returns at once instead of waiting for a writer. A
/// symlink at the last component fails; see [`is_symlink_refusal`].
#[cfg(unix)]
pub(crate) fn open(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

/// Off Unix there is no `O_NOFOLLOW | O_NONBLOCK` open, and emulating it
/// with `lstat` leaves the gap. So no file is opened: a missing file is
/// still `NotFound`, and any other is `Unsupported`, which each caller
/// treats as a file it cannot read.
#[cfg(not(unix))]
pub(crate) fn open(path: &Path) -> io::Result<File> {
    std::fs::symlink_metadata(path)?;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "files that can be swapped are opened only on Unix",
    ))
}

/// Whether `error` is [`open`] refusing a symlink at the last component.
/// Linux and macOS both fail that open with `ELOOP`.
#[cfg(unix)]
pub(crate) fn is_symlink_refusal(error: &io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
}

/// Off Unix [`open`] opens nothing, so it refuses no symlink as such.
#[cfg(not(unix))]
pub(crate) fn is_symlink_refusal(_error: &io::Error) -> bool {
    false
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::time::Duration;

    /// Make a FIFO at `path`. A plain open to read it waits for a writer
    /// that never comes. `rustix` has no `mknodat` on Apple targets, so the
    /// tests that plant a FIFO run on Linux only.
    #[cfg(target_os = "linux")]
    pub(crate) fn make_fifo(path: &Path) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
    }

    /// Run `f` on its own thread and return its result, or fail the test if
    /// it has not returned in 10 s. A thread blocked on a FIFO never
    /// returns; it is left behind and ends with the test process.
    pub(crate) fn returns_promptly<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the call blocked, as an open that waits on a FIFO does")
    }
}
