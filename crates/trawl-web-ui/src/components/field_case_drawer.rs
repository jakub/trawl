// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FieldCaseDrawer/>` — the field case file (ADR-0011).
//!
//! A peer of [`ServiceDrawer`](super::service_drawer::ServiceDrawer),
//! never a child: `fleet_ui::Drawer` does not nest (its Escape
//! arbitration assumes one drawer layer), so the Schema page swaps
//! between the two and this one mounts from `?field=` alone — which is
//! what makes `/search/schema?field=<name>` work as a deep link with no
//! service context and no services snapshot loaded.
//!
//! Everything rendered here is server-decided. The pin, the per-service
//! observations, the conflict evidence and the analyzer's verdict all
//! come from one `GET /api/v1/schema/field?name=` response; nothing is
//! re-derived client-side, and a field with no verdict gets no repin
//! affordance and no command hint at all.
//!
//! Field names, service names and conflict samples are client-chosen
//! text. They render in leptos text positions only — never `inner_html`,
//! never string-built markup — and each display copy goes through
//! `trawl_core::sanitize::sanitize_display_text` first, so a bidi
//! override in a sample cannot reorder the line beneath it.
//!
//! The case file also owns the repin job: the trigger (behind
//! `can_repin`, an affordance only — the server is the sole
//! enforcement), the progress poll, and the completion receipt. The poll
//! lives here and nowhere else — an `Interval` in a drawer-owned
//! `StoredValue` that `on_cleanup` drops, so closing the drawer, going
//! back, or navigating to another field stops it. There is no global
//! poller, and a job started somewhere else while this drawer was closed
//! raises no toast: the mount-time status read shows it as a receipt
//! instead.

use gloo_timers::callback::Interval;
use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::{CatalogConflictRow, CatalogFieldResponse, DegradedVerdict, RepinJobResponse};
use trawl_core::sanitize::sanitize_display_text;

use crate::api;
use crate::api::ApiError;
use crate::components::repin_modal::RepinModal;
use crate::repin_flow::{
    PollAction, ProbedJob, REPIN_POLL_MS, StatusProbe, accepted_ceilings, claim_toast, poll_decide,
    refusal_is_reusable,
};
use crate::repin_hint::{REPIN_HINT_REFUSED, hint_segments, repin_command_hint, repin_is_running};
use crate::service_card_fmt::format_exact;
use fleet_ui::{
    Badge, Btn, Drawer, Icon, IconView, LoadMore, LoadState, Loaded, ToastBus, ToastKind, Tone,
    Variant,
};

/// The wire status a refusal carries. Matched as a literal where the SPA
/// has to act on one specific status rather than merely render it; the
/// vocabulary itself is `trawl-server/src/store/repin.rs`.
const STATUS_REFUSED: &str = "refused_needs_force";

/// Ditto, for the one status that licenses a case-file refetch.
const STATUS_SUCCEEDED: &str = "succeeded";

/// Shown once — not once per tick — while status reads are failing but
/// the retry budget still has room.
const POLL_RETRY_WARNING: &str = "Couldn't read the repin status just now; still trying.";

/// The operator's own re-check answered, and had nothing this page can
/// follow. Said out loud because a button that appears to do nothing is
/// indistinguishable from a broken one.
const STATUS_RECHECK_NONE: &str =
    "Checked again: the status route has no job this page can follow.";

/// The re-check itself failed, which is not the same as an empty answer.
const STATUS_RECHECK_FAILED: &str = "Couldn't read the repin status just now.";

/// What the repin modal was opened with.
#[derive(Clone)]
struct ModalReq {
    /// The pin as it stands — the rung the selector must not offer.
    current_type: String,
    /// The rung the selector starts on.
    suggested_to: String,
    /// A finished `refused_needs_force` job whose plan the modal
    /// re-presents instead of scanning again.
    refused: Option<RepinJobResponse>,
}

/// Service observations per page. The service axis is client-chosen and
/// never pruned, so the detail route pages it; this is the server's own
/// default, spelled out because the cursor round-trip depends on it.
const SERVICES_PAGE: usize = 100;

/// The field case file. `back` carries the service the drawer was
/// reached through (the `?svc=` return context) — absent on a bare deep
/// link, in which case there is no back affordance and Escape closes.
#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn FieldCaseDrawer(
    field: String,
    back: Option<String>,
    /// Whether to offer the repin trigger. Advisory: the server gates
    /// `POST /schema/repin` on `schema_write` and is the only thing that
    /// does. False (including before `/me` resolves) renders the
    /// read-only footer with the CLI line instead.
    #[prop(into)]
    can_repin: Signal<bool>,
    /// Drill into another field's case file — the page owns routing, so
    /// the "a repin is running on X" line hands the name back rather
    /// than navigating itself.
    on_open_field: Callback<String>,
    on_back: Callback<()>,
    on_close: Callback<()>,
) -> impl IntoView {
    // Page one. `reload` re-runs the fetcher, which is how the inline
    // retry recovers from a 503 or a dropped connection.
    let reload = RwSignal::new(0_u32);
    let field_for_page_one = field.clone();
    let detail = LocalResource::new(move || {
        let field = field_for_page_one.clone();
        let _ = reload.get();
        async move { api::catalog_field(&field, None, SERVICES_PAGE).await }
    });

    // Paged "services carrying this field". Seeded from page one, so a
    // retry or a fresh field starts the list over rather than appending
    // to a previous field's rows.
    let services = RwSignal::new(Vec::<trawl_api::CatalogFieldServiceRow>::new());
    let cursor = RwSignal::new(None::<String>);
    let more_busy = RwSignal::new(false);
    let more_error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        if let Some(Ok(resp)) = detail.get() {
            services.set(resp.services.clone());
            cursor.set(resp.services_cursor.clone());
            more_error.set(None);
        }
    });

    let field_for_more = field.clone();
    let on_load_more = Callback::new(move |()| {
        let Some(after) = cursor.get_untracked() else {
            return;
        };
        if more_busy.get_untracked() {
            return;
        }
        more_busy.set(true);
        more_error.set(None);
        let field = field_for_more.clone();
        spawn_local(async move {
            match api::catalog_field(&field, Some(&after), SERVICES_PAGE).await {
                Ok(resp) => {
                    let next = resp.services_cursor;
                    services.update(|rows| {
                        for row in resp.services {
                            // Exact-name dedupe: a page boundary can
                            // repeat a row, and page one's facts win.
                            if !rows.iter().any(|r| r.service == row.service) {
                                rows.push(row);
                            }
                        }
                    });
                    cursor.set(next);
                }
                // The rows already on screen stay; the failure is
                // reported inline and the button is the retry.
                Err(e) => more_error.set(Some(e.to_string())),
            }
            more_busy.set(false);
        });
    });

    // -- repin job state ---------------------------------------------
    //
    // Canonical: the job row as the server returned it. Nothing here
    // synthesizes a status or edits one — `job` is only ever replaced
    // wholesale by a status read or by the 202 the modal hands up.
    let bus = expect_context::<ToastBus>();
    let job = RwSignal::new(None::<RepinJobResponse>);
    // The id polling follows. Set on a 202 and on adopting a running
    // job at mount; cleared the moment the job settles or the
    // install-wide slot moves on.
    let tracked_job_id = RwSignal::new(None::<i64>);
    // Whether this surface started the job it holds — a 202 from its own
    // modal — as against merely adopting one the status route reported.
    // Only the operator who is mid-flow gets the force dialog reopened
    // under them; an adopted refusal is a receipt with a button.
    let job_initiated = RwSignal::new(false);
    let poll_errors = RwSignal::new(0_u32);
    let poll_in_flight = RwSignal::new(false);
    let poll_warning = RwSignal::new(None::<String>);
    let status_lost = RwSignal::new(false);
    // A one-shot status read is in flight (mount, or "Check again").
    let status_probing = RwSignal::new(false);
    // What the operator's own re-check found, when it found nothing to
    // follow. Empty for the silent mount read.
    let recheck_note = RwSignal::new(None::<String>);
    let other_running = RwSignal::new(None::<String>);
    let modal = RwSignal::new(None::<ModalReq>);

    // The one interval in this crate. Dropping the StoredValue's
    // contents cancels it; `on_cleanup` runs on close, back, Escape and
    // on navigating to a different field (the page rebuilds the drawer),
    // so no poll outlives the surface that started it.
    let poller: StoredValue<Option<Interval>, LocalStorage> = StoredValue::new_local(None);
    on_cleanup(move || poller.update_value(|p| *p = None));

    // Cancelling the interval cannot cancel a read already awaiting: a
    // status response can land after this drawer is gone. Signal writes
    // are silently dropped once the owner is disposed, but the ToastBus
    // is not this drawer's — it outlives it by design, and a toast from a
    // dead surface is a notification about a page nobody is looking at.
    // A disposed `StoredValue` reads as `None`, which is the same answer
    // as the flag itself.
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let is_alive = move || alive.try_get_value() == Some(true);
    let stop_poll = move || {
        poller.update_value(|p| *p = None);
        poll_in_flight.set(false);
    };

    // A job reaching a status that will not change. The toast fires only
    // here, and only once per id — a completion that happened while this
    // drawer was closed is a receipt, never a notification.
    let settle = move |finished: RepinJobResponse| {
        // The whole settle path, not just the toast: a refetch and a
        // re-presented refusal are as meaningless as a toast once the
        // surface they belong to is gone.
        if !is_alive() {
            return;
        }
        let id = finished.id;
        let status = finished.status.clone();
        let dry_run = finished.dry_run;
        let detail = job_outcome_line(&finished);
        job.set(Some(finished.clone()));
        // Deduped across drawer instances (`repin_flow::claim_toast`):
        // navigating A → B → A builds a third instance, and per-instance
        // bookkeeping would announce one job twice.
        if claim_toast(id) {
            match (status.as_str(), dry_run) {
                (STATUS_SUCCEEDED, true) => {
                    bus.push(ToastKind::Success, "Repin plan ready", Some(detail));
                }
                (STATUS_SUCCEEDED, false) => {
                    bus.push(ToastKind::Success, "Repin finished", Some(detail));
                }
                _ => bus.push(ToastKind::Error, "Repin did not complete", Some(detail)),
            }
        }
        if status == STATUS_SUCCEEDED && !dry_run {
            // The pin changed under the case file: refetch page one. A
            // dry run changed nothing, so it earns no refetch.
            reload.update(|n| *n += 1);
        }
        // Only for a job this surface started: the operator is mid-flow,
        // and the refusal is the next rung of the ladder they are on. A
        // job merely adopted from the install-wide status route belongs
        // to whoever started it — opening a dialog pre-armed to null
        // values under a reader who asked for none is not their decision
        // to be handed. That case renders as a receipt with an explicit
        // "review" button instead (`job_block`).
        // …and only for a target the modal offers. A refusal for any
        // other one carries a plan the dialog cannot re-present, so
        // opening it would buy a fresh full-corpus scan for a target
        // nobody asked about.
        if status == STATUS_REFUSED
            && job_initiated.try_get_untracked() == Some(true)
            && refusal_is_reusable(&finished.from_type, &finished.to_type)
        {
            // The refusal is a plan, scanned by the run that just
            // stopped. Re-present it rather than asking for another
            // full-corpus pass.
            modal.set(Some(ModalReq {
                current_type: finished.from_type.clone(),
                suggested_to: finished.to_type.clone(),
                refused: Some(finished),
            }));
        }
    };

    // One status read. Every branch is `repin_flow::poll_decide`'s —
    // this closure only carries out the decision.
    let poll_once = Callback::new(move |()| {
        let Some(tracked) = tracked_job_id.get_untracked() else {
            stop_poll();
            return;
        };
        // In-flight guard: a slow read must never stack a second one
        // behind it, or a struggling store gets a queue instead of a tick.
        if poll_in_flight.get_untracked() {
            return;
        }
        poll_in_flight.set(true);
        spawn_local(async move {
            let result = api::repin_status().await;
            // First, before any read: the interval cannot cancel a read
            // already awaiting, and every branch below reads this
            // drawer's own signals — which panic once disposed, where a
            // write is a silent no-op.
            if !is_alive() {
                return;
            }
            poll_in_flight.set(false);
            let (probe, row) = match result {
                Ok(resp) => match resp.job {
                    Some(j) => (StatusProbe::Job(ProbedJob::from(&j)), Some(j)),
                    None => (StatusProbe::NoJob, None),
                },
                Err(_) => (StatusProbe::Failed, None),
            };
            let errors = if matches!(probe, StatusProbe::Failed) {
                poll_errors.try_get_untracked().unwrap_or(0) + 1
            } else {
                0
            };
            poll_errors.set(errors);
            match poll_decide(tracked, errors, &probe) {
                PollAction::Track => {
                    poll_warning.set(None);
                    job.set(row);
                }
                PollAction::Settle => {
                    poll_warning.set(None);
                    stop_poll();
                    tracked_job_id.set(None);
                    if let Some(finished) = row {
                        settle(finished);
                    }
                }
                PollAction::Lost | PollAction::GiveUp => {
                    stop_poll();
                    tracked_job_id.set(None);
                    poll_warning.set(None);
                    status_lost.set(true);
                }
                PollAction::Retry => poll_warning.set(Some(POLL_RETRY_WARNING.to_string())),
            }
        });
    });

    // Immediate first read, then the interval: a job that is already
    // finished shows its receipt without waiting out a tick.
    let start_poll = Callback::new(move |id: i64| {
        tracked_job_id.set(Some(id));
        poll_errors.set(0);
        poll_warning.set(None);
        status_lost.set(false);
        poll_once.run(());
        poller.update_value(|p| {
            *p = Some(Interval::new(REPIN_POLL_MS, move || {
                poll_once.try_run(());
            }));
        });
    });

    // One status read. The route is install-wide, so what comes back is
    // either this field's job (adopt it — poll if it is still running,
    // show the receipt if it is not), or another field's (say so, and do
    // not poll for it), or nothing.
    //
    // Run at mount, and again from the "Check again" button on the
    // status-lost alert. `announce` is that button's: an operator who
    // asked has to be told when the answer was "still nothing", where
    // the mount read stays silent (the case file is the point; the
    // receipt is a bonus, and a surface that loaded fine must not be
    // banner-ed by it).
    let field_for_status = field.clone();
    let probe_status = Callback::new(move |announce: bool| {
        if status_probing.get_untracked() {
            return;
        }
        status_probing.set(true);
        recheck_note.set(None);
        let field = field_for_status.clone();
        spawn_local(async move {
            let result = api::repin_status().await;
            if !is_alive() {
                return;
            }
            status_probing.set(false);
            let Ok(status) = result else {
                if announce {
                    recheck_note.set(Some(STATUS_RECHECK_FAILED.to_string()));
                }
                return;
            };
            let Some(found) = status.job else {
                if announce {
                    recheck_note.set(Some(STATUS_RECHECK_NONE.to_string()));
                }
                return;
            };
            // Catalog keys are ASCII-folded at ingest, but the `?field=`
            // spelling in the URL is whatever the operator typed.
            if found.field.eq_ignore_ascii_case(&field) {
                let running = repin_is_running(&found.status);
                let id = found.id;
                // Adopted, not initiated: whatever this job turns out to
                // be, this surface did not start it.
                job_initiated.set(false);
                job.set(Some(found));
                status_lost.set(false);
                if running {
                    // `try_run`: the drawer may already be gone — closed
                    // inside this read's round trip — and running a
                    // disposed callback panics where a disposed signal
                    // write is a silent no-op.
                    start_poll.try_run(id);
                }
            } else if repin_is_running(&found.status) {
                other_running.set(Some(found.field));
            } else if announce {
                recheck_note.set(Some(STATUS_RECHECK_NONE.to_string()));
            }
        });
    });
    probe_status.run(false);

    let field_for_modal = field.clone();
    let open_repin = Callback::new(move |(current_type, suggested_to): (String, String)| {
        modal.set(Some(ModalReq {
            current_type,
            suggested_to,
            refused: None,
        }));
    });
    // `Some(job)` only ever arrives from a real 202 (or from the modal's
    // own recovery probe proving that job is the one it asked for), so
    // adopting it here is the one place polling starts from an operator
    // action.
    let on_modal_close = Callback::new(move |started: Option<RepinJobResponse>| {
        modal.set(None);
        if let Some(started) = started {
            // Started from here: the operator is mid-ladder, so a
            // refusal may reopen the dialog on them.
            job_initiated.set(true);
            status_lost.set(false);
            recheck_note.set(None);
            if repin_is_running(&started.status) {
                let id = started.id;
                job.set(Some(started));
                start_poll.run(id);
            } else {
                // Already terminal when it was handed up: the recovery
                // probe found a run that finished inside the round trip
                // whose response was lost. Nothing will poll it, so this
                // is the only chance to settle it — without which a
                // succeeded repin would sit as a receipt beside a case
                // file still showing the old pin, and raise no toast.
                settle(started);
            }
        }
    });

    // The adopted-refusal path back into the ladder: the operator asks
    // for the force dialog rather than being handed it. The refusal's
    // own job row is the plan, so this costs no second corpus scan.
    let review_refused = Callback::new(move |()| {
        let Some(refused) = job.get_untracked() else {
            return;
        };
        modal.set(Some(ModalReq {
            current_type: refused.from_type.clone(),
            suggested_to: refused.to_type.clone(),
            refused: Some(refused),
        }));
    });

    let shown_field = sanitize_display_text(&field);
    let title_field = shown_field.clone();
    let missing_field = shown_field.clone();
    let back_for_title = back.clone();
    let back_for_copy = back.clone();
    // Escape is back-or-close: from a service drawer it returns there,
    // from a deep link it closes the page's only drawer.
    let has_back = back.is_some();
    let escape = Callback::new(move |()| {
        if has_back {
            on_back.run(());
        } else {
            on_close.run(());
        }
    });

    view! {
        // Mounted as a sibling of the drawer, not inside it: the drawer
        // sits in its own stacking context (z-index 51) and the modal
        // scrim has to cover the whole viewport from the page's.
        // The `can_repin` guard is belt and braces: every path that sets
        // `modal` is already behind it (the Remedy button, and a refusal
        // for a job this surface started — which needed the permission
        // to start). A mount-time adoption must never be able to raise
        // this dialog for a read-only session, so the render site
        // re-asks rather than trusting that inventory to stay complete.
        {move || modal.get().filter(|_| can_repin.get()).map(|req| view! {
            <RepinModal
                field=field_for_modal.clone()
                current_type=req.current_type
                suggested_to=req.suggested_to
                refused=req.refused
                on_close=on_modal_close
            />
        })}
        <Drawer
            // No tabs: the case file is one surface. The strip renders
            // as the meta bar below the header.
            tabs=vec![]
            active_tab=Signal::derive(String::new)
            on_tab_change=Callback::new(|_: String| {})
            on_close=on_close
            on_escape=escape
            close_size=12
            meta="Field case file".to_string()
            title=Box::new(move || view! {
                {back_for_title.map(|svc| {
                    let label = format!("Back to {}", sanitize_display_text(&svc));
                    view! {
                        <button
                            type="button"
                            class="fc-back"
                            title=label.clone()
                            aria-label=label
                            on:click=move |_| on_back.run(())
                        >
                            <IconView icon=Icon::Chevron size=14 stroke_width=1.5/>
                        </button>
                    }
                })}
                <span class="name mono">{title_field.clone()}</span>
            }.into_any())
        >
            <Loaded
                state=Signal::derive(move || {
                    LoadState::from_resource_with_missing(
                        detail.get(),
                        |e| matches!(e, ApiError::Status(404)),
                        |e| match e {
                            ApiError::Status(503) => {
                                "the catalog store is unavailable".to_string()
                            }
                            other => other.to_string(),
                        },
                    )
                })
                label="field"
                // A name with no pin is not a failure and never a blank
                // drawer: it is a case file that says so.
                missing=Box::new(move || view! {
                    <div class="fc-case">
                        <div class="fc-note">
                            <p>
                                "No pin for "
                                <span class="mono">{missing_field.clone()}</span>
                                "."
                            </p>
                            <p>
                                "The catalog types a field the first time a batch carries a \
                                 value for it, so an unpinned name has never arrived with a \
                                 typed value — or is spelled differently, or arrived after the \
                                 pin capacity filled. Names are ASCII-lowercased at ingest, and \
                                 its values, if any, stay findable in "
                                <span class="mono">"_raw"</span>
                                "."
                            </p>
                        </div>
                    </div>
                }.into_any())
                error=Box::new(move |msg: String| view! {
                    <div class="fc-case">
                        <div class="fc-err" role="alert">{format!("Couldn't load field: {msg}")}</div>
                        <div class="sfd-actions">
                            <Btn
                                variant=Variant::Secondary
                                on_click=Callback::new(move |()| reload.update(|n| *n += 1))
                            >
                                "Retry"
                            </Btn>
                        </div>
                    </div>
                }.into_any())
                render=Box::new(move |resp: CatalogFieldResponse| {
                    let pinned_from = resp.pinned_from.clone()
                        .map_or_else(|| "\u{2014}".to_string(), |s| sanitize_display_text(&s));
                    let data_type = resp.data_type.clone();
                    let pinned_at = resp.pinned_at.clone();
                    let verdict = resp.verdict.clone();
                    let conflicts = resp.conflicts.clone();
                    // The remedy exists only for a degraded pin: a healthy
                    // field gets no trigger and no command line.
                    let remedy = verdict.as_ref().map(|v| {
                        (
                            resp.data_type.clone(),
                            v.suggested_to.clone(),
                            repin_command_hint(&resp.name, &v.suggested_to),
                        )
                    });
                    // The deep-link arm has no service to contrast with,
                    // so it stops at the scope itself.
                    let scope_copy = back_for_copy.clone().map_or_else(
                        || "A repin rewrites this field across the entire corpus \u{2014} every \
                            service, every environment, every day.".to_string(),
                        |svc| format!(
                            "A repin rewrites this field across the entire corpus \u{2014} every \
                             service, every environment, every day \u{2014} not just {}.",
                            sanitize_display_text(&svc),
                        ),
                    );

                    view! {
                        <div class="fc-case">
                            <div class="fc-sec">
                                <div class="fc-lb">"Pin"</div>
                                <div class="sfd-kv"><span>"Type"</span><span>{data_type}</span></div>
                                <div class="sfd-kv"><span>"Pinned by"</span><span>{pinned_from}</span></div>
                                <div class="sfd-kv"><span>"Pinned at"</span><span>{pinned_at}</span></div>
                            </div>

                            {verdict.map_or_else(
                                || view! {
                                    <div class="fc-sec">
                                        <div class="fc-lb">"Health"</div>
                                        <div class="fc-note">
                                            "This pin is not degraded. The catalog sees no \
                                             sustained pattern of values it has to shelve, so \
                                             there is nothing to repin."
                                        </div>
                                    </div>
                                }.into_any(),
                                |v| verdict_block(&v),
                            )}

                            // Directly under the verdict: the remedy is
                            // what the verdict is for, and a reader who
                            // has just been told the pin is shelving
                            // values should not have to scroll past the
                            // evidence to find out what to do about it.
                            {remedy.map(|(current, suggested, hint)| view! {
                                <div class="fc-sec">
                                    <div class="fc-lb">"Remedy"</div>
                                    // The trigger and the command line are
                                    // alternatives, never a disabled pair:
                                    // with `schema_write` the button is the
                                    // remedy, without it the CLI line is.
                                    {move || if can_repin.get() {
                                        let current = current.clone();
                                        let suggested = suggested.clone();
                                        view! {
                                            <div class="sfd-actions">
                                                <Btn
                                                    variant=Variant::Primary
                                                    on_click=Callback::new(move |()| {
                                                        open_repin.run((
                                                            current.clone(),
                                                            suggested.clone(),
                                                        ));
                                                    })
                                                >
                                                    "Repin this field"
                                                </Btn>
                                            </div>
                                            <p class="fc-note">
                                                "Opens the plan first \u{2014} the scan runs, and \
                                                 nothing is rewritten until you confirm it."
                                            </p>
                                        }.into_any()
                                    } else {
                                        match hint.clone() {
                                            Some(cmd) => view! {
                                                <p class="fc-note">
                                                    "Repinning needs the "
                                                    <span class="mono">"schema_write"</span>
                                                    " permission, which this session doesn't \
                                                     hold. From a shell:"
                                                </p>
                                                <pre class="fc-cmd">{cmd}</pre>
                                            }.into_any(),
                                            None => view! {
                                                <p class="fc-note">
                                                    {hint_segments(REPIN_HINT_REFUSED)
                                                        .into_iter()
                                                        .map(|(code, text)| if code {
                                                            view! {
                                                                <span class="mono">{text}</span>
                                                            }.into_any()
                                                        } else {
                                                            view! { {text} }.into_any()
                                                        })
                                                        .collect::<Vec<_>>()}
                                                </p>
                                            }.into_any(),
                                        }
                                    }}
                                </div>
                            })}

                            {(!conflicts.is_empty()).then(|| conflicts_block(&conflicts))}

                            <div class="fc-sec">
                                <div class="fc-lb">"Services carrying this field"</div>
                                {move || services.get().into_iter().map(|row| {
                                    let service = sanitize_display_text(&row.service);
                                    let rows_label = super::service_card_fmt::format_count(row.row_count);
                                    view! {
                                        <div class="fc-svc">
                                            <span class="mono nm">{service}</span>
                                            <span class="dt">{row.first_seen}" \u{2192} "{row.last_seen}</span>
                                            <span class="ct">{rows_label}" rows"</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                                {move || more_error.get().map(|msg| view! {
                                    <div class="fc-err" role="alert">
                                        {format!("Couldn't load more services: {msg}")}
                                    </div>
                                })}
                                <LoadMore
                                    has_more=Signal::derive(move || cursor.get().is_some())
                                    busy=more_busy
                                    empty=Signal::derive(move || services.get().is_empty())
                                    empty_text="No service has been observed carrying this field"
                                    end_text="All observed services shown"
                                    on_load=on_load_more
                                />
                            </div>

                            // The footer is the standing scope statement
                            // and nothing else: it is true of every case
                            // file, degraded or not.
                            <div class="fc-foot">
                                <p class="fc-note">{scope_copy}</p>
                            </div>
                        </div>
                    }.into_any()
                })
            />

            // Repin job state. Deliberately outside `Loaded`: a job runs
            // against the corpus, not against this response, so it stays
            // visible while page one refetches after a success.
            //
            // A stable region, so a screen reader hears the outcome once
            // rather than the file counter every three seconds — the
            // per-tick progress below is plain text on purpose.
            <div class="fc-live" aria-live="polite">
                {move || job.get()
                    .filter(|j| !repin_is_running(&j.status))
                    .map(|j| job_outcome_line(&j))}
            </div>
            {move || job.get().map(|j| {
                // An adopted refusal offers the ladder rather than
                // opening it: a button the reader can take, and only
                // when the session may actually repin.
                // A refusal for a target the modal does not offer — a
                // CLI-started severity or resurrection repin — has no
                // dialog to go back into: its plan is a scan of that
                // target and means nothing under any other rung. It
                // stays a receipt, with the shell as the way on.
                let off_ladder =
                    j.status == STATUS_REFUSED && !refusal_is_reusable(&j.from_type, &j.to_type);
                let review = (j.status == STATUS_REFUSED
                    && !job_initiated.get()
                    && can_repin.get()
                    && !off_ladder)
                    .then_some(review_refused);
                view! {
                    {job_block(&j, review)}
                    {off_ladder.then(|| view! {
                        <p class="fc-note">
                            "This refusal is for a target this page does not offer: putting a \
                             field on the severity ladder needs a dialect asserted, and \
                             re-extracting shelved values under the pin it already has is a \
                             resurrection pass. Both stay shell decisions — "
                            <span class="mono">"trawl schema repin"</span>
                            " re-runs the plan and "
                            <span class="mono">"--force"</span>
                            " accepts it."
                        </p>
                    })}
                }
            })}
            {move || poll_warning.get().map(|msg| view! {
                <div class="fc-note" role="status">{msg}</div>
            })}
            {move || status_lost.get().then(|| view! {
                <div class="fc-err" role="alert">
                    "Repin status unavailable \u{2014} the one-running slot has moved on to \
                     another job, or the store stopped answering. What is shown above is the \
                     last state this page saw; "
                    <span class="mono">"trawl schema repin-status"</span>
                    " reads the current one."
                    {move || recheck_note.get().map(|note| view! { <p>{note}</p> })}
                    <div class="sfd-actions">
                        <Btn
                            variant=Variant::Secondary
                            disabled=Signal::derive(move || status_probing.get())
                            on_click=Callback::new(move |()| probe_status.run(true))
                        >
                            {move || if status_probing.get() { "Checking\u{2026}" } else { "Check again" }}
                        </Btn>
                    </div>
                </div>
            })}
            {move || other_running.get().map(|other| {
                let shown = sanitize_display_text(&other);
                view! {
                    <div class="fc-note">
                        "A repin is running on another field: "
                        <button
                            type="button"
                            class="fc-link"
                            on:click=move |_| on_open_field.run(other.clone())
                        >
                            {shown.clone()}
                        </button>
                        ". Only one repin runs at a time, install-wide, so this field has to \
                         wait for it."
                    </div>
                }
            })}
        </Drawer>
    }
}

/// One sentence for a finished job — the toast's detail line and the
/// case file's polite announcement, written once so the two cannot
/// disagree about what happened.
fn job_outcome_line(job: &RepinJobResponse) -> String {
    let field = sanitize_display_text(&job.field);
    match job.status.as_str() {
        STATUS_SUCCEEDED if job.dry_run => format!(
            "{field}: dry run finished \u{2014} {} files carry it, {} values cannot be kept, \
             {} would come back from _raw.",
            format_exact(job.files_total),
            format_exact(job.projected_nulls),
            format_exact(job.resurrectable),
        ),
        STATUS_SUCCEEDED => format!(
            "{field} is now pinned {} \u{2014} {} rows rewritten, {} nulled, {} resurrected \
             from _raw.",
            job.to_type,
            format_exact(job.rows_rewritten),
            format_exact(job.rows_nulled),
            format_exact(job.rows_resurrected),
        ),
        // Two refusals wear this status, and they are not the same news.
        // An unforced run is refused because nothing accepted the loss;
        // a FORCED one is refused because the finished rewrite came out
        // worse than the ceilings the operator did accept, so those are
        // the numbers to name.
        STATUS_REFUSED => {
            let projected = format_exact(job.projected_nulls);
            match (job.force, accepted_ceilings(job)) {
                // The number the gate compared is the finished shadow's own
                // tally, not the scan projection: a cutover-gate refusal
                // means the shadow grew PAST the ceiling, and the ceiling
                // carries headroom above the scan, so quoting the
                // projection here would read "8 is past 22". A scan-gate
                // refusal persists no shadow tally and the projection is
                // the number the gate saw.
                (true, Some(bound)) => {
                    let lost = if job.rows_nulled > 0 {
                        format_exact(job.rows_nulled)
                    } else {
                        projected
                    };
                    format!(
                        "{field}: refused \u{2014} {lost} stored values cannot be kept as {}, \
                         past the {} row(s) and {} dialect-ambiguous numeral(s) accepted. The \
                         corpus is untouched.",
                        job.to_type,
                        format_exact(bound.max_nulled),
                        format_exact(bound.max_ambiguous),
                    )
                }
                // Forced, but the row carries no resolved pair to name —
                // a job a server older than the ceiling columns wrote.
                (true, None) => format!(
                    "{field}: refused \u{2014} {projected} stored values cannot be kept as {}, \
                     past what the forced run was held to. The corpus is untouched.",
                    job.to_type,
                ),
                (false, _) => format!(
                    "{field}: refused \u{2014} {projected} stored values cannot be kept as {}, \
                     and no force was given. The corpus is untouched.",
                    job.to_type,
                ),
            }
        }
        // Including `blocked`, `failed`, and any status a later server
        // adds: the wire word verbatim, plus whatever it said went wrong.
        other => {
            let status = sanitize_display_text(other);
            job.error.as_deref().map_or_else(
                || format!("{field}: {status}."),
                |e| format!("{field}: {status} \u{2014} {}", sanitize_display_text(e)),
            )
        }
    }
}

/// The job as facts. Progress while it runs, the wire status and the
/// server's own error text when it stops — never a spinner standing in
/// for a `blocked` or `failed` job.
///
/// `on_review` is present only for an adopted `refused_needs_force` job
/// a session that may repin is reading: the refusal then renders as a
/// receipt with a way back into the ladder, instead of the force dialog
/// opening itself over a reader who started nothing.
fn job_block(job: &RepinJobResponse, on_review: Option<Callback<()>>) -> AnyView {
    let running = repin_is_running(&job.status);
    let refused = job.status == STATUS_REFUSED;
    let tone = if running {
        Tone::Info
    } else if job.status == STATUS_SUCCEEDED {
        Tone::Success
    } else if refused {
        // A refusal is the server declining to lose data: a warning to
        // act on, not a failure.
        Tone::Warn
    } else {
        // `failed`, `blocked`, and any status a later server adds.
        Tone::Danger
    };
    let status = sanitize_display_text(&job.status);
    let kind = if job.dry_run { "Dry run" } else { "Repin" };
    let route = format!("{} \u{2192} {}", job.from_type, job.to_type);
    let requested_by = job.requested_by.as_deref().map(sanitize_display_text);
    let started_at = job.started_at.clone();
    // Plain text, re-read on every tick — see the aria-live note above.
    let progress = running.then(|| {
        format!(
            "{} of {} files rewritten",
            format_exact(job.files_done),
            format_exact(job.files_total),
        )
    });
    let error = job.error.as_deref().map(sanitize_display_text);
    let lagging = job.status == STATUS_SUCCEEDED && !job.dry_run;
    // What the refusal was about. `projected_nulls`, not `rows_nulled`:
    // the projection is the number the server declined over. A forced
    // refusal does carry outcome counters (its shadow rewrite finished
    // before the cutover gate turned it down), but those describe a
    // generation that was thrown away, never the live corpus.
    let projected_nulls = format_exact(job.projected_nulls);
    view! {
        <div class="fc-sec fc-job">
            <div class="fc-lb">
                {kind}" job "
                <Badge tone=tone>{status}</Badge>
            </div>
            <div class="sfd-kv"><span>"Retype"</span><span>{route}</span></div>
            {requested_by.map(|by| view! {
                <div class="sfd-kv"><span>"Requested by"</span><span>{by}</span></div>
            })}
            <div class="sfd-kv"><span>"Started"</span><span>{started_at}</span></div>
            {refused.then(|| view! {
                <div class="sfd-kv">
                    <span>"Values the pin cannot keep"</span><span>{projected_nulls}</span>
                </div>
            })}
            {progress.map(|p| view! { <p class="fc-note">{p}</p> })}
            {error.map(|e| view! { <div class="fc-err" role="alert">{e}</div> })}
            {on_review.map(|review| view! {
                <div class="sfd-actions">
                    <Btn variant=Variant::Danger on_click=review>"Review forced repin\u{2026}"</Btn>
                </div>
                <p class="fc-note">
                    "This refusal is a plan the scan has already paid for. Reviewing it \
                     re-presents those numbers with the force acceptance \u{2014} nothing is \
                     rewritten until that is checked and confirmed."
                </p>
            })}
            {lagging.then(|| view! {
                <p class="fc-note">
                    "The pin has changed. Degraded badges and query notices elsewhere read a \
                     cached snapshot and can lag this by one refresh tick."
                </p>
            })}
        </div>
    }
    .into_any()
}

/// The analyzer's verdict, as facts. The words are written here rather
/// than stored server-side (ADR-0011), and `rows shelved` is labelled
/// lifetime because it deliberately disagrees with the per-conflict
/// `rows nulled` below it, which sums only the evidence still inside the
/// per-field recency window.
fn verdict_block(v: &DegradedVerdict) -> AnyView {
    let samples: Vec<String> = v.samples.iter().map(|s| sanitize_display_text(s)).collect();
    let since = v.since.clone();
    let services = v.services;
    let episodes = v.episodes;
    let rows_shelved = v.rows_shelved;
    let suggested = v.suggested_to.clone();
    view! {
        <div class="fc-sec">
            <div class="fc-lb">
                "Health "
                <Badge tone=Tone::Warn>"degraded"</Badge>
            </div>
            <div class="sfd-kv"><span>"Shelving since"</span><span>{since}</span></div>
            <div class="sfd-kv">
                <span>"Services with conflict evidence"</span><span>{services}</span>
            </div>
            <div class="sfd-kv"><span>"Conflict episodes"</span><span>{episodes}</span></div>
            <div class="sfd-kv">
                <span>"Rows shelved (lifetime)"</span><span>{rows_shelved}</span>
            </div>
            <div class="sfd-kv"><span>"Suggested type"</span><span>{suggested}</span></div>
            {(!samples.is_empty()).then(|| view! {
                <div class="fc-samples">
                    <div class="fc-lb2">"Values the pin shelved"</div>
                    <ul>
                        {samples.into_iter()
                            .map(|s| view! { <li class="mono">{s}</li> })
                            .collect::<Vec<_>>()}
                    </ul>
                </div>
            })}
        </div>
    }
    .into_any()
}

/// Retained conflict evidence, newest first. Per-row samples sit inside
/// a collapsed `<details>` — the verdict's sample list leads, and this
/// is the long tail behind it.
fn conflicts_block(conflicts: &[CatalogConflictRow]) -> AnyView {
    let rows = conflicts
        .iter()
        .map(|c| {
            let service = sanitize_display_text(&c.service);
            let cast = format!("{} \u{2192} {}", c.observed_type, c.expected_type);
            let rows_nulled = c.rows_nulled;
            let at = c.at.clone();
            let samples: Vec<String> = c.samples.iter().map(|s| sanitize_display_text(s)).collect();
            view! {
                <div class="fc-conflict">
                    <div class="hd">
                        <span class="mono nm">{service}</span>
                        <span class="cast mono">{cast}</span>
                        <span class="ct">{rows_nulled}" rows nulled"</span>
                        <span class="dt">{at}</span>
                    </div>
                    {(!samples.is_empty()).then(|| view! {
                        <details>
                            <summary>"sample values"</summary>
                            <ul>
                                {samples.into_iter()
                                    .map(|s| view! { <li class="mono">{s}</li> })
                                    .collect::<Vec<_>>()}
                            </ul>
                        </details>
                    })}
                </div>
            }
        })
        .collect::<Vec<_>>();
    view! {
        <div class="fc-sec">
            <div class="fc-lb">"Recent conflicts"</div>
            {rows}
        </div>
    }
    .into_any()
}
