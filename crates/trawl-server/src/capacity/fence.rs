// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one guard both capacity samples take against a repin (ADR-0042).
//!
//! A repin holds two generations of stored bytes on the data filesystem
//! until its sweep, so a Parquet scan or a headroom sample taken while one
//! runs overstates stored bytes or understates free space. Neither sample
//! holds a cutover guard, and a repin that hardlinks unchanged files can
//! build, swap, sweep and clear its marker while the sampling thread is
//! descheduled. [`RepinFence`] brackets a measurement so that such a repin
//! cannot pass unseen.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A bracket around one capacity measurement of a data root: open it
/// before the measurement and close it after.
///
/// Each end reads two things: the repin authority
/// ([`crate::repin::in_flight_evidence`]: the marker, the shadow root or
/// the aside root) and the inode of every env directory
/// ([`crate::env_dirs::is_env_dir_name`], not following symlinks). The
/// reads nest: [`RepinFence::open`] lists the env directories before it
/// reads the evidence, and [`RepinFence::close`] reads the evidence before
/// it lists them again.
///
/// The guarantee: a repin that overlaps the measurement is either seen by
/// one of the evidence reads, or its cutover replaced an env directory
/// that was present at open, and the fence reports it torn. The evidence
/// stands for a repin's whole life, from the marker written before its
/// shadow build to the marker removed after its sweep, and the cutover
/// renames each env directory it moves. A repin that neither evidence read
/// saw started after the open read and finished before the close read, so
/// its cutover fell between the two listings that enclose those reads.
///
/// Env directories added between open and close are ordinary: compaction
/// creates one on an env's first write, and the swap never adds a name
/// the live generation lacks. Only a name present at open that is absent
/// or has another inode at close is a tear.
///
/// The fence is no transactional snapshot. Files written, compacted or
/// deleted during the measurement still land in it or not, as ADR-0033
/// already allows. It only keeps a repin's two generations out of a
/// measurement that reports itself clean.
#[derive(Debug)]
pub struct RepinFence {
    data_root: PathBuf,
    envs: BTreeMap<OsString, u64>,
    repin_at_open: bool,
}

/// What a closed [`RepinFence`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceOutcome {
    /// Every env directory present at open is still the same directory.
    /// `saw_repin` is whether either evidence read found a repin.
    Clean { saw_repin: bool },
    /// An env directory present at open was removed or replaced: the
    /// measurement may have read two generations, and the attempt fails.
    Torn,
}

impl RepinFence {
    /// List the env directories, then read the repin evidence.
    ///
    /// # Errors
    /// An unreadable data root or env directory, or unreadable evidence.
    /// Unreadable evidence fails the attempt, so the cache keeps the last
    /// complete sample (ADR-0033).
    pub fn open(data_root: &Path) -> std::io::Result<Self> {
        let envs = env_dir_inodes(data_root)?;
        let repin_at_open = repin_evidence(data_root)?;
        Ok(Self {
            data_root: data_root.to_owned(),
            envs,
            repin_at_open,
        })
    }

    /// Read the repin evidence, then list the env directories again.
    ///
    /// # Errors
    /// As [`RepinFence::open`].
    pub fn close(self) -> std::io::Result<FenceOutcome> {
        let repin_at_close = repin_evidence(&self.data_root)?;
        let envs = env_dir_inodes(&self.data_root)?;
        let torn = self
            .envs
            .iter()
            .any(|(name, inode)| envs.get(name) != Some(inode));
        Ok(if torn {
            FenceOutcome::Torn
        } else {
            FenceOutcome::Clean {
                saw_repin: self.repin_at_open || repin_at_close,
            }
        })
    }
}

impl FenceOutcome {
    /// Whether the measurement saw a repin.
    ///
    /// # Errors
    /// A torn fence, which fails the attempt so the cache keeps the last
    /// complete sample (ADR-0033).
    pub fn saw_repin(self) -> std::io::Result<bool> {
        match self {
            Self::Clean { saw_repin } => Ok(saw_repin),
            Self::Torn => Err(std::io::Error::other(
                "an environment directory was replaced during the measurement",
            )),
        }
    }
}

fn repin_evidence(data_root: &Path) -> std::io::Result<bool> {
    crate::repin::in_flight_evidence(data_root).map(|evidence| evidence.is_some())
}

/// The inode of every env directory under `data_root`. Symlinks are not
/// followed: the storage walk does not descend them, and the cutover
/// renames the directory entry itself.
fn env_dir_inodes(data_root: &Path) -> std::io::Result<BTreeMap<OsString, u64>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut inodes = BTreeMap::new();
    for entry in std::fs::read_dir(data_root)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(crate::env_dirs::is_env_dir_name) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            inodes.insert(name, metadata.ino());
        }
    }
    Ok(inodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repin::marker::{aside_root, marker_path, shadow_root};

    fn plant(base: &Path, env: &str) {
        let dir = base.join(env).join("2026-09-20");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.parquet"), "0123456789").unwrap();
    }

    /// A data root holding `prod` and `lab`.
    fn data_root() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        plant(&root, "prod");
        plant(&root, "lab");
        (tmp, root)
    }

    /// A self-referencing symlink where the shadow root belongs: asking
    /// whether it exists loops, so the evidence cannot be read.
    fn unreadable_evidence(root: &Path) {
        let shadow = shadow_root(root);
        std::os::unix::fs::symlink(&shadow, &shadow).unwrap();
    }

    const QUIET: FenceOutcome = FenceOutcome::Clean { saw_repin: false };
    const REPIN: FenceOutcome = FenceOutcome::Clean { saw_repin: true };

    #[test]
    fn fence_is_clean_when_nothing_moves() {
        let (_tmp, root) = data_root();
        let fence = RepinFence::open(&root).unwrap();
        // Writes inside an env and outside the env set are no tear.
        plant(&root, "prod");
        std::fs::create_dir_all(root.join("wal/prod")).unwrap();
        std::fs::create_dir_all(root.join("scheduled")).unwrap();
        assert_eq!(fence.close().unwrap(), QUIET);
    }

    /// Compaction creates an env directory on the env's first write.
    #[test]
    fn fence_is_clean_when_an_env_dir_is_added() {
        let (_tmp, root) = data_root();
        let fence = RepinFence::open(&root).unwrap();
        plant(&root, "newenv");
        assert_eq!(fence.close().unwrap(), QUIET);
    }

    /// The real cutover runs whole between open and close, sweep and all,
    /// so neither evidence read sees it: the replaced env inodes do.
    #[test]
    fn fence_is_torn_when_a_repin_replaces_an_env_dir() {
        let (_tmp, root) = data_root();
        let fence = RepinFence::open(&root).unwrap();
        let shadow = shadow_root(&root);
        plant(&shadow, "prod");
        plant(&shadow, "lab");
        std::fs::write(marker_path(&root), "{}").unwrap();
        crate::repin::cutover::swap_envs(&root, &shadow, &aside_root(&root)).unwrap();
        crate::repin::cutover::finish_post_swap_staging(&root);
        assert_eq!(crate::repin::in_flight_evidence(&root).unwrap(), None);
        assert_eq!(fence.close().unwrap(), FenceOutcome::Torn);
    }

    #[test]
    fn fence_is_torn_when_an_env_dir_is_removed() {
        let (_tmp, root) = data_root();
        let fence = RepinFence::open(&root).unwrap();
        std::fs::remove_dir_all(root.join("lab")).unwrap();
        assert_eq!(fence.close().unwrap(), FenceOutcome::Torn);
    }

    /// Evidence at either end is a repin the measurement overlapped.
    #[test]
    fn fence_saw_repin_from_evidence_at_either_end() {
        let (_tmp, root) = data_root();
        let marker = marker_path(&root);

        // At open only: the repin finished during the measurement.
        std::fs::write(&marker, "{}").unwrap();
        let fence = RepinFence::open(&root).unwrap();
        std::fs::remove_file(&marker).unwrap();
        assert_eq!(fence.close().unwrap(), REPIN);

        // At close only: the repin started during the measurement.
        let fence = RepinFence::open(&root).unwrap();
        std::fs::create_dir_all(shadow_root(&root)).unwrap();
        assert_eq!(fence.close().unwrap(), REPIN);

        // At both ends.
        let fence = RepinFence::open(&root).unwrap();
        assert_eq!(fence.close().unwrap(), REPIN);
    }

    #[test]
    fn fence_fails_on_unreadable_evidence_at_either_end() {
        let (_tmp, root) = data_root();
        let fence = RepinFence::open(&root).unwrap();
        unreadable_evidence(&root);
        assert!(fence.close().is_err());
        assert!(RepinFence::open(&root).is_err());
    }

    #[test]
    fn torn_fails_the_attempt_and_clean_answers_the_repin() {
        assert!(FenceOutcome::Torn.saw_repin().is_err());
        assert!(!QUIET.saw_repin().unwrap());
        assert!(REPIN.saw_repin().unwrap());
    }
}
