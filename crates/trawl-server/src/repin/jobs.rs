// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The in-process repin job generation: how a capacity sample learns that
//! a repin job overlapped it (ADR-0042).
//!
//! A repin job's disk work (the marker, the shadow build, the swap, the
//! sweeps) all runs inside the engine's background half, and every exit
//! from that half ends its [`JobGuard`]. So the question "did a repin job
//! touch the data root while I measured it" has an exact in-process
//! answer, with no need to infer one from what the job left on disk.

use std::sync::atomic::{AtomicU64, Ordering};

/// One generation step, in the high half of the word.
const GENERATION_STEP: u64 = 1 << 32;

/// Every repin job in this process, packed into one atomic word: how many
/// run now (the low 32 bits), and a generation (the high 32 bits) that
/// advances once when a job begins and once when it ends.
///
/// This is the even/odd seqlock generalised to overlapping jobs. Two jobs
/// can overlap: a new job may claim the running slot while the previous
/// job's post-cutover sweep is still deleting its aside
/// ([`crate::repin::CancelRegistry::arm`]). A parity bit would read idle
/// while both ran, so the running count answers "is a job in flight" and
/// the generation answers "did a job begin or end since".
///
/// One word, so a reader gets both halves from one atomic load. The
/// generation wraps after 2^32 steps; a reader would need a measurement
/// spanning exactly 2^31 jobs to read two equal generations across a
/// change.
#[derive(Debug, Default)]
pub struct JobGeneration {
    word: AtomicU64,
}

/// One read of a [`JobGeneration`]. The default is a fresh generation's
/// read: no job running, none ever begun.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobReading {
    running: u32,
    generation: u32,
}

impl JobGeneration {
    /// Read the running count and the generation at once.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // splitting the word into its halves
    pub fn read(&self) -> JobReading {
        let word = self.word.load(Ordering::SeqCst);
        JobReading {
            running: (word & (GENERATION_STEP - 1)) as u32,
            generation: (word >> 32) as u32,
        }
    }

    /// Begin a job: count it running and advance the generation. The job
    /// ends when the guard drops, on every path, unwinding included.
    ///
    /// Take the guard before the job's first write under the data root,
    /// and hold it until its last cleanup.
    pub(crate) fn begin(&self) -> JobGuard<'_> {
        self.word.fetch_add(GENERATION_STEP + 1, Ordering::SeqCst);
        JobGuard { jobs: self }
    }
}

impl JobReading {
    /// Whether a job was running at this read.
    #[must_use]
    pub fn in_flight(self) -> bool {
        self.running > 0
    }

    /// How many jobs run at this read.
    #[must_use]
    pub fn running(self) -> u32 {
        self.running
    }

    /// How many job begins and ends happened between `earlier` and this
    /// read. Zero means no job began or ended in between.
    #[must_use]
    pub fn changes_since(self, earlier: Self) -> u32 {
        self.generation.wrapping_sub(earlier.generation)
    }
}

/// A running repin job. Dropping it ends the job.
#[must_use = "the job ends when the guard drops"]
#[derive(Debug)]
pub struct JobGuard<'a> {
    jobs: &'a JobGeneration,
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        // Advance the generation and drop the running count in one add.
        // The count is at least 1 here, since this guard's begin counted
        // it, so subtracting one never borrows from the generation.
        self.jobs
            .word
            .fetch_add(GENERATION_STEP - 1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_counts_while_its_guard_lives_and_each_edge_advances_the_generation() {
        let jobs = JobGeneration::default();
        let idle = jobs.read();
        assert!(!idle.in_flight());

        let guard = jobs.begin();
        let running = jobs.read();
        assert!(running.in_flight());
        assert_eq!(running.running(), 1);
        assert_eq!(running.changes_since(idle), 1);

        drop(guard);
        let ended = jobs.read();
        assert!(!ended.in_flight());
        assert_eq!(ended.changes_since(idle), 2);
        assert_eq!(ended.changes_since(ended), 0);
    }

    /// The overlap a parity bit misreads: two jobs running reads as two
    /// generation steps, which parity would call idle.
    #[test]
    fn overlapping_jobs_stay_in_flight_until_the_last_one_ends() {
        let jobs = JobGeneration::default();
        let first = jobs.begin();
        let second = jobs.begin();
        assert_eq!(jobs.read().running(), 2);
        drop(first);
        assert!(jobs.read().in_flight());
        drop(second);
        assert!(!jobs.read().in_flight());
        assert_eq!(jobs.read().changes_since(JobReading::default()), 4);
    }

    /// A panicking job still ends: the guard drops during the unwind.
    #[test]
    fn a_job_that_panics_ends_during_the_unwind() {
        let jobs = JobGeneration::default();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _job = jobs.begin();
            panic!("job died");
        }));
        assert!(unwound.is_err());
        assert!(!jobs.read().in_flight());
        assert_eq!(jobs.read().changes_since(JobReading::default()), 2);
    }

    #[test]
    fn the_generation_wraps_without_touching_the_running_count() {
        let jobs = JobGeneration {
            word: AtomicU64::new(u64::from(u32::MAX) << 32),
        };
        let before = jobs.read();
        let guard = jobs.begin();
        assert_eq!(jobs.read().running(), 1);
        assert_eq!(jobs.read().changes_since(before), 1);
        drop(guard);
        assert!(!jobs.read().in_flight());
        assert_eq!(jobs.read().changes_since(before), 2);
    }
}
