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
//!
//! Closing the modal does not cancel anything: past the claim the job
//! runs DETACHED server-side, so a dismissed dialog abandons the
//! RESPONSE, never the work.

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::RepinJobResponse;
use trawl_core::sanitize::sanitize_display_text;

use crate::api::{self, ApiError, RepinOutcome};
use crate::repin_flow::{default_target, repin_targets};
use crate::repin_hint::repin_is_running;
use crate::service_card_fmt::format_bytes;
use fleet_ui::{Btn, Icon, Modal, Segmented, SegmentedOption, Variant};

/// Which step a [`Phase::Failed`] offers to retry — the one that failed,
/// never a step further down the ladder.
#[derive(Debug, Clone, Copy)]
enum RetryStep {
    /// Re-run the dry run.
    Plan,
    /// Re-issue the real run, carrying the force flag it was sent with.
    Run(bool),
}

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
    /// The one-running slot is held elsewhere. No confirm from here.
    Busy(String),
    /// The server's own message for a status this modal cannot act on.
    /// The modal STAYS OPEN and keeps the plan it was acting on, if
    /// there was one — a 503 must not cost the operator the scan they
    /// have already paid for.
    Failed {
        message: String,
        retry: RetryStep,
        plan: Option<Box<RepinJobResponse>>,
    },
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
    // Every initiating click bumps this; a response whose generation is
    // stale (the operator changed target and re-planned meanwhile) is
    // dropped rather than painted over the current state.
    let generation = RwSignal::new(0_u32);
    // Which field holds the slot, when the status route was willing to
    // say. Advisory: the busy message stands on its own without it.
    let busy_field = RwSignal::new(None::<String>);

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
            if generation.get_untracked() != seq {
                // A newer request owns the dialog now.
                return;
            }
            busy.set(false);
            match outcome {
                // 200 is the scan report and nothing else — the server
                // only answers it to a dry run.
                Ok(RepinOutcome::DryRun(job)) => phase.set(Phase::Plan(Box::new(job))),
                // 202 is the only outcome the case file adopts.
                // `try_run` because the operator may have closed the
                // dialog while the claim was in flight — running a
                // disposed callback panics.
                Ok(RepinOutcome::Started(job)) => {
                    on_close.try_run(Some(job));
                }
                Ok(RepinOutcome::Refused(job)) => {
                    // Re-arm the acceptance: a refusal must be accepted
                    // for the plan actually shown, never carried over.
                    force.set(false);
                    phase.set(Phase::NeedsForce(Box::new(job)));
                }
                Ok(RepinOutcome::Busy(msg)) => {
                    phase.set(Phase::Busy(msg));
                    // Which field holds it, if the status route says.
                    // Best-effort: a failed probe leaves the message as
                    // it stands.
                    spawn_local(async move {
                        if let Ok(status) = api::repin_status().await
                            && let Some(job) = status.job
                            && repin_is_running(&job.status)
                        {
                            busy_field.set(Some(sanitize_display_text(&job.field)));
                        }
                    });
                }
                Err(e) => {
                    if dry_run {
                        phase.set(Phase::Failed {
                            message: failure_text(&e),
                            retry: RetryStep::Plan,
                            plan: carried,
                        });
                    } else {
                        // A real run that failed IN TRANSIT may still
                        // have been claimed: the server detaches the job
                        // at the claim, so a dropped response is not a
                        // job that did not start. Probe once and adopt a
                        // job that matches this field and target before
                        // offering a retry that would 409 — or worse,
                        // start a second rewrite.
                        recover_from_lost_response(
                            &field, forced, carried, e, phase, force, on_close,
                        );
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

    // The primary action depends on the rung: plan, run, forced run —
    // or nothing at all when the slot is held elsewhere.
    let primary = Callback::new(move |()| match phase.get_untracked() {
        Phase::NeedsPlan => submit.run((true, false)),
        Phase::Plan(_) => submit.run((false, false)),
        Phase::NeedsForce(_) => {
            if force.get_untracked() {
                submit.run((false, true));
            }
        }
        Phase::Failed { retry, .. } => match retry {
            RetryStep::Plan => submit.run((true, false)),
            RetryStep::Run(forced) => submit.run((false, forced)),
        },
        Phase::Planning | Phase::Submitting | Phase::Busy(_) => {}
    });

    let primary_label = move || match phase.get() {
        Phase::Planning => "Planning…",
        Phase::NeedsPlan => "Get plan",
        Phase::Plan(_) => "Run repin",
        Phase::NeedsForce(_) => "Run forced repin",
        Phase::Submitting => "Starting…",
        Phase::Busy(_) => "Unavailable",
        Phase::Failed { retry, .. } => match retry {
            RetryStep::Plan => "Retry plan",
            RetryStep::Run(_) => "Retry run",
        },
    };
    let primary_disabled = Signal::derive(move || match phase.get() {
        Phase::Planning | Phase::Submitting | Phase::Busy(_) => true,
        Phase::NeedsForce(_) => !force.get(),
        Phase::NeedsPlan | Phase::Plan(_) | Phase::Failed { .. } => false,
    });
    let primary_variant = move || match phase.get() {
        Phase::NeedsForce(_) => Variant::Danger,
        _ => Variant::Primary,
    };

    let strip = targets
        .into_iter()
        .map(|c| SegmentedOption::new(c.as_duckdb(), c.as_duckdb()))
        .collect::<Vec<_>>();

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
                        // A real run is in flight: its 202 is the only
                        // thing that can hand the job to the case file,
                        // so it must not be dropped by a generation bump.
                        // The scan of a DRY run is safely abandonable —
                        // it terminalizes server-side either way.
                        if matches!(phase.get_untracked(), Phase::Submitting) {
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
                    </div>
                }.into_any(),
                Phase::Failed { message, plan, .. } => view! {
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

/// A real run whose RESPONSE was lost. Probe the status route once: a
/// job matching this field and target is OURS — adopt it (or its
/// refusal) rather than offering a retry that would either 409 against
/// our own job or start a second rewrite.
fn recover_from_lost_response(
    field: &str,
    forced: bool,
    plan: Option<Box<RepinJobResponse>>,
    err: ApiError,
    phase: RwSignal<Phase>,
    force: RwSignal<bool>,
    on_close: Callback<Option<RepinJobResponse>>,
) {
    let field = field.to_owned();
    spawn_local(async move {
        if let Ok(status) = api::repin_status().await
            && let Some(job) = status.job
            && !job.dry_run
            && job.field.eq_ignore_ascii_case(&field)
        {
            if job.status == "refused_needs_force" {
                force.set(false);
                phase.set(Phase::NeedsForce(Box::new(job)));
            } else {
                // Running or terminal, it is the job this modal asked
                // for: hand it to the case file, which owns progress and
                // receipts.
                on_close.try_run(Some(job));
            }
            return;
        }
        phase.set(Phase::Failed {
            message: format!(
                "{} — the status route shows no repin for this field, so it most likely never \
                 started.",
                failure_text(&err)
            ),
            retry: RetryStep::Run(forced),
            plan,
        });
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
