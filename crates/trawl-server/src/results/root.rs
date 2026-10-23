// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The results root, `[server] results_dir` (ADR-0057): the directory that
//! holds every captured result's file, held for the life of the process.
//!
//! ```text
//! <results_dir>/                 0700, held no-follow and flock-locked
//!   <id>.parquet                 0600, published: committed or awaiting its row
//!   .stage-<16 alnum>.parquet    0600, staged; never named by a row
//! ```
//!
//! [`ResultsRoot::open_apart_from`] holds the directory through
//! [`trawl_engine::scratch::hold_apart_from`], as ADR-0052 holds a storage
//! root: made 0700 if missing, its final component opened no-follow,
//! refused if it is, sits inside or sits above the data root, the WAL
//! directory, a repin staging root or the query scratch directory, closed
//! to its owner, and locked. Unlike query scratch it is never cleared: what
//! it holds outlives a restart. Boot reconciles it against the metadata
//! rows instead.
//!
//! Every entry is reached through the held handle, by `openat`,
//! `renameat2` and `unlinkat`, so no link in the configured path can
//! redirect a call to another directory. A file is published in ADR-0057's
//! order: staged under a fresh name in the root itself, so the rename stays
//! on one filesystem, written, synced, hashed, renamed to its final name
//! without replacing anything, and the directory synced; only then does
//! the caller commit the row. A [`StagedFile`] that is dropped before it is
//! published unlinks itself.
//!
//! Result ids are 32 lowercase hex digits. Every method checks the id
//! before it builds a name, so no id can name an entry outside the root or
//! one that is not a result.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use trawl_engine::scratch::{HeldRoot, OwnedRoot, RootRole, ScratchError};

/// Suffix of a published result's file name, after its id.
const RESULT_SUFFIX: &str = ".parquet";

/// Prefix and suffix of a staged file's name.
const STAGE_PREFIX: &str = ".stage-";
/// See [`STAGE_PREFIX`].
const STAGE_SUFFIX: &str = ".parquet";

/// Length of the random part of a staged file's name, pinned so the
/// recognizer demands exactly what [`ResultsRoot::stage`] makes.
const STAGE_RANDOM_LEN: usize = 16;

/// Length of a result id: 128 random bits as lowercase hex.
const ID_LEN: usize = 32;

/// Whether `id` is a result id: exactly 32 lowercase hex digits.
#[must_use]
pub fn is_result_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `name` is a published result's file name, `<id>.parquet`.
#[must_use]
pub fn is_result_name(name: &str) -> bool {
    result_id_of(name).is_some()
}

/// Whether `name` is a staged file's name, `.stage-<16 alnum>.parquet`, as
/// [`ResultsRoot::stage`] makes it.
#[must_use]
pub fn is_stage_name(name: &str) -> bool {
    name.strip_prefix(STAGE_PREFIX)
        .and_then(|rest| rest.strip_suffix(STAGE_SUFFIX))
        .is_some_and(|random| {
            random.len() == STAGE_RANDOM_LEN && random.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// The id a published result's file name carries.
fn result_id_of(name: &str) -> Option<&str> {
    name.strip_suffix(RESULT_SUFFIX)
        .filter(|id| is_result_id(id))
}

/// The file name of result `id`, or an `InvalidInput` error when `id` is
/// not a result id.
fn result_name(id: &str) -> io::Result<String> {
    if is_result_id(id) {
        Ok(format!("{id}{RESULT_SUFFIX}"))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a result id is 32 lowercase hex digits",
        ))
    }
}

/// A fresh staged file name.
fn stage_name() -> String {
    use rand::Rng as _;
    let random: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(STAGE_RANDOM_LEN)
        .map(char::from)
        .collect();
    format!("{STAGE_PREFIX}{random}{STAGE_SUFFIX}")
}

/// The results root, held for this process.
///
/// The lock is the [`HeldRoot`]'s: it lives as long as this value and
/// every [`StagedFile`] it handed out, since each staged file keeps a
/// duplicate of the held handle to unlink itself with.
#[derive(Debug)]
pub struct ResultsRoot {
    held: HeldRoot,
}

/// A file staged in the results root for one capture's write.
///
/// Dropping it before [`ResultsRoot::publish`] commits it unlinks it, so a
/// failed write, a client abort, a timeout or a dropped task leaves no
/// staged file behind; boot removes any that a killed process left
/// ([`ResultsRoot::remove_stages`]).
#[derive(Debug)]
pub struct StagedFile {
    /// A duplicate of the root's held handle, so the drop can unlink by
    /// descriptor wherever the value travels.
    dir: File,
    path: PathBuf,
    name: String,
    committed: bool,
}

impl StagedFile {
    /// The staged file's path, for the engine's writer, which writes into
    /// it in place and so keeps its inode and its 0600 mode.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(error) = sys::unlink(&self.dir, &self.name) {
            tracing::warn!(
                event_type = "results_stage_unlink_failed",
                path = %self.path.display(),
                error = %error,
                "could not remove an unpublished capture file; the next start removes it"
            );
        }
    }
}

/// What [`ResultsRoot::publish`] committed to the file's final name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    /// The file's size.
    pub file_bytes: u64,
    /// The blake3 digest of the whole file.
    pub digest: [u8; 32],
}

/// What [`ResultsRoot::verify`] found at a result's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    /// The file is there and its digest matches.
    Ok,
    /// Nothing is there.
    Missing,
    /// The file is there and its digest does not match.
    Mismatch,
}

impl ResultsRoot {
    /// Hold the results root at `path` apart from `roots`
    /// ([`trawl_engine::scratch::hold_apart_from`]), for a trawld running
    /// as this process's euid. Refusals name [`RootRole::RESULTS`]. Nothing
    /// in the directory is touched.
    pub fn open_apart_from(path: &Path, roots: &[OwnedRoot]) -> Result<Self, ScratchError> {
        #[cfg(unix)]
        let euid = rustix::process::geteuid().as_raw();
        // Off Unix the hold refuses before it reads the euid.
        #[cfg(not(unix))]
        let euid = 0;
        let held = trawl_engine::scratch::hold_apart_from(path, roots, euid, RootRole::RESULTS)?;
        Ok(Self { held })
    }

    /// The root's path, as configured, with a trailing `/` or `.` dropped.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.held.path()
    }

    /// Create a fresh staged file, 0600, by `openat` with `O_CREAT |
    /// O_EXCL | O_NOFOLLOW`, so an existing entry is never reused.
    pub fn stage(&self) -> io::Result<StagedFile> {
        let dir = self.held.dir().try_clone()?;
        let name = stage_name();
        drop(sys::create_new(&dir, &name)?);
        Ok(StagedFile {
            path: self.path().join(&name),
            dir,
            name,
            committed: false,
        })
    }

    /// Publish `staged` as result `id`: `fsync` the file, hash it with
    /// blake3 and read its size, both through one no-follow handle, rename
    /// it to `<id>.parquet` with `RENAME_NOREPLACE`, so an existing name is
    /// an error and is never replaced, then `fsync` the root.
    ///
    /// On an error before the rename, the staged file is unlinked as it
    /// drops. A failed `fsync` of the root after the rename unlinks the
    /// published name again: the caller commits no row for it.
    pub fn publish(&self, mut staged: StagedFile, id: &str) -> io::Result<Published> {
        let name = result_name(id)?;
        let dir = self.held.dir();
        let mut file = sys::open_regular(dir, &staged.name)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the staged file is gone"))?;
        file.sync_all()?;
        let file_bytes = file.metadata()?.len();
        let digest = digest(&mut file)?;
        drop(file);
        sys::rename_noreplace(dir, &staged.name, &name)?;
        staged.committed = true;
        if let Err(error) = dir.sync_all() {
            // The name is this publish's own: the rename refused to replace
            // anything. Without a durable directory the caller commits no
            // row, so the file would only wait for the sweep.
            let _ = sys::unlink(dir, &name);
            return Err(error);
        }
        Ok(Published { file_bytes, digest })
    }

    /// Open result `id`'s file read-only, by `openat` with `O_NOFOLLOW`.
    /// `None` when nothing is there. A symlink at the name fails the open,
    /// and anything other than a regular file is an error.
    pub fn open_file(&self, id: &str) -> io::Result<Option<File>> {
        sys::open_regular(self.held.dir(), &result_name(id)?)
    }

    /// The path of result `id`'s file, for the engine's `read_parquet`,
    /// which opens by path.
    ///
    /// A path is acceptable here, where an `openat` is not possible. The
    /// root is 0700, owned by trawld's euid, held no-follow and locked, so
    /// only trawld's own uid can add, rename or replace an entry in it, and
    /// the directories above it are operator-trusted storage (ADR-0052,
    /// ADR-0056). A reader that checks the digest through [`Self::verify`]
    /// and then reads by path therefore reads the file it checked unless
    /// trawld's own uid replaced it in between.
    pub fn path_of(&self, id: &str) -> io::Result<PathBuf> {
        Ok(self.path().join(result_name(id)?))
    }

    /// Check result `id`'s file against `expected`, its digest at publish
    /// (ADR-0057, D27): blake3 over the whole file, read through a
    /// no-follow handle. A file that cannot be opened, such as a symlink at
    /// the name, is an error, never [`Verified::Ok`].
    pub fn verify(&self, id: &str, expected: &[u8; 32]) -> io::Result<Verified> {
        let Some(mut file) = self.open_file(id)? else {
            return Ok(Verified::Missing);
        };
        Ok(if digest(&mut file)? == *expected {
            Verified::Ok
        } else {
            Verified::Mismatch
        })
    }

    /// When result `id`'s file was last written, or `None` when nothing is
    /// there.
    pub fn modified(&self, id: &str) -> io::Result<Option<SystemTime>> {
        self.open_file(id)?
            .map(|file| file.metadata()?.modified())
            .transpose()
    }

    /// Unlink result `id`'s file, by `unlinkat`. Returns whether a file was
    /// there.
    pub fn unlink(&self, id: &str) -> io::Result<bool> {
        sys::unlink(self.held.dir(), &result_name(id)?)
    }

    /// Unlink every staged file, which no row ever names. Returns how many
    /// went. Boot calls this while it holds the lock, so no capture of this
    /// process is staging. An entry with a staged name that is not a
    /// regular file is not one trawld made: it is logged and left alone.
    pub fn remove_stages(&self) -> io::Result<usize> {
        let dir = self.held.dir();
        let mut removed = 0;
        for entry in sys::entries(dir)? {
            let Some(name) = entry.name.to_str().filter(|name| is_stage_name(name)) else {
                continue;
            };
            if !entry.regular {
                warn_foreign(
                    self.path(),
                    "a staged name that is not a regular file",
                    None,
                );
                continue;
            }
            if sys::unlink(dir, name)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The ids of every published result file in the root, sorted.
    ///
    /// Staged files are skipped. Anything else, an entry with a name
    /// trawld does not make or a result name that is not a regular file,
    /// is logged at warn and left alone.
    pub fn list_ids(&self) -> io::Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in sys::entries(self.held.dir())? {
            let Some(name) = entry.name.to_str() else {
                warn_foreign(self.path(), "a name that is not UTF-8", None);
                continue;
            };
            if is_stage_name(name) {
                continue;
            }
            match result_id_of(name) {
                Some(id) if entry.regular => ids.push(id.to_owned()),
                // The name may be a real capture's id, so it is not logged.
                Some(_) => warn_foreign(
                    self.path(),
                    "a result name that is not a regular file",
                    None,
                ),
                None => warn_foreign(self.path(), "a name trawld does not make", Some(name)),
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
}

/// Log an entry in the results root that trawld did not make and leaves
/// alone.
fn warn_foreign(root: &Path, what: &str, name: Option<&str>) {
    tracing::warn!(
        event_type = "results_root_foreign_entry",
        root = %root.display(),
        entry = name.unwrap_or_default(),
        "the results root holds {what}; trawld leaves it alone"
    );
}

/// The blake3 digest of everything `file` holds from its current position.
fn digest(file: &mut File) -> io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(file)?;
    Ok(*hasher.finalize().as_bytes())
}

/// An entry of the results root.
struct Entry {
    name: std::ffi::OsString,
    /// Whether it is a regular file, read without following a symlink.
    regular: bool,
}

/// The descriptor-relative calls behind [`ResultsRoot`].
#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;

    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    use super::Entry;

    /// Create `name` in `dir`, 0600, failing if anything is there.
    pub(super) fn create_new(dir: &File, name: &str) -> io::Result<File> {
        let flags =
            OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::RDWR | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(dir, name, flags, Mode::RUSR | Mode::WUSR)?;
        Ok(File::from(fd))
    }

    /// Open the regular file `name` in `dir` read-only without following a
    /// symlink, and without waiting on a FIFO. `None` when nothing is
    /// there; anything other than a regular file is an error.
    pub(super) fn open_regular(dir: &File, name: &str) -> io::Result<Option<File>> {
        let flags =
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
        let file = match rustix::fs::openat(dir, name, flags, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !file.metadata()?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a results root entry is not a regular file",
            ));
        }
        Ok(Some(file))
    }

    /// Rename `from` to `to` within `dir`, failing with `AlreadyExists`
    /// when `to` exists.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    pub(super) fn rename_noreplace(dir: &File, from: &str, to: &str) -> io::Result<()> {
        use rustix::fs::RenameFlags;
        rustix::fs::renameat_with(dir, from, dir, to, RenameFlags::NOREPLACE)?;
        Ok(())
    }

    /// This platform has no rename that refuses to replace its target, and
    /// a check before a plain rename would race, so nothing is renamed.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    pub(super) fn rename_noreplace(_dir: &File, _from: &str, _to: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform cannot rename without replacing",
        ))
    }

    /// Unlink `name` in `dir`. Returns whether it was there.
    pub(super) fn unlink(dir: &File, name: &str) -> io::Result<bool> {
        match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
            Ok(()) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The entries of `dir`, `.` and `..` left out, each with whether it is
    /// a regular file.
    pub(super) fn entries(dir: &File) -> io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::read_from(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            #[allow(
                clippy::unnecessary_cast,
                reason = "st_mode is u32 on Linux and u16 on macOS"
            )]
            let regular = FileType::from_raw_mode(stat.st_mode as _) == FileType::RegularFile;
            out.push(Entry {
                name: std::ffi::OsStr::from_bytes(name.to_bytes()).to_owned(),
                regular,
            });
        }
        Ok(out)
    }
}

/// Off Unix no root is ever held ([`ResultsRoot::open_apart_from`] refuses),
/// so these are never reached.
#[cfg(not(unix))]
mod sys {
    use std::fs::File;
    use std::io;

    use super::Entry;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "the results root is held only on Unix",
        )
    }

    pub(super) fn create_new(_: &File, _: &str) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn open_regular(_: &File, _: &str) -> io::Result<Option<File>> {
        Err(unsupported())
    }

    pub(super) fn rename_noreplace(_: &File, _: &str, _: &str) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn unlink(_: &File, _: &str) -> io::Result<bool> {
        Err(unsupported())
    }

    pub(super) fn entries(_: &File) -> io::Result<Vec<Entry>> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_result_id_is_32_lowercase_hex_digits() {
        assert!(is_result_id(ID));
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            "../3456789abcdef0123456789abcdef",
            "0123456789abcdef/123456789abcdef",
        ] {
            assert!(!is_result_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn result_names_are_an_id_and_the_parquet_suffix() {
        assert!(is_result_name(&format!("{ID}.parquet")));
        assert_eq!(result_id_of(&format!("{ID}.parquet")), Some(ID));
        for bad in [
            ID.to_owned(),
            format!("{ID}.parquet.tmp"),
            format!("{ID}.PARQUET"),
            format!(".{ID}.parquet"),
            format!("tmp_{ID}.parquet"),
            ".parquet".to_owned(),
        ] {
            assert!(!is_result_name(&bad), "{bad:?}");
        }
        assert_eq!(result_name(ID).unwrap(), format!("{ID}.parquet"));
        for bad in ["", "../etc/passwd", "0123456789ABCDEF0123456789ABCDEF"] {
            assert_eq!(
                result_name(bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn stage_names_are_recognized_exactly_as_made() {
        for _ in 0..64 {
            let name = stage_name();
            assert!(is_stage_name(&name), "{name}");
            assert!(!is_result_name(&name), "{name}");
        }
        for bad in [
            ".stage-.parquet",
            ".stage-abcdefghijklmno.parquet",
            ".stage-abcdefghijklmnopq.parquet",
            ".stage-abcdefgh-jklmnop.parquet",
            ".stage-abcdefghijklmnop.ndjson",
            "stage-abcdefghijklmnop.parquet",
            "trawl-stage-abcdef.ndjson",
        ] {
            assert!(!is_stage_name(bad), "{bad:?}");
        }
        assert!(is_stage_name(".stage-AbCdEfGh01234567.parquet"));
    }
}
