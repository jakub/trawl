// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::cell::RefCell;
use std::collections::HashSet;

use trawl_api::{ErrorResponse, RepinJobResponse, RepinResponse};
use trawl_core::sanitize::sanitize_display_text;
use trawl_core::schema::CanonicalType;

use crate::repin_hint::{repin_is_running, repin_is_terminal};

pub const REPIN_POLL_MS: u32 = 3_000;

pub const MAX_POLL_ERRORS: u32 = 3;

pub const REPIN_LADDER: [CanonicalType; 5] = [
    CanonicalType::BigInt,
    CanonicalType::Double,
    CanonicalType::Timestamp,
    CanonicalType::Boolean,
    CanonicalType::Varchar,
];

#[must_use]
pub fn repin_targets(current: &str) -> Vec<CanonicalType> {
    let current = current.to_ascii_uppercase();
    REPIN_LADDER
        .into_iter()
        .filter(|c| c.as_catalog() != current)
        .collect()
}

/// The plan is a scan of ONE target, so it means nothing for any other
/// rung.
#[must_use]
pub fn refusal_is_reusable(current: &str, to_type: &str) -> bool {
    let to = to_type.to_ascii_uppercase();
    repin_targets(current).iter().any(|c| c.as_catalog() == to)
}

#[must_use]
pub fn default_target(current: &str, suggested: &str) -> CanonicalType {
    let offered = repin_targets(current);
    let suggested = suggested.to_ascii_uppercase();
    offered
        .iter()
        .find(|c| c.as_catalog() == suggested)
        .or_else(|| offered.iter().find(|c| **c == CanonicalType::Varchar))
        .or_else(|| offered.first())
        .copied()
        .unwrap_or(CanonicalType::Varchar)
}

#[derive(Debug, Clone)]
pub enum RepinOutcome {
    DryRun(RepinJobResponse),
    /// 202 — the job is claimed and was still running when the server
    /// stopped waiting for it, dry runs included.
    Started(RepinJobResponse),
    Refused(RepinJobResponse),
    Busy(String),
}

#[derive(Debug, Clone)]
pub enum ConflictBody {
    Plan(Box<RepinJobResponse>),
    Busy(String),
}

/// The one-running slot is the only other thing a 409 means here.
pub const REPIN_BUSY_FALLBACK: &str = "another repin already holds the one-running slot";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedJob {
    pub id: i64,
    pub field: String,
    pub to_type: String,
    pub dry_run: bool,
    pub force: bool,
    pub status: String,
}

impl From<&RepinJobResponse> for ProbedJob {
    fn from(job: &RepinJobResponse) -> Self {
        Self {
            id: job.id,
            field: job.field.clone(),
            to_type: job.to_type.clone(),
            dry_run: job.dry_run,
            force: job.force,
            status: job.status.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusProbe {
    Job(ProbedJob),
    NoJob,
    /// The read failed. It said nothing at all, so every decision has to
    /// treat it as an absence of information, never as an absence of a job.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollAction {
    Track,
    Settle,
    Lost,
    Retry,
    GiveUp,
}

/// `consecutive_errors` counts the tick being decided, so the first
/// failure arrives as 1.
#[must_use]
pub fn poll_decide(tracked_id: i64, consecutive_errors: u32, probe: &StatusProbe) -> PollAction {
    match probe {
        StatusProbe::Job(job) if job.id == tracked_id => {
            if repin_is_terminal(&job.status) {
                PollAction::Settle
            } else {
                PollAction::Track
            }
        }
        // A different id is a different job, never a later view of ours,
        // and no job cannot be ours either. Both mean our record is
        // unreachable from here.
        StatusProbe::Job(_) | StatusProbe::NoJob => PollAction::Lost,
        StatusProbe::Failed => {
            if consecutive_errors >= MAX_POLL_ERRORS {
                PollAction::GiveUp
            } else {
                PollAction::Retry
            }
        }
    }
}

/// The server claims the one-running `repin_jobs` row before it answers,
/// so only a status decided before that claim can be read as "nothing
/// started". A 5xx can land after the claim, and a transport or decode
/// failure carries no status at all.
#[must_use]
pub fn is_pre_claim_failure(status: Option<u16>) -> bool {
    matches!(status, Some(400..=499))
}

#[derive(Debug, Clone)]
pub struct LostRun {
    pub field: String,
    pub to: String,
    pub force: bool,
    /// The newest job id known to exist before the request went out —
    /// the plan's own row, since every dry run writes one.
    pub bound: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    Adopt,
    /// Nothing was proven. The run may be rewriting the corpus right
    /// now, so this can never license a second one.
    Indeterminate(Unproven),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unproven {
    ProbeFailed,
    Unmatched,
}

/// The status route is install-wide and answers about the running job if
/// there is one, else the newest — so a job matching field, target and
/// force could equally be the previous repin of the same field. Only an id
/// strictly newer than a row that existed before the request went out can
/// have been created by that request.
/// The negative is deliberately never proven.
#[must_use]
pub fn recovery_verdict(run: &LostRun, probe: &StatusProbe) -> Recovery {
    let StatusProbe::Job(job) = probe else {
        return Recovery::Indeterminate(match probe {
            StatusProbe::Failed => Unproven::ProbeFailed,
            StatusProbe::Job(_) | StatusProbe::NoJob => Unproven::Unmatched,
        });
    };
    let ours = !job.dry_run
        && job.force == run.force
        && job.field.eq_ignore_ascii_case(&run.field)
        && job.to_type.eq_ignore_ascii_case(&run.to)
        && run.bound.is_some_and(|bound| job.id > bound);
    if ours {
        Recovery::Adopt
    } else {
        Recovery::Indeterminate(Unproven::Unmatched)
    }
}

#[must_use]
pub fn indeterminate_text(lost: &str, why: Unproven) -> String {
    match why {
        Unproven::ProbeFailed => format!(
            "{lost} \u{2014} and the repin status route did not answer either, so whether the \
             repin started is unknown."
        ),
        Unproven::Unmatched => format!(
            "{lost} \u{2014} and the status route reports no job this dialog can prove is the run \
             it asked for, so whether the repin started is unknown."
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotCheck {
    Free,
    /// The field name verbatim, for display through `sanitize_display_text`.
    Held(String),
    Unknown,
}

/// A dry run holds the slot exactly like a rewrite does (its scan is a
/// full-corpus pass).
#[must_use]
pub fn slot_check(probe: &StatusProbe) -> SlotCheck {
    match probe {
        StatusProbe::Job(job) if repin_is_running(&job.status) => {
            SlotCheck::Held(job.field.clone())
        }
        StatusProbe::Job(_) | StatusProbe::NoJob => SlotCheck::Free,
        StatusProbe::Failed => SlotCheck::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundCeilings {
    pub max_nulled: u64,
    pub max_ambiguous: u64,
}

/// The arithmetic lives on the server, and a copy of it here would be a
/// second one to keep in step. This is the SPA's half of the CLI's
/// `schema::accepted_ceilings`. Both read the same two wire fields and
/// both refuse a half-filled pair.
#[must_use]
pub fn accepted_ceilings(job: &RepinJobResponse) -> Option<BoundCeilings> {
    Some(BoundCeilings {
        max_nulled: job.accepted_max_nulled_rows?,
        max_ambiguous: job.accepted_max_ambiguous_rows?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceStep {
    NeedsPreview,
    Bound(BoundCeilings),
    /// A forced scan came back with no resolved pair. Rescanning would only
    /// produce the same answer, so the forced run is offered.
    Unbound,
}

/// `previewed` is whether a FORCED scan has answered in this dialog for
/// the plan on screen, and it alone opens the acceptance. Reading the
/// ceilings off the job row instead would re-open it for a refusal that
/// already busted them.
#[must_use]
pub fn force_step(job: &RepinJobResponse, previewed: bool) -> ForceStep {
    if !previewed {
        return ForceStep::NeedsPreview;
    }
    match accepted_ceilings(job) {
        Some(bound) => ForceStep::Bound(bound),
        None => ForceStep::Unbound,
    }
}

pub const PLAN_LOST: &str = "The plan's scan outlasted the server's answer, and the repin status \
                             no longer reports its job, so its report can't be read. Get the plan \
                             again.";

pub const PLAN_UNREADABLE: &str = "The plan's scan outlasted the server's answer, and the repin \
                                   status could not be read, so its report is unknown. Get the \
                                   plan again.";

#[derive(Debug, Clone)]
pub enum FollowedPlan {
    Plan(Box<RepinJobResponse>),
    Ended(String),
}

#[must_use]
pub fn followed_plan(job: RepinJobResponse) -> FollowedPlan {
    if job.status == "succeeded" {
        return FollowedPlan::Plan(Box::new(job));
    }
    let status = sanitize_display_text(&job.status);
    FollowedPlan::Ended(job.error.as_deref().map_or_else(
        || format!("The plan's scan ended without a report ({status})."),
        |error| {
            format!(
                "The plan's scan ended without a report ({status}): {}",
                sanitize_display_text(error)
            )
        },
    ))
}

thread_local! {
    /// Global on purpose. The field case file is a component rebuilt whenever
    /// the `?field=` it mounts from changes, so per-instance bookkeeping would
    /// toast one job's completion twice.
    static TOASTED_JOBS: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
}

pub fn claim_toast(job_id: i64) -> bool {
    TOASTED_JOBS.with_borrow_mut(|seen| seen.insert(job_id))
}

#[cfg(test)]
mod tests {
    use super::{
        BoundCeilings, ConflictBody, FollowedPlan, ForceStep, LostRun, MAX_POLL_ERRORS, PollAction,
        ProbedJob, REPIN_BUSY_FALLBACK, Recovery, RepinJobResponse, SlotCheck, StatusProbe,
        Unproven, accepted_ceilings, claim_toast, classify_conflict, default_target, followed_plan,
        force_step, indeterminate_text, is_pre_claim_failure, poll_decide, recovery_verdict,
        refusal_is_reusable, repin_targets, slot_check,
    };
    use trawl_core::schema::CanonicalType;

    fn probed(id: i64, status: &str) -> ProbedJob {
        ProbedJob {
            id,
            field: "duration".to_string(),
            to_type: "VARCHAR".to_string(),
            dry_run: false,
            force: false,
            status: status.to_string(),
        }
    }

    fn lost_run() -> LostRun {
        LostRun {
            field: "duration".to_string(),
            to: "VARCHAR".to_string(),
            force: false,
            bound: Some(41),
        }
    }

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

    fn job_with(extra: &str) -> RepinJobResponse {
        let body = job_body(1, "refused_needs_force").replace(
            r#""rows_resurrected":0}}"#,
            &format!(r#""rows_resurrected":0{extra}}}}}"#),
        );
        match classify_conflict(&body) {
            ConflictBody::Plan(job) => *job,
            ConflictBody::Busy(m) => panic!("test fixture failed to decode: {m}"),
        }
    }

    #[test]
    fn a_followed_dry_run_is_a_plan_only_when_it_succeeded() {
        let mut planned =
            job_with(r#","accepted_max_nulled_rows":18,"accepted_max_ambiguous_rows":10"#);
        planned.dry_run = true;
        planned.status = "succeeded".to_string();
        match followed_plan(planned) {
            FollowedPlan::Plan(job) => {
                assert_eq!(job.status, "succeeded");
                assert_eq!(
                    accepted_ceilings(&job),
                    Some(BoundCeilings {
                        max_nulled: 18,
                        max_ambiguous: 10,
                    }),
                    "the followed plan keeps the ceilings a forced scan resolved"
                );
            }
            FollowedPlan::Ended(text) => panic!("a succeeded scan is the plan: {text}"),
        }

        let mut cancelled = job_with("");
        cancelled.dry_run = true;
        cancelled.status = "cancelled".to_string();
        cancelled.error = Some("cancelled by ops during scan".to_string());
        assert!(
            matches!(
                followed_plan(cancelled),
                FollowedPlan::Ended(text)
                    if text == "The plan's scan ended without a report (cancelled): \
                                cancelled by ops during scan"
            ),
            "a cancelled scan says so in its own words"
        );

        let mut failed = job_with("");
        failed.dry_run = true;
        failed.status = "failed".to_string();
        assert!(
            matches!(
                followed_plan(failed),
                FollowedPlan::Ended(text)
                    if text == "The plan's scan ended without a report (failed)."
            ),
            "a failed scan is no plan"
        );
    }

    #[test]
    fn only_a_complete_ceiling_pair_can_be_restated() {
        assert_eq!(
            accepted_ceilings(&job_with(
                r#","accepted_max_nulled_rows":18,"accepted_max_ambiguous_rows":10"#
            )),
            Some(BoundCeilings {
                max_nulled: 18,
                max_ambiguous: 10,
            })
        );
        assert_eq!(
            accepted_ceilings(&job_with(r#","accepted_max_nulled_rows":18"#)),
            None,
            "half a pair binds nothing"
        );
        assert_eq!(
            accepted_ceilings(&job_with(r#","accepted_max_ambiguous_rows":10"#)),
            None
        );
        assert_eq!(
            accepted_ceilings(&job_with("")),
            None,
            "an unforced plan resolves no ceilings at all"
        );
        assert_eq!(
            accepted_ceilings(&job_with(
                r#","accepted_max_nulled_rows":0,"accepted_max_ambiguous_rows":0"#
            )),
            Some(BoundCeilings {
                max_nulled: 0,
                max_ambiguous: 0,
            })
        );
    }

    #[test]
    fn the_forced_rung_asks_for_numbers_once_and_then_binds_what_it_has() {
        let unforced = job_with("");
        assert_eq!(force_step(&unforced, false), ForceStep::NeedsPreview);
        assert_eq!(force_step(&unforced, true), ForceStep::Unbound);

        let forced = job_with(r#","accepted_max_nulled_rows":18,"accepted_max_ambiguous_rows":0"#);
        assert_eq!(
            force_step(&forced, true),
            ForceStep::Bound(BoundCeilings {
                max_nulled: 18,
                max_ambiguous: 0,
            })
        );
    }

    #[test]
    fn a_refusal_that_carries_ceilings_still_needs_a_fresh_forced_scan() {
        let refused_forced =
            job_with(r#","accepted_max_nulled_rows":18,"accepted_max_ambiguous_rows":4"#);
        assert_eq!(
            force_step(&refused_forced, false),
            ForceStep::NeedsPreview,
            "the pair on a refused row is the one it busted, not a new consent"
        );
    }

    #[test]
    fn only_a_refusal_for_an_offered_target_is_a_reusable_plan() {
        assert!(refusal_is_reusable("BIGINT", "VARCHAR"));
        assert!(refusal_is_reusable("BIGINT", "varchar"));
        assert!(!refusal_is_reusable("BIGINT", "SEVERITY"));
        assert!(!refusal_is_reusable("BIGINT", "BIGINT"));
        assert!(!refusal_is_reusable("VARCHAR", "varchar"));
        assert!(!refusal_is_reusable("BIGINT", "HUGEINT"));
    }

    #[test]
    fn the_current_pin_is_never_offered_as_a_target() {
        let targets = repin_targets("VARCHAR");
        assert_eq!(targets.len(), 4);
        assert!(!targets.contains(&CanonicalType::Varchar));
        assert!(!repin_targets("varchar").contains(&CanonicalType::Varchar));
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
        assert_eq!(default_target("BIGINT", "BIGINT"), CanonicalType::Varchar);
        assert_eq!(default_target("VARCHAR", "VARCHAR"), CanonicalType::BigInt);
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
        let running = StatusProbe::Job(probed(42, "running"));
        assert_eq!(poll_decide(42, 0, &running), PollAction::Track);
        for status in ["succeeded", "failed", "blocked", "refused_needs_force"] {
            let tick = StatusProbe::Job(probed(42, status));
            assert_eq!(poll_decide(42, 0, &tick), PollAction::Settle, "{status}");
        }
        let unknown = StatusProbe::Job(probed(42, "quiesced"));
        assert_eq!(poll_decide(42, 0, &unknown), PollAction::Settle);
    }

    #[test]
    fn another_id_or_no_job_loses_the_trail() {
        let other = StatusProbe::Job(probed(43, "running"));
        assert_eq!(poll_decide(42, 0, &other), PollAction::Lost);
        let other_done = StatusProbe::Job(probed(43, "succeeded"));
        assert_eq!(poll_decide(42, 0, &other_done), PollAction::Lost);
        assert_eq!(poll_decide(42, 0, &StatusProbe::NoJob), PollAction::Lost);
    }

    #[test]
    fn read_failures_retry_to_a_bound_then_give_up() {
        for n in 1..MAX_POLL_ERRORS {
            assert_eq!(
                poll_decide(42, n, &StatusProbe::Failed),
                PollAction::Retry,
                "{n}"
            );
        }
        assert_eq!(
            poll_decide(42, MAX_POLL_ERRORS, &StatusProbe::Failed),
            PollAction::GiveUp
        );
        assert_eq!(
            poll_decide(42, MAX_POLL_ERRORS + 5, &StatusProbe::Failed),
            PollAction::GiveUp
        );
    }

    #[test]
    fn recovery_adopts_only_a_provably_own_job() {
        assert_eq!(
            recovery_verdict(&lost_run(), &StatusProbe::Job(probed(42, "running"))),
            Recovery::Adopt,
            "field, target, force and an id past the bound all match"
        );
        let mut folded = probed(42, "running");
        folded.field = "DURATION".to_string();
        folded.to_type = "varchar".to_string();
        assert_eq!(
            recovery_verdict(&lost_run(), &StatusProbe::Job(folded)),
            Recovery::Adopt
        );
        assert_eq!(
            recovery_verdict(&lost_run(), &StatusProbe::Job(probed(42, "succeeded"))),
            Recovery::Adopt
        );
    }

    #[test]
    fn recovery_never_adopts_a_job_it_cannot_prove_is_ours() {
        let unmatched = Recovery::Indeterminate(Unproven::Unmatched);
        let spoil = |f: fn(&mut ProbedJob)| {
            let mut job = probed(42, "running");
            f(&mut job);
            recovery_verdict(&lost_run(), &StatusProbe::Job(job))
        };
        assert_eq!(spoil(|j| j.field = "status".to_string()), unmatched);
        assert_eq!(spoil(|j| j.to_type = "BIGINT".to_string()), unmatched);
        assert_eq!(spoil(|j| j.dry_run = true), unmatched);
        assert_eq!(spoil(|j| j.force = true), unmatched);
        assert_eq!(spoil(|j| j.id = 41), unmatched);
        assert_eq!(spoil(|j| j.id = 7), unmatched);
        assert_eq!(
            recovery_verdict(&lost_run(), &StatusProbe::NoJob),
            unmatched
        );
        let unbounded = LostRun {
            bound: None,
            ..lost_run()
        };
        assert_eq!(
            recovery_verdict(&unbounded, &StatusProbe::Job(probed(42, "running"))),
            unmatched
        );
    }

    #[test]
    fn a_failed_probe_is_told_apart_from_a_route_that_answered() {
        assert_eq!(
            recovery_verdict(&lost_run(), &StatusProbe::Failed),
            Recovery::Indeterminate(Unproven::ProbeFailed)
        );
        let lost = "network error";
        let failed = indeterminate_text(lost, Unproven::ProbeFailed);
        let unmatched = indeterminate_text(lost, Unproven::Unmatched);
        for text in [&failed, &unmatched] {
            assert!(text.starts_with(lost), "{text}");
            assert!(text.contains("unknown"), "{text}");
            assert!(!text.contains("never"), "{text}");
        }
        assert!(failed.contains("did not answer"), "{failed}");
        assert!(!failed.contains("reports no job"), "{failed}");
        assert!(unmatched.contains("reports no job"), "{unmatched}");
    }

    #[test]
    fn the_slot_is_free_only_when_nothing_is_running() {
        assert_eq!(
            slot_check(&StatusProbe::Job(probed(42, "running"))),
            SlotCheck::Held("duration".to_string())
        );
        let mut scan = probed(42, "running");
        scan.dry_run = true;
        assert_eq!(
            slot_check(&StatusProbe::Job(scan)),
            SlotCheck::Held("duration".to_string())
        );
        for status in ["succeeded", "failed", "blocked", "refused_needs_force"] {
            assert_eq!(
                slot_check(&StatusProbe::Job(probed(42, status))),
                SlotCheck::Free,
                "{status}"
            );
        }
        assert_eq!(slot_check(&StatusProbe::NoJob), SlotCheck::Free);
        assert_eq!(slot_check(&StatusProbe::Failed), SlotCheck::Unknown);
    }

    #[test]
    fn only_a_4xx_proves_a_real_run_was_never_claimed() {
        for status in [400_u16, 401, 403, 404, 422, 429, 499] {
            assert!(is_pre_claim_failure(Some(status)), "{status}");
        }
        for status in [500_u16, 502, 503, 504] {
            assert!(!is_pre_claim_failure(Some(status)), "{status}");
        }
        assert!(!is_pre_claim_failure(None));
    }

    #[test]
    fn a_job_is_toastable_exactly_once_per_page_load() {
        assert!(claim_toast(1));
        assert!(!claim_toast(1), "a second instance must not re-toast it");
        assert!(claim_toast(2));
        assert!(!claim_toast(2));
    }
}
