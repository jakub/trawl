// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cooperative cancellation of the running repin job.

use std::sync::Arc;

use axum::http::StatusCode;
use parking_lot::Mutex;
use trawl_api::RepinCancelOutcome;

/// The stage a cancel took effect in: the one vocabulary shared by the
/// audit events and the job row's error sentence.
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
    /// Nothing is running: no job is armed, the running job's task has
    /// fully ended, or its outcome is already settled and there is no
    /// cancellable work left to stop.
    NoJobRunning,
}

/// What [`CancelRegistry::request`] decided: the verdict the handler
/// renders, and the actor the registry retained for it.
///
/// The two are separate because a repeat request has two actors. The
/// durable record wants the first asker: `record_cancel_request` keeps
/// the first row to LAND, not the first to be decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelDecision {
    pub verdict: CancelVerdict,
    /// The actor the job row must name, present exactly when the verdict is
    /// [`CancelVerdict::Cancelling`]. On a first request it is this caller;
    /// on a repeat it is the earlier asker the registry kept.
    pub retained: Option<CancelActor>,
}

impl CancelDecision {
    fn plain(verdict: CancelVerdict) -> Self {
        Self {
            verdict,
            retained: None,
        }
    }
}

/// What [`CancelRegistry::commit`] decided at the point of no return.
#[derive(Debug, PartialEq, Eq)]
pub enum Commit<E> {
    /// The marker is published and the point of no return is latched.
    /// Everything past this is forward-only.
    Latched,
    /// A cancel is pending, or this job no longer owns the registry. No
    /// marker was written and nothing latched.
    Refused,
    /// Publishing the marker failed. Nothing latched; `E` is the writer's
    /// own error.
    Failed(E),
}

/// What [`CancelRegistry::settle`] decided for a job that is about to write
/// a terminal status short of the point of no return.
///
/// The two arms are exclusive by construction, because the choice is made
/// under the same lock a cancel request takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// No cancel held the job, so the caller's own verdict stands. The
    /// registry is latched settled: every later request answers
    /// [`CancelVerdict::NoJobRunning`].
    Candidate,
    /// A cancel was already pending. The caller must drop its candidate and
    /// terminalize as cancelled, naming this actor.
    Cancelled(CancelActor),
}

impl CancelVerdict {
    /// The one status/outcome/sentence table. The handler renders this and
    /// decides nothing.
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

    #[must_use]
    pub fn job_id(&self) -> Option<i64> {
        match self {
            Self::Cancelling { job_id, .. } | Self::PastPointOfNoReturn { job_id } => Some(*job_id),
            Self::NoJobRunning => None,
        }
    }
}

/// Who asked for a cancel.
///
/// `name` is operator-chosen and two keys may carry the same one.
/// `key_prefix` is the keystore's stable identifier for the key itself.
/// The prefix is the key's public half, so naming it leaks no token
/// material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelActor {
    name: String,
    key_prefix: String,
}

impl CancelActor {
    pub fn new(name: impl Into<String>, key_prefix: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            key_prefix: key_prefix.into(),
        }
    }

    /// The display name. What the job row's `cancelled_by` column and its
    /// error sentence carry.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The stable key prefix. Audit events only.
    #[must_use]
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }
}

#[derive(Debug, Clone)]
struct Entry {
    job_id: i64,
    /// The first asker, once a request has been accepted.
    cancelled_by: Option<CancelActor>,
    /// Latched by [`CancelRegistry::commit`] immediately before the
    /// Cutover marker write. Past it every request is refused.
    committed: bool,
    /// Latched by [`CancelRegistry::settle`] when a pre-cutover terminal
    /// verdict is chosen. Past it there is no work left to cancel, so a
    /// request answers `NoJobRunning` rather than accepting a stop that
    /// nothing would ever act on.
    settled: bool,
}

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
    /// An occupied slot should not happen: the request task disarms the
    /// registry before it drops its share of the repin permit, and no job
    /// is claimed without the permit. If it does, the new job owns the
    /// registry from here, the old entry's handle stops matching and its
    /// checks become no-ops, and the warning below reports the overlap.
    pub fn arm(self: &Arc<Self>, job_id: i64) -> CancelHandle {
        let mut slot = self.slot.lock();
        if let Some(previous) = slot.as_ref() {
            tracing::warn!(
                event_type = "repin_cancel_registry_overlap",
                job_id,
                previous_job_id = previous.job_id,
                "a new repin job armed the cancel registry while the \
                 previous job's entry was still armed, which the repin \
                 permit should prevent; the previous job is no longer \
                 cancellable"
            );
        }
        *slot = Some(Entry {
            job_id,
            cancelled_by: None,
            committed: false,
            settled: false,
        });
        CancelHandle {
            registry: Arc::clone(self),
            job_id,
        }
    }

    /// Ask to cancel whatever is running. The handler's one call.
    pub fn request(&self, by: &CancelActor) -> CancelDecision {
        let mut slot = self.slot.lock();
        let Some(entry) = slot.as_mut() else {
            return CancelDecision::plain(CancelVerdict::NoJobRunning);
        };
        // The two latches are asked in this order because they mean
        // different things to an operator. `committed` is "too late, the
        // corpus is being swapped" and earns the 409 the docs describe.
        // `settled` is "the job already has its answer", which reads the
        // same as nothing running: there is no work left to stop, and a
        // 409 would tell the operator the corpus is moving when it is not.
        if entry.committed {
            return CancelDecision::plain(CancelVerdict::PastPointOfNoReturn {
                job_id: entry.job_id,
            });
        }
        if entry.settled {
            return CancelDecision::plain(CancelVerdict::NoJobRunning);
        }
        let already_requested = entry.cancelled_by.is_some();
        let retained = entry.cancelled_by.get_or_insert_with(|| by.clone()).clone();
        CancelDecision {
            verdict: CancelVerdict::Cancelling {
                job_id: entry.job_id,
                already_requested,
            },
            retained: Some(retained),
        }
    }

    /// Cross the point of no return: refuse a pending cancel, publish the
    /// Cutover marker, and latch — all three under the one lock.
    ///
    /// `publish` is the marker write itself, and it runs inside the lock on
    /// purpose. `publish` must stay sync: nothing may await under this lock.
    ///
    /// # Errors
    /// [`Commit::Failed`] carries `publish`'s own error, with the entry
    /// left unlatched so the failure routes through the ordinary
    /// pre-cutover settlement, where a pending cancel may now win.
    pub fn commit<E>(&self, job_id: i64, publish: impl FnOnce() -> Result<(), E>) -> Commit<E> {
        let mut slot = self.slot.lock();
        let Some(entry) = slot
            .as_mut()
            .filter(|entry| entry.job_id == job_id && entry.cancelled_by.is_none())
        else {
            return Commit::Refused;
        };
        match publish() {
            Ok(()) => {
                entry.committed = true;
                Commit::Latched
            }
            Err(e) => Commit::Failed(e),
        }
    }

    /// Choose, under the one lock, between a pending cancel and the
    /// terminal verdict a job short of the point of no return is about to
    /// write.
    ///
    /// A job that no longer owns the registry settles nothing and gets
    /// [`Settlement::Candidate`]: its own verdict is all it can write, and
    /// its successor's entry must not be touched.
    pub fn settle(&self, job_id: i64) -> Settlement {
        let mut slot = self.slot.lock();
        let Some(entry) = slot.as_mut().filter(|entry| entry.job_id == job_id) else {
            return Settlement::Candidate;
        };
        // A latched job is past the point of no return and is not settling
        // a pre-cutover verdict; leave `committed` answering 409 rather
        // than downgrading it to 404.
        if entry.committed {
            return Settlement::Candidate;
        }
        entry.settled = true;
        entry
            .cancelled_by
            .clone()
            .map_or(Settlement::Candidate, Settlement::Cancelled)
    }

    /// The pending request for `job_id`, if the job is still armed, has
    /// been asked to cancel, and has not latched.
    #[must_use]
    pub fn pending(&self, job_id: i64) -> Option<CancelActor> {
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
    pub fn cancelled_by(&self) -> Option<CancelActor> {
        self.registry.pending(self.job_id)
    }

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
pub fn audit_cancel_requested(job_id: i64, actor: &CancelActor, already_requested: bool) {
    tracing::info!(
        event_type = "repin_cancel_requested",
        job_id,
        actor = %actor.name(),
        actor_key_prefix = %actor.key_prefix(),
        already_requested,
        "repin cancellation requested"
    );
}

/// The audit event for the effect: a file boundary observed the request and
/// the unwind is about to run. Emitted before `abandon_build`'s warn line,
/// so the log reads request → effect → sweep.
pub fn audit_cancelled(job_id: i64, actor: &CancelActor, stage: &str) {
    tracing::info!(
        event_type = "repin_cancelled",
        job_id,
        actor = %actor.name(),
        actor_key_prefix = %actor.key_prefix(),
        stage = %stage,
        "repin job cancelled before the point of no return; the live \
         corpus was never touched"
    );
}

/// The audit event for a refusal. Only the 409 has a job to name; a 404
/// emits nothing, because an event with no subject is noise.
pub fn audit_cancel_refused(job_id: i64, actor: &CancelActor) {
    tracing::info!(
        event_type = "repin_cancel_refused",
        job_id,
        actor = %actor.name(),
        actor_key_prefix = %actor.key_prefix(),
        "repin cancellation refused: the job is past its point of no return"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(name: &str) -> CancelActor {
        CancelActor::new(name, format!("trwl_{name}"))
    }

    /// Latch with a publication that always succeeds: the tests that do
    /// not care about the marker write.
    fn latch(registry: &CancelRegistry, job_id: i64) -> Commit<String> {
        registry.commit(job_id, || Ok(()))
    }

    fn armed(job_id: i64) -> (Arc<CancelRegistry>, CancelHandle) {
        let registry = Arc::new(CancelRegistry::default());
        let handle = registry.arm(job_id);
        (registry, handle)
    }

    /// The race the whole module exists for, driven from two threads: one
    /// asks to cancel while the other latches the point of no return.
    #[test]
    fn cancel_and_commit_race_to_one_answer_from_two_threads() {
        let mut cancelled_won = 0;
        let mut committed_won = 0;
        for _ in 0..200 {
            let (registry, _handle) = armed(7);
            let asker = Arc::clone(&registry);
            let closer = Arc::clone(&registry);
            let request = std::thread::spawn(move || asker.request(&actor("operator")).verdict);
            let commit = std::thread::spawn(move || latch(&closer, 7));
            let verdict = request.join().expect("request thread");
            let latched = commit.join().expect("commit thread");

            match (verdict, latched) {
                (CancelVerdict::Cancelling { job_id, .. }, Commit::Refused) => {
                    assert_eq!(job_id, 7);
                    assert_eq!(registry.pending(7), Some(actor("operator")));
                    cancelled_won += 1;
                }
                (CancelVerdict::PastPointOfNoReturn { job_id }, Commit::Latched) => {
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
        assert_eq!(cancelled_won + committed_won, 200);
    }

    #[test]
    fn a_latched_job_refuses_every_later_request() {
        let (registry, handle) = armed(11);
        assert_eq!(latch(&registry, 11), Commit::Latched);
        for _ in 0..3 {
            assert_eq!(
                registry.request(&actor("operator")).verdict,
                CancelVerdict::PastPointOfNoReturn { job_id: 11 }
            );
        }
        assert_eq!(handle.check(STAGE_BUILD).ok(), Some(()));
        assert_eq!(handle.cancelled_by(), None);
    }

    #[test]
    fn a_failed_publication_leaves_the_job_cancellable() {
        let (registry, handle) = armed(11);
        assert_eq!(
            registry.commit(11, || Err("no space left on device".to_owned())),
            Commit::Failed("no space left on device".to_owned())
        );

        assert_eq!(
            registry.request(&actor("alice")).verdict,
            CancelVerdict::Cancelling {
                job_id: 11,
                already_requested: false
            },
            "a job that never published its marker never crossed"
        );
        assert_eq!(
            registry.settle(11),
            Settlement::Cancelled(actor("alice")),
            "and the pre-cutover settlement hands it the cancel"
        );
        assert_eq!(handle.cancelled_by(), Some(actor("alice")));

        // A second attempt after a pending cancel is refused, as any latch
        // is: the failed write did not put the entry in a state that skips
        // the arbitration.
        assert_eq!(latch(&registry, 11), Commit::Refused);
    }

    #[test]
    fn a_pending_request_stops_the_latch() {
        let (registry, handle) = armed(11);
        assert_eq!(
            registry.request(&actor("alice")).verdict,
            CancelVerdict::Cancelling {
                job_id: 11,
                already_requested: false
            }
        );
        assert_eq!(latch(&registry, 11), Commit::Refused);
        assert!(matches!(
            handle.check(STAGE_FINAL_GATE),
            Err(PassStop::Cancelled {
                stage: STAGE_FINAL_GATE
            })
        ));
        assert_eq!(handle.cancelled_by(), Some(actor("alice")));
    }

    #[test]
    fn a_settled_job_has_nothing_left_to_cancel() {
        let (registry, _handle) = armed(5);
        assert_eq!(registry.settle(5), Settlement::Candidate);
        for _ in 0..3 {
            assert_eq!(
                registry.request(&actor("operator")).verdict,
                CancelVerdict::NoJobRunning
            );
        }
        assert_eq!(
            registry.settle(5),
            Settlement::Candidate,
            "settling twice is the same answer; a request cannot slip in \
             between two terminal writes"
        );
    }

    #[test]
    fn a_pending_request_beats_a_terminal_candidate() {
        let (registry, handle) = armed(5);
        assert!(matches!(
            registry.request(&actor("alice")).verdict,
            CancelVerdict::Cancelling { .. }
        ));
        assert_eq!(registry.settle(5), Settlement::Cancelled(actor("alice")));
        // The effect site still reads the actor it audits and records.
        assert_eq!(handle.cancelled_by(), Some(actor("alice")));
        // And the job is settled either way: a second asker is not told
        // their cancel was accepted when the first one already took it.
        assert_eq!(
            registry.request(&actor("bob")).verdict,
            CancelVerdict::NoJobRunning
        );
    }

    #[test]
    fn a_stale_settle_does_not_latch_a_live_entry() {
        let registry = Arc::new(CancelRegistry::default());
        let _first = registry.arm(1);
        let _second = registry.arm(2);
        assert_eq!(registry.settle(1), Settlement::Candidate);
        assert!(matches!(
            registry.request(&actor("operator")).verdict,
            CancelVerdict::Cancelling { job_id: 2, .. }
        ));
    }

    #[test]
    fn settling_never_downgrades_the_point_of_no_return() {
        let (registry, _handle) = armed(9);
        assert_eq!(latch(&registry, 9), Commit::Latched);
        assert_eq!(registry.settle(9), Settlement::Candidate);
        assert_eq!(
            registry.request(&actor("operator")).verdict,
            CancelVerdict::PastPointOfNoReturn { job_id: 9 }
        );
    }

    #[test]
    fn a_second_request_preserves_the_first_actor() {
        let (registry, handle) = armed(3);
        assert_eq!(
            registry.request(&actor("alice")).verdict,
            CancelVerdict::Cancelling {
                job_id: 3,
                already_requested: false
            }
        );
        assert_eq!(
            registry.request(&actor("bob")).verdict,
            CancelVerdict::Cancelling {
                job_id: 3,
                already_requested: true
            }
        );
        assert_eq!(handle.cancelled_by(), Some(actor("alice")));
    }

    #[test]
    fn a_repeat_request_carries_the_first_actor_for_persistence() {
        let (registry, _handle) = armed(3);
        let first = registry.request(&actor("alice"));
        assert_eq!(first.retained, Some(actor("alice")));

        let repeat = registry.request(&actor("bob"));
        assert_eq!(
            repeat.verdict,
            CancelVerdict::Cancelling {
                job_id: 3,
                already_requested: true
            },
            "bob is still told the cancel is accepted"
        );
        assert_eq!(
            repeat.retained,
            Some(actor("alice")),
            "but the name that reaches the job row is the first asker's"
        );

        // The refusals have nothing to persist.
        let (settled, _handle) = armed(4);
        settled.settle(4);
        assert_eq!(settled.request(&actor("bob")).retained, None);
        let (latched, _handle) = armed(5);
        assert_eq!(latch(&latched, 5), Commit::Latched);
        assert_eq!(latched.request(&actor("bob")).retained, None);
    }

    #[test]
    fn a_stale_disarm_does_not_clear_a_live_entry() {
        let registry = Arc::new(CancelRegistry::default());
        let _first = registry.arm(1);
        let second = registry.arm(2);

        registry.disarm(1);
        assert_eq!(
            registry.request(&actor("operator")).verdict,
            CancelVerdict::Cancelling {
                job_id: 2,
                already_requested: false
            },
            "job 2 is still armed"
        );
        assert_eq!(second.cancelled_by(), Some(actor("operator")));

        registry.disarm(2);
        assert_eq!(
            registry.request(&actor("operator")).verdict,
            CancelVerdict::NoJobRunning
        );
        assert_eq!(second.cancelled_by(), None);
    }

    #[test]
    fn commit_needs_the_job_to_own_the_registry() {
        let registry = Arc::new(CancelRegistry::default());
        assert_eq!(latch(&registry, 1), Commit::Refused);
        let _handle = registry.arm(1);
        assert_eq!(
            latch(&registry, 2),
            Commit::Refused,
            "another job's id may not latch"
        );
        assert_eq!(latch(&registry, 1), Commit::Latched);
    }

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

    #[test]
    fn a_string_error_converts_to_a_failed_stop() {
        fn failing() -> Result<(), PassStop> {
            Err("duckdb said no".to_owned())?;
            Ok(())
        }
        assert!(matches!(failing(), Err(PassStop::Failed(msg)) if msg == "duckdb said no"));
    }
}
