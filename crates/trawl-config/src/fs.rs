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
    }
    #[cfg(not(unix))]
    let _ = mode;

    let file = opts.open(path)?;

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
