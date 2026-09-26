// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where the trial keeps its host state, and the owner-only file rules.
//!
//! ```text
//! $XDG_STATE_HOME/trawl/      root, 0700
//!     trial.lock              lifecycle lock, never unlinked
//!     trial/                  dir, 0700, deleted by `down`
//!         state.json          0600
//!         ca.pem              0644, public
//!         operator.token      0600
//!         ingest.token        0600
//! ```
//!
//! The lock sits beside the trial directory rather than inside it: `down`
//! deletes the directory while holding the lock, and a lock file inside it
//! would be unlinked under a waiter, which then locks an orphaned inode
//! while the next arrival creates a fresh one.
//!
//! Both directories must be real directories owned by the current user
//! with no group or other access. A symlink at either path is refused, not
//! followed, and a looser directory is refused rather than re-moded,
//! because trawl did not create it that way. The pattern follows fleet-dev's
//! `state_root` and `secure_directory`; fleet-dev is a development
//! controller, so it is copied rather than depended on.

use std::fs::{self, DirBuilder, File, Metadata};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::TrialError;

/// Directory mode for the state root and the trial directory.
const DIR_MODE: u32 = 0o700;

/// The trial's host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialPaths {
    /// `$XDG_STATE_HOME/trawl`.
    pub root: PathBuf,
    /// `root/trial`: everything `down` deletes.
    pub dir: PathBuf,
    /// `root/trial.lock`: outside `dir`, so it survives `down`.
    pub lock: PathBuf,
}

impl TrialPaths {
    /// Resolve the paths from `XDG_STATE_HOME` and `HOME`.
    ///
    /// Per the XDG Base Directory spec, an unset, empty, or relative
    /// `XDG_STATE_HOME` is ignored in favour of `$HOME/.local/state`.
    pub fn resolve(xdg_state_home: Option<&Path>, home: Option<&Path>) -> Result<Self, TrialError> {
        let base = match xdg_state_home.filter(|p| p.is_absolute()) {
            Some(xdg) => xdg.to_owned(),
            None => home
                .filter(|p| p.is_absolute())
                .map(|home| home.join(".local/state"))
                .ok_or(TrialError::NoStateHome)?,
        };
        let root = base.join("trawl");
        Ok(Self {
            dir: root.join("trial"),
            lock: root.join("trial.lock"),
            root,
        })
    }

    /// Resolve the paths from this process's environment.
    pub fn from_env() -> Result<Self, TrialError> {
        let xdg = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::resolve(xdg.as_deref(), home.as_deref())
    }

    /// `state.json`.
    pub fn state_file(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// The trial's public certificate, pinned by `-p trial`.
    pub fn ca_file(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }

    /// The operator token: the token and one newline.
    pub fn operator_token_file(&self) -> PathBuf {
        self.dir.join("operator.token")
    }

    /// The ingest token: the token and one newline.
    #[cfg_attr(not(test), expect(dead_code, reason = "up writes it"))]
    pub fn ingest_token_file(&self) -> PathBuf {
        self.dir.join("ingest.token")
    }

    /// Create the state root (0700) if absent and verify it: a real
    /// directory, owned by this user, with no group or other access.
    /// Missing ancestors are created 0700 too.
    #[cfg_attr(not(test), expect(dead_code, reason = "up takes the lock under it"))]
    pub fn ensure_root(&self) -> Result<(), TrialError> {
        if let Some(parent) = self.root.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(parent)
                .map_err(|e| TrialError::io("create", parent, e))?;
        }
        secure_dir(&self.root)
    }

    /// [`Self::ensure_root`], then the same for the trial directory.
    #[cfg_attr(not(test), expect(dead_code, reason = "up writes the trial directory"))]
    pub fn ensure_dir(&self) -> Result<(), TrialError> {
        self.ensure_root()?;
        secure_dir(&self.dir)
    }

    /// Verify the root and the trial directory without creating either.
    /// `Ok(false)` when there is no trial directory.
    pub fn check_dir(&self) -> Result<bool, TrialError> {
        for path in [&self.root, &self.dir] {
            match fs::symlink_metadata(path) {
                Ok(meta) => verify_dir(path, &meta, euid())?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(TrialError::io("inspect", path, e)),
            }
        }
        Ok(true)
    }
}

fn euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// Create `path` at 0700 if it is absent, then verify it. A symlink at
/// `path` is refused whether it was there before or appeared in between.
fn secure_dir(path: &Path) -> Result<(), TrialError> {
    match DirBuilder::new().mode(DIR_MODE).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(TrialError::io("create", path, e)),
    }
    let meta = fs::symlink_metadata(path).map_err(|e| TrialError::io("inspect", path, e))?;
    verify_dir(path, &meta, euid())
}

/// The directory rule, over metadata read without following a symlink.
fn verify_dir(path: &Path, meta: &Metadata, me: u32) -> Result<(), TrialError> {
    let path = path.to_owned();
    if meta.file_type().is_symlink() {
        return Err(TrialError::Symlink { path });
    }
    if !meta.is_dir() {
        return Err(TrialError::NotADirectory { path });
    }
    if meta.uid() != me {
        return Err(TrialError::ForeignOwner {
            path,
            owner: meta.uid(),
            me,
        });
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(TrialError::LooseMode { path, mode });
    }
    Ok(())
}

/// Distinguishes temporary files written concurrently by one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `bytes`, atomically, at `mode`.
///
/// The bytes go to a new file in the same directory, opened
/// `O_CREAT | O_EXCL | O_NOFOLLOW` at the final mode, then fsynced and
/// renamed over `path`, and the directory is fsynced. A reader sees the old
/// file or the new one, never a partial write. A symlink at `path` is
/// replaced, not followed. The caller verifies the directory first.
#[cfg_attr(not(test), expect(dead_code, reason = "up writes state and tokens"))]
pub fn write_private(path: &Path, bytes: &[u8], mode: u32) -> Result<(), TrialError> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| TrialError::NotPrivateFile {
            path: path.to_owned(),
            reason: "it has no parent directory",
        })?;
    let name = path.file_name().ok_or_else(|| TrialError::NotPrivateFile {
        path: path.to_owned(),
        reason: "it has no file name",
    })?;

    let (tmp, mut file) = loop {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(format!(".{}.{n}.tmp", std::process::id()));
        let tmp = dir.join(tmp_name);
        match trawl_config::fs::open_with_mode(&tmp, mode, |opts| {
            opts.write(true).create_new(true);
        }) {
            Ok((file, None)) => break (tmp, file),
            Ok((file, Some(chmod))) => {
                drop(file);
                let _ = fs::remove_file(&tmp);
                return Err(TrialError::io("set the mode of", tmp, chmod));
            }
            // Left by a crashed writer with a recycled pid: pick another name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(TrialError::io("create", tmp, e)),
        }
    };

    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| TrialError::io("write", &tmp, e))
        .and_then(|()| fs::rename(&tmp, path).map_err(|e| TrialError::io("replace", path, e)));
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| TrialError::io("sync", dir, e))
}

/// Read an owner-only file the trial wrote: `Ok(None)` when absent.
///
/// The open does not follow a symlink, and the file must be a regular
/// file, owned by this user, with no group or other access, and at most
/// `limit` bytes.
pub fn read_private(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, TrialError> {
    // Not `open_with_mode`: that re-modes the file after opening it, and a
    // read must leave the mode as it found it.
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(TrialError::io("open", path, e)),
    };
    let meta = file
        .metadata()
        .map_err(|e| TrialError::io("inspect", path, e))?;
    let refuse = |reason| TrialError::NotPrivateFile {
        path: path.to_owned(),
        reason,
    };
    if !meta.is_file() {
        return Err(refuse("it is not a regular file"));
    }
    if meta.uid() != euid() {
        return Err(refuse("it is owned by another user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refuse("group or other can access it"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| TrialError::io("read", path, e))?;
    if bytes.len() as u64 > limit {
        return Err(refuse("it is larger than trawl ever writes"));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn paths_in(base: &Path) -> TrialPaths {
        TrialPaths::resolve(Some(base), None).unwrap()
    }

    #[test]
    fn xdg_state_home_places_the_root() {
        let paths = TrialPaths::resolve(Some(Path::new("/xdg")), Some(Path::new("/home/u")))
            .expect("resolve");
        assert_eq!(paths.root, Path::new("/xdg/trawl"));
        assert_eq!(paths.dir, Path::new("/xdg/trawl/trial"));
        assert_eq!(paths.lock, Path::new("/xdg/trawl/trial.lock"));
    }

    #[test]
    fn unset_empty_or_relative_xdg_falls_back_to_home() {
        let home = Some(Path::new("/home/u"));
        for xdg in [None, Some(Path::new("")), Some(Path::new("state"))] {
            let paths = TrialPaths::resolve(xdg, home).expect("resolve");
            assert_eq!(
                paths.root,
                Path::new("/home/u/.local/state/trawl"),
                "{xdg:?}"
            );
        }
    }

    #[test]
    fn no_usable_base_is_an_error() {
        for home in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            assert!(matches!(
                TrialPaths::resolve(Some(Path::new("")), home),
                Err(TrialError::NoStateHome)
            ));
        }
    }

    /// `down` deletes `dir` while holding the lock, so the lock must not
    /// live under it.
    #[test]
    fn the_lock_lives_outside_the_trial_directory() {
        let paths = paths_in(Path::new("/xdg"));
        assert_ne!(paths.lock.parent(), Some(paths.dir.as_path()));
        assert!(!paths.lock.starts_with(&paths.dir));
        assert_eq!(paths.lock.parent(), Some(paths.root.as_path()));
    }

    #[test]
    fn ensure_dir_creates_both_directories_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        // The XDG base itself does not exist yet.
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_dir().expect("ensure_dir");
        assert_eq!(mode_of(&paths.root), 0o700);
        assert_eq!(mode_of(&paths.dir), 0o700);
        assert_eq!(mode_of(paths.root.parent().unwrap()), 0o700);
        // Idempotent.
        paths.ensure_dir().expect("second ensure_dir");
        assert!(paths.check_dir().unwrap());
    }

    #[test]
    fn a_symlinked_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let base = tmp.path().join("state");
        fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(&target, base.join("trawl")).unwrap();

        let paths = paths_in(&base);
        let err = paths
            .ensure_dir()
            .expect_err("a symlinked root must be refused");
        assert!(
            matches!(&err, TrialError::Symlink { path } if *path == paths.root),
            "{err:?}"
        );
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(
            fs::read_dir(&target).unwrap().next().is_none(),
            "nothing may be created through the link"
        );
        assert!(matches!(paths.check_dir(), Err(TrialError::Symlink { .. })));
    }

    #[test]
    fn a_symlinked_trial_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_root().unwrap();
        let target = tmp.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, &paths.dir).unwrap();

        let err = paths
            .ensure_dir()
            .expect_err("a symlinked dir must be refused");
        assert!(
            matches!(&err, TrialError::Symlink { path } if *path == paths.dir),
            "{err:?}"
        );
        assert!(matches!(paths.check_dir(), Err(TrialError::Symlink { .. })));
    }

    #[test]
    fn a_dangling_symlink_is_refused_too() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_root().unwrap();
        std::os::unix::fs::symlink(tmp.path().join("absent"), &paths.dir).unwrap();
        assert!(matches!(
            paths.ensure_dir(),
            Err(TrialError::Symlink { .. })
        ));
        assert!(!tmp.path().join("absent").exists());
    }

    #[test]
    fn a_group_or_other_accessible_directory_is_refused_not_remoded() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_root().unwrap();

        fs::create_dir(&paths.dir).unwrap();
        fs::set_permissions(&paths.dir, fs::Permissions::from_mode(0o750)).unwrap();
        let err = paths.ensure_dir().expect_err("0750 must be refused");
        assert!(
            matches!(err, TrialError::LooseMode { mode: 0o750, .. }),
            "{err:?}"
        );
        assert_eq!(mode_of(&paths.dir), 0o750, "the mode is left as found");

        fs::set_permissions(&paths.root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            paths.ensure_root(),
            Err(TrialError::LooseMode { mode: 0o755, .. })
        ));
    }

    #[test]
    fn a_file_where_the_directory_goes_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_root().unwrap();
        fs::write(&paths.dir, "").unwrap();
        assert!(matches!(
            paths.ensure_dir(),
            Err(TrialError::NotADirectory { .. })
        ));
    }

    /// Tests run as one user, so the owner rule is checked on real
    /// metadata against a different expected uid.
    #[test]
    fn a_directory_owned_by_another_user_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let meta = fs::symlink_metadata(tmp.path()).unwrap();
        verify_dir(tmp.path(), &meta, meta.uid()).expect("own directory passes");
        let other = meta.uid().wrapping_add(1);
        assert!(matches!(
            verify_dir(tmp.path(), &meta, other),
            Err(TrialError::ForeignOwner { owner, me, .. }) if owner == meta.uid() && me == other
        ));
    }

    #[test]
    fn check_dir_reports_absence_without_creating() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        assert!(!paths.check_dir().unwrap());
        assert!(!paths.root.exists());
        paths.ensure_root().unwrap();
        assert!(!paths.check_dir().unwrap());
        assert!(!paths.dir.exists());
    }

    #[test]
    fn write_private_writes_atomically_at_the_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_dir().unwrap();
        let path = paths.dir.join("operator.token");

        write_private(&path, b"first\n", 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first\n");
        assert_eq!(mode_of(&path), 0o600);

        write_private(&path, b"second\n", 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second\n");

        let public = paths.dir.join("ca.pem");
        write_private(&public, b"pem", 0o644).unwrap();
        assert_eq!(mode_of(&public), 0o644);

        let names: Vec<_> = fs::read_dir(&paths.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "no temporary file is left: {names:?}");
    }

    #[test]
    fn write_private_replaces_a_symlink_without_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_dir().unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "untouched").unwrap();
        let path = paths.state_file();
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        write_private(&path, b"{}", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
    }

    #[test]
    fn read_private_refuses_links_and_loose_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_dir().unwrap();
        let path = paths.operator_token_file();
        assert_eq!(read_private(&path, 64).unwrap(), None);

        write_private(&path, b"tok\n", 0o600).unwrap();
        assert_eq!(
            read_private(&path, 64).unwrap().as_deref(),
            Some(&b"tok\n"[..])
        );
        assert!(matches!(
            read_private(&path, 2),
            Err(TrialError::NotPrivateFile { .. })
        ));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            read_private(&path, 64),
            Err(TrialError::NotPrivateFile { .. })
        ));
        assert_eq!(mode_of(&path), 0o640, "a read never re-modes the file");

        let link = paths.ingest_token_file();
        let target = tmp.path().join("target");
        fs::write(&target, "x").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            read_private(&link, 64).is_err(),
            "a symlink is not followed"
        );
    }
}
