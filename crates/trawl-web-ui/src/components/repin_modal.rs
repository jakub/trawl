// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RepinModal/>` — the dry-run-first repin ladder (ADR-0011).

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::RepinJobResponse;
use trawl_core::sanitize::sanitize_display_text;

use crate::api::{self, ApiError, RepinOutcome};
use crate::repin_flow::{
    BoundCeilings, FollowedPlan, ForceStep, LostRun, PLAN_LOST, PLAN_UNREADABLE, PollAction,
    ProbedJob, REPIN_POLL_MS, Recovery, SlotCheck, StatusProbe, Unproven, default_target,
    followed_plan, force_step, indeterminate_text, is_pre_claim_failure, poll_decide,
    recovery_verdict, refusal_is_reusable, repin_targets, slot_check,
};
use crate::service_card_fmt::{format_bytes, format_exact};
use fleet_ui::{Btn, Icon, Modal, Segmented, SegmentedOption, Variant};

const STATUS_REFUSED: &str = "refused_needs_force";

const SLOT_CHECK_FAILED: &str = "Couldn't read the repin status just now, so whether the slot is \
                                 still held is unknown. Try again.";

#[derive(Debug, Clone)]
enum Phase {
    Planning,
    /// The shown plan no longer describes the selected target, so it is
    /// gone until the operator asks for a new one.
    NeedsPlan,
    Plan(Box<RepinJobResponse>),
    /// The real run came back `refused_needs_force`: the same plan,
    /// re-presented with the numbers the refusal was based on.
    NeedsForce(Box<RepinJobResponse>),
    Submitting,
    /// The one-running slot is held elsewhere. No confirm from here —
    /// only a re-read of the slot, which returns to [`Phase::NeedsPlan`]
    /// once it is free.
    Busy(String),
    /// A real run's response was lost and the recovery probe could not
    /// prove what became of it. The rewrite may be running right now, so
    /// this phase offers exactly one action: probe again.
    Indeterminate {
        lost: String,
        why: Unproven,
        plan: Option<Box<RepinJobResponse>>,
        /// What was asked for, so a re-probe can recognise it.
        run: LostRun,
    },
    /// The server's own message for a status this modal cannot act on.
    /// The modal stays open and keeps the plan it was acting on, if
    /// there was one — a 503 must not cost the operator the scan they
    /// have already paid for.
    ///
    /// Two things reach this: any dry-run failure (the scan mutates
    /// nothing, so retrying is free), and a real run refused with a
    /// definitive 4xx. The claim happens before the server answers, so a
    /// 4xx is decided before it — nothing started, and saying the
    /// outcome is unknown would be a lie the operator has to chase. A
    /// 5xx or a lost connection is [`Phase::Indeterminate`] instead.
    Failed {
        message: String,
        plan: Option<Box<RepinJobResponse>>,
    },
}

#[derive(Debug, Clone, Copy)]
struct Rung {
    dry_run: bool,
    force: bool,
    /// The ceilings this request states, `Some` on the forced execution
    /// alone — and then always the pair the operator has just read.
    ceilings: Option<BoundCeilings>,
}

impl Rung {
    fn plan() -> Self {
        Self {
            dry_run: true,
            force: false,
            ceilings: None,
        }
    }

    /// The forced scan: the only thing that resolves the ceilings a
    /// forced execution can restate. It writes nothing.
    fn forced_plan() -> Self {
        Self {
            dry_run: true,
            force: true,
            ceilings: None,
        }
    }

    /// The real run, accepting no loss. The server refuses it if the
    /// scan projects any.
    fn execute() -> Self {
        Self {
            dry_run: false,
            force: false,
            ceilings: None,
        }
    }

    /// The forced run, held to `ceilings` — the pair on screen. `None`
    /// is the case where a forced scan resolved none and there is
    /// nothing to restate.
    fn forced_execute(ceilings: Option<BoundCeilings>) -> Self {
        Self {
            dry_run: false,
            force: true,
            ceilings,
        }
    }
}

#[derive(Clone, Copy)]
struct Handles {
    phase: RwSignal<Phase>,
    probing: RwSignal<bool>,
    force: RwSignal<bool>,
    /// Whether a FORCED scan has answered in this dialog for the plan on
    /// screen. It is what opens the acceptance, and it survives nothing:
    /// a target change, a refusal, an adopted job all clear it. A forced
    /// report that resolved no ceilings has nothing more to give, so it
    /// also stops the dialog offering a rescan that would read the whole
    /// corpus for the same answer.
    previewed: RwSignal<bool>,
    busy_field: RwSignal<Option<String>>,
    slot_note: RwSignal<Option<String>>,
    /// The request generation: every initiating click bumps it. A probe
    /// carries the generation it was made for.
    generation: RwSignal<u32>,
    /// False once this dialog is disposed. Every signal above then
    /// panics on `get_untracked` (a write is a silent no-op), and any
    /// state a dead dialog computes is state nobody can see.
    alive: StoredValue<bool>,
}

impl Handles {
    fn is_alive(self) -> bool {
        self.alive.try_get_value() == Some(true)
    }

    fn leaves_for_sign_in(self, turn: u32, failure: Option<&ApiError>) -> bool {
        self.is_alive()
            && self.generation.try_get_untracked() == Some(turn)
            && failure.and_then(crate::auth_return::sign_in_if_unauthorized) == Some(true)
    }
}

struct Probed {
    probe: StatusProbe,
    job: Option<RepinJobResponse>,
    failure: Option<ApiError>,
}

/// The repin dialog. `on_close` carries `Some(job)` only for a real
/// 202 — the case file adopts that job, starts polling, and owns every
/// receipt from there on.
#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn RepinModal(
    field: String,
    current_type: String,
    suggested_to: String,
    /// A refusal to re-present instead of planning afresh: `Some` when the
    /// case file's poll watched a running job land `refused_needs_force`.
    /// That job's scan is the plan; a second one would be a second
    /// full-corpus pass for the same answer.
    refused: Option<RepinJobResponse>,
    on_close: Callback<Option<RepinJobResponse>>,
) -> impl IntoView {
    let shown_field = sanitize_display_text(&field);
    let targets = repin_targets(&current_type);
    // A refusal is the scan of ONE target, so a target this dialog does
    // not offer drops the plan here and the dialog starts from a fresh
    // scan.
    let refused = refused.filter(|job| refusal_is_reusable(&current_type, &job.to_type));
    let initial = refused.as_ref().map_or_else(
        || default_target(&current_type, &suggested_to).as_catalog(),
        |job| {
            // The refusal's own target, so the re-presented numbers and
            // the selected rung cannot disagree. Offered by the filter
            // above, so the lookup always finds it.
            targets
                .iter()
                .find(|c| c.as_catalog().eq_ignore_ascii_case(&job.to_type))
                .map_or_else(
                    || default_target(&current_type, &suggested_to).as_catalog(),
                    |c| c.as_catalog(),
                )
        },
    );

    let target = RwSignal::new(initial.to_string());
    let phase =
        RwSignal::new(refused.map_or(Phase::Planning, |job| Phase::NeedsForce(Box::new(job))));
    let force = RwSignal::new(false);
    // A refusal handed in was scanned unforced, exactly like one this
    // dialog provokes: no forced scan has run for it either way.
    let previewed = RwSignal::new(false);
    let busy = RwSignal::new(false);
    // A status probe is in flight — the recovery read after a lost
    // response, or the slot re-check. Tracked apart from `busy`: neither
    // probe mutates anything, but both own the primary action while they
    // run.
    let probing = RwSignal::new(false);
    let slot_note = RwSignal::new(None::<String>);
    // Every initiating click bumps this; a response whose generation is
    // stale (the operator changed target and re-planned meanwhile) is
    // dropped rather than painted over the current state.
    let generation = RwSignal::new(0_u32);
    // Which field holds the slot, when the status route was willing to
    // say. Advisory: the busy message stands on its own without it.
    let busy_field = RwSignal::new(None::<String>);

    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let handles = Handles {
        phase,
        probing,
        force,
        previewed,
        busy_field,
        slot_note,
        generation,
        alive,
    };

    let field_for_calls = field.clone();
    let submit = Callback::new(move |run: Rung| {
        let Rung {
            dry_run,
            force: forced,
            ceilings,
        } = run;
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
            let outcome = api::repin(&field, &to, dry_run, forced, ceilings).await;
            // A real run's 202 first, before any liveness or generation
            // gate: the job is claimed and running detached server-side,
            // so the one thing that must survive a dialog closed mid-claim
            // is the hand-up. `try_run` because the callback may be
            // disposed too. A dry run's 202 is a scan that outlasted the
            // server's answer bound (ADR-0054), so the dialog follows the
            // job on the status route by its id.
            let outcome = match outcome {
                Ok(RepinOutcome::Started(job)) if !job.dry_run => {
                    on_close.try_run(Some(job));
                    return;
                }
                Ok(RepinOutcome::Started(job)) => {
                    let Some(followed) = follow_plan(job, seq, handles).await else {
                        return;
                    };
                    match followed {
                        FollowedPlan::Plan(job) => Ok(RepinOutcome::DryRun(*job)),
                        FollowedPlan::Ended(message) => {
                            if generation.try_get_untracked() == Some(seq) {
                                busy.set(false);
                                phase.set(Phase::Failed {
                                    message,
                                    plan: carried,
                                });
                            }
                            return;
                        }
                    }
                }
                other => other,
            };
            if !handles.is_alive() {
                return;
            }
            if generation.try_get_untracked() != Some(seq) {
                return;
            }
            busy.set(false);
            match outcome {
                // 200 is the scan report and nothing else — the server
                // only answers it to a dry run.
                Ok(RepinOutcome::DryRun(job)) => {
                    // A FORCED scan is the preview: its report resolved
                    // the ceilings the execution restates, so it belongs
                    // back on the forced rung rather than presented as a
                    // plan that could be run unforced.
                    previewed.set(forced);
                    force.set(false);
                    phase.set(if forced {
                        Phase::NeedsForce(Box::new(job))
                    } else {
                        Phase::Plan(Box::new(job))
                    });
                }
                // A real run was handed to the case file above, before the
                // gates, and a dry run was followed to its report.
                Ok(RepinOutcome::Started(_)) => {}
                Ok(RepinOutcome::Refused(job)) => {
                    // Re-arm the acceptance: a refusal must be accepted
                    // for the plan actually shown, never carried over.
                    force.set(false);
                    // And a refusal is never itself a preview, however it
                    // was reached. An unforced run has no ceilings to
                    // show; a FORCED run refused at the cutover gate has
                    // the pair it just exceeded, which is the last thing
                    // to re-offer. Either way the next forced run buys a
                    // fresh scan first.
                    previewed.set(false);
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
                    // Past both checks above, so the answer is this
                    // dialog's own: a 401 sends the page to sign-in. A
                    // declined one is a pre-claim refusal below, shown as
                    // the session-ended sentence.
                    if crate::auth_return::sign_in_if_unauthorized(&e) == Some(true) {
                        return;
                    }
                    // A dry run mutates nothing, and a real run answered
                    // a definitive 4xx was decided before the claim. Both keep
                    // the plan and say what the server said.
                    if dry_run || is_pre_claim_failure(e.http_status()) {
                        phase.set(Phase::Failed {
                            message: failure_text(&e),
                            plan: carried,
                        });
                    } else {
                        // A real run that failed in transit may still
                        // have been claimed: the server detaches the job
                        // at the claim, so a dropped response is not a
                        // job that did not start. Probe once and adopt
                        // the job — but only one that can be proven to
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
        submit.run(Rung::plan());
    }

    let cancel = Callback::new(move |()| on_close.run(None));

    let primary = Callback::new(move |()| {
        if probing.get_untracked() {
            return;
        }
        match phase.get_untracked() {
            // A dry-run failure is retryable because it is a dry run:
            // the scan mutates nothing, so a second one at worst costs
            // another full-corpus pass.
            Phase::NeedsPlan | Phase::Failed { .. } => submit.run(Rung::plan()),
            Phase::Plan(_) => submit.run(Rung::execute()),
            Phase::NeedsForce(job) => match force_step(&job, previewed.get_untracked()) {
                // The ceilings are not known yet, so there is nothing to
                // accept: this click buys the numbers, and writes
                // nothing.
                ForceStep::NeedsPreview => submit.run(Rung::forced_plan()),
                ForceStep::Bound(bound) => {
                    if force.get_untracked() {
                        submit.run(Rung::forced_execute(Some(bound)));
                    }
                }
                ForceStep::Unbound => {
                    if force.get_untracked() {
                        submit.run(Rung::forced_execute(None));
                    }
                }
            },
            // The only action an unproven outcome offers. Re-probing is
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
            Phase::NeedsForce(job) => match force_step(&job, previewed.get()) {
                ForceStep::NeedsPreview => "Get forced plan",
                ForceStep::Bound(_) | ForceStep::Unbound => "Run forced repin",
            },
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
            // Asking for the numbers needs no acceptance; accepting them
            // does.
            Phase::NeedsForce(job) => match force_step(&job, previewed.get()) {
                ForceStep::NeedsPreview => false,
                ForceStep::Bound(_) | ForceStep::Unbound => !force.get(),
            },
            Phase::NeedsPlan
            | Phase::Plan(_)
            | Phase::Busy(_)
            | Phase::Indeterminate { .. }
            | Phase::Failed { .. } => false,
        }
    });
    let primary_variant = move || match phase.get() {
        // A scan is not a mutation, however red the phase around it.
        Phase::NeedsForce(job) => match force_step(&job, previewed.get()) {
            ForceStep::NeedsPreview => Variant::Primary,
            ForceStep::Bound(_) | ForceStep::Unbound => Variant::Danger,
        },
        _ => Variant::Primary,
    };

    let strip = targets
        .into_iter()
        .map(|c| SegmentedOption::new(c.as_catalog(), c.as_catalog()))
        .collect::<Vec<_>>();

    // The target is what every in-flight request was issued for, so it
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
                        // A probe changes its label without changing the phase.
                        // Keep this child reactive after Btn mounts.
                        {primary_label}
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
                        // by a generation bump, and an indeterminate
                        // run's target is the only record of what may be
                        // rewriting the corpus right now.
                        if target_locked.get_untracked() {
                            return;
                        }
                        target.set(id);
                        force.set(false);
                        // Ceilings are a function of the scan, and the
                        // scan was for another target.
                        previewed.set(false);
                        generation.update(|g| *g += 1);
                        busy.set(false);
                        phase.set(Phase::NeedsPlan);
                    })
                />
                <p class="rp-note">
                    "Current pin: "<span class="mono">{current_type.clone()}</span>
                    ". The pin it already has is not offered — re-extracting shelved values "
                    "under the "<strong>"same pin"</strong>" is a "
                    <span class="mono">"--force"</span>
                    " resurrection pass, and stays a CLI decision."
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
                Phase::NeedsForce(job) => {
                    let step = force_step(&job, previewed.get());
                    let projected = format_exact(job.projected_nulls);
                    view! {
                        {plan_block(&job, true)}
                        {match step {
                            // No forced scan has answered here yet, and
                            // only a forced scan settles the numbers.
                            // Nothing to accept, so no acceptance is
                            // offered.
                            ForceStep::NeedsPreview => view! {
                                <p class="rp-note">
                                    "A forced repin is held to a limit per dimension — rows the \
                                     new pin cannot read, and numerals that mean different \
                                     severities in each dialect. The server resolves those \
                                     numbers from a forced scan, so \"Get forced plan\" reads \
                                     the corpus once more and shows exactly what a forced run \
                                     would be allowed to do. It writes nothing."
                                </p>
                            }.into_any(),
                            ForceStep::Bound(bound) => view! {
                                <div class="rp-force">
                                    <label>
                                        <input
                                            type="checkbox"
                                            prop:checked=move || force.get()
                                            on:change=move |e| {
                                                force.set(checked_from_event(&e));
                                            }
                                        />
                                        // The numbers accepted here are
                                        // the ones the request sends, so
                                        // the bound the server enforces
                                        // is the bound on screen.
                                        " I accept up to "
                                        {format_exact(bound.max_nulled)}
                                        " row(s) the new pin cannot keep becoming NULL, and up to "
                                        {format_exact(bound.max_ambiguous)}
                                        " dialect-ambiguous numeral(s)"
                                    </label>
                                    <p class="rp-note">
                                        "The last scan projected "{projected}
                                        " unreadable value(s); the limits above are that scan \
                                         plus headroom, because ingest keeps running while the \
                                         rewrite does. The repin refuses at the cutover, with \
                                         the corpus untouched, if the finished rewrite is worse \
                                         than what you accept here."
                                    </p>
                                    <p class="rp-note">
                                        "A forced repin writes NULL wherever the new pin cannot \
                                         keep the stored value. The originals stay findable in "
                                        <span class="mono">"_raw"</span>
                                        ", and a later repin back can resurrect them from there \
                                         — but the typed column will not carry them until it \
                                         does."
                                    </p>
                                </div>
                            }.into_any(),
                            // A forced scan that resolved no ceilings.
                            // Rescanning would say the same thing, so the
                            // run is offered without a number to restate.
                            ForceStep::Unbound => view! {
                                <div class="rp-force">
                                    <label>
                                        <input
                                            type="checkbox"
                                            prop:checked=move || force.get()
                                            on:change=move |e| {
                                                force.set(checked_from_event(&e));
                                            }
                                        />
                                        " I accept that the values the new pin cannot keep — "
                                        {projected}
                                        " at the last scan — become NULL"
                                    </label>
                                    <p class="rp-note">
                                        "The server reported no resolved limits for this job, so \
                                         the run is held to the ones its own scan derives rather \
                                         than to a number shown here."
                                    </p>
                                </div>
                            }.into_any(),
                        }}
                    }.into_any()
                },
                Phase::Submitting => view! {
                    <p class="rp-note">"Claiming the repin slot…"</p>
                }.into_any(),
                Phase::Busy(msg) => view! {
                    <div class="rp-err" role="alert">
                        <p>"Another repin is already running — only one repin runs at a time, \
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
                    // corpus unless the retry is the scan.
                    {plan.map(|plan| plan_block(&plan, true))}
                }.into_any(),
            }}
        </Modal>
    }
}

fn checked_from_event(e: &leptos::ev::Event) -> bool {
    use wasm_bindgen::JsCast;
    e.target()
        .and_then(|t| t.dyn_into::<leptos::web_sys::HtmlInputElement>().ok())
        .is_some_and(|el| el.checked())
}

fn plan_block(job: &RepinJobResponse, refused: bool) -> AnyView {
    // Exact digits, not the compact `format_count` the tables use: these
    // are numbers an operator accepts responsibility for, and "1.2k
    // values become NULL" is not a fact anyone can act on. Bytes stay
    // compact — nobody acts on the last byte.
    let files_total = job.files_total;
    let files_label = format_exact(job.files_total);
    let rows_carrying = format_exact(job.rows_carrying);
    let projected_nulls = format_exact(job.projected_nulls);
    let resurrectable = format_exact(job.resurrectable);
    let bytes = format_bytes(job.affected_bytes);
    let lossy = job.projected_nulls > 0;
    view! {
        <div class="rp-plan">
            <div class="rp-kv"><span>"Files affected"</span><span>{files_label}</span></div>
            <div class="rp-kv"><span>"Rows carrying the field"</span><span>{rows_carrying}</span></div>
            <div class="rp-kv"><span>"Values the new pin cannot keep"</span><span>{projected_nulls}</span></div>
            <div class="rp-kv">
                <span>"Values that would come back from "<span class="mono">"_raw"</span></span>
                <span>{resurrectable}</span>
            </div>
            <div class="rp-kv"><span>"Bytes to rewrite"</span><span>{bytes}</span></div>

            {(files_total == 0).then(|| view! {
                <p class="rp-note">
                    "No retained files currently carry this field, so the rewrite has nothing \
                     to do — but the pin still flips, and every event ingested from then on is \
                     typed by it."
                </p>
            })}

            <p class="rp-note">
                "A repin rewrites this field across the "<strong>"entire corpus"</strong>
                " — every service, every environment, every day. Retention stands down for the \
                 job's whole life, and the affected bytes are held twice until it sweeps."
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
            // The server's own words for why force is required. The numbers
            // above cannot say it on their own: a plan can be refused for
            // dialect ambiguity with zero projected nulls.
            {job.requires_force_reason.clone().map(|reason| view! {
                <p class="rp-note">{reason}</p>
            })}
        </div>
    }
    .into_any()
}

impl Probed {
    fn job(job: Option<RepinJobResponse>) -> Self {
        Self {
            probe: job.as_ref().map_or(StatusProbe::NoJob, |job| {
                StatusProbe::Job(ProbedJob::from(job))
            }),
            job,
            failure: None,
        }
    }

    /// The read said nothing. It must never be read as "no job".
    fn failed(failure: ApiError) -> Self {
        Self {
            probe: StatusProbe::Failed,
            job: None,
            failure: Some(failure),
        }
    }
}

async fn status_probe() -> Probed {
    match api::repin_status().await {
        Ok(resp) => Probed::job(resp.job),
        Err(e) => Probed::failed(e),
    }
}

async fn job_probe(id: i64) -> Probed {
    match api::repin_job_status(id).await {
        Ok(job) => Probed::job(job),
        Err(e) => Probed::failed(e),
    }
}

/// Follow the dry run a 202 answered with, under the case file's own
/// [`poll_decide`] rules. By id, so another job in the one-running slot
/// can never stand in for this one. `None` once the dialog is gone or a
/// read sent the page to sign-in.
async fn follow_plan(
    started: RepinJobResponse,
    seq: u32,
    handles: Handles,
) -> Option<FollowedPlan> {
    let job_id = started.id;
    let mut probe = StatusProbe::Job(ProbedJob::from(&started));
    let mut found = Some(started);
    let mut errors = 0_u32;
    loop {
        match poll_decide(job_id, errors, &probe) {
            PollAction::Track | PollAction::Retry => {}
            PollAction::Settle => {
                return Some(
                    found.map_or_else(|| FollowedPlan::Ended(PLAN_LOST.to_string()), followed_plan),
                );
            }
            PollAction::Lost => return Some(FollowedPlan::Ended(PLAN_LOST.to_string())),
            PollAction::GiveUp => return Some(FollowedPlan::Ended(PLAN_UNREADABLE.to_string())),
        }
        gloo_timers::future::TimeoutFuture::new(REPIN_POLL_MS).await;
        if !handles.is_alive() {
            return None;
        }
        let read = job_probe(job_id).await;
        if !handles.is_alive() || handles.leaves_for_sign_in(seq, read.failure.as_ref()) {
            return None;
        }
        (probe, found) = (read.probe, read.job);
        errors = if matches!(probe, StatusProbe::Failed) {
            errors + 1
        } else {
            0
        };
    }
}

fn probe_recovery(
    run: LostRun,
    plan: Option<Box<RepinJobResponse>>,
    lost: String,
    handles: Handles,
    on_close: Callback<Option<RepinJobResponse>>,
) {
    handles.probing.set(true);
    let turn = handles.generation.get_untracked();
    spawn_local(async move {
        let Probed {
            probe,
            job: found,
            failure,
        } = status_probe().await;
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
        if handles.leaves_for_sign_in(turn, failure.as_ref()) {
            return;
        }
        match verdict {
            // `Adopt` is only ever returned for a `Job` probe, so the row
            // is in hand here — and the non-refused half returned above.
            Recovery::Adopt => {
                if let Some(job) = found {
                    handles.force.set(false);
                    // No forced scan has answered in this dialog for the
                    // adopted row, whatever ceilings it carries, so the
                    // forced rung starts at the preview.
                    handles.previewed.set(false);
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
    let turn = handles.generation.get_untracked();
    spawn_local(async move {
        let read = status_probe().await;
        if !handles.is_alive() {
            return;
        }
        handles.probing.set(false);
        if handles.leaves_for_sign_in(turn, read.failure.as_ref()) {
            return;
        }
        match slot_check(&read.probe) {
            SlotCheck::Held(field) => {
                handles.slot_note.set(None);
                handles.busy_field.set(Some(sanitize_display_text(&field)));
            }
            SlotCheck::Free => {
                if adopt_free {
                    handles.busy_field.set(None);
                    handles.slot_note.set(None);
                    handles.force.set(false);
                    handles.previewed.set(false);
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

fn failure_text(err: &ApiError) -> String {
    if err.http_status() == Some(503) {
        format!(
            "{err} (a repin runs only on the node that owns the data root — a query-only node \
             cannot start one)"
        )
    } else {
        err.to_string()
    }
}
