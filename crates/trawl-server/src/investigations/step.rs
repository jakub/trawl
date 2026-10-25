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
//! outlive the request:
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
//! handle drops, the outcome is written in a task of its own on the
//! runtime the completion was made on, so a drop on the pool's blocking
//! thread still writes it. A completion dropped with nothing recorded
//! writes `aborted`: the client went away before the commit task took
//! over, and the work stopped. The store writes an outcome only on a step
//! that has none, so the fill never runs twice.
//!
//! A runtime that shuts down drops the task with the write still pending;
//! boot records `interrupted` on every step left without an outcome
//! ([`crate::store::InvestigationStore::interrupt_running_steps`]).
//!
//! Each written outcome logs one `investigation_step` line. The capture's
//! ADR-0046 timing event gains no field.

use std::sync::Arc;
use std::time::Instant;

use chrono::TimeDelta;
use fleet_auth::VerifiedKey;
use parking_lot::Mutex;
use trawl_api::investigations::{
    Attach, EntryPayload, InvestigationId, PURPOSE_MAX_BYTES, StepBounds, StepOutcome,
    StepOutcomeKind,
};

use crate::config::InvestigationConfig;
use crate::error::ServerError;
use crate::results::ResultsClock;
use crate::state::AppState;
use crate::store::{Appended, InvestigationStore, RetentionRule, StoreActor};

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
/// # Errors
/// 404 for an unknown investigation, 409 `investigation_closed`, or a
/// store failure.
pub(crate) async fn begin(
    state: &AppState,
    verified: &VerifiedKey,
    attach: &Attach,
    executed: &str,
    bounds: Option<StepBounds>,
    request_id: &str,
    query_id: u64,
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
    let results = &state.results;
    let retention = retention(&results.investigation);
    let store = state.storage.investigations.clone();
    let Appended { entry, pruned } = store
        .begin_step(&attach.id, &actor, &payload, results.clock.now(), retention)
        .await?;
    crate::investigations::service::unlink_evidence(&results.root, pruned).await;
    Ok(StepCompletion::new(
        store,
        results.clock.clone(),
        retention,
        attach.id.clone(),
        entry.seq,
    ))
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
    /// The runtime the write is spawned on, whatever thread drops the last
    /// handle.
    runtime: tokio::runtime::Handle,
    fill: Mutex<Fill>,
}

impl StepCompletion {
    /// The completion of step `seq` of `investigation`, writing through
    /// `store` at `clock`'s time under `retention`. Made on the runtime that
    /// will write it.
    ///
    /// # Panics
    /// Outside a Tokio runtime.
    #[must_use]
    pub(crate) fn new(
        store: InvestigationStore,
        clock: ResultsClock,
        retention: RetentionRule,
        investigation: InvestigationId,
        seq: u64,
    ) -> Self {
        Self(Arc::new(Inner {
            store,
            clock,
            retention,
            investigation,
            seq,
            began: Instant::now(),
            runtime: tokio::runtime::Handle::current(),
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
}

impl Drop for Inner {
    fn drop(&mut self) {
        let outcome = match std::mem::replace(self.fill.get_mut(), Fill::Written) {
            Fill::Written => return,
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
        let store = self.store.clone();
        let now = self.clock.now();
        let retention = self.retention;
        let investigation = self.investigation.clone();
        let seq = self.seq;
        self.runtime.spawn(async move {
            match store
                .complete_step(&investigation, seq, &outcome, now, retention)
                .await
            {
                Ok(written) => tracing::info!(
                    event_type = "investigation_step",
                    investigation_id = %investigation,
                    step_seq = seq,
                    outcome = outcome_name(outcome.outcome),
                    written,
                    "an investigation step's work ended"
                ),
                Err(error) => tracing::warn!(
                    event_type = "investigation_step",
                    investigation_id = %investigation,
                    step_seq = seq,
                    outcome = outcome_name(outcome.outcome),
                    error_class = ServerError::from(error).error_class(),
                    "could not write an investigation step's outcome; boot records it as \
                     interrupted"
                ),
            }
        });
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
