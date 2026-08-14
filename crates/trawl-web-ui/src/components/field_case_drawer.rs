// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FieldCaseDrawer/>` — the field case file (ADR-0011 slice C2).
//!
//! A PEER of [`ServiceDrawer`](super::service_drawer::ServiceDrawer),
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
//! Field names, service names and conflict SAMPLES are client-chosen
//! text. They are rendered in leptos TEXT positions only — never
//! `inner_html`, never string-built markup — and each display copy goes
//! through `trawl_core::sanitize::sanitize_display_text` first, so a
//! bidi override in a sample cannot reorder the line beneath it.
//!
//! Since M3 the case file also owns the repin JOB: the trigger (behind
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
    PollAction, ProbedJob, REPIN_POLL_MS, StatusProbe, claim_toast, poll_decide,
};
use crate::repin_hint::{REPIN_HINT_REFUSED, repin_command_hint, repin_is_running};
use fleet_ui::{
    Badge, Btn, Drawer, Icon, IconView, LoadMore, LoadState, Loaded, ToastBus, ToastKind, Tone,
    Variant,
};

/// The wire status a refusal carries. Matched as a literal in the two
/// places the SPA has to ACT on one specific status rather than merely
/// render it; the vocabulary itself is
/// `trawl-server/src/store/repin.rs`.
const STATUS_REFUSED: &str = "refused_needs_force";

/// Ditto, for the one status that licenses a case-file refetch.
const STATUS_SUCCEEDED: &str = "succeeded";

/// Shown once — not once per tick — while status reads are failing but
/// the retry budget still has room.
const POLL_RETRY_WARNING: &str = "Couldn't read the repin status just now; still trying.";

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
    /// Whether to OFFER the repin trigger. Advisory: the server gates
    /// `POST /schema/repin` on `schema_write` and is the only thing that
    /// does. False (including before `/me` resolves) renders the M2
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
    // Canonical: the job row as the SERVER returned it. Nothing here
    // synthesizes a status or edits one — `job` is only ever replaced
    // wholesale by a status read or by the 202 the modal hands up.
    let bus = expect_context::<ToastBus>();
    let job = RwSignal::new(None::<RepinJobResponse>);
    // The id polling follows. Set on a 202 and on adopting a running
    // job at mount; cleared the moment the job settles or the
    // install-wide slot moves on.
    let tracked_job_id = RwSignal::new(None::<i64>);
    let poll_errors = RwSignal::new(0_u32);
    let poll_in_flight = RwSignal::new(false);
    let poll_warning = RwSignal::new(None::<String>);
    let status_lost = RwSignal::new(false);
    let other_running = RwSignal::new(None::<String>);
    let modal = RwSignal::new(None::<ModalReq>);

    // The ONE interval in this crate. Dropping the StoredValue's
    // contents cancels it; `on_cleanup` runs on close, back, Escape and
    // on navigating to a different field (the page rebuilds the drawer),
    // so no poll outlives the surface that started it.
    let poller: StoredValue<Option<Interval>, LocalStorage> = StoredValue::new_local(None);
    on_cleanup(move || poller.update_value(|p| *p = None));

    // Cancelling the interval cannot cancel a read already awaiting: a
    // status response can land after this drawer is gone. Signal writes
    // are silently dropped once the owner is disposed, but the ToastBus
    // is NOT this drawer's — it outlives it by design, and a toast from a
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
        // Deduped across drawer INSTANCES (`repin_flow::claim_toast`):
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
        if status == STATUS_REFUSED {
            // The refusal IS a plan, scanned by the run that just
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
            poll_in_flight.set(false);
            let (probe, row) = match result {
                Ok(resp) => match resp.job {
                    Some(j) => (StatusProbe::Job(ProbedJob::from(&j)), Some(j)),
                    None => (StatusProbe::NoJob, None),
                },
                Err(_) => (StatusProbe::Failed, None),
            };
            let errors = if matches!(probe, StatusProbe::Failed) {
                poll_errors.get_untracked() + 1
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

    // Immediate first read, then the interval (synthesis R10).
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

    // Mount: ONE status read. The route is install-wide, so what comes
    // back is either this field's job (adopt it — poll if it is still
    // running, show the receipt if it is not), or another field's (say
    // so, and do not poll for it), or nothing.
    let field_for_status = field.clone();
    spawn_local(async move {
        let Ok(status) = api::repin_status().await else {
            // The case file is the point; the receipt is a bonus. A
            // failed probe stays silent rather than banner-ing a surface
            // that loaded fine.
            return;
        };
        let Some(found) = status.job else { return };
        // Catalog keys are ASCII-folded at ingest, but the `?field=`
        // spelling in the URL is whatever the operator typed.
        if found.field.eq_ignore_ascii_case(&field_for_status) {
            let running = repin_is_running(&found.status);
            let id = found.id;
            job.set(Some(found));
            if running {
                // `try_run`: the drawer may already be gone — closed
                // inside this read's round trip — and running a disposed
                // callback panics where a disposed signal write is a
                // silent no-op.
                start_poll.try_run(id);
            }
        } else if repin_is_running(&found.status) {
            other_running.set(Some(found.field));
        }
    });

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
            status_lost.set(false);
            if repin_is_running(&started.status) {
                let id = started.id;
                job.set(Some(started));
                start_poll.run(id);
            } else {
                // Already TERMINAL when it was handed up: the recovery
                // probe found a run that finished inside the round trip
                // whose response was lost. Nothing will poll it, so this
                // is the only chance to settle it — without which a
                // succeeded repin would sit as a receipt beside a case
                // file still showing the OLD pin, and raise no toast.
                settle(started);
            }
        }
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
        // Mounted as a SIBLING of the drawer, not inside it: the drawer
        // sits in its own stacking context (z-index 51) and the modal
        // scrim has to cover the whole viewport from the page's.
        {move || modal.get().map(|req| view! {
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
                                 value for it, so an unpinned name has never been ingested — \
                                 or is spelled differently. Names are ASCII-lowercased at \
                                 ingest, and its values, if any, stay findable in "
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
                    // field gets no trigger and no command line (M2).
                    let remedy = verdict.as_ref().map(|v| {
                        (
                            resp.data_type.clone(),
                            v.suggested_to.clone(),
                            repin_command_hint(&resp.name, &v.suggested_to),
                        )
                    });
                    let scope_copy = back_for_copy.clone().map_or_else(
                        || "A repin rewrites this field across the entire corpus \u{2014} every \
                            service, every environment, every day \u{2014} not just the service \
                            you reached it from.".to_string(),
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

                            <div class="fc-foot">
                                <p class="fc-note">{scope_copy}</p>
                                {remedy.map(|(current, suggested, hint)| view! {
                                    <div class="fc-lb">"Remedy"</div>
                                    // The trigger and the command line are
                                    // ALTERNATIVES, never a disabled pair:
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
                                                <pre class="fc-cmd">{cmd}</pre>
                                            }.into_any(),
                                            None => view! {
                                                <p class="fc-note">{REPIN_HINT_REFUSED}</p>
                                            }.into_any(),
                                        }
                                    }}
                                })}
                            </div>
                        </div>
                    }.into_any()
                })
            />

            // Repin job state. Deliberately OUTSIDE `Loaded`: a job runs
            // against the corpus, not against this response, so it stays
            // visible while page one refetches after a success.
            //
            // A stable region, so a screen reader hears the OUTCOME once
            // rather than the file counter every three seconds — the
            // per-tick progress below is plain text on purpose.
            <div class="fc-live" aria-live="polite">
                {move || job.get()
                    .filter(|j| !repin_is_running(&j.status))
                    .map(|j| job_outcome_line(&j))}
            </div>
            {move || job.get().map(|j| job_block(&j))}
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

/// One sentence for a finished job — the toast's detail line AND the
/// case file's polite announcement, written once so the two cannot
/// disagree about what happened.
fn job_outcome_line(job: &RepinJobResponse) -> String {
    let field = sanitize_display_text(&job.field);
    match job.status.as_str() {
        STATUS_SUCCEEDED if job.dry_run => format!(
            "{field}: dry run finished \u{2014} {} files carry it, {} values could not be kept, \
             {} would come back from _raw.",
            job.files_total, job.projected_nulls, job.resurrectable,
        ),
        STATUS_SUCCEEDED => format!(
            "{field} is now pinned {} \u{2014} {} rows rewritten, {} nulled, {} resurrected \
             from _raw.",
            job.to_type, job.rows_rewritten, job.rows_nulled, job.rows_resurrected,
        ),
        STATUS_REFUSED => format!(
            "{field}: refused \u{2014} {} stored values cannot be kept as {}, and no force was \
             given. The corpus is untouched.",
            job.projected_nulls, job.to_type,
        ),
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
fn job_block(job: &RepinJobResponse) -> AnyView {
    let running = repin_is_running(&job.status);
    let tone = if running {
        Tone::Info
    } else if job.status == STATUS_SUCCEEDED {
        Tone::Success
    } else {
        Tone::Warn
    };
    let status = sanitize_display_text(&job.status);
    let kind = if job.dry_run { "Dry run" } else { "Repin" };
    let route = format!("{} \u{2192} {}", job.from_type, job.to_type);
    let requested_by = job.requested_by.as_deref().map(sanitize_display_text);
    let started_at = job.started_at.clone();
    // Plain text, re-read on every tick — see the aria-live note above.
    let progress =
        running.then(|| format!("{} of {} files rewritten", job.files_done, job.files_total));
    let error = job.error.as_deref().map(sanitize_display_text);
    let lagging = job.status == STATUS_SUCCEEDED && !job.dry_run;
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
            {progress.map(|p| view! { <p class="fc-note">{p}</p> })}
            {error.map(|e| view! { <div class="fc-err" role="alert">{e}</div> })}
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
/// than stored server-side (ADR-0011 slice C ruling 5), and `rows
/// shelved` is labelled LIFETIME because it deliberately disagrees with
/// the per-conflict `rows nulled` below it, which sums only the evidence
/// still inside the per-field recency window.
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
            <div class="sfd-kv"><span>"Episodes"</span><span>{episodes}</span></div>
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
