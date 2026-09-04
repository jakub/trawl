// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin trigger's pure logic: the target ladder, the 409
//! double-shape decode, and the poll decision.
//!
//! Everything here is data-in / data-out so the parts that are easy to
//! get subtly wrong are table-testable natively, off the browser:
//!
//! - [`classify_conflict`] — `POST /schema/repin` answers 409 with two
//!   different bodies (the refusal plan, and the error envelope for a
//!   slot held elsewhere). They are told apart by decoding, never by the
//!   status code, and never by sniffing error text.
//! - [`poll_decide`] — the status route is install-wide (there is no
//!   `?id=`), so every read has to be matched against the job id the
//!   case file holds. A different id, or no job at all, means the
//!   one-running slot moved on and our job's record is gone from the
//!   route: stop, keep what we last saw, say so.
//! - [`recovery_verdict`] — a real run whose response was lost either
//!   claimed a job or did not, and the client cannot tell which. The
//!   route only ever gets to say yes (a job it can prove is ours), and
//!   the absence of that proof is [`Unproven`], never "it never ran".
//! - [`slot_check`] — whether the install-wide one-running slot is free
//!   right now, which is the only thing that unwedges a modal parked on
//!   someone else's job.
//!
//! All three decisions read one reduction of a status response
//! ([`StatusProbe`]), so they cannot disagree about what a read said.
//!
//! The ladder rungs are [`trawl_core::schema::CanonicalType`] values
//! rather than mirrored strings — the crate is already an unconditional
//! dependency, and the `DuckDB` spellings on the wire are exactly what
//! [`CanonicalType::as_catalog`] writes.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::cell::RefCell;
use std::collections::HashSet;

use trawl_api::{ErrorResponse, RepinJobResponse, RepinResponse};
use trawl_core::schema::CanonicalType;

use crate::repin_hint::{repin_is_running, repin_is_terminal};

/// How often the case file re-reads `/schema/repin/status` while it
/// tracks a running job. One immediate read comes first, then this.
pub const REPIN_POLL_MS: u32 = 3_000;

/// Consecutive failed status reads tolerated before polling stops and
/// the case file says the status is unavailable. Bounded on purpose: a
/// store outage must cost one inline warning, not a toast per tick.
pub const MAX_POLL_ERRORS: u32 = 3;

/// The candidate ladder this SPA offers, in the engine's own order.
///
/// The engine admits six targets: these five physical rungs plus
/// `SEVERITY`. `SEVERITY` is left out deliberately, not forgotten:
/// putting a field on the severity ladder means asserting which dialect
/// its numerals are in (`otel` counts up, `syslog` counts down and
/// inverts), and that assertion is an operator decision about a sender's
/// provenance which no modal should be making on their behalf. It is
/// reachable through `trawl schema repin --to severity --dialect …` and
/// `POST /api/v1/schema/repin`.
pub const REPIN_LADDER: [CanonicalType; 5] = [
    CanonicalType::BigInt,
    CanonicalType::Double,
    CanonicalType::Timestamp,
    CanonicalType::Boolean,
    CanonicalType::Varchar,
];

/// The rungs the modal offers for a field pinned `current`: every rung
/// except the one it already has. `to == current` is the
/// resurrection-only pass, which stays a CLI decision because its only
/// effect is re-extracting shelved values under the same pin.
///
/// An unrecognised `current` (a catalog spelling this build doesn't
/// know) offers the whole ladder rather than nothing.
#[must_use]
pub fn repin_targets(current: &str) -> Vec<CanonicalType> {
    let current = current.to_ascii_uppercase();
    REPIN_LADDER
        .into_iter()
        .filter(|c| c.as_catalog() != current)
        .collect()
}

/// Whether a refused job's plan can be re-presented for a field pinned
/// `current`, instead of being read as a receipt only.
///
/// The plan is a scan of ONE target, so it means nothing for any other
/// rung: its counts, and the ceilings a forced run would restate, were
/// resolved by casting the corpus to `job.to_type` and to nothing else.
/// The two targets the modal never offers are exactly the two a refusal
/// can carry from elsewhere: `SEVERITY` (started from the CLI, since the
/// dialect assertion is not a modal's to make) and the same-type
/// resurrection pass. Both stay CLI work, so their refusals stay
/// receipts.
#[must_use]
pub fn refusal_is_reusable(current: &str, to_type: &str) -> bool {
    let to = to_type.to_ascii_uppercase();
    repin_targets(current).iter().any(|c| c.as_catalog() == to)
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
        .find(|c| c.as_catalog() == suggested)
        .or_else(|| offered.iter().find(|c| **c == CanonicalType::Varchar))
        .or_else(|| offered.first())
        .copied()
        .unwrap_or(CanonicalType::Varchar)
}

/// What a 409 from `POST /api/v1/schema/repin` turned out to be.
#[derive(Debug, Clone)]
pub enum ConflictBody {
    /// The body decoded as a repin job: the scan projected values the
    /// new pin cannot keep and no force flag was passed. The body is the
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
/// required field (`job` vs `error`), so neither can decode as the
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

/// The job a status read carried, reduced to the fields the decisions
/// below read. A separate type from [`RepinJobResponse`] so the rules can
/// be table-tested without a wire row per case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedJob {
    /// The job's id — matched against the tracked one, and ordered
    /// against the pre-request bound.
    pub id: i64,
    /// The field being repinned, exactly as the catalog spells it.
    pub field: String,
    /// The target pin.
    pub to_type: String,
    /// Whether the job stopped after the scan.
    pub dry_run: bool,
    /// Whether the job was started with a lossy projection accepted.
    pub force: bool,
    /// The job's status string, verbatim from the wire.
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

/// One `/schema/repin/status` read, reduced to what the decisions need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusProbe {
    /// The route answered with a job.
    Job(ProbedJob),
    /// The route answered `job: null` — no repin has ever run, or the
    /// row this install had is gone.
    NoJob,
    /// The read failed (network, or the store behind the route). It said
    /// nothing at all, so every decision has to treat it as an absence
    /// of information, never as an absence of a job.
    Failed,
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
        // The route is install-wide: a different id is a different job,
        // never a later view of ours, and `job: null` cannot be ours
        // either. Both mean our record is unreachable from here.
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

/// Whether a failed real run's HTTP status proves nothing was claimed.
///
/// The server claims the one-running `repin_jobs` row before it answers,
/// so only a status decided before that claim can be read as "nothing
/// started". Every 4xx is one: 400 is request validation, 401/403 is
/// authorization, 404 is a field the catalog does not hold, and the 409s
/// are decoded elsewhere as a refusal or a held slot. A 5xx can land
/// after the claim — the rewrite may be running right now — and a
/// transport or decode failure carries no status at all, so both stay
/// indeterminate and must never be reported as "provably not running".
#[must_use]
pub fn is_pre_claim_failure(status: Option<u16>) -> bool {
    matches!(status, Some(400..=499))
}

/// The real run whose response was lost, and the one fact that can make
/// its outcome provable.
#[derive(Debug, Clone)]
pub struct LostRun {
    /// The field the request named, exactly as it was sent.
    pub field: String,
    /// The target pin the request named.
    pub to: String,
    /// The force flag the request carried.
    pub force: bool,
    /// The newest job id known to exist before the request went out —
    /// the plan's own row, since every dry run writes one. The request's
    /// job, if it claimed one at all, is strictly newer than this.
    /// `None` means nothing about the request's outcome can be proven.
    pub bound: Option<i64>,
}

/// What a recovery probe settled about a [`LostRun`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The probed job is provably the run that was issued: adopt it.
    Adopt,
    /// Nothing was proven. The run may be rewriting the corpus right
    /// now, so this can never license a second one.
    Indeterminate(Unproven),
}

/// Why a recovery probe proved nothing. Kept apart because the two say
/// materially different things to an operator, and conflating them would
/// have the modal claim the route reported no job when the route itself
/// was what failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unproven {
    /// The status read itself failed, so the route said nothing at all.
    ProbeFailed,
    /// The route answered, but what it returned cannot be proven to be
    /// the run in question — a different field or target, a dry run,
    /// another force flag, an id no newer than the pre-request bound, no
    /// job at all, or no bound to order against.
    Unmatched,
}

/// Whether a probed job is provably the lost run's own job.
///
/// Every clause is necessary, and the id ordering is what makes the rest
/// mean anything: the status route is install-wide and answers about the
/// running job if there is one, else the newest — so a job matching
/// field, target and force could equally be the previous repin of the
/// same field with the same target, days old. Only an id strictly newer
/// than a row that existed before the request went out can have been
/// created by that request.
///
/// The negative is deliberately never proven. A transport failure does
/// not mean the request was never delivered (a client-side timeout can
/// abandon a request that is still in flight), so "the route shows
/// nothing newer" is not evidence the run did not start — it is
/// [`Unproven::Unmatched`], and the caller must not offer to run again.
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

/// The sentence an indeterminate outcome is reported with: the failure
/// that lost the response, then what the recovery probe could and could
/// not establish. Neither wording claims the run did not start.
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

/// What one status read says about the install-wide one-running slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotCheck {
    /// Nothing is running: a repin can be planned again.
    Free,
    /// Held, by a job on this field (the name verbatim, for display
    /// through `sanitize_display_text`).
    Held(String),
    /// The read failed, so the slot's state is unknown — which is not
    /// the same as free.
    Unknown,
}

/// Whether the one-running slot is free right now.
///
/// A dry run holds the slot exactly like a rewrite does (its scan is a
/// full-corpus pass), so the question is only ever whether something is
/// running, never what kind of job it is.
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

/// The ceilings a forced repin is held to, both resolved.
///
/// The pair travels together because a request that states one dimension
/// and leaves the other to the server would bind a number nobody was
/// shown, which is the whole thing ceilings exist to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundCeilings {
    /// Rows the new pin may fail to read before the cutover refuses.
    pub max_nulled: u64,
    /// Dialect-ambiguous numerals allowed before the cutover refuses.
    pub max_ambiguous: u64,
}

/// The ceilings a report resolved, when it resolved both.
///
/// Absent for an unforced job (which accepts no loss at all), for a
/// claimed job that has not scanned yet, and for a job row a server
/// older than the ceiling columns wrote. The modal has nothing honest to
/// show or to restate in those cases, so it asks for a forced plan
/// instead of guessing the formula — the arithmetic lives on the server,
/// and a copy of it here would be a second one to keep in step.
///
/// This is the SPA's half of the CLI's `schema::accepted_ceilings`. Both
/// read the same two wire fields and both refuse a half-filled pair.
#[must_use]
pub fn accepted_ceilings(job: &RepinJobResponse) -> Option<BoundCeilings> {
    Some(BoundCeilings {
        max_nulled: job.accepted_max_nulled_rows?,
        max_ambiguous: job.accepted_max_ambiguous_rows?,
    })
}

/// What the forced rung of the ladder can do with the job on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceStep {
    /// No forced scan has answered for the plan on screen, so there are
    /// no numbers to accept yet. A forced dry run produces them, and it
    /// is the operator's click to make.
    NeedsPreview,
    /// The forced scan resolved a pair, so the forced run restates it
    /// and the bound the server enforces is the bound on screen.
    Bound(BoundCeilings),
    /// A forced scan came back with no resolved pair. Nothing to
    /// restate; the execution derives its own ceilings, exactly as a
    /// repin did before ceilings existed. Rescanning would only produce
    /// the same answer, so the forced run is offered.
    Unbound,
}

/// Which of the three [`ForceStep`] cases the dialog is in.
///
/// `previewed` is whether a FORCED scan has answered in this dialog for
/// the plan on screen, and it alone opens the acceptance. Reading the
/// ceilings off the job row instead would re-open it for a refusal that
/// already busted them: a forced execution refused at the cutover gate
/// comes back carrying the very pair it exceeded, and offering that pair
/// again would send the same run at the same ceilings for the same
/// answer. Every forced refusal therefore costs a fresh forced scan,
/// whose report is the corpus as it stands now — which is the point,
/// since ingest kept running while the refused job read it.
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

thread_local! {
    /// Repin job ids that have already raised a toast, for the life of
    /// the page.
    ///
    /// Global on purpose, and the one piece of state in this module. The
    /// field case file is a component rebuilt whenever the `?field=` it
    /// mounts from changes, so navigating A → B → A leaves the third
    /// instance with no memory of what the first announced: per-instance
    /// bookkeeping would toast one job's completion twice. A
    /// `thread_local` rather than a lock because the SPA is
    /// single-threaded wasm, and unbounded only in the sense that it
    /// holds one `i64` per job an operator watched finish in one page
    /// load.
    static TOASTED_JOBS: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
}

/// Claim the single toast for `job_id`: true exactly once per id per
/// page load, whichever component instance asks first.
pub fn claim_toast(job_id: i64) -> bool {
    TOASTED_JOBS.with_borrow_mut(|seen| seen.insert(job_id))
}

#[cfg(test)]
mod tests {
    use super::{
        BoundCeilings, ConflictBody, ForceStep, LostRun, MAX_POLL_ERRORS, PollAction, ProbedJob,
        REPIN_BUSY_FALLBACK, Recovery, RepinJobResponse, SlotCheck, StatusProbe, Unproven,
        accepted_ceilings, claim_toast, classify_conflict, default_target, force_step,
        indeterminate_text, is_pre_claim_failure, poll_decide, recovery_verdict,
        refusal_is_reusable, repin_targets, slot_check,
    };
    use trawl_core::schema::CanonicalType;

    /// A job as the status route reports it: our field, our target, a
    /// real run, and an id past the bound. Each test spoils exactly the
    /// clause it is about.
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

    /// The run the modal issued, bounded by the plan's own job row.
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

    /// A job as the wire carries it, with whatever ceiling keys the
    /// caller wants spliced in. Built by decoding, not by struct
    /// literal, so a job that simply omits the keys — the pre-ceiling
    /// server — is one of the cases under test.
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

    /// The modal may only restate a pair the report actually resolved.
    /// A half-filled pair is not half a consent: it means the other
    /// number would come from the server's own derivation, and the
    /// operator would be confirming a bound nobody printed.
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
        // Zero is a real ceiling — "force the ambiguity, not one row of
        // loss" — and must not read as absent.
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

    /// The three rungs of the forced step, and the one that must not be
    /// reachable twice: without a forced scan there is nothing to
    /// accept, so the dialog asks for one; a forced scan that resolved
    /// the pair binds it; and a forced scan that resolved none stops
    /// asking rather than reading the corpus again for the same answer.
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

    /// A forced execution refused at the cutover gate comes back
    /// carrying the ceilings it exceeded. Re-offering those is offering
    /// the run that just failed: the acceptance stays shut until a fresh
    /// forced scan answers.
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

    /// A refusal is a scan of one target, so it may only be re-presented
    /// as a plan for that same target — and only when the modal offers
    /// it at all. The two it never offers are the two a refusal can
    /// carry in from the CLI.
    #[test]
    fn only_a_refusal_for_an_offered_target_is_a_reusable_plan() {
        assert!(refusal_is_reusable("BIGINT", "VARCHAR"));
        // The catalog spells pins uppercase; a lowercase wire spelling
        // is the same rung.
        assert!(refusal_is_reusable("BIGINT", "varchar"));
        // The severity ladder needs a dialect asserted, so the modal
        // offers no such rung and the refusal stays a receipt.
        assert!(!refusal_is_reusable("BIGINT", "SEVERITY"));
        // The resurrection pass repins to the pin the field already has.
        assert!(!refusal_is_reusable("BIGINT", "BIGINT"));
        assert!(!refusal_is_reusable("VARCHAR", "varchar"));
        // A target this build cannot name is not invented into a rung.
        assert!(!refusal_is_reusable("BIGINT", "HUGEINT"));
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
        let running = StatusProbe::Job(probed(42, "running"));
        assert_eq!(poll_decide(42, 0, &running), PollAction::Track);
        for status in ["succeeded", "failed", "blocked", "refused_needs_force"] {
            let tick = StatusProbe::Job(probed(42, status));
            assert_eq!(poll_decide(42, 0, &tick), PollAction::Settle, "{status}");
        }
        // An unknown future status is terminal, so polling stops rather
        // than spinning forever.
        let unknown = StatusProbe::Job(probed(42, "quiesced"));
        assert_eq!(poll_decide(42, 0, &unknown), PollAction::Settle);
    }

    #[test]
    fn another_id_or_no_job_loses_the_trail() {
        let other = StatusProbe::Job(probed(43, "running"));
        assert_eq!(poll_decide(42, 0, &other), PollAction::Lost);
        // Even a terminal row for another id: the install-wide route
        // never speaks about our job again once the slot moves on.
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
        // The catalog folds names and the wire spells types uppercase,
        // but neither casing may cost us our own job.
        let mut folded = probed(42, "running");
        folded.field = "DURATION".to_string();
        folded.to_type = "varchar".to_string();
        assert_eq!(
            recovery_verdict(&lost_run(), &StatusProbe::Job(folded)),
            Recovery::Adopt
        );
        // A terminal job is still ours — the run can have finished
        // inside the round trip whose response was lost.
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
        // Another field's job, however recent.
        assert_eq!(spoil(|j| j.field = "status".to_string()), unmatched);
        // Our field, but a target we did not ask for: someone else (or
        // the CLI) started it.
        assert_eq!(spoil(|j| j.to_type = "BIGINT".to_string()), unmatched);
        // A dry run is never the real run whose response was lost.
        assert_eq!(spoil(|j| j.dry_run = true), unmatched);
        // A forced job when we sent no force: a different request.
        assert_eq!(spoil(|j| j.force = true), unmatched);
        // The bound's own row, and anything older: created before the
        // request, so the request cannot have created it.
        assert_eq!(spoil(|j| j.id = 41), unmatched);
        assert_eq!(spoil(|j| j.id = 7), unmatched);
        // No row at all is not proof the run never started: the request
        // may still be in flight past a client-side timeout.
        assert_eq!(
            recovery_verdict(&lost_run(), &StatusProbe::NoJob),
            unmatched
        );
        // No bound: nothing to order against, so nothing is provable
        // even when every other clause matches.
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
            // The one thing an indeterminate outcome must never say.
            assert!(!text.contains("never"), "{text}");
        }
        // A failed probe must not be reported as the route having spoken.
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
        // A dry run's scan holds the slot exactly like a rewrite does.
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
        // A failed read is not a free slot.
        assert_eq!(slot_check(&StatusProbe::Failed), SlotCheck::Unknown);
    }

    #[test]
    fn only_a_4xx_proves_a_real_run_was_never_claimed() {
        // Decided before the claim: validation, authorization, a name
        // the catalog does not hold.
        for status in [400_u16, 401, 403, 404, 422, 429, 499] {
            assert!(is_pre_claim_failure(Some(status)), "{status}");
        }
        // A 5xx can be answered after the row was claimed, so the job
        // may be rewriting the corpus right now.
        for status in [500_u16, 502, 503, 504] {
            assert!(!is_pre_claim_failure(Some(status)), "{status}");
        }
        // No status at all — a dropped connection or an undecodable
        // body — proves nothing either way.
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
