// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cooperative cancellation of the running repin job (#109).
//!
//! One in-process decision object, [`CancelRegistry`], carries both the
//! operator's request and the engine's own point-of-no-return latch through
//! a single mutex. That single lock is the whole design: a request and a
//! latch are the two sides of one race, and two independent signals (a
//! flag plus a watch channel, say) cannot be compared atomically, so the
//! answer would depend on which the reader looked at first. Nobody awaits a
//! change here; every check is a poll at a file boundary.
//!
//! What the engine promises is therefore a latency, not an instant: the
//! scan and build loops check before and after each file, so a job stops at
//! the next file boundary. The whole-corpus snapshot walk and the
//! filesystem pre-flight are not checkpointed, and
//! [`CANCEL_LATENCY_CONTRACT`] says so in the words the 202 response
//! carries.
//!
//! Two things this module deliberately does not do. It never writes to the
//! job store: the store's `record_cancel_request` is the engine's call,
//! made both by the detached request path and again by the effect site, so
//! a `cancelled` terminal status is impossible without the request row the
//! database CHECK demands. And it never decides an HTTP status by hand
//! outside [`CancelVerdict::wire`], which is the one status/outcome/
//! sentence table the handler renders.

use std::sync::Arc;

use axum::http::StatusCode;
use parking_lot::Mutex;

/// The stage a cancel took effect in: the one vocabulary shared by the
/// audit events, the job row's error sentence and (through M3) the CLI.
pub const STAGE_SCAN: &str = "scan";
/// The build and catch-up passes, including the final increment under the
/// cutover exclusion.
pub const STAGE_BUILD: &str = "build";
/// The last check before the Cutover marker is written.
pub const STAGE_FINAL_GATE: &str = "final gate";

/// A macro, not a `const`, so [`CANCEL_LATENCY_CONTRACT`] and the 202
/// sentence that must contain it are one literal rather than two that drift
/// apart (`concat!` takes literals only).
macro_rules! latency_contract {
    () => {
        "the job stops at the next file boundary of the scan or build \
         loop; the whole-corpus snapshot walk and the filesystem \
         pre-flight are not checkpointed, so a job inside one of those \
         stops when it leaves it"
    };
}

/// What an operator is promised when a cancel is accepted. Published
/// because the API docs and the 202 body quote it verbatim.
pub const CANCEL_LATENCY_CONTRACT: &str = latency_contract!();

/// The 202 sentence. It embeds the latency contract and says plainly that
/// accepting a request is not promising a terminal `cancelled` status: a
/// job that reaches the point of no return first completes, and the status
/// route is where the verdict actually lands.
const CANCELLING_DETAIL: &str = concat!(
    "cancel accepted for the running repin job. ",
    latency_contract!(),
    ". A 202 is not a guarantee of a terminal `cancelled` status: a job \
     that reaches its point of no return first completes normally. Read \
     the outcome from /api/v1/schema/repin/status"
);

const PAST_NO_RETURN_DETAIL: &str = "the repin has passed its point of no return: the corpus is being \
     swapped to the new generation and there is nothing left to unwind. \
     The job is not queued for cancellation; it will complete";

const NO_JOB_DETAIL: &str = "no repin job is running on this node";

/// What a cancel request is answered with. The registry decides it under
/// its lock, which is what makes "cancelling" and "too late" mutually
/// exclusive rather than a matter of timing between two reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelVerdict {
    /// The request is accepted. `already_requested` marks a repeat: an
    /// earlier asker's request is already pending, and this one changed
    /// nothing but the audit trail.
    Cancelling {
        job_id: i64,
        already_requested: bool,
    },
    /// The job latched its point of no return before this request arrived.
    PastPointOfNoReturn { job_id: i64 },
    /// Nothing is running (or the running job's task has fully ended).
    NoJobRunning,
}

/// The wire discriminant for a cancel verdict.
///
// M3: this enum moves to `trawl-api` (mirrored, `serde(rename_all =
// "snake_case")`) so the client can validate that the status code and the
// body's discriminant agree. It lives here for M2 because M2 ships no
// transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepinCancelOutcome {
    Cancelling,
    PastPointOfNoReturn,
    NoJobRunning,
}

impl CancelVerdict {
    /// The one status/outcome/sentence table. The handler renders this and
    /// decides nothing: three verdicts, three distinct status codes, and
    /// the accepted one carries the latency contract so an operator is
    /// never told "accepted" without being told what accepted means.
    #[must_use]
    pub fn wire(&self) -> (StatusCode, RepinCancelOutcome, &'static str) {
        match self {
            Self::Cancelling { .. } => (
                StatusCode::ACCEPTED,
                RepinCancelOutcome::Cancelling,
                CANCELLING_DETAIL,
            ),
            Self::PastPointOfNoReturn { .. } => (
                StatusCode::CONFLICT,
                RepinCancelOutcome::PastPointOfNoReturn,
                PAST_NO_RETURN_DETAIL,
            ),
            Self::NoJobRunning => (
                StatusCode::NOT_FOUND,
                RepinCancelOutcome::NoJobRunning,
                NO_JOB_DETAIL,
            ),
        }
    }

    /// The job the verdict is about, when there is one.
    #[must_use]
    pub fn job_id(&self) -> Option<i64> {
        match self {
            Self::Cancelling { job_id, .. } | Self::PastPointOfNoReturn { job_id } => Some(*job_id),
            Self::NoJobRunning => None,
        }
    }
}

/// The registry's record of the one armed job.
#[derive(Debug, Clone)]
struct Entry {
    job_id: i64,
    /// The first asker's name, once a request has been accepted. The
    /// display name of the verified key, the same identity source
    /// `requested_by` uses, so the row reads coherently.
    cancelled_by: Option<String>,
    /// Latched by [`CancelRegistry::commit`] immediately before the
    /// Cutover marker write. Past it every request is refused.
    committed: bool,
}

/// The armed job's cancel state: one slot, one mutex, one truth.
#[derive(Debug, Default)]
pub struct CancelRegistry {
    slot: Mutex<Option<Entry>>,
}

impl CancelRegistry {
    /// Arm the registry for a freshly claimed job and hand back the token
    /// its loops check.
    ///
    /// Synchronous on purpose: the engine calls it with no `await` between
    /// the store claim and this line, so there is no window in which a job
    /// holds the one-running slot but cannot be cancelled.
    ///
    /// An occupied slot means an earlier job's task has not ended yet even
    /// though its row is terminal (the post-cutover sweep runs after
    /// `finish_cutover`). The new job owns the registry from here; the old
    /// entry's handle stops matching and its checks become no-ops, which is
    /// correct — nothing can cancel a job that already swapped the corpus.
    pub fn arm(self: &Arc<Self>, job_id: i64) -> CancelHandle {
        let mut slot = self.slot.lock();
        if let Some(previous) = slot.as_ref() {
            tracing::warn!(
                event_type = "repin_cancel_registry_overlap",
                job_id,
                previous_job_id = previous.job_id,
                "a new repin job armed the cancel registry while the \
                 previous job's task was still finishing; the previous \
                 job is no longer cancellable"
            );
        }
        *slot = Some(Entry {
            job_id,
            cancelled_by: None,
            committed: false,
        });
        CancelHandle {
            registry: Arc::clone(self),
            job_id,
        }
    }

    /// Ask to cancel whatever is running. The handler's one call.
    pub fn request(&self, by: &str) -> CancelVerdict {
        let mut slot = self.slot.lock();
        let Some(entry) = slot.as_mut() else {
            return CancelVerdict::NoJobRunning;
        };
        if entry.committed {
            return CancelVerdict::PastPointOfNoReturn {
                job_id: entry.job_id,
            };
        }
        let already_requested = entry.cancelled_by.is_some();
        if !already_requested {
            entry.cancelled_by = Some(by.to_owned());
        }
        CancelVerdict::Cancelling {
            job_id: entry.job_id,
            already_requested,
        }
    }

    /// The engine's last check before the Cutover marker write.
    ///
    /// `false` means a cancel is pending (or this job no longer owns the
    /// registry) and the marker must not be written; `true` latches the
    /// point of no return, and every later request is refused. Comparing
    /// and latching under one lock is what makes cancel-vs-commit a
    /// decision instead of a race.
    pub fn commit(&self, job_id: i64) -> bool {
        let mut slot = self.slot.lock();
        match slot.as_mut() {
            Some(entry) if entry.job_id == job_id && entry.cancelled_by.is_none() => {
                entry.committed = true;
                true
            }
            _ => false,
        }
    }

    /// The pending request for `job_id`, if the job is still armed, has
    /// been asked to cancel, and has not latched.
    #[must_use]
    pub fn pending(&self, job_id: i64) -> Option<String> {
        let slot = self.slot.lock();
        slot.as_ref()
            .filter(|entry| entry.job_id == job_id && !entry.committed)
            .and_then(|entry| entry.cancelled_by.clone())
    }

    /// Release the registry, if `job_id` still owns it.
    ///
    /// The job-id condition is what keeps a slow ending job from clearing
    /// its successor's entry: the loser of that overlap disarms nothing.
    pub fn disarm(&self, job_id: i64) {
        let mut slot = self.slot.lock();
        if slot.as_ref().is_some_and(|entry| entry.job_id == job_id) {
            *slot = None;
        }
    }
}

/// A job's token into the registry: cheap to clone, crosses into the
/// blocking pool with the scan and build passes.
#[derive(Debug, Clone)]
pub struct CancelHandle {
    registry: Arc<CancelRegistry>,
    job_id: i64,
}

impl CancelHandle {
    /// The file-boundary check. `Err` means stop and unwind; the stage
    /// rides along because it is what the operator is told.
    ///
    /// # Errors
    /// [`PassStop::Cancelled`] when a cancel request is pending for this
    /// job.
    pub fn check(&self, stage: &'static str) -> Result<(), PassStop> {
        if self.registry.pending(self.job_id).is_some() {
            return Err(PassStop::Cancelled { stage });
        }
        Ok(())
    }

    /// Who asked, if anyone has. The effect site reads this to name the
    /// actor in the job row's error sentence and the audit event.
    #[must_use]
    pub fn cancelled_by(&self) -> Option<String> {
        self.registry.pending(self.job_id)
    }

    /// The job this handle speaks for.
    #[must_use]
    pub fn job_id(&self) -> i64 {
        self.job_id
    }
}

/// Why a scan or build pass stopped short.
///
/// The `From<String>` conversion keeps the `?` on the existing
/// `Result<_, String>` filesystem and `DuckDB` helpers working unchanged,
/// so adding a check to a loop is one line and never a rewrite of its error
/// handling.
#[derive(Debug)]
pub enum PassStop {
    Cancelled { stage: &'static str },
    Failed(String),
}

impl From<String> for PassStop {
    fn from(msg: String) -> Self {
        Self::Failed(msg)
    }
}

/// The audit event for an accepted request, emitted after the store write
/// attempt so the trail matches what a reader of the job row can see.
/// A repeat carries the new asker's name with `already_requested = true`:
/// the row still names the first, and both facts belong in the log.
pub fn audit_cancel_requested(job_id: i64, actor: &str, already_requested: bool) {
    tracing::info!(
        event_type = "repin_cancel_requested",
        job_id,
        actor = %actor,
        already_requested,
        "repin cancellation requested"
    );
}

/// The audit event for the effect: a file boundary observed the request and
/// the unwind is about to run. Emitted before `abandon_build`'s warn line,
/// so the log reads request → effect → sweep.
pub fn audit_cancelled(job_id: i64, actor: &str, stage: &str) {
    tracing::info!(
        event_type = "repin_cancelled",
        job_id,
        actor = %actor,
        stage = %stage,
        "repin job cancelled before the point of no return; the live \
         corpus was never touched"
    );
}

/// The audit event for a refusal. Only the 409 has a job to name; a 404
/// emits nothing, because an event with no subject is noise.
pub fn audit_cancel_refused(job_id: i64, actor: &str) {
    tracing::info!(
        event_type = "repin_cancel_refused",
        job_id,
        actor = %actor,
        "repin cancellation refused: the job is past its point of no return"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(job_id: i64) -> (Arc<CancelRegistry>, CancelHandle) {
        let registry = Arc::new(CancelRegistry::default());
        let handle = registry.arm(job_id);
        (registry, handle)
    }

    /// The race the whole module exists for, driven from two threads: one
    /// asks to cancel while the other latches the point of no return.
    ///
    /// The assertion is not about which wins — that is genuinely timing —
    /// but about the two answers being consistent with each other. A
    /// latched commit must refuse the request, and an accepted request must
    /// stop the commit. The forbidden outcomes are both "yes" (a cancelled
    /// job whose corpus was swapped anyway) and both "no" (a job that
    /// neither cancels nor cuts over).
    #[test]
    fn cancel_and_commit_race_to_one_answer_from_two_threads() {
        let mut cancelled_won = 0;
        let mut committed_won = 0;
        for _ in 0..200 {
            let (registry, _handle) = armed(7);
            let asker = Arc::clone(&registry);
            let closer = Arc::clone(&registry);
            let request = std::thread::spawn(move || asker.request("operator"));
            let commit = std::thread::spawn(move || closer.commit(7));
            let verdict = request.join().expect("request thread");
            let latched = commit.join().expect("commit thread");

            match (verdict, latched) {
                (CancelVerdict::Cancelling { job_id, .. }, false) => {
                    assert_eq!(job_id, 7);
                    assert_eq!(registry.pending(7).as_deref(), Some("operator"));
                    cancelled_won += 1;
                }
                (CancelVerdict::PastPointOfNoReturn { job_id }, true) => {
                    assert_eq!(job_id, 7);
                    assert_eq!(
                        registry.pending(7),
                        None,
                        "a latched job has no pending cancel for an effect \
                         site to act on"
                    );
                    committed_won += 1;
                }
                other => panic!("cancel and commit disagreed: {other:?}"),
            }
        }
        // Both interleavings must be reachable in principle; the loop is
        // not asserted to produce both, because a scheduler is free to be
        // consistent. The counters are here so a failure message can say
        // which family it was stuck in.
        assert_eq!(cancelled_won + committed_won, 200);
    }

    /// Commit first, then ask: the answer is the refusal, and it stays the
    /// refusal however many times it is asked.
    #[test]
    fn a_latched_job_refuses_every_later_request() {
        let (registry, handle) = armed(11);
        assert!(registry.commit(11));
        for _ in 0..3 {
            assert_eq!(
                registry.request("operator"),
                CancelVerdict::PastPointOfNoReturn { job_id: 11 }
            );
        }
        assert_eq!(handle.check(STAGE_BUILD).ok(), Some(()));
        assert_eq!(handle.cancelled_by(), None);
    }

    /// Ask first, then latch: the latch fails, and the effect site can
    /// still read the actor it must name.
    #[test]
    fn a_pending_request_stops_the_latch() {
        let (registry, handle) = armed(11);
        assert_eq!(
            registry.request("alice"),
            CancelVerdict::Cancelling {
                job_id: 11,
                already_requested: false
            }
        );
        assert!(!registry.commit(11));
        assert!(matches!(
            handle.check(STAGE_FINAL_GATE),
            Err(PassStop::Cancelled {
                stage: STAGE_FINAL_GATE
            })
        ));
        assert_eq!(handle.cancelled_by().as_deref(), Some("alice"));
    }

    /// A second asker changes nothing but the audit trail. The row's actor
    /// is the one whose request actually took effect, so the registry must
    /// keep the first name.
    #[test]
    fn a_second_request_preserves_the_first_actor() {
        let (registry, handle) = armed(3);
        assert_eq!(
            registry.request("alice"),
            CancelVerdict::Cancelling {
                job_id: 3,
                already_requested: false
            }
        );
        assert_eq!(
            registry.request("bob"),
            CancelVerdict::Cancelling {
                job_id: 3,
                already_requested: true
            }
        );
        assert_eq!(handle.cancelled_by().as_deref(), Some("alice"));
    }

    /// Disarm is owner-checked. A job whose task ends late must not clear
    /// the entry its successor armed, or the successor would answer
    /// "no job running" while it rewrites the corpus.
    #[test]
    fn a_stale_disarm_does_not_clear_a_live_entry() {
        let registry = Arc::new(CancelRegistry::default());
        let _first = registry.arm(1);
        let second = registry.arm(2);

        registry.disarm(1);
        assert_eq!(
            registry.request("operator"),
            CancelVerdict::Cancelling {
                job_id: 2,
                already_requested: false
            },
            "job 2 is still armed"
        );
        assert_eq!(second.cancelled_by().as_deref(), Some("operator"));

        registry.disarm(2);
        assert_eq!(registry.request("operator"), CancelVerdict::NoJobRunning);
        assert_eq!(second.cancelled_by(), None);
    }

    /// An unarmed registry cannot latch: the commit's `false` sends the
    /// caller down the unwind rather than past the point of no return,
    /// which is the safe direction when the bookkeeping is inconsistent.
    #[test]
    fn commit_needs_the_job_to_own_the_registry() {
        let registry = Arc::new(CancelRegistry::default());
        assert!(!registry.commit(1));
        let _handle = registry.arm(1);
        assert!(!registry.commit(2), "another job's id may not latch");
        assert!(registry.commit(1));
    }

    /// The wire table: three verdicts, three distinct status codes, three
    /// distinct discriminants, and the accepted sentence quotes the latency
    /// contract rather than paraphrasing it.
    #[test]
    fn the_wire_table_is_pairwise_distinct_and_states_the_contract() {
        let verdicts = [
            CancelVerdict::Cancelling {
                job_id: 1,
                already_requested: false,
            },
            CancelVerdict::PastPointOfNoReturn { job_id: 1 },
            CancelVerdict::NoJobRunning,
        ];
        let wired: Vec<_> = verdicts.iter().map(CancelVerdict::wire).collect();

        for (i, (status, outcome, detail)) in wired.iter().enumerate() {
            for (j, (other_status, other_outcome, other_detail)) in wired.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert_ne!(status, other_status, "{i} vs {j}");
                assert_ne!(outcome, other_outcome, "{i} vs {j}");
                assert_ne!(detail, other_detail, "{i} vs {j}");
            }
        }
        assert_eq!(wired[0].0, StatusCode::ACCEPTED);
        assert_eq!(wired[1].0, StatusCode::CONFLICT);
        assert_eq!(wired[2].0, StatusCode::NOT_FOUND);
        assert!(
            wired[0].2.contains(CANCEL_LATENCY_CONTRACT),
            "the 202 detail must carry the latency contract verbatim: {}",
            wired[0].2
        );
        // `already_requested` is audit detail, not a different answer.
        assert_eq!(
            CancelVerdict::Cancelling {
                job_id: 1,
                already_requested: true
            }
            .wire(),
            wired[0]
        );
    }

    /// `?` on the existing `Result<_, String>` helpers keeps working, which
    /// is what lets a check be added to a loop without rewriting the loop's
    /// error handling.
    #[test]
    fn a_string_error_converts_to_a_failed_stop() {
        fn failing() -> Result<(), PassStop> {
            Err("duckdb said no".to_owned())?;
            Ok(())
        }
        assert!(matches!(failing(), Err(PassStop::Failed(msg)) if msg == "duckdb said no"));
    }
}
