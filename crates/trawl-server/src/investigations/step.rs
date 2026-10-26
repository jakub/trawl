// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The step an attached capture appends to its investigation, and the
//! completion that writes the step's terminal outcome (ADR-0059).
//!
//! A capture that names an investigation appends one `step` entry at
//! admission, with no outcome, and writes the outcome once when its work
//! ends: one step, two moments. The second moment belongs to a
//! [`StepCompletion`], a handle whose clones ride everything that can
//! outlive the request. The admission builds it in a task of its own the
//! moment the step commits ([`begin`]), so a request that goes away before
//! it takes the handle leaves the handle there, and the task writes
//! `aborted` inside the request's count, which it gives back no later than
//! the capture's deadline ([`StepCompletion::finish`]). From then on:
//!
//! - one rides the executor slot's holds beside the admission
//!   ([`crate::pool::WorkContext::holding`]), so it lives until the work
//!   physically stops, however the request ends;
//! - one rides the request, and passes to the commit task once the work
//!   has produced a result.
//!
//! Whoever learns how the work ended records it ([`StepCompletion::record`]).
//! The first record wins and later ones change nothing. A success is
//! written by the transaction that keeps the result
//! ([`crate::store::ResultsStore::insert_kept`]), which then marks the
//! completion [`StepCompletion::written`]. When the last
//! handle drops, the outcome joins the queue of the one [`StepWriter`], so
//! a drop on the pool's blocking thread still writes it. A completion
//! dropped with nothing recorded writes `aborted`: the client went away
//! before the commit task took over, and the work stopped. The store
//! writes an outcome only on a step that has none, so the fill never runs
//! twice.
//!
//! The writer writes the outcomes no request waits for, in turn: those the
//! last handle's drop leaves, and those a bounded write handed over past
//! its capture's deadline ([`StepCompletion::finish`]). One task takes them
//! from one queue and writes each in a transaction bounded by `[capture]
//! deadline_secs`, so however many steps are abandoned at once, their
//! writes hold at most one connection and wait on at most one lock. An
//! attempt that reaches its bound, or the pool's own acquire timeout,
//! writes nothing and goes to the back of the queue to be tried again:
//! the fill is written once, whenever the lock it waits on is released.
//! The one wait the bound leaves alone is a `COMMIT` already sent, awaited
//! until it is answered, since cutting it short would leave the outcome
//! unknown; the queue waits with it.
//! A write that fails otherwise is logged.
//!
//! The writer's places bound its queue, since a drop cannot wait to join
//! it. A step takes a place before it is appended ([`StepWriter::admit`]),
//! waiting no later than its capture's deadline, and a capture that finds
//! none by then answers 429 `capture_busy` with no step written. The place
//! rides the completion and then its write, and goes back only once the
//! outcome is written, or its write has failed for good: never when a
//! write is handed over or put back in the queue. A completion whose
//! outcome the kept transaction wrote gives it back with its last handle.
//! The writer has `[capture] installation_running + queue_depth` places,
//! so however long a sent `COMMIT` waits for its answer, at most that many
//! outcomes wait for the writer, the one it awaits included.
//!
//! A runtime that shuts down drops the writer with its queue; boot records
//! `interrupted` on every step left without an outcome
//! ([`crate::store::InvestigationStore::interrupt_running_steps`]), and so
//! it does for a write that failed. Those writes do not go through the
//! writer and take no place.
//!
//! Each written outcome logs one `investigation_step` line. The capture's
//! ADR-0046 timing event gains no field.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use fleet_auth::VerifiedKey;
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use trawl_api::investigations::{
    Attach, EntryPayload, InvestigationId, PURPOSE_MAX_BYTES, StepBounds, StepOutcome,
    StepOutcomeKind,
};

use crate::config::InvestigationConfig;
use crate::deadline::{Deadline, Expired};
use crate::error::ServerError;
use crate::results::ResultsClock;
use crate::state::AppState;
use crate::store::{Appended, InvestigationStore, RetentionRule, StoreActor, StoreError};
use crate::transport::request_limit::{self, Lease};

/// How far each change slides the evidence clock: `[investigation]
/// evidence_idle_secs`, as the investigation routes slide it.
#[must_use]
pub(crate) fn retention(config: &InvestigationConfig) -> RetentionRule {
    RetentionRule {
        idle: i64::try_from(config.evidence_idle_secs)
            .ok()
            .and_then(TimeDelta::try_seconds)
            .unwrap_or(TimeDelta::MAX),
    }
}

/// Append the step of an attached capture at admission: the executed text,
/// its bounds (none for a stored source), the request and query ids and the
/// caller's purpose, with no outcome. Unlinks the evidence the due-write
/// rule pruned before the append. Answers the step's completion, made on
/// this runtime.
///
/// The request first waits for one of the [`StepWriter`]'s places, no
/// later than `deadline`, and a request that goes away meanwhile leaves
/// nothing behind. The admission takes the place, and the step's
/// completion holds it once the step commits.
///
/// The admission runs in a task of its own, which builds the completion as
/// soon as the step commits and hands it to the request. A request that
/// goes away during the append or the unlink cuts neither short: the
/// handover finds no one, and the task writes `aborted` at once
/// ([`StepCompletion::finish`]), since the request that would have run the
/// work is gone. The promotion of a cited result runs the same way, for
/// the same reason ([`crate::investigations::service`]).
///
/// The task keeps the request's count ([`request_limit::current`]) on a
/// [`Lease`] from the moment it is spawned until the admission and that
/// write end, and no later than the capture's `deadline` whatever it
/// awaits then, an unlink included, so an admission that outlives its
/// request still counts against `[server] max_concurrent_requests`
/// (ADR-0054) and never past its deadline. The admission's
/// waits, for a connection, the investigation, its evidence, its log and
/// each answer alike, end together at `deadline` with nothing written,
/// and so do the `aborted` write's, which the [`StepWriter`] then
/// finishes outside the count, a rollback's wait included. No commit is
/// cut short, and none is sent after the deadline: a commit sent before
/// it lands, and the unlink may end after it, outside the count.
///
/// # Errors
/// 404 for an unknown investigation, 409 `investigation_closed`, 429
/// `capture_busy` when the wait for a place or the admission's reached
/// `deadline`, or a store failure.
#[allow(clippy::too_many_arguments)] // one admission, each part named
pub(crate) async fn begin(
    state: &AppState,
    verified: &VerifiedKey,
    attach: &Attach,
    executed: &str,
    bounds: Option<StepBounds>,
    request_id: &str,
    query_id: u64,
    deadline: Deadline,
) -> Result<StepCompletion, ServerError> {
    let payload = EntryPayload::Step {
        executed: executed.to_owned(),
        bounds,
        request_id: request_id.to_owned(),
        query_id: Some(query_id),
        purpose: attach.purpose.clone(),
        outcome: None,
    };
    let actor = StoreActor {
        key_id: verified.id,
        prefix: verified.prefix.clone(),
        name: verified.name.clone(),
    };
    let results = Arc::clone(&state.results);
    // No place came free by the deadline: the capture never started, so
    // busy, as a deadline that expires in the gate's admission is.
    let place =
        results
            .steps
            .admit(deadline)
            .await
            .map_err(|Expired| ServerError::CaptureBusy {
                retry_after_secs: 1,
            })?;
    let store = state.storage.investigations.clone();
    let id = attach.id.clone();
    let count = Lease::until(request_limit::current(), deadline);
    let (handover, taken) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let retention = retention(&results.investigation);
        let begun = store
            .begin_step(
                &id,
                &actor,
                &payload,
                results.clock.now(),
                retention,
                deadline,
            )
            .await;
        let completion = begun.map(|Appended { entry, pruned }| {
            (
                StepCompletion::new(
                    store,
                    results.clock.clone(),
                    retention,
                    id,
                    entry.seq,
                    results.steps.clone(),
                    place,
                ),
                pruned,
            )
        });
        let answer = match completion {
            Ok((completion, pruned)) => {
                crate::investigations::service::unlink_evidence(&results.root, pruned).await;
                Ok(completion)
            }
            Err(e) => Err(e),
        };
        // Refused, the request is gone: nothing recorded, so the
        // completion writes `aborted`, here and inside the request's count.
        match handover.send(answer) {
            Err(Ok(completion)) => completion.finish(deadline, count).await,
            Ok(()) | Err(Err(_)) => drop(count),
        }
    });
    taken
        .await
        .map_err(|_| ServerError::Internal("the step's admission ended without an answer".into()))?
        .map_err(|e| match e {
            // The capture never started: busy, as a deadline that expires
            // in the gate's admission is.
            StoreError::WaitExpired => ServerError::CaptureBusy {
                retry_after_secs: 1,
            },
            e => ServerError::from(e),
        })
}

/// 400 unless the attachment's purpose is within its bound and free of
/// NUL, which the store's text refuses.
///
/// # Errors
/// 400 naming the purpose and the rule it breaks.
pub(crate) fn check_attach(attach: &Attach) -> Result<(), ServerError> {
    let Some(purpose) = &attach.purpose else {
        return Ok(());
    };
    if purpose.len() > PURPOSE_MAX_BYTES {
        return Err(ServerError::BadRequest(format!(
            "purpose is {} bytes, above the limit of {PURPOSE_MAX_BYTES} bytes",
            purpose.len()
        )));
    }
    if purpose.contains('\0') {
        return Err(ServerError::BadRequest(
            "purpose contains a NUL character, which cannot be stored".into(),
        ));
    }
    Ok(())
}

/// The outcome of work that ended with `error` after `duration_ms`: a
/// timeout, a refusal by a limit or a budget, or a failure, each with the
/// error's wire code.
#[must_use]
pub(crate) fn outcome_of(error: &ServerError, duration_ms: Option<u64>) -> StepOutcome {
    let outcome = match error {
        ServerError::Timeout => StepOutcomeKind::Timeout,
        ServerError::CaptureBusy { .. }
        | ServerError::CaptureQuotaExceeded { .. }
        | ServerError::EvidenceBudgetExceeded { .. } => StepOutcomeKind::Refused,
        _ => StepOutcomeKind::Failed,
    };
    StepOutcome {
        outcome,
        code: Some(error.code()),
        result_id: None,
        rows: None,
        duration_ms,
    }
}

/// The outcome of work that kept result `result_id` with `rows` rows.
#[must_use]
pub(crate) fn success(result_id: &str, rows: u64, duration_ms: u64) -> StepOutcome {
    StepOutcome {
        outcome: StepOutcomeKind::Success,
        code: None,
        result_id: Some(result_id.to_owned()),
        rows: Some(rows),
        duration_ms: Some(duration_ms),
    }
}

/// What the last handle's drop writes.
#[derive(Debug)]
enum Fill {
    /// Nothing recorded: the work was abandoned.
    Pending,
    /// The outcome to write.
    Recorded(StepOutcome),
    /// A transaction of the caller's wrote it already.
    Written,
}

/// The completion of one step (module docs). Clones share it; the last to
/// drop writes the outcome.
#[derive(Debug, Clone)]
pub struct StepCompletion(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    store: InvestigationStore,
    clock: ResultsClock,
    retention: RetentionRule,
    investigation: InvestigationId,
    seq: u64,
    /// When the step was appended, for an abandoned step's duration.
    began: Instant,
    /// Where the last handle's write goes, whatever thread drops it.
    writer: StepWriter,
    /// The step's place among `writer`'s, until its write takes it
    /// ([`Inner::take_write`]).
    place: Option<OwnedSemaphorePermit>,
    fill: Mutex<Fill>,
}

impl StepCompletion {
    /// The completion of step `seq` of `investigation`, writing through
    /// `store` at `clock`'s time under `retention`, its last handle's
    /// write handed to `writer`, holding `place`, one of `writer`'s
    /// ([`StepWriter::admit`]), until the outcome is written.
    #[must_use]
    pub(crate) fn new(
        store: InvestigationStore,
        clock: ResultsClock,
        retention: RetentionRule,
        investigation: InvestigationId,
        seq: u64,
        writer: StepWriter,
        place: OwnedSemaphorePermit,
    ) -> Self {
        Self(Arc::new(Inner {
            store,
            clock,
            retention,
            investigation,
            seq,
            began: Instant::now(),
            writer,
            place: Some(place),
            fill: Mutex::new(Fill::Pending),
        }))
    }

    /// Record how the work ended, to be written when the last handle drops.
    /// The first record wins.
    pub(crate) fn record(&self, outcome: StepOutcome) {
        let mut fill = self.0.fill.lock();
        if matches!(*fill, Fill::Pending) {
            *fill = Fill::Recorded(outcome);
        }
    }

    /// The step's entry in its investigation's log.
    #[must_use]
    pub(crate) fn seq(&self) -> u64 {
        self.0.seq
    }

    /// How far the outcome's write slides the evidence clock.
    #[must_use]
    pub(crate) fn retention(&self) -> RetentionRule {
        self.0.retention
    }

    /// A transaction of the caller's wrote `outcome`, so the last handle
    /// writes nothing. Wins over anything recorded, since the store holds
    /// it. Logs the line the drop logs for a write.
    pub(crate) fn written(&self, outcome: &StepOutcome) {
        *self.0.fill.lock() = Fill::Written;
        tracing::info!(
            event_type = "investigation_step",
            investigation_id = %self.0.investigation,
            step_seq = self.0.seq,
            outcome = outcome_name(outcome.outcome),
            written = true,
            "an investigation step's work ended"
        );
    }

    /// Drop this handle, and when it is the last, write the outcome on this
    /// task and wait for the write, instead of in a task of its own: the
    /// write the drop would make, ended before the caller goes on.
    ///
    /// `count` is the request's, leased no later than `deadline`
    /// (ADR-0059). Until then the write waits for a connection and its
    /// locks inside it, through the pool's own acquire timeout if that
    /// ends first. Past the deadline the count is back, one line says so,
    /// and the write joins the [`StepWriter`]'s queue, outside the count,
    /// as any abandoned step's does: the step never stays running for want
    /// of a deadline.
    pub(crate) async fn finish(self, deadline: Deadline, count: Lease) {
        if let Ok(mut inner) = Arc::try_unwrap(self.0)
            && let Some(write) = inner.take_write()
        {
            write.within(deadline, count, &inner.writer).await;
        }
    }
}

/// The outcome the last handle writes, once ([`Inner::take_write`]).
struct Write {
    store: InvestigationStore,
    investigation: InvestigationId,
    seq: u64,
    outcome: StepOutcome,
    now: DateTime<Utc>,
    retention: RetentionRule,
    /// The step's place among the writer's, given back once the outcome
    /// is written or its write failed for good ([`Self::log`]).
    _place: OwnedSemaphorePermit,
}

impl Write {
    /// Write the outcome, waiting for a connection and the locks no later
    /// than `deadline`.
    async fn attempt(&self, deadline: Deadline) -> Result<bool, StoreError> {
        self.store
            .complete_step_until(
                &self.investigation,
                self.seq,
                &self.outcome,
                self.now,
                self.retention,
                deadline,
            )
            .await
    }

    /// Write the outcome inside `count` no later than `deadline`, and past
    /// it hand it to `writer`, outside the count
    /// ([`StepCompletion::finish`]). A wait for a connection that the
    /// pool's own acquire timeout ends before the deadline is tried again:
    /// the write keeps the count until the deadline, and is handed over
    /// only once it has passed.
    async fn within(self, deadline: Deadline, count: Lease, writer: &StepWriter) {
        let written = loop {
            match self.attempt(deadline).await {
                Err(StoreError::Unavailable(sqlx::Error::PoolTimedOut)) if !deadline.expired() => {}
                written => break written,
            }
        };
        drop(count);
        if matches!(
            written,
            Err(StoreError::WaitExpired | StoreError::Unavailable(sqlx::Error::PoolTimedOut))
        ) {
            tracing::warn!(
                event_type = "investigation_step_past_deadline",
                investigation_id = %self.investigation,
                step_seq = self.seq,
                outcome = outcome_name(self.outcome.outcome),
                "an investigation step's outcome waited past its capture's deadline; it is \
                 written outside the request's count"
            );
            writer.enqueue(self);
        } else {
            self.log(written);
        }
    }

    /// Log the line a write logs, and give the step's place back: the
    /// outcome is written, or its write failed for good.
    fn log(self, written: Result<bool, StoreError>) {
        match written {
            Ok(written) => tracing::info!(
                event_type = "investigation_step",
                investigation_id = %self.investigation,
                step_seq = self.seq,
                outcome = outcome_name(self.outcome.outcome),
                written,
                "an investigation step's work ended"
            ),
            Err(error) => tracing::warn!(
                event_type = "investigation_step",
                investigation_id = %self.investigation,
                step_seq = self.seq,
                outcome = outcome_name(self.outcome.outcome),
                error_class = ServerError::from(error).error_class(),
                "could not write an investigation step's outcome; boot records it as \
                 interrupted"
            ),
        }
    }
}

impl Inner {
    /// The write the last handle makes, once, carrying the step's place:
    /// `None` when a transaction of the caller's wrote the outcome, whose
    /// place goes back with this completion, or this was taken before.
    fn take_write(&mut self) -> Option<Write> {
        let outcome = match std::mem::replace(self.fill.get_mut(), Fill::Written) {
            Fill::Written => return None,
            Fill::Recorded(outcome) => outcome,
            Fill::Pending => StepOutcome {
                outcome: StepOutcomeKind::Aborted,
                code: None,
                result_id: None,
                rows: None,
                duration_ms: Some(
                    u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
            },
        };
        Some(Write {
            store: self.store.clone(),
            investigation: self.investigation.clone(),
            seq: self.seq,
            outcome,
            now: self.clock.now(),
            retention: self.retention,
            _place: self.place.take()?,
        })
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(write) = self.take_write() {
            self.writer.enqueue(write);
        }
    }
}

/// The one writer of the step outcomes no request waits for (module docs).
/// Clones share its queue and its places.
#[derive(Debug, Clone)]
pub struct StepWriter {
    queue: mpsc::UnboundedSender<Write>,
    /// One permit per step appended whose outcome is not yet written.
    places: Arc<Semaphore>,
}

impl StepWriter {
    /// Start the writer on this runtime, each attempt bounded by `budget`:
    /// `[capture] deadline_secs`, with `places` for the steps whose
    /// outcome is not yet written: `[capture] installation_running +
    /// queue_depth`.
    ///
    /// # Panics
    /// Outside a Tokio runtime.
    #[must_use]
    pub fn start(budget: Duration, places: usize) -> Self {
        let (queue, writes) = mpsc::unbounded_channel();
        tokio::spawn(write_in_turn(writes, budget));
        Self {
            queue,
            places: Arc::new(Semaphore::new(places.min(Semaphore::MAX_PERMITS))),
        }
    }

    /// Take a place for one step about to be appended, waiting no later
    /// than `deadline` (module docs).
    ///
    /// # Errors
    /// [`Expired`] when no place came free by `deadline`.
    pub(crate) async fn admit(&self, deadline: Deadline) -> Result<OwnedSemaphorePermit, Expired> {
        match deadline.run(Arc::clone(&self.places).acquire_owned()).await {
            Ok(Ok(place)) => Ok(place),
            // The writer never closes its semaphore, so only the deadline
            // ends this wait without a place.
            Ok(Err(_)) | Err(Expired) => Err(Expired),
        }
    }

    /// Queue `write` behind every write already waiting. The channel is
    /// unbounded, since a drop cannot wait; the place each write carries
    /// bounds the queue (module docs). A writer that has stopped went with
    /// its runtime, and boot records the step `interrupted`.
    fn enqueue(&self, write: Write) {
        let _ = self.queue.send(write);
    }
}

/// The writer's task: take the writes in arrival order, one at a time, and
/// put each attempt that reached `budget` behind the writes that arrived
/// meanwhile, its place with it.
async fn write_in_turn(mut writes: mpsc::UnboundedReceiver<Write>, budget: Duration) {
    let mut queue = VecDeque::new();
    loop {
        while let Ok(write) = writes.try_recv() {
            queue.push_back(write);
        }
        let Some(write) = queue.pop_front() else {
            match writes.recv().await {
                Some(write) => queue.push_back(write),
                None => return,
            }
            continue;
        };
        let written = write.attempt(Deadline::after(budget)).await;
        if matches!(
            written,
            Err(StoreError::WaitExpired | StoreError::Unavailable(sqlx::Error::PoolTimedOut))
        ) {
            while let Ok(arrived) = writes.try_recv() {
                queue.push_back(arrived);
            }
            queue.push_back(write);
        } else {
            write.log(written);
        }
    }
}

/// The outcome's wire spelling, for the log line.
const fn outcome_name(outcome: StepOutcomeKind) -> &'static str {
    match outcome {
        StepOutcomeKind::Success => "success",
        StepOutcomeKind::Refused => "refused",
        StepOutcomeKind::Timeout => "timeout",
        StepOutcomeKind::Failed => "failed",
        StepOutcomeKind::Aborted => "aborted",
        StepOutcomeKind::Interrupted => "interrupted",
    }
}

#[cfg(test)]
mod pg_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::{DateTime, TimeDelta, TimeZone as _, Utc};
    use sqlx::{Connection as _, PgPool};
    use tokio::sync::OwnedSemaphorePermit;
    use trawl_api::investigations::{
        CreateInvestigation, EntryPayload, InvestigationId, StepOutcomeKind,
    };

    use super::{StepCompletion, StepWriter};
    use crate::deadline::Deadline;
    use crate::results::ResultsClock;
    use crate::store::{InvestigationStore, NewInvestigation, RetentionRule, StoreActor};
    use crate::transport::request_limit::Lease;

    const RULE: RetentionRule = RetentionRule {
        idle: TimeDelta::days(7),
    };

    /// The places of a writer whose test does not test them: more than
    /// any test here holds at once.
    const PLACES: usize = 4;

    /// One of `writer`'s places, free at once.
    fn place(writer: &StepWriter) -> OwnedSemaphorePermit {
        Arc::clone(&writer.places)
            .try_acquire_owned()
            .expect("a free place")
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 9, 0, 0).unwrap()
    }

    fn agent() -> StoreActor {
        StoreActor {
            key_id: 7,
            prefix: "AbCd_-12".into(),
            name: "agent".into(),
        }
    }

    /// Sessions of this database waiting on a lock.
    async fn lock_waiters(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Wait, bounded, until `count` sessions wait on a lock.
    async fn until_waiters(pool: &PgPool, count: i64) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while lock_waiters(pool).await != count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} sessions never waited on a lock"));
    }

    /// A write handed over past its deadline and one a last handle's drop
    /// leaves go to the same writer, in the order they arrive. With the
    /// investigation's lock held, the first step's bounded write reaches
    /// its deadline and is handed over, and the writer's attempt waits on
    /// the lock; the second step's completion then drops. One session
    /// still waits, and once the lock goes the first is written before the
    /// second.
    #[sqlx::test]
    async fn a_handed_over_write_and_a_dropped_one_share_one_writer(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let id = InvestigationId::parse(&format!("{:032x}", 310)).unwrap();
        store
            .create_if_absent(&NewInvestigation {
                id: id.clone(),
                actor: agent(),
                creation: CreateInvestigation {
                    title: "502s on checkout".into(),
                    question: "Why does checkout return 502?".into(),
                    scope: "checkout, last day".into(),
                },
                now: at(),
                retain_until: at() + TimeDelta::days(7),
                retain_ceiling: at() + TimeDelta::days(30),
            })
            .await
            .unwrap();
        let mut seqs = Vec::new();
        for request in ["r1", "r2"] {
            let step = EntryPayload::Step {
                executed: "service=checkout".into(),
                bounds: None,
                request_id: request.into(),
                query_id: None,
                purpose: None,
                outcome: None,
            };
            let begun = store
                .begin_step(
                    &id,
                    &agent(),
                    &step,
                    at(),
                    RULE,
                    Deadline::after(Duration::from_mins(1)),
                )
                .await
                .unwrap();
            seqs.push(begun.entry.seq);
        }
        let writer = StepWriter::start(Duration::from_mins(1), PLACES);
        let completion = |seq| {
            StepCompletion::new(
                store.clone(),
                ResultsClock::fixed(at()),
                RULE,
                id.clone(),
                seq,
                writer.clone(),
                place(&writer),
            )
        };

        let mut held = sqlx::PgConnection::connect_with(&pool.connect_options())
            .await
            .unwrap();
        sqlx::query("BEGIN").execute(&mut held).await.unwrap();
        sqlx::query("SELECT 1 FROM investigations WHERE id = $1 FOR UPDATE")
            .bind(id.as_str())
            .execute(&mut held)
            .await
            .unwrap();

        let deadline = Deadline::after(Duration::from_millis(500));
        completion(seqs[0])
            .finish(deadline, Lease::until(None, deadline))
            .await;
        assert!(
            deadline.expired(),
            "the bounded write ended before its deadline"
        );
        until_waiters(&pool, 1).await;
        drop(completion(seqs[1]));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lock_waiters(&pool).await, 1, "a second write waits");

        sqlx::query("COMMIT").execute(&mut held).await.unwrap();
        let give_up = tokio::time::Instant::now() + Duration::from_secs(30);
        let written = loop {
            let page = store.read_since(&id, 0, 10).await.unwrap().unwrap();
            let outcomes: Vec<_> = page
                .entries
                .iter()
                .filter_map(|entry| match &entry.payload {
                    EntryPayload::Step {
                        outcome: Some(outcome),
                        ..
                    } => Some((entry.seq, entry.updated_seq, outcome.outcome)),
                    _ => None,
                })
                .collect();
            if outcomes.len() == 2 {
                break outcomes;
            }
            assert!(
                tokio::time::Instant::now() < give_up,
                "the writes never landed: {page:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(
            written,
            [
                (seqs[0], 3, StepOutcomeKind::Aborted),
                (seqs[1], 4, StepOutcomeKind::Aborted)
            ],
            "written in the order they reached the writer"
        );
    }

    /// Investigation `n` with one running step, through `store`. Answers
    /// the investigation and the step's sequence.
    async fn running_step(store: &InvestigationStore, n: u32) -> (InvestigationId, u64) {
        let id = InvestigationId::parse(&format!("{n:032x}")).unwrap();
        store
            .create_if_absent(&NewInvestigation {
                id: id.clone(),
                actor: agent(),
                creation: CreateInvestigation {
                    title: "502s on checkout".into(),
                    question: "Why does checkout return 502?".into(),
                    scope: "checkout, last day".into(),
                },
                now: at(),
                retain_until: at() + TimeDelta::days(7),
                retain_ceiling: at() + TimeDelta::days(30),
            })
            .await
            .unwrap();
        let step = EntryPayload::Step {
            executed: "service=checkout".into(),
            bounds: None,
            request_id: format!("r{n}"),
            query_id: None,
            purpose: None,
            outcome: None,
        };
        let begun = store
            .begin_step(
                &id,
                &agent(),
                &step,
                at(),
                RULE,
                Deadline::after(Duration::from_mins(1)),
            )
            .await
            .unwrap();
        (id, begun.entry.seq)
    }

    /// The outcome step `seq` of `id` holds, if any.
    async fn outcome(
        store: &InvestigationStore,
        id: &InvestigationId,
        seq: u64,
    ) -> Option<StepOutcomeKind> {
        let page = store.read_since(id, 0, 10).await.unwrap().unwrap();
        page.entries.iter().find_map(|entry| match &entry.payload {
            EntryPayload::Step {
                outcome: Some(outcome),
                ..
            } if entry.seq == seq => Some(outcome.outcome),
            _ => None,
        })
    }

    /// Wait until step `seq` of `id` holds an outcome, no later than
    /// `give_up`.
    async fn until_written(
        store: &InvestigationStore,
        id: &InvestigationId,
        seq: u64,
        give_up: tokio::time::Instant,
        what: &str,
    ) {
        while outcome(store, id, seq).await.is_none() {
            assert!(tokio::time::Instant::now() < give_up, "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A one-connection pool through which every wait for a connection
    /// gives up after 300 ms, the pool's own acquire timeout, well before
    /// a test's deadline.
    async fn impatient(pool: &PgPool) -> PgPool {
        #[allow(clippy::disallowed_methods)] // a pool whose timeout is the test's
        let impatient = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(300))
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        impatient
    }

    /// A step's outcome written inside the request's count waits for a
    /// connection until the capture's deadline, not until the pool's own
    /// acquire timeout: with the pool's one connection held past two of
    /// those timeouts and let go before the deadline, the outcome is
    /// written before `finish` returns, inside the count.
    #[sqlx::test]
    async fn a_pool_timeout_before_the_deadline_is_waited_out_inside_the_count(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let (id, seq) = running_step(&store, 310).await;
        let impatient = impatient(&pool).await;
        let held = impatient.acquire().await.unwrap();
        let writer = StepWriter::start(Duration::from_mins(1), PLACES);
        let completion = StepCompletion::new(
            InvestigationStore::new(impatient),
            ResultsClock::fixed(at()),
            RULE,
            id.clone(),
            seq,
            writer.clone(),
            place(&writer),
        );

        let deadline = Deadline::after(Duration::from_millis(1_500));
        let finishing = tokio::spawn(completion.finish(deadline, Lease::until(None, deadline)));
        tokio::time::sleep(Duration::from_millis(700)).await;
        drop(held);
        finishing.await.unwrap();
        assert!(
            !deadline.expired(),
            "the write was not done by its deadline"
        );
        assert_eq!(
            outcome(&store, &id, seq).await,
            Some(StepOutcomeKind::Aborted),
            "the outcome was not written inside the count"
        );
    }

    /// With the pool's one connection held past the capture's deadline,
    /// the count is kept until the deadline, not given back at the pool's
    /// first acquire timeout, and the outcome then lands through the
    /// writer once the connection is free.
    #[sqlx::test]
    async fn a_pool_timeout_at_the_deadline_hands_the_outcome_to_the_writer(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let (id, seq) = running_step(&store, 310).await;
        let impatient = impatient(&pool).await;
        let held = impatient.acquire().await.unwrap();
        let writer = StepWriter::start(Duration::from_secs(1), PLACES);
        let completion = StepCompletion::new(
            InvestigationStore::new(impatient),
            ResultsClock::fixed(at()),
            RULE,
            id.clone(),
            seq,
            writer.clone(),
            place(&writer),
        );

        let deadline = Deadline::after(Duration::from_secs(1));
        completion
            .finish(deadline, Lease::until(None, deadline))
            .await;
        assert!(
            deadline.expired(),
            "the count came back before the deadline"
        );
        assert!(
            tokio::time::Instant::now() < deadline.instant() + Duration::from_secs(1),
            "the count came back long after the deadline"
        );
        assert_eq!(
            outcome(&store, &id, seq).await,
            None,
            "written past the deadline"
        );
        drop(held);
        until_written(
            &store,
            &id,
            seq,
            tokio::time::Instant::now() + Duration::from_secs(30),
            "the writer never wrote the outcome handed to it",
        )
        .await;
    }

    /// An attempt whose answer never comes ends at the writer's bound like
    /// one that waits on a lock: the outcome queued behind it is written
    /// within the bound and a second, and the stalled one, tried again on
    /// a new session, after it. The stalled attempt's `BEGIN` answer is
    /// held back by a proxy until the end of the test.
    #[sqlx::test]
    async fn a_stalled_attempt_does_not_stop_the_writer(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let (stalled, stalled_seq) = running_step(&store, 310).await;
        let (healthy, healthy_seq) = running_step(&store, 311).await;
        let proxy = crate::answer_hold::Proxy::start(&pool.connect_options()).await;
        #[allow(clippy::disallowed_methods)] // the stalled write's sessions, through the proxy
        let through = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(proxy.options(&pool.connect_options()))
            .await
            .unwrap();
        let budget = Duration::from_secs(1);
        let writer = StepWriter::start(budget, PLACES);
        let completion = |store: &InvestigationStore, id: &InvestigationId, seq| {
            StepCompletion::new(
                store.clone(),
                ResultsClock::fixed(at()),
                RULE,
                id.clone(),
                seq,
                writer.clone(),
                place(&writer),
            )
        };

        proxy.hold_next(b"BEGIN");
        drop(completion(
            &InvestigationStore::new(through),
            &stalled,
            stalled_seq,
        ));
        proxy.holding().await;
        let queued = tokio::time::Instant::now();
        drop(completion(&store, &healthy, healthy_seq));
        until_written(
            &store,
            &healthy,
            healthy_seq,
            queued + budget + Duration::from_secs(1),
            "the outcome behind a stalled attempt waited past the writer's bound",
        )
        .await;
        until_written(
            &store,
            &stalled,
            stalled_seq,
            tokio::time::Instant::now() + Duration::from_secs(30),
            "the stalled outcome was never written on a new session",
        )
        .await;
        proxy.release();
    }

    /// An attempt Postgres refuses before the writer's bound, whose
    /// `ROLLBACK` answer never comes, ends at the bound too: the outcome
    /// queued behind it is written within the bound and a second, the
    /// stalled attempt's session is closed while its answer is held, and
    /// the stalled outcome lands on a new session once its investigation's
    /// lock is free. Another session holds that lock, and the stalled
    /// writes' sessions wait for it no longer than a 50 ms `lock_timeout`.
    #[sqlx::test]
    async fn a_stalled_rollback_does_not_stop_the_writer(pool: PgPool) {
        let store = InvestigationStore::new(pool.clone());
        let (stalled, stalled_seq) = running_step(&store, 310).await;
        let (healthy, healthy_seq) = running_step(&store, 311).await;
        let proxy = crate::answer_hold::Proxy::start(&pool.connect_options()).await;
        #[allow(clippy::disallowed_methods)] // the stalled write's sessions, through the proxy
        let through = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                proxy
                    .options(&pool.connect_options())
                    .options([("lock_timeout", "50ms")]),
            )
            .await
            .unwrap();
        let mut held = sqlx::PgConnection::connect_with(&pool.connect_options())
            .await
            .unwrap();
        sqlx::query("BEGIN").execute(&mut held).await.unwrap();
        sqlx::query("SELECT 1 FROM investigations WHERE id = $1 FOR UPDATE")
            .bind(stalled.as_str())
            .execute(&mut held)
            .await
            .unwrap();
        let budget = Duration::from_secs(1);
        let writer = StepWriter::start(budget, PLACES);
        let completion = |store: &InvestigationStore, id: &InvestigationId, seq| {
            StepCompletion::new(
                store.clone(),
                ResultsClock::fixed(at()),
                RULE,
                id.clone(),
                seq,
                writer.clone(),
                place(&writer),
            )
        };

        proxy.hold_next(b"ROLLBACK");
        drop(completion(
            &InvestigationStore::new(through),
            &stalled,
            stalled_seq,
        ));
        proxy.holding().await;
        let queued = tokio::time::Instant::now();
        drop(completion(&store, &healthy, healthy_seq));
        until_written(
            &store,
            &healthy,
            healthy_seq,
            queued + budget + Duration::from_secs(1),
            "the outcome behind a stalled rollback waited past the writer's bound",
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), proxy.closed())
            .await
            .expect("the stalled attempt's session was not closed while its answer was held");
        assert_eq!(
            outcome(&store, &stalled, stalled_seq).await,
            None,
            "the stalled outcome was written behind a held lock"
        );
        sqlx::query("COMMIT").execute(&mut held).await.unwrap();
        until_written(
            &store,
            &stalled,
            stalled_seq,
            tokio::time::Instant::now() + Duration::from_secs(30),
            "the stalled outcome was never written on a new session",
        )
        .await;
        proxy.release();
    }

    /// The writer's places bound the outcomes waiting for it, however long
    /// a `COMMIT` it sent waits for its answer. With three places, a first
    /// step's abandoned outcome is written through a proxy that holds back
    /// its `COMMIT`'s answer, so the writer awaits it, and two more steps
    /// are admitted and abandoned behind it. A fourth admission finds no
    /// place: it ends at its deadline, within a second of it, so its step
    /// is never appended. Postgres committed the first outcome, the writer
    /// has written neither queued one, and no place is left. Once the
    /// answer comes, every admitted outcome lands and all three places are
    /// free again.
    #[sqlx::test]
    async fn a_held_commit_answer_holds_no_more_outcomes_than_the_writers_places(pool: PgPool) {
        const BOUND: u32 = 3;
        let store = InvestigationStore::new(pool.clone());
        let proxy = crate::answer_hold::Proxy::start(&pool.connect_options()).await;
        #[allow(clippy::disallowed_methods)] // the held write's session, through the proxy
        let through = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(proxy.options(&pool.connect_options()))
            .await
            .unwrap();
        let through = InvestigationStore::new(through);
        let writer = StepWriter::start(Duration::from_secs(1), BOUND as usize);
        let admitted = |store: &InvestigationStore, id: &InvestigationId, seq, place| {
            StepCompletion::new(
                store.clone(),
                ResultsClock::fixed(at()),
                RULE,
                id.clone(),
                seq,
                writer.clone(),
                place,
            )
        };

        proxy.hold_next(b"COMMIT");
        let mut steps = Vec::new();
        for n in 0..BOUND {
            let place = writer
                .admit(Deadline::after(Duration::from_secs(1)))
                .await
                .unwrap_or_else(|_| panic!("step {n} found no place"));
            let (id, seq) = running_step(&store, 310 + n).await;
            let writes_through = if n == 0 { &through } else { &store };
            drop(admitted(writes_through, &id, seq, place));
            if n == 0 {
                proxy.holding().await;
            }
            steps.push((id, seq));
        }

        let deadline = Deadline::after(Duration::from_millis(500));
        assert!(
            writer.admit(deadline).await.is_err(),
            "a step was admitted past the writer's places"
        );
        assert!(
            tokio::time::Instant::now() < deadline.instant() + Duration::from_secs(1),
            "the refused admission ended long after its deadline"
        );
        let (first, first_seq) = &steps[0];
        assert_eq!(
            outcome(&store, first, *first_seq).await,
            Some(StepOutcomeKind::Aborted),
            "Postgres did not commit the held write"
        );
        for (id, seq) in &steps[1..] {
            assert_eq!(
                outcome(&store, id, *seq).await,
                None,
                "an outcome queued behind the held answer was written"
            );
        }
        assert_eq!(writer.places.available_permits(), 0, "a place is left");

        proxy.release();
        for (id, seq) in &steps {
            until_written(
                &store,
                id,
                *seq,
                tokio::time::Instant::now() + Duration::from_secs(30),
                "an admitted outcome never landed once the answer came",
            )
            .await;
        }
        let mut freed = Vec::new();
        for n in 0..BOUND {
            freed.push(
                writer
                    .admit(Deadline::after(Duration::from_secs(5)))
                    .await
                    .unwrap_or_else(|_| panic!("place {n} never came back")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use trawl_api::ErrorCode;

    use super::*;

    #[test]
    fn each_ending_maps_to_its_outcome_and_wire_code() {
        let timeout = outcome_of(&ServerError::Timeout, Some(1_000));
        assert_eq!(timeout.outcome, StepOutcomeKind::Timeout);
        assert_eq!(timeout.code, Some(ErrorCode::Timeout));
        assert_eq!(timeout.duration_ms, Some(1_000));
        let busy = ServerError::CaptureBusy {
            retry_after_secs: 1,
        };
        assert_eq!(outcome_of(&busy, None).outcome, StepOutcomeKind::Refused);
        assert_eq!(outcome_of(&busy, None).code, Some(ErrorCode::CaptureBusy));
        let budget = ServerError::EvidenceBudgetExceeded { used: 1, limit: 2 };
        assert_eq!(outcome_of(&budget, None).outcome, StepOutcomeKind::Refused);
        assert_eq!(
            outcome_of(&budget, None).code,
            Some(ErrorCode::EvidenceBudgetExceeded)
        );
        let failed = outcome_of(&ServerError::Internal("x".into()), Some(3));
        assert_eq!(failed.outcome, StepOutcomeKind::Failed);
        assert_eq!(failed.code, Some(ErrorCode::InternalError));
    }

    #[test]
    fn a_purpose_holds_its_bound_and_no_nul() {
        let attach = |purpose: &str| Attach {
            id: InvestigationId::parse(&"0".repeat(32)).unwrap(),
            seen_seq: None,
            purpose: Some(purpose.to_owned()),
        };
        assert!(check_attach(&attach(&"p".repeat(PURPOSE_MAX_BYTES))).is_ok());
        let Err(ServerError::BadRequest(message)) =
            check_attach(&attach(&"p".repeat(PURPOSE_MAX_BYTES + 1)))
        else {
            panic!("an oversize purpose is a 400");
        };
        assert_eq!(
            message,
            "purpose is 1025 bytes, above the limit of 1024 bytes"
        );
        assert!(matches!(
            check_attach(&attach("a\0b")),
            Err(ServerError::BadRequest(_))
        ));
    }
}
