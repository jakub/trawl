// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RepinModal/>` — the dry-run-first repin ladder (ADR-0011 slice C2).
//!
//! The CLI's `--dry-run` → `--yes` → `--force` ladder, rendered. The
//! rules it exists to keep:
//!
//! 1. **Nothing runs before a plan.** Mounting POSTs `dry_run: true`;
//!    the real run is reachable only from a plan the operator has seen.
//!    Changing the target invalidates that plan rather than carrying its
//!    numbers onto a different type.
//! 2. **Force is reachable only AFTER a refusal.** The server decides
//!    lossiness — the modal never pre-empts it by offering force on a
//!    plan whose `projected_nulls` looks non-zero. The checkbox appears
//!    once `refused_needs_force` has come back, defaults OFF, and must
//!    be checked before the forced run can be started.
//! 3. **The plan is a snapshot, not a reservation.** Ingest keeps
//!    running; the real run rescans, and its numbers can differ.
//! 4. **An indeterminate outcome never offers to run again.** A real run
//!    whose response was lost may have claimed a job; only a status
//!    probe that can PROVE which job is ours resolves that, and the
//!    absence of proof leaves the operator a re-probe, never a second
//!    rewrite.
//!
//! Closing the modal does not cancel anything: past the claim the job
//! runs DETACHED server-side, so a dismissed dialog abandons the
//! RESPONSE, never the work.

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::RepinJobResponse;
use trawl_core::sanitize::sanitize_display_text;

use crate::api::{self, ApiError, RepinOutcome};
use crate::repin_flow::{
    LostRun, ProbedJob, Recovery, SlotCheck, StatusProbe, Unproven, default_target,
    indeterminate_text, is_pre_claim_failure, recovery_verdict, repin_targets, slot_check,
};
use crate::service_card_fmt::format_bytes;
use fleet_ui::{Btn, Icon, Modal, Segmented, SegmentedOption, Variant};

/// The wire status a refusal carries — the one status this dialog has to
/// ACT on rather than render. The vocabulary is
/// `trawl-server/src/store/repin.rs`.
const STATUS_REFUSED: &str = "refused_needs_force";

/// Shown in the busy block when the slot re-check could not be read. The
/// slot's state is then UNKNOWN, which is not "free".
const SLOT_CHECK_FAILED: &str = "Couldn't read the repin status just now, so whether the slot is \
                                 still held is unknown. Try again.";

#[derive(Debug, Clone)]
enum Phase {
    /// A dry run is in flight. An honest busy state: the scan is a
    /// full-corpus pass and can take minutes.
    Planning,
    /// The shown plan no longer describes the selected target, so it is
    /// gone until the operator asks for a new one.
    NeedsPlan,
    /// A plan is on screen and can be executed.
    Plan(Box<RepinJobResponse>),
    /// The real run came back `refused_needs_force`: the same plan,
    /// re-presented with the numbers the refusal was based on.
    NeedsForce(Box<RepinJobResponse>),
    /// A real run is in flight.
    Submitting,
    /// The one-running slot is held elsewhere. No confirm from here —
    /// only a re-read of the slot, which returns to [`Phase::NeedsPlan`]
    /// once it is free.
    Busy(String),
    /// A real run's response was lost and the recovery probe could not
    /// prove what became of it. The rewrite may be running RIGHT NOW, so
    /// this phase offers exactly one action: probe again.
    Indeterminate {
        /// The failure that lost the response.
        lost: String,
        /// What the last probe could not establish.
        why: Unproven,
        /// The plan the run was started from, kept on screen.
        plan: Option<Box<RepinJobResponse>>,
        /// What was asked for, so a re-probe can recognise it.
        run: LostRun,
    },
    /// The server's own message for a status this modal cannot act on.
    /// The modal STAYS OPEN and keeps the plan it was acting on, if
    /// there was one — a 503 must not cost the operator the scan they
    /// have already paid for.
    ///
    /// Two things reach this: any dry-run failure (the scan mutates
    /// nothing, so retrying is free), and a real run refused with a
    /// DEFINITIVE 4xx. The claim happens before the server answers, so a
    /// 4xx is decided before it — nothing started, and saying the
    /// outcome is unknown would be a lie the operator has to chase. A
    /// 5xx or a lost connection is [`Phase::Indeterminate`] instead.
    Failed {
        message: String,
        plan: Option<Box<RepinJobResponse>>,
    },
}

/// The dialog's own reactive handles, carried to the out-of-line probes
/// as ONE value. They are `Copy` handles; a probe that took eight
/// positional signals is a probe that writes the wrong one.
#[derive(Clone, Copy)]
struct Handles {
    phase: RwSignal<Phase>,
    probing: RwSignal<bool>,
    force: RwSignal<bool>,
    busy_field: RwSignal<Option<String>>,
    slot_note: RwSignal<Option<String>>,
    /// False once this dialog is disposed. Every signal above then
    /// PANICS on `get_untracked` (a write is a silent no-op), and any
    /// state a dead dialog computes is state nobody can see.
    alive: StoredValue<bool>,
}

impl Handles {
    /// Whether the dialog these handles belong to is still mounted. A
    /// disposed `StoredValue` reads as `None`, which is the same answer
    /// as the flag itself.
    fn is_alive(self) -> bool {
        self.alive.try_get_value() == Some(true)
    }
}

/// The repin dialog. `on_close` carries `Some(job)` ONLY for a real
/// 202 — the case file adopts that job, starts polling, and owns every
/// receipt from there on.
#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn RepinModal(
    field: String,
    /// The pin as it stands — excluded from the target strip.
    current_type: String,
    /// The analyzer's suggestion; the selector's starting rung.
    suggested_to: String,
    /// A refusal to re-present instead of planning afresh. `Some` when
    /// the case file's poll watched a running job land
    /// `refused_needs_force`: that job's scan IS the plan, so asking for
    /// a second one would be a second full-corpus pass for the same
    /// answer.
    refused: Option<RepinJobResponse>,
    on_close: Callback<Option<RepinJobResponse>>,
) -> impl IntoView {
    let shown_field = sanitize_display_text(&field);
    let targets = repin_targets(&current_type);
    let initial = refused.as_ref().map_or_else(
        || default_target(&current_type, &suggested_to).as_duckdb(),
        |job| {
            // The refusal's own target, so the re-presented numbers and
            // the selected rung cannot disagree.
            repin_targets(&current_type)
                .into_iter()
                .find(|c| c.as_duckdb() == job.to_type)
                .map_or_else(
                    || default_target(&current_type, &suggested_to).as_duckdb(),
                    |c| c.as_duckdb(),
                )
        },
    );

    let target = RwSignal::new(initial.to_string());
    let phase =
        RwSignal::new(refused.map_or(Phase::Planning, |job| Phase::NeedsForce(Box::new(job))));
    let force = RwSignal::new(false);
    let busy = RwSignal::new(false);
    // A status PROBE is in flight — the recovery read after a lost
    // response, or the slot re-check. Tracked apart from `busy`: neither
    // probe mutates anything, but both own the primary action while they
    // run.
    let probing = RwSignal::new(false);
    // Why the last slot re-check told us nothing.
    let slot_note = RwSignal::new(None::<String>);
    // Every initiating click bumps this; a response whose generation is
    // stale (the operator changed target and re-planned meanwhile) is
    // dropped rather than painted over the current state.
    let generation = RwSignal::new(0_u32);
    // Which field holds the slot, when the status route was willing to
    // say. Advisory: the busy message stands on its own without it.
    let busy_field = RwSignal::new(None::<String>);

    // Modal-level liveness. Every request below outlives the click that
    // issued it, and closing the dialog disposes the signals above: a
    // read of one then PANICS, so each future re-checks this the moment
    // it comes back from an await.
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let handles = Handles {
        phase,
        probing,
        force,
        busy_field,
        slot_note,
        alive,
    };

    let field_for_calls = field.clone();
    // The one place a repin request is issued. `dry_run`/`force` decide
    // which rung of the ladder; nothing else varies.
    let submit = Callback::new(move |(dry_run, forced): (bool, bool)| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let seq = generation.get_untracked() + 1;
        generation.set(seq);
        // Carried across the in-flight phase so a failure can hand the
        // plan back instead of blanking it.
        let carried = match phase.get_untracked() {
            Phase::Plan(plan)
            | Phase::NeedsForce(plan)
            | Phase::Failed {
                plan: Some(plan), ..
            } => Some(plan),
            _ => None,
        };
        phase.set(if dry_run {
            Phase::Planning
        } else {
            Phase::Submitting
        });
        let field = field_for_calls.clone();
        let to = target.get_untracked();
        spawn_local(async move {
            let outcome = api::repin(&field, &to, dry_run, forced).await;
            // 202 FIRST, before any liveness or generation gate: the job
            // is claimed and running detached server-side, so the one
            // thing that must survive a dialog closed mid-claim is the
            // hand-up. `try_run` because the callback may be disposed
            // too — the case-file drawer that owns it outlives this
            // modal, and in the case where even that is gone, the
            // drawer's mount-time status read adopts the running job the
            // next time the case file is opened.
            let outcome = match outcome {
                Ok(RepinOutcome::Started(job)) => {
                    on_close.try_run(Some(job));
                    return;
                }
                other => other,
            };
            // Everything past here paints THIS dialog, so a closed one
            // has nothing left to say — and reading a disposed signal
            // panics where writing one is a silent no-op.
            if !handles.is_alive() {
                return;
            }
            if generation.try_get_untracked() != Some(seq) {
                // A newer request owns the dialog now.
                return;
            }
            busy.set(false);
            match outcome {
                // 200 is the scan report and nothing else — the server
                // only answers it to a dry run.
                Ok(RepinOutcome::DryRun(job)) => phase.set(Phase::Plan(Box::new(job))),
                // Handed to the case file above, before the gates.
                Ok(RepinOutcome::Started(_)) => {}
                Ok(RepinOutcome::Refused(job)) => {
                    // Re-arm the acceptance: a refusal must be accepted
                    // for the plan actually shown, never carried over.
                    force.set(false);
                    phase.set(Phase::NeedsForce(Box::new(job)));
                }
                Ok(RepinOutcome::Busy(msg)) => {
                    phase.set(Phase::Busy(msg));
                    // Which field holds it, if the status route says.
                    // Annotation only (`adopt_free: false`): the 409 is
                    // the fact on screen and was true when the server
                    // said it, so this probe never moves the dialog on
                    // its own — the operator's "Check again" does.
                    probe_slot(false, handles);
                }
                Err(e) => {
                    // A dry run mutates nothing, and a real run answered
                    // a DEFINITIVE 4xx was decided before the claim: a
                    // 400's validation, a 403 from a permission that
                    // expired between the plan and the confirm, a 404
                    // for a name the catalog does not hold. Both keep
                    // the plan and say what the server said — calling
                    // either outcome unknown would send an operator
                    // hunting a job that does not exist.
                    if dry_run || is_pre_claim_failure(e.http_status()) {
                        phase.set(Phase::Failed {
                            message: failure_text(&e),
                            plan: carried,
                        });
                    } else {
                        // A real run that failed IN TRANSIT may still
                        // have been claimed: the server detaches the job
                        // at the claim, so a dropped response is not a
                        // job that did not start. Probe once and adopt
                        // the job — but only one that can be PROVEN to
                        // be this request's, which is what the plan's own
                        // row bounds.
                        let run = LostRun {
                            field: field.clone(),
                            to: to.clone(),
                            force: forced,
                            bound: carried.as_ref().map(|p| p.id),
                        };
                        probe_recovery(run, carried, failure_text(&e), handles, on_close);
                    }
                }
            }
        });
    });

    // Mount: the plan comes first, always — unless a refusal was handed
    // in, which already carries one.
    if matches!(phase.get_untracked(), Phase::Planning) {
        submit.run((true, false));
    }

    let cancel = Callback::new(move |()| on_close.run(None));

    // The primary action depends on the rung: plan, run, forced run, or
    // — where the outcome is not this dialog's to decide — a status read
    // that can never mutate anything.
    let primary = Callback::new(move |()| {
        if probing.get_untracked() {
            return;
        }
        match phase.get_untracked() {
            // A dry-run failure is retryable BECAUSE it is a dry run:
            // the scan mutates nothing, so a second one at worst costs
            // another full-corpus pass.
            Phase::NeedsPlan | Phase::Failed { .. } => submit.run((true, false)),
            Phase::Plan(_) => submit.run((false, false)),
            Phase::NeedsForce(_) => {
                if force.get_untracked() {
                    submit.run((false, true));
                }
            }
            // The ONLY action an unproven outcome offers. Re-probing is
            // idempotent; re-running would not be.
            Phase::Indeterminate {
                lost, plan, run, ..
            } => probe_recovery(run, plan, lost, handles, on_close),
            Phase::Busy(_) => probe_slot(true, handles),
            Phase::Planning | Phase::Submitting => {}
        }
    });

    let primary_label = move || {
        if probing.get() {
            return "Checking…";
        }
        match phase.get() {
            Phase::Planning => "Planning…",
            Phase::NeedsPlan => "Get plan",
            Phase::Plan(_) => "Run repin",
            Phase::NeedsForce(_) => "Run forced repin",
            Phase::Submitting => "Starting…",
            Phase::Busy(_) => "Check again",
            Phase::Indeterminate { .. } => "Check status",
            Phase::Failed { .. } => "Retry plan",
        }
    };
    let primary_disabled = Signal::derive(move || {
        if probing.get() {
            return true;
        }
        match phase.get() {
            Phase::Planning | Phase::Submitting => true,
            Phase::NeedsForce(_) => !force.get(),
            Phase::NeedsPlan
            | Phase::Plan(_)
            | Phase::Busy(_)
            | Phase::Indeterminate { .. }
            | Phase::Failed { .. } => false,
        }
    });
    let primary_variant = move || match phase.get() {
        Phase::NeedsForce(_) => Variant::Danger,
        _ => Variant::Primary,
    };

    let strip = targets
        .into_iter()
        .map(|c| SegmentedOption::new(c.as_duckdb(), c.as_duckdb()))
        .collect::<Vec<_>>();

    // The target is what every in-flight request was issued FOR, so it
    // is frozen for as long as one is outstanding — a dry run included,
    // whose abandoned scan would otherwise keep holding the install-wide
    // slot the follow-up "Get plan" needs.
    let target_locked = Signal::derive(move || {
        busy.get() || probing.get() || matches!(phase.get(), Phase::Indeterminate { .. })
    });

    view! {
        <Modal
            title="Repin field"
            icon=Icon::Bolt
            on_cancel=cancel
            on_submit=primary
            footer=Box::new(move || view! {
                <div class="hint">
                    "Closing does not cancel a started job."
                </div>
                <Btn variant=Variant::Secondary on_click=cancel>"Close"</Btn>
                {move || view! {
                    <Btn
                        variant=primary_variant()
                        disabled=primary_disabled
                        on_click=primary
                    >
                        {primary_label()}
                    </Btn>
                }}
            }.into_any())
        >
            <div class="m-field">
                <label>"Field"</label>
                <div class="preview mono">{shown_field.clone()}</div>
            </div>

            <div class="m-field">
                <label>"Repin to"</label>
                <Segmented
                    full=true
                    options=strip
                    active=Signal::derive(move || target.get())
                    on_change=Callback::new(move |id: String| {
                        if id == target.get_untracked() {
                            return;
                        }
                        // Frozen while anything is outstanding. A real
                        // run's 202 is the only thing that can hand the
                        // job to the case file, so it must not be dropped
                        // by a generation bump; a dry run's scan holds
                        // the one-running slot until it terminalizes
                        // server-side, so abandoning it here would leave
                        // the follow-up plan 409-ing against our OWN
                        // scan with no way back; and an indeterminate
                        // run's target is the only record of what may be
                        // rewriting the corpus right now.
                        if target_locked.get_untracked() {
                            return;
                        }
                        target.set(id);
                        force.set(false);
                        // The plan on screen was scanned for a DIFFERENT
                        // target. It is not adapted, it is discarded.
                        generation.update(|g| *g += 1);
                        busy.set(false);
                        phase.set(Phase::NeedsPlan);
                    })
                />
                <p class="rp-note">
                    "Current pin: "<span class="mono">{current_type.clone()}</span>
                    ". The pin it already has is not offered — re-extracting shelved values "
                    "under the SAME pin is a `--force` resurrection pass, and stays a CLI decision."
                </p>
                {move || target_locked.get().then(|| view! {
                    <p class="rp-note">
                        "The target is locked while this request is outstanding: it is what the \
                         request was issued for, and the scan behind it holds the install-wide \
                         repin slot until the server finishes with it."
                    </p>
                })}
            </div>

            {move || match phase.get() {
                Phase::Planning => view! {
                    <p class="rp-note">
                        "Scanning the corpus for this field. This reads every retained file, \
                         so it can take minutes on a large archive — the scan holds the \
                         one-running repin slot while it runs."
                    </p>
                }.into_any(),
                Phase::NeedsPlan => view! {
                    <p class="rp-note">
                        "The plan you were reading was scanned for a different target type, so \
                         it has been discarded rather than reinterpreted. Ask for a new one \
                         before running anything. If the previous scan is still going, the new \
                         one has to wait for the slot."
                    </p>
                }.into_any(),
                Phase::Plan(job) => plan_block(&job, false),
                Phase::NeedsForce(job) => view! {
                    {plan_block(&job, true)}
                    <div class="rp-force">
                        <label>
                            <input
                                type="checkbox"
                                prop:checked=move || force.get()
                                on:change=move |e| {
                                    force.set(checked_from_event(&e));
                                }
                            />
                            " I accept that "
                            {job.projected_nulls}
                            " stored values become NULL"
                        </label>
                        <p class="rp-note">
                            "A forced repin writes NULL wherever the new pin cannot keep the \
                             stored value. The originals stay findable in "
                            <span class="mono">"_raw"</span>
                            ", and a later repin back can resurrect them from there — but the \
                             typed column will not carry them until it does."
                        </p>
                    </div>
                }.into_any(),
                Phase::Submitting => view! {
                    <p class="rp-note">"Claiming the repin slot…"</p>
                }.into_any(),
                Phase::Busy(msg) => view! {
                    <div class="rp-err" role="alert">
                        <p>"Another repin is already running — the slot is one at a time, \
                            install-wide."</p>
                        <p class="mono">{sanitize_display_text(&msg)}</p>
                        {move || busy_field.get().map(|f| view! {
                            <p>"It is repinning "<span class="mono">{f}</span>"."</p>
                        })}
                        {move || slot_note.get().map(|note| view! { <p>{note}</p> })}
                    </div>
                    <p class="rp-note">
                        "Nothing was started. \"Check again\" re-reads the slot; once it is free \
                         this dialog goes back to asking for a plan."
                    </p>
                }.into_any(),
                Phase::Indeterminate { lost, why, plan, .. } => view! {
                    <div class="rp-err" role="alert">
                        <p>{sanitize_display_text(&indeterminate_text(&lost, why))}</p>
                        <p>
                            "The repin may be running right now: the server claims the job before \
                             it answers, and then runs it detached. This dialog will not offer to \
                             start it again — a second run would rewrite the corpus twice."
                        </p>
                        <p>
                            "\"Check status\" reads the slot again; "
                            <span class="mono">"trawl schema repin-status"</span>
                            " reads the same thing from a shell. Closing is safe, and the field's \
                             case file picks up whatever is running."
                        </p>
                    </div>
                    // The scan is still the last thing known about this
                    // field, and it cost a full-corpus pass.
                    {plan.map(|plan| plan_block(&plan, true))}
                }.into_any(),
                Phase::Failed { message, plan } => view! {
                    <div class="rp-err" role="alert">
                        <p>{sanitize_display_text(&message)}</p>
                    </div>
                    // The scan that was already paid for survives the
                    // failure — retrying does not mean re-reading the
                    // corpus unless the retry IS the scan.
                    {plan.map(|plan| plan_block(&plan, true))}
                }.into_any(),
            }}
        </Modal>
    }
}

/// Read a checkbox's state off its change event (the `fleet_ui::Toggle`
/// idiom — this control is a plain checkbox, not a switch, because it is
/// an acceptance and not a setting).
fn checked_from_event(e: &leptos::ev::Event) -> bool {
    use wasm_bindgen::JsCast;
    e.target()
        .and_then(|t| t.dyn_into::<leptos::web_sys::HtmlInputElement>().ok())
        .is_some_and(|el| el.checked())
}

/// The scan's numbers. `refused` swaps the copy from "what this will do"
/// to "what the refusal is based on" — the numbers themselves are the
/// same shape, produced by the same expressions the rewrite writes.
fn plan_block(job: &RepinJobResponse, refused: bool) -> AnyView {
    // Exact digits, not the compact `format_count` the tables use: this
    // is a number an operator accepts responsibility for, and "1.2k
    // values become NULL" is not a fact anyone can act on.
    let files_total = job.files_total;
    let files_label = job.files_total;
    let rows_carrying = job.rows_carrying;
    let projected_nulls = job.projected_nulls;
    let resurrectable = job.resurrectable;
    let bytes = format_bytes(job.affected_bytes);
    let lossy = job.projected_nulls > 0;
    view! {
        <div class="rp-plan">
            <div class="rp-kv"><span>"Files affected"</span><span>{files_label}</span></div>
            <div class="rp-kv"><span>"Rows carrying the field"</span><span>{rows_carrying}</span></div>
            <div class="rp-kv"><span>"Values the new pin cannot keep"</span><span>{projected_nulls}</span></div>
            <div class="rp-kv">
                <span>"Values resurrected from _raw"</span><span>{resurrectable}</span>
            </div>
            <div class="rp-kv"><span>"Bytes rewritten"</span><span>{bytes}</span></div>

            {(files_total == 0).then(|| view! {
                <p class="rp-note">
                    "No retained files currently carry this field, so the rewrite has nothing \
                     to do — but the pin still flips, and every event ingested from then on is \
                     typed by it."
                </p>
            })}

            <p class="rp-note">
                "A repin rewrites this field across the ENTIRE corpus — every service, every \
                 environment, every day. Retention stands down for the job's whole life, and \
                 the affected bytes are held twice until it sweeps."
            </p>
            <p class="rp-note">
                "These numbers are a snapshot of the scan, not a reservation: ingest keeps \
                 running, and the real run rescans."
            </p>
            {(lossy && !refused).then(|| view! {
                <p class="rp-note">
                    "This plan projects values the target pin cannot keep. Running it will \
                     stop at the refusal and ask you to accept the loss explicitly."
                </p>
            })}
        </div>
    }
    .into_any()
}

/// One `/schema/repin/status` read: the decision's reduction of it, and
/// the row itself for the caller that may adopt it.
async fn status_probe() -> (StatusProbe, Option<RepinJobResponse>) {
    match api::repin_status().await {
        Ok(resp) => match resp.job {
            Some(job) => (StatusProbe::Job(ProbedJob::from(&job)), Some(job)),
            None => (StatusProbe::NoJob, None),
        },
        // The read said NOTHING. It must never be read as "no job".
        Err(_) => (StatusProbe::Failed, None),
    }
}

/// A real run whose RESPONSE was lost, or a re-probe of one. The status
/// route gets to say exactly one thing: whether it holds a job that is
/// PROVABLY this request's ([`recovery_verdict`]). It is adopted if so —
/// anything else, including a probe that failed outright, leaves the
/// dialog [`Phase::Indeterminate`], which offers no mutation at all.
fn probe_recovery(
    run: LostRun,
    plan: Option<Box<RepinJobResponse>>,
    lost: String,
    handles: Handles,
    on_close: Callback<Option<RepinJobResponse>>,
) {
    handles.probing.set(true);
    spawn_local(async move {
        let (probe, found) = status_probe().await;
        let verdict = recovery_verdict(&run, &probe);
        // Running or terminal, a proven job is the one this modal asked
        // for: hand it to the case file, which owns progress and
        // receipts. Before the liveness gate for the same reason a 202
        // is — the job is real whether or not the dialog survived.
        let adopt_up = matches!(verdict, Recovery::Adopt)
            && found
                .as_ref()
                .is_some_and(|job| job.status != STATUS_REFUSED);
        if adopt_up {
            on_close.try_run(found);
            return;
        }
        if !handles.is_alive() {
            return;
        }
        handles.probing.set(false);
        match verdict {
            // `Adopt` is only ever returned for a `Job` probe, so the row
            // is in hand here — and the non-refused half returned above.
            Recovery::Adopt => {
                if let Some(job) = found {
                    handles.force.set(false);
                    handles.phase.set(Phase::NeedsForce(Box::new(job)));
                }
            }
            Recovery::Indeterminate(why) => handles.phase.set(Phase::Indeterminate {
                lost,
                why,
                plan,
                run,
            }),
        }
    });
}

/// Read the install-wide one-running slot. `adopt_free` belongs to the
/// operator's own re-check, which returns the dialog to
/// [`Phase::NeedsPlan`] once nothing holds the slot; the annotation probe
/// fired right after a 409 passes `false` and only fills in which field
/// holds it.
fn probe_slot(adopt_free: bool, handles: Handles) {
    handles.probing.set(true);
    spawn_local(async move {
        let (probe, _) = status_probe().await;
        // Nothing this read can decide matters to a dialog that is gone,
        // and it mutates nothing server-side either way.
        if !handles.is_alive() {
            return;
        }
        handles.probing.set(false);
        match slot_check(&probe) {
            SlotCheck::Held(field) => {
                handles.slot_note.set(None);
                handles.busy_field.set(Some(sanitize_display_text(&field)));
            }
            SlotCheck::Free => {
                if adopt_free {
                    handles.busy_field.set(None);
                    handles.slot_note.set(None);
                    handles.force.set(false);
                    handles.phase.set(Phase::NeedsPlan);
                }
            }
            // Unknown is not free: the dialog stays where it is and says
            // the read failed.
            SlotCheck::Unknown => {
                if adopt_free {
                    handles.slot_note.set(Some(SLOT_CHECK_FAILED.to_string()));
                }
            }
        }
    });
}

/// The sentence shown for a failed request. The server's own message
/// where there is one, plus the one piece of context a bare 503 leaves
/// out.
fn failure_text(err: &ApiError) -> String {
    match err {
        ApiError::Server { status: 503, .. } | ApiError::Status(503) => format!(
            "{err} (a repin runs only on the node that owns the data root — a query-only node \
             cannot start one)"
        ),
        other => other.to_string(),
    }
}
