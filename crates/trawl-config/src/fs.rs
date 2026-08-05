// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filesystem helpers shared by trawl's owner-only diagnostic files.
//!
//! Lives here rather than in each consumer because getting an
//! owner-only file right takes two steps that are easy to half-do:
//! `OpenOptions::mode` applies only when the file is *created*, so a
//! pre-existing looser file must additionally be `chmod`ed after the
//! open. Every trawl file that carries query text (the query debug log,
//! the TUI trace log) goes through this one helper.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Open `path` with the flags applied by `configure`, owner-restricted
/// to `mode` on Unix — set at creation, then re-applied so a
/// pre-existing looser file is tightened too. `mode` is ignored on
/// non-Unix platforms.
///
/// On Unix the open also carries `O_NOFOLLOW`, so a symlink at the
/// final path component is refused (`ELOOP`) rather than followed.
/// These paths are operator-configured, appended to, and `chmod`ed:
/// following a link planted by a local user — in a shared-writable
/// directory, or in the window a rollover's rename leaves open — would
/// append trawl's most sensitive output into a file of their choosing
/// and re-mode that file to `mode`. A log path that is *deliberately*
/// a symlink must be given as the link target instead.
///
/// The post-open `chmod` failure is *returned, not raised*: POSIX
/// `chmod` requires the caller to own the file, so a file owned by
/// another uid can open fine and still refuse to be re-moded. Whether
/// that is fatal is the caller's policy decision (`trawl-server`'s
/// query log tolerates it when the existing mode is already tight;
/// the TUI log does not), so the file and the error come back together
/// and nothing here decides for them.
pub fn open_with_mode(
    path: &Path,
    mode: u32,
    configure: impl FnOnce(&mut OpenOptions),
) -> io::Result<(File, Option<io::Error>)> {
    let mut opts = OpenOptions::new();
    configure(&mut opts);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(mode);
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = mode;

    let file = opts
        .open(path)
        .map_err(|err| describe_open_error(path, err))?;

    #[cfg(unix)]
    let chmod_error = {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .err()
    };
    #[cfg(not(unix))]
    let chmod_error = None;

    Ok((file, chmod_error))
}

/// Turn the `O_NOFOLLOW` refusal into a message an operator can act on —
/// a bare `ELOOP` on a path they configured reads like a filesystem bug.
/// Every other error is passed through untouched.
fn describe_open_error(path: &Path, err: io::Error) -> io::Error {
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::ELOOP) {
        return io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a symbolic link; trawl will not follow one for an owner-only \
                 diagnostic file (configure the link target instead)",
                path.display()
            ),
        );
    }
    #[cfg(not(unix))]
    let _ = path;
    err
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

    use super::open_with_mode;

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_the_file_at_the_requested_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("new.log");
        let (_file, chmod_error) = open_with_mode(&path, 0o600, |opts| {
            opts.create(true).append(true);
        })
        .unwrap();
        assert!(chmod_error.is_none());
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn tightens_a_pre_existing_looser_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.log");
        std::fs::write(&path, "existing").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let (_file, chmod_error) = open_with_mode(&path, 0o600, |opts| {
            opts.create(true).write(true).truncate(true);
        })
        .unwrap();
        assert!(chmod_error.is_none());
        assert_eq!(
            mode_of(&path),
            0o600,
            "mode() only applies at creation; the tighten step must run"
        );
    }

    /// A symlink at the log path is a local user's way of redirecting
    /// trawl's most sensitive output into a file they control — and, via
    /// the tighten step, of getting an arbitrary file `chmod`ed. The open
    /// must fail without touching the target at all.
    #[test]
    fn refuses_to_follow_a_symlink_at_the_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("victim");
        std::fs::write(&target, "untouched").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = tmp.path().join("planted.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = open_with_mode(&link, 0o600, |opts| {
            opts.create(true).append(true);
        })
        .expect_err("a symlinked log path must be refused, not followed");
        assert!(
            err.to_string().contains("symbolic link"),
            "the refusal must name the cause: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "untouched",
            "the link target must not be opened for append"
        );
        assert_eq!(
            mode_of(&target),
            0o644,
            "the link target's mode must not be changed"
        );
    }

    #[test]
    fn honours_the_configured_open_flags() {
        use std::io::Write as _;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("append.log");
        std::fs::write(&path, "first\n").unwrap();

        let (mut file, _) = open_with_mode(&path, 0o600, |opts| {
            opts.create(true).append(true);
        })
        .unwrap();
        writeln!(file, "second").unwrap();
        drop(file);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }
}
