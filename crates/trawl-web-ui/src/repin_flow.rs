// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin TRIGGER's pure logic (ADR-0011 slice C2): the target
//! ladder, the 409 double-shape decode, and the poll decision.
//!
//! Everything here is data-in / data-out so the parts that are easy to
//! get subtly wrong are table-testable natively, off the browser:
//!
//! - [`classify_conflict`] — `POST /schema/repin` answers 409 with TWO
//!   different bodies (the refusal plan, and the error envelope for a
//!   slot held elsewhere). They are told apart by DECODING, never by the
//!   status code, and never by sniffing error text.
//! - [`poll_decide`] — the status route is INSTALL-WIDE (synthesis R4:
//!   no `?id=`), so every read has to be matched against the job id the
//!   case file holds. A different id, or no job at all, means the
//!   one-running slot moved on and our job's record is gone from the
//!   route: stop, keep what we last saw, say so.
//!
//! The ladder rungs are [`trawl_core::schema::CanonicalType`] values
//! rather than mirrored strings — the crate is already an unconditional
//! dependency, and the `DuckDB` spellings on the wire are exactly what
//! [`CanonicalType::as_duckdb`] writes.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::{ErrorResponse, RepinJobResponse, RepinResponse};
use trawl_core::schema::CanonicalType;

use crate::repin_hint::repin_is_terminal;

/// How often the case file re-reads `/schema/repin/status` while it
/// tracks a running job (synthesis R10: one immediate read, then this).
pub const REPIN_POLL_MS: u32 = 3_000;

/// Consecutive failed status reads tolerated before polling stops and
/// the case file says the status is unavailable. Bounded on purpose: a
/// store outage must cost one inline warning, not a toast per tick.
pub const MAX_POLL_ERRORS: u32 = 3;

/// The candidate ladder a repin target is chosen from, in the engine's
/// own order (`trawl-server/src/repin/engine.rs` rejects anything else
/// with exactly this list).
pub const REPIN_LADDER: [CanonicalType; 5] = [
    CanonicalType::BigInt,
    CanonicalType::Double,
    CanonicalType::Timestamp,
    CanonicalType::Boolean,
    CanonicalType::Varchar,
];

/// The rungs the modal offers for a field pinned `current`: every rung
/// EXCEPT the one it already has (synthesis R9 — `to == current` is the
/// resurrection-only pass, which stays a CLI decision because its only
/// effect is re-extracting shelved values under the same pin).
///
/// An unrecognised `current` (a catalog spelling this build doesn't
/// know) offers the whole ladder rather than nothing.
#[must_use]
pub fn repin_targets(current: &str) -> Vec<CanonicalType> {
    let current = current.to_ascii_uppercase();
    REPIN_LADDER
        .into_iter()
        .filter(|c| c.as_duckdb() != current)
        .collect()
}

/// The rung the target selector starts on: the analyzer's suggestion
/// when it is a real rung and not the current pin, else VARCHAR — the
/// honest fallback that keeps every value — else the first rung on
/// offer.
#[must_use]
pub fn default_target(current: &str, suggested: &str) -> CanonicalType {
    let offered = repin_targets(current);
    let suggested = suggested.to_ascii_uppercase();
    offered
        .iter()
        .find(|c| c.as_duckdb() == suggested)
        .or_else(|| offered.iter().find(|c| **c == CanonicalType::Varchar))
        .or_else(|| offered.first())
        .copied()
        .unwrap_or(CanonicalType::Varchar)
}

/// What a 409 from `POST /api/v1/schema/repin` turned out to be.
#[derive(Debug, Clone)]
pub enum ConflictBody {
    /// The body decoded as a repin job: the scan projected values the
    /// new pin cannot keep and no force flag was passed. The body IS the
    /// plan the refusal is based on — boxed because a job row dwarfs the
    /// other variant.
    Plan(Box<RepinJobResponse>),
    /// The body decoded as the error envelope: the one-running slot is
    /// held by some other job, install-wide.
    Busy(String),
}

/// What to say when a 409 carries neither shape. The one-running slot is
/// the only other thing a 409 means here, so this errs toward the truth
/// an operator can act on rather than inventing a plan.
pub const REPIN_BUSY_FALLBACK: &str = "another repin already holds the one-running slot";

/// Decide which of the two 409 bodies arrived.
///
/// Order matters only for clarity: the two shapes are disjoint by
/// REQUIRED field (`job` vs `error`), so neither can decode as the
/// other — which is why this is a decode and not a heuristic.
#[must_use]
pub fn classify_conflict(body: &str) -> ConflictBody {
    if let Ok(plan) = serde_json::from_str::<RepinResponse>(body) {
        return ConflictBody::Plan(Box::new(plan.job));
    }
    serde_json::from_str::<ErrorResponse>(body).map_or_else(
        |_| ConflictBody::Busy(REPIN_BUSY_FALLBACK.to_string()),
        |env| ConflictBody::Busy(env.error.message),
    )
}

/// One status read, reduced to what the decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollTick {
    /// The route answered with a job.
    Job {
        /// The job's id — matched against the tracked one.
        id: i64,
        /// The job's status string, verbatim from the wire.
        status: String,
    },
    /// The route answered `job: null` — no repin has ever run, or the
    /// row this install had is gone.
    NoJob,
    /// The read failed (network, or the store behind the route).
    Error,
}

/// What the case file does with one status read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollAction {
    /// Adopt the row as current state and keep polling.
    Track,
    /// Adopt the row as final state and stop polling.
    Settle,
    /// The slot moved on (or is empty). Stop polling, keep the last
    /// state we saw, and say the status is no longer available.
    Lost,
    /// Transient read failure inside the retry budget: keep polling,
    /// warn inline once.
    Retry,
    /// The retry budget is spent. Stop polling and say so.
    GiveUp,
}

/// Decide what one status read means for the job the case file tracks.
///
/// `consecutive_errors` COUNTS the tick being decided, so the first
/// failure arrives as 1.
#[must_use]
pub fn poll_decide(tracked_id: i64, consecutive_errors: u32, tick: &PollTick) -> PollAction {
    match tick {
        PollTick::Job { id, status } if *id == tracked_id => {
            if repin_is_terminal(status) {
                PollAction::Settle
            } else {
                PollAction::Track
            }
        }
        // The route is install-wide: a different id is a DIFFERENT job,
        // never a later view of ours, and `job: null` cannot be ours
        // either. Both mean our record is unreachable from here.
        PollTick::Job { .. } | PollTick::NoJob => PollAction::Lost,
        PollTick::Error => {
            if consecutive_errors >= MAX_POLL_ERRORS {
                PollAction::GiveUp
            } else {
                PollAction::Retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConflictBody, MAX_POLL_ERRORS, PollAction, PollTick, REPIN_BUSY_FALLBACK,
        classify_conflict, default_target, poll_decide, repin_targets,
    };
    use trawl_core::schema::CanonicalType;

    fn job_body(id: i64, status: &str) -> String {
        format!(
            r#"{{"job":{{"id":{id},"field":"duration","from_type":"BIGINT",
             "to_type":"VARCHAR","dry_run":false,"force":false,"status":"{status}",
             "requested_by":"ops","started_at":"2026-08-14T00:00:00Z","finished_at":null,
             "error":null,"files_total":12,"rows_carrying":900,"projected_nulls":7,
             "resurrectable":5,"affected_bytes":4096,"files_done":0,"rows_rewritten":0,
             "rows_nulled":0,"rows_resurrected":0}}}}"#
        )
    }

    #[test]
    fn the_current_pin_is_never_offered_as_a_target() {
        let targets = repin_targets("VARCHAR");
        assert_eq!(targets.len(), 4);
        assert!(!targets.contains(&CanonicalType::Varchar));
        // The catalog spells pins uppercase, but a lowercase spelling
        // must not sneak the current pin back onto the strip.
        assert!(!repin_targets("varchar").contains(&CanonicalType::Varchar));
        // An unknown spelling offers the whole ladder rather than none.
        assert_eq!(repin_targets("HUGEINT").len(), 5);
    }

    #[test]
    fn the_selector_starts_on_the_suggestion_when_it_is_one() {
        assert_eq!(
            default_target("BIGINT", "VARCHAR"),
            CanonicalType::Varchar,
            "the analyzer's suggestion wins"
        );
        assert_eq!(
            default_target("VARCHAR", "TIMESTAMP"),
            CanonicalType::Timestamp
        );
        // Suggestion == current pin: not on offer, so fall back to the
        // rung that keeps every value.
        assert_eq!(default_target("BIGINT", "BIGINT"), CanonicalType::Varchar);
        // …unless VARCHAR is the current pin, in which case the first
        // rung on offer is the honest default.
        assert_eq!(default_target("VARCHAR", "VARCHAR"), CanonicalType::BigInt);
        // A suggestion this build doesn't know is not invented into one.
        assert_eq!(default_target("BIGINT", "HUGEINT"), CanonicalType::Varchar);
    }

    #[test]
    fn a_conflict_carrying_a_plan_is_the_refusal() {
        match classify_conflict(&job_body(7, "refused_needs_force")) {
            ConflictBody::Plan(job) => {
                assert_eq!(job.id, 7);
                assert_eq!(job.status, "refused_needs_force");
                assert_eq!(job.projected_nulls, 7);
            }
            ConflictBody::Busy(m) => panic!("decoded as busy: {m}"),
        }
    }

    #[test]
    fn a_conflict_carrying_the_envelope_is_the_slot() {
        let body = r#"{"error":{"code":"bad_request",
            "message":"a repin job is already running (one at a time, install-wide)"}}"#;
        match classify_conflict(body) {
            ConflictBody::Busy(msg) => {
                assert_eq!(
                    msg,
                    "a repin job is already running (one at a time, install-wide)"
                );
            }
            ConflictBody::Plan(_) => panic!("an error envelope decoded as a plan"),
        }
    }

    #[test]
    fn a_conflict_carrying_neither_shape_is_not_invented_into_a_plan() {
        for body in [
            "",
            "not json",
            "{}",
            r#"{"job":{"id":1}}"#,
            "<html>502</html>",
        ] {
            match classify_conflict(body) {
                ConflictBody::Busy(msg) => assert_eq!(msg, REPIN_BUSY_FALLBACK, "{body:?}"),
                ConflictBody::Plan(_) => panic!("{body:?} decoded as a plan"),
            }
        }
    }

    #[test]
    fn our_job_running_keeps_polling_and_terminal_settles() {
        let running = PollTick::Job {
            id: 42,
            status: "running".to_string(),
        };
        assert_eq!(poll_decide(42, 0, &running), PollAction::Track);
        for status in ["succeeded", "failed", "blocked", "refused_needs_force"] {
            let tick = PollTick::Job {
                id: 42,
                status: status.to_string(),
            };
            assert_eq!(poll_decide(42, 0, &tick), PollAction::Settle, "{status}");
        }
        // An unknown future status is terminal (synthesis R5), so
        // polling stops rather than spinning forever.
        let unknown = PollTick::Job {
            id: 42,
            status: "quiesced".to_string(),
        };
        assert_eq!(poll_decide(42, 0, &unknown), PollAction::Settle);
    }

    #[test]
    fn another_id_or_no_job_loses_the_trail() {
        let other = PollTick::Job {
            id: 43,
            status: "running".to_string(),
        };
        assert_eq!(poll_decide(42, 0, &other), PollAction::Lost);
        // Even a TERMINAL row for another id: the install-wide route
        // never speaks about our job again once the slot moves on.
        let other_done = PollTick::Job {
            id: 43,
            status: "succeeded".to_string(),
        };
        assert_eq!(poll_decide(42, 0, &other_done), PollAction::Lost);
        assert_eq!(poll_decide(42, 0, &PollTick::NoJob), PollAction::Lost);
    }

    #[test]
    fn read_failures_retry_to_a_bound_then_give_up() {
        for n in 1..MAX_POLL_ERRORS {
            assert_eq!(
                poll_decide(42, n, &PollTick::Error),
                PollAction::Retry,
                "{n}"
            );
        }
        assert_eq!(
            poll_decide(42, MAX_POLL_ERRORS, &PollTick::Error),
            PollAction::GiveUp
        );
        assert_eq!(
            poll_decide(42, MAX_POLL_ERRORS + 5, &PollTick::Error),
            PollAction::GiveUp
        );
    }
}
