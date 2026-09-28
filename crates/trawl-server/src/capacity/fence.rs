// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one guard both capacity samples take against a repin (ADR-0042).
//!
//! A repin holds two generations of stored bytes on the data filesystem
//! until its sweep, so a Parquet scan or a headroom sample taken while one
//! runs overstates stored bytes or understates free space. Neither sample
//! holds a cutover guard, and a repin job can build a shadow, swap or
//! abandon it, sweep and clear its marker while the sampling thread is
//! descheduled. [`RepinFence`] brackets a measurement so that such a job
//! cannot pass unseen.

use std::path::{Path, PathBuf};

use crate::repin::jobs::{JobGeneration, JobReading};

/// A bracket around one capacity measurement of a data root: open it
/// before the measurement and close it after.
///
/// The guarantee: every repin job in this process that overlaps the
/// measurement is reported, whatever its outcome (completed, cancelled,
/// refused, failed, or unwound by a panic). Each end reads the in-process
/// [`JobGeneration`], which a job enters before its first write under the
/// data root and leaves after its last cleanup. A job running at the open
/// read is in its running count; a job that begins or ends after the open
/// read advances its generation before the close read.
///
/// Each end also reads the on-disk repin evidence
/// ([`crate::repin::in_flight_evidence`]: the marker, the shadow root or
/// the aside root). That catches what an earlier process left behind, such
/// as a crash before boot recovery ran, or staging a job in this process
/// kept because its sweep failed. Unreadable evidence fails the attempt.
///
/// Out of scope: writes to the data directory from outside this process.
/// The data root is trusted storage that only this daemon rearranges.
///
/// The fence is no transactional snapshot. Files written, compacted or
/// deleted during the measurement still land in it or not, as ADR-0033
/// already allows. It only keeps a repin's two generations out of a
/// measurement that reports itself clean.
#[derive(Debug)]
pub struct RepinFence<'a> {
    data_root: PathBuf,
    jobs: &'a JobGeneration,
    jobs_at_open: JobReading,
    evidence_at_open: bool,
}

impl<'a> RepinFence<'a> {
    /// Read the job generation, then the on-disk evidence.
    ///
    /// # Errors
    /// Unreadable evidence, which fails the attempt so the cache keeps the
    /// last complete sample (ADR-0033).
    pub fn open(data_root: &Path, jobs: &'a JobGeneration) -> std::io::Result<Self> {
        let jobs_at_open = jobs.read();
        let evidence_at_open = repin_evidence(data_root)?;
        Ok(Self {
            data_root: data_root.to_owned(),
            jobs,
            jobs_at_open,
            evidence_at_open,
        })
    }

    /// Read the on-disk evidence, then the job generation, and answer
    /// whether the measurement between open and close saw a repin.
    ///
    /// # Errors
    /// As [`RepinFence::open`].
    pub fn close(self) -> std::io::Result<bool> {
        let evidence_at_close = repin_evidence(&self.data_root)?;
        let jobs_at_close = self.jobs.read();
        Ok(self.jobs_at_open.in_flight()
            || jobs_at_close.changes_since(self.jobs_at_open) != 0
            || self.evidence_at_open
            || evidence_at_close)
    }
}

fn repin_evidence(data_root: &Path) -> std::io::Result<bool> {
    crate::repin::in_flight_evidence(data_root).map(|evidence| evidence.is_some())
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

    /// With no job and no evidence, ordinary writes are no repin: writes
    /// inside an env, outside the env set, and a new env directory, which
    /// compaction creates on an env's first write.
    #[test]
    fn fence_is_clean_with_no_job_and_no_evidence() {
        let (_tmp, root) = data_root();
        let jobs = JobGeneration::default();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        plant(&root, "prod");
        plant(&root, "newenv");
        std::fs::create_dir_all(root.join("wal/prod")).unwrap();
        std::fs::create_dir_all(root.join("scheduled")).unwrap();
        assert!(!fence.close().unwrap());
    }

    /// A job running at open is seen, whether it ends before close or is
    /// still running then.
    #[test]
    fn fence_sees_a_job_begun_before_open() {
        let (_tmp, root) = data_root();
        let jobs = JobGeneration::default();

        let job = jobs.begin();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        drop(job);
        assert!(fence.close().unwrap());

        let _job = jobs.begin();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        assert!(fence.close().unwrap());
    }

    /// A whole job between the two reads, disk work and cleanup included,
    /// leaves no evidence on disk: the generation still moved.
    #[test]
    fn fence_sees_a_job_begun_and_ended_between_open_and_close() {
        let (_tmp, root) = data_root();
        let jobs = JobGeneration::default();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        {
            let _job = jobs.begin();
            let shadow = shadow_root(&root);
            plant(&shadow, "prod");
            plant(&shadow, "lab");
            std::fs::write(marker_path(&root), "{}").unwrap();
            crate::repin::cutover::swap_envs(&root, &shadow, &aside_root(&root)).unwrap();
            crate::repin::cutover::finish_post_swap_staging(&root);
        }
        assert_eq!(crate::repin::in_flight_evidence(&root).unwrap(), None);
        assert!(fence.close().unwrap());
    }

    #[test]
    fn fence_sees_a_job_begun_during_and_still_running() {
        let (_tmp, root) = data_root();
        let jobs = JobGeneration::default();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        let _job = jobs.begin();
        assert!(fence.close().unwrap());
    }

    /// Two overlapping jobs, both running through the measurement: two
    /// generation steps, which an even/odd parity bit would read as idle.
    #[test]
    fn fence_sees_overlapping_jobs() {
        let (_tmp, root) = data_root();
        let jobs = JobGeneration::default();
        let _first = jobs.begin();
        let _second = jobs.begin();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        assert!(fence.close().unwrap());
    }

    /// Evidence at either end, with no job in this process: staging an
    /// earlier process left behind.
    #[test]
    fn fence_sees_leftover_evidence_at_either_end() {
        let (_tmp, root) = data_root();
        let jobs = JobGeneration::default();
        let marker = marker_path(&root);

        // At open only.
        std::fs::write(&marker, "{}").unwrap();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        std::fs::remove_file(&marker).unwrap();
        assert!(fence.close().unwrap());

        // At close only.
        let fence = RepinFence::open(&root, &jobs).unwrap();
        std::fs::create_dir_all(shadow_root(&root)).unwrap();
        assert!(fence.close().unwrap());

        // At both ends.
        let fence = RepinFence::open(&root, &jobs).unwrap();
        assert!(fence.close().unwrap());
    }

    #[test]
    fn fence_fails_on_unreadable_evidence_at_either_end() {
        let (_tmp, root) = data_root();
        let jobs = JobGeneration::default();
        let fence = RepinFence::open(&root, &jobs).unwrap();
        unreadable_evidence(&root);
        assert!(fence.close().is_err());
        assert!(RepinFence::open(&root, &jobs).is_err());
    }
}
