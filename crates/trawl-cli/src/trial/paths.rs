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
//!
//! Those two checks read metadata and keep no handle, so the files below
//! are reached by path again later. That is sound only if no other user can
//! swap a directory on the way there. Every directory above the root, from
//! `/` down, must therefore be owned by root or the current user, and must
//! not be group- or other-writable unless it has the sticky bit (as `/tmp`
//! does). The walk follows a symlinked ancestor, such as a dotfile manager's
//! `~/.local`, and checks the directories it resolves to. The link itself
//! must be owned by root or the current user too, because in a sticky
//! directory the link's owner can re-point it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, Metadata};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::TrialError;

/// Directory mode for the state root and the trial directory.
const DIR_MODE: u32 = 0o700;

/// Symlinks the ancestry walk follows before giving up, as the kernel's
/// own `ELOOP` limit.
const MAX_SYMLINK_HOPS: usize = 40;

/// The trial's host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialPaths {
    /// `$XDG_STATE_HOME`: every directory from `/` down to it must be
    /// trusted.
    base: PathBuf,
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
            base,
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
    pub fn ingest_token_file(&self) -> PathBuf {
        self.dir.join("ingest.token")
    }

    /// Create the state root (0700) if absent and verify it: a real
    /// directory, owned by this user, with no group or other access.
    /// Missing ancestors are created 0700 too, and only after the existing
    /// ones pass the ancestry rule, so nothing is created under a
    /// directory another user controls.
    pub fn ensure_root(&self) -> Result<(), TrialError> {
        if !check_ancestry(&self.base)? {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(&self.base)
                .map_err(|e| TrialError::io("create", &self.base, e))?;
            if !check_ancestry(&self.base)? {
                return Err(TrialError::io(
                    "create",
                    &self.base,
                    std::io::ErrorKind::NotFound.into(),
                ));
            }
        }
        secure_dir(&self.root)
    }

    /// [`Self::ensure_root`], then the same for the trial directory.
    pub fn ensure_dir(&self) -> Result<(), TrialError> {
        self.ensure_root()?;
        secure_dir(&self.dir)
    }

    /// Verify the ancestry, the root, and the trial directory without
    /// creating anything. `Ok(false)` when there is no trial directory.
    pub fn check_dir(&self) -> Result<bool, TrialError> {
        if !check_ancestry(&self.base)? {
            return Ok(false);
        }
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

/// One step of the ancestry walk.
enum Step {
    Root,
    Parent,
    Name(OsString),
}

/// The steps of `path`, as the kernel would take them.
fn steps(path: &Path) -> impl DoubleEndedIterator<Item = Step> + '_ {
    path.components().filter_map(|component| match component {
        Component::RootDir => Some(Step::Root),
        Component::ParentDir => Some(Step::Parent),
        Component::Normal(name) => Some(Step::Name(name.to_owned())),
        // Unix paths have no prefix; `.` changes nothing.
        Component::Prefix(_) | Component::CurDir => None,
    })
}

/// [`walk_ancestry`] for this process's user.
fn check_ancestry(path: &Path) -> Result<bool, TrialError> {
    walk_ancestry(path, euid())
}

/// Resolve absolute `path` one component at a time, from `/`, and verify
/// every directory and symlink on the way with [`verify_ancestor`].
/// `Ok(false)` when some component does not exist yet; everything above
/// it has passed.
///
/// A walk rather than `fs::canonicalize`: canonicalizing returns only the
/// final directory and hides the links it followed, and each link's owner
/// matters as much as its target's.
fn walk_ancestry(path: &Path, me: u32) -> Result<bool, TrialError> {
    let mut pending: VecDeque<Step> = steps(path).collect();
    if !matches!(pending.front(), Some(Step::Root)) {
        return Err(TrialError::NotPrivateFile {
            path: path.to_owned(),
            reason: "trial state needs an absolute path",
        });
    }
    // Always a real directory, reached without a symlink, so `pop` is `..`.
    let mut resolved = PathBuf::new();
    let mut hops = 0;
    while let Some(step) = pending.pop_front() {
        let name = match step {
            Step::Root => {
                resolved = PathBuf::from("/");
                let meta = fs::symlink_metadata(&resolved)
                    .map_err(|e| TrialError::io("inspect", &resolved, e))?;
                verify_ancestor(&resolved, &meta, me)?;
                continue;
            }
            Step::Parent => {
                resolved.pop();
                continue;
            }
            Step::Name(name) => name,
        };
        let next = resolved.join(name);
        let meta = match fs::symlink_metadata(&next) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(TrialError::io("inspect", next, e)),
        };
        verify_ancestor(&next, &meta, me)?;
        if meta.file_type().is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                return Err(TrialError::io(
                    "resolve",
                    next,
                    std::io::Error::from_raw_os_error(nix::libc::ELOOP),
                ));
            }
            let target = fs::read_link(&next).map_err(|e| TrialError::io("resolve", &next, e))?;
            // A relative target resolves from the link's own directory,
            // which is `resolved` as it stands.
            for step in steps(&target).rev() {
                pending.push_front(step);
            }
        } else {
            resolved = next;
        }
    }
    Ok(true)
}

/// The ancestry rule for one directory or symlink above the state root:
/// owned by root or `me`, and for a directory, no group or other write
/// unless the sticky bit is set. Anything else is refused by name.
fn verify_ancestor(path: &Path, meta: &Metadata, me: u32) -> Result<(), TrialError> {
    let path = path.to_owned();
    let file_type = meta.file_type();
    if !file_type.is_dir() && !file_type.is_symlink() {
        return Err(TrialError::NotADirectory { path });
    }
    if meta.uid() != 0 && meta.uid() != me {
        return Err(TrialError::ForeignOwner {
            path,
            owner: meta.uid(),
            me,
        });
    }
    let mode = meta.mode();
    if file_type.is_dir() && mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Err(TrialError::NotPrivateFile {
            path,
            reason: "group or other can write to it and it has no sticky bit, \
                     so another user could replace the trial state below it",
        });
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
    read_owned(path, limit, 0o077, "group or other can access it")
}

/// Read a public file the trial wrote, such as `ca.pem` (0644):
/// [`read_private`], except that group and other may read it. They still
/// may not write it.
pub fn read_public(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, TrialError> {
    read_owned(path, limit, 0o022, "group or other can write to it")
}

/// The shared read: refuse the file when any `forbidden` mode bit is set.
fn read_owned(
    path: &Path,
    limit: u64,
    forbidden: u32,
    loose: &'static str,
) -> Result<Option<Vec<u8>>, TrialError> {
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
    if meta.mode() & forbidden != 0 {
        return Err(refuse(loose));
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

    /// A directory at `path` with exactly `mode`.
    fn dir_at(path: &Path, mode: u32) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The finding this rule closes: another user who can rename entries
    /// in an ancestor can swap the trial directory between two reads.
    #[test]
    fn an_ancestor_writable_without_the_sticky_bit_is_refused() {
        for mode in [0o770, 0o777, 0o757] {
            let tmp = tempfile::tempdir().unwrap();
            let shared = tmp.path().join("shared");
            dir_at(&shared, mode);
            let paths = paths_in(&shared.join("state"));

            let err = paths.ensure_dir().expect_err("a loose ancestor");
            assert!(
                matches!(&err, TrialError::NotPrivateFile { path, .. } if *path == shared),
                "{mode:o}: {err:?}"
            );
            assert!(err.to_string().contains("no sticky bit"), "{err}");
            assert!(
                err.to_string().contains(&shared.display().to_string()),
                "the refusal names the directory: {err}"
            );
            assert!(
                !shared.join("state").exists(),
                "{mode:o}: nothing is created below a loose ancestor"
            );
            assert!(
                matches!(paths.check_dir(), Err(TrialError::NotPrivateFile { .. })),
                "{mode:o}: the -p trial read path refuses too"
            );
        }
    }

    /// A loose directory further up than the immediate parent counts too,
    /// and so does a trial that already exists below it.
    #[test]
    fn a_loose_ancestor_is_refused_even_over_an_existing_trial() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        dir_at(&shared, 0o700);
        let paths = paths_in(&shared.join("a/b/state"));
        paths.ensure_dir().unwrap();
        assert!(paths.check_dir().unwrap());

        fs::set_permissions(&shared, fs::Permissions::from_mode(0o775)).unwrap();
        for result in [
            paths.ensure_root(),
            paths.ensure_dir(),
            paths.check_dir().map(|_| ()),
        ] {
            assert!(
                matches!(&result, Err(TrialError::NotPrivateFile { path, .. }) if *path == shared),
                "{result:?}"
            );
        }
    }

    /// `/tmp`: world-writable, but the sticky bit stops other users from
    /// renaming entries they do not own.
    #[test]
    fn a_sticky_world_writable_ancestor_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let public = tmp.path().join("public");
        dir_at(&public, 0o1777);
        let paths = paths_in(&public.join("state"));
        paths.ensure_dir().expect("a sticky ancestor is trusted");
        assert!(paths.check_dir().unwrap());
    }

    /// A dotfile manager's symlinked `~/.local`: the link is followed and
    /// its target's directories are what the rule checks.
    #[test]
    fn a_symlinked_ancestor_with_a_trusted_target_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        dir_at(&dotfiles, 0o755);
        let home = tmp.path().join("home");
        dir_at(&home, 0o700);
        // One absolute link and one relative link with `..`.
        std::os::unix::fs::symlink(&dotfiles, home.join("abs")).unwrap();
        std::os::unix::fs::symlink("../dotfiles", home.join("rel")).unwrap();

        for link in ["abs", "rel"] {
            let paths = paths_in(&home.join(link).join(format!("{link}-state")));
            paths.ensure_dir().expect(link);
            assert!(paths.check_dir().unwrap(), "{link}");
            assert!(
                dotfiles.join(format!("{link}-state/trawl/trial")).is_dir(),
                "{link}: the state lands in the link's target"
            );
        }
    }

    /// The link is followed, so a loose target is refused by its own name.
    #[test]
    fn a_symlinked_ancestor_is_judged_by_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let loose = tmp.path().join("loose");
        dir_at(&loose, 0o777);
        let home = tmp.path().join("home");
        dir_at(&home, 0o700);
        std::os::unix::fs::symlink(&loose, home.join("link")).unwrap();

        let paths = paths_in(&home.join("link/state"));
        let err = paths.ensure_dir().expect_err("a loose target");
        assert!(
            matches!(&err, TrialError::NotPrivateFile { path, .. } if *path == loose),
            "{err:?}"
        );
        assert!(!loose.join("state").exists());
    }

    /// Tests run as one user, so the owner rule is walked against a
    /// different expected uid: root-owned `/` and `/tmp` pass, and the
    /// first directory the test user owns is refused by name.
    #[test]
    fn an_ancestor_owned_by_another_user_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("state");
        dir_at(&base, 0o700);
        let owner = fs::symlink_metadata(tmp.path()).unwrap().uid();
        assert_ne!(owner, 0, "the suite does not run as root");
        assert!(walk_ancestry(&base, owner).unwrap());

        let err = walk_ancestry(&base, owner.wrapping_add(1)).unwrap_err();
        assert!(
            matches!(&err, TrialError::ForeignOwner { path, owner: o, .. }
                if o == &owner && tmp.path().starts_with(path)),
            "{err:?}"
        );
    }

    /// A link another user owns can be re-pointed in a sticky directory,
    /// so its owner is checked like a directory's.
    #[test]
    fn a_symlink_owned_by_another_user_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        dir_at(&target, 0o700);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let meta = fs::symlink_metadata(&link).unwrap();
        verify_ancestor(&link, &meta, meta.uid()).expect("own link passes");
        assert!(matches!(
            verify_ancestor(&link, &meta, meta.uid().wrapping_add(1)),
            Err(TrialError::ForeignOwner { .. })
        ));
    }

    #[test]
    fn a_symlink_loop_in_the_ancestry_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(tmp.path().join("b"), tmp.path().join("a")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a"), tmp.path().join("b")).unwrap();
        let paths = paths_in(&tmp.path().join("a/state"));
        assert!(matches!(
            paths.ensure_dir(),
            Err(TrialError::Io {
                action: "resolve",
                ..
            })
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

    #[test]
    fn read_public_lets_others_read_but_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(&tmp.path().join("state"));
        paths.ensure_dir().unwrap();
        let path = paths.ca_file();
        assert_eq!(read_public(&path, 64).unwrap(), None);

        write_private(&path, b"pem", 0o644).unwrap();
        assert_eq!(
            read_public(&path, 64).unwrap().as_deref(),
            Some(&b"pem"[..])
        );
        assert!(
            matches!(
                read_private(&path, 64),
                Err(TrialError::NotPrivateFile { .. })
            ),
            "control: the private read refuses the same file"
        );

        for mode in [0o664, 0o646] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                matches!(
                    read_public(&path, 64),
                    Err(TrialError::NotPrivateFile { .. })
                ),
                "{mode:o}"
            );
        }

        let link = paths.dir.join("link.pem");
        let target = tmp.path().join("target.pem");
        fs::write(&target, "pem").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_public(&link, 64).is_err(), "a symlink is not followed");
    }
}
