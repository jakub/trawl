// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<NetDrawer/>` — slide-out detail inspector for a saved query ("net").
//!
//! Two tabs, URL-synced via `?ntab=query|runs`:
//! - Query + Schedule: inline-editable DSL, schedule controls with
//!   interval chips + custom input, enabled toggle, max-runs.
//! - Runs: paginated run history with expandable inline result previews.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys;
use trawl_api::SavedQueryResponse;
use trawl_api::value::QueryResult;

use crate::api;
use fleet_ui::time::{format_duration, time_ago};
use fleet_ui::{
    Btn, Drawer, LoadState, Loaded, OffsetPager, PageTotal, PageWindow, Segmented, SegmentedOption,
    Size, Sparkline, StatusDot, TabItem, ToastBus, ToastKind, Toggle, Variant, effective_active,
};

use crate::schedule_edit::{WindowDraft, WindowMode, preview_cap, validate_max_runs};

use crate::api::RUNS_PAGE_SIZE;

/// Rows per page inside an expanded run's stored result. The preview
/// pages what the response already carries; page turns never re-fetch.
/// Background refresh can supply a newly completed result.
const PREVIEW_PAGE_SIZE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(20).unwrap();

const INTERVAL_PRESETS: &[&str] = &["5m", "15m", "1h", "6h", "24h", "1w"];

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn NetDrawer(
    net: SavedQueryResponse,
    saved: Signal<Option<SavedQueryResponse>>,
    tab: Signal<String>,
    on_close: Callback<()>,
    on_tab_change: Callback<String>,
    on_search: Callback<String>,
    on_refresh: Callback<()>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let net_for_query = net.clone();
    let missing = Signal::derive(move || saved.get().is_none());
    let mutation = RwSignal::new(0u64);
    let parent_refresh = on_refresh;
    let on_refresh = Callback::new(move |()| {
        mutation.update(|n| *n += 1);
        parent_refresh.run(());
    });
    let net_for_runs = net.clone();
    let query_for_run = net.query.clone();
    let net_id_for_trigger = net.id;
    let name_for_trigger = net.name.clone();
    // A windowed schedule computes each run's bounds from the last one
    // it covered, so a manual run out of band moves that point and
    // leaves a hole the schedule will not revisit. The offer is withdrawn
    // from the SAVED state, never from an unsaved draft of it.
    let manual_run_allowed = move || {
        saved
            .get()
            .is_some_and(|n| n.schedule.and_then(|s| s.window).is_none())
    };

    // -- inline rename --
    let editing_name = RwSignal::new(false);
    let name_buf = RwSignal::new(net.name.clone());
    // The trigger REPLACES itself with the input, so focus has to be
    // handed over in both directions or a keyboard user is dropped on
    // `<body>`: the browser has no memory of an element it removed.
    // Same obligation ADR-0028 puts on an overlay opener, minus the
    // overlay.
    let name_btn_ref = NodeRef::<leptos::html::Button>::new();
    let name_input_ref = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |was_editing: Option<bool>| {
        let editing = editing_name.get();
        if editing {
            if let Some(input) = name_input_ref.get() {
                let _ = input.focus();
            }
        } else if was_editing == Some(true) {
            // Leaving the editor, however it ended: back to the control
            // that opened it.
            if let Some(btn) = name_btn_ref.get() {
                let _ = btn.focus();
            }
        }
        editing
    });
    let original_name = net.name.clone();
    Effect::new(move |_| {
        if let Some(current) = saved.get()
            && !editing_name.get_untracked()
        {
            name_buf.set(current.name);
        }
    });

    let do_rename = {
        let net_id = net.id;
        let original_query = net.query.clone();
        let orig = original_name.clone();
        move || {
            if missing.get_untracked() {
                return;
            }
            let draft = name_buf.get_untracked();
            let Ok(new_name) = trawl_core::saved_name::normalize(&draft) else {
                return;
            };
            let new_name = new_name.to_owned();
            if new_name
                == saved
                    .get_untracked()
                    .map_or_else(|| orig.clone(), |n| n.name)
            {
                editing_name.set(false);
                name_buf.set(
                    saved
                        .get_untracked()
                        .map_or_else(|| orig.clone(), |n| n.name),
                );
                return;
            }
            let q = saved
                .get_untracked()
                .map_or_else(|| original_query.clone(), |n| n.query);
            spawn_local(async move {
                match api::update_saved_full(net_id, &q, Some(&new_name)).await {
                    Ok(_) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        bus.push(ToastKind::Success, "Renamed", None);
                        editing_name.set(false);
                        on_refresh.run(());
                    }
                    Err(e) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        bus.push(ToastKind::Error, "Rename failed", Some(e.to_string()));
                    }
                }
            });
        }
    };

    // Escape cancels an in-flight rename before it closes the drawer —
    // threaded into fleet_ui::Drawer's window-level listener.
    let on_escape = {
        let orig = original_name.clone();
        Callback::new(move |()| {
            if editing_name.get_untracked() {
                editing_name.set(false);
                name_buf.set(
                    saved
                        .get_untracked()
                        .map_or_else(|| orig.clone(), |n| n.name),
                );
            } else {
                on_close.run(());
            }
        })
    };

    // Drawer compares ids verbatim; map the URL signal's empty default.
    let eff_tab = effective_active(tab, "query");

    let on_run_click = {
        let q = query_for_run;
        let cb = on_search;
        Callback::new(move |()| {
            cb.run(saved.get_untracked().map_or_else(|| q.clone(), |n| n.query));
        })
    };

    let on_trigger_click = Callback::new(move |()| {
        if missing.get_untracked() {
            return;
        }
        let name = name_for_trigger.clone();
        let id = net_id_for_trigger;
        spawn_local(async move {
            match api::trigger_run(id).await {
                Ok(_) => {
                    if alive.try_get_value() != Some(true) {
                        return;
                    }
                    bus.push(
                        ToastKind::Success,
                        "Run triggered",
                        Some(format!("'{name}' is executing.")),
                    );
                    on_refresh.run(());
                }
                Err(e) => {
                    if alive.try_get_value() != Some(true) {
                        return;
                    }
                    bus.push(ToastKind::Error, "Trigger failed", Some(e.to_string()));
                }
            }
        });
    });

    view! {
        <Drawer
            tabs=vec![
                TabItem::new("query", "Query + Schedule"),
                TabItem::new("runs", "Runs"),
            ]
            tabs_label="Saved query details"
            active_tab=eff_tab
            on_tab_change=on_tab_change
            on_close=on_close
            on_escape=on_escape
            title=Box::new(move || view! {
                <Show
                    when=move || editing_name.get()
                    fallback={
                        move || view! {
                            <button
                                type="button"
                                class="name"
                                node_ref=name_btn_ref
                                aria-label=move || format!("Rename {}", saved.get().map_or_else(|| "deleted net".to_string(), |n| n.name))
                                // The title looks like the drawer's
                                // heading, so nothing but the tooltip
                                // tells a pointer user it can be edited.
                                title="Rename"
                                on:click=move |_| editing_name.set(true)
                            >{move || saved.get().map_or_else(|| name_buf.get(), |n| n.name)}</button>
                        }
                    }
                >
                    {
                        let do_rename = do_rename.clone();
                        let do_rename_blur = do_rename.clone();
                        // The input REPLACES the heading it edits, so the
                        // name it is editing is nowhere on screen to label
                        // it: without this the field is an unnamed textbox.
                        view! {
                            <input
                                class="name-edit"
                                aria-invalid=move || trawl_core::saved_name::normalize(&name_buf.get()).is_err().to_string()
                                aria-describedby="netRenameError"
                                node_ref=name_input_ref
                                aria-label=move || format!("New name for {}", saved.get().map_or_else(|| "deleted net".to_string(), |n| n.name))
                                prop:value=move || name_buf.get()
                                on:input=move |e| name_buf.set(event_target_value(&e))
                                on:blur=move |_| do_rename_blur()
                                on:keydown=move |e: web_sys::KeyboardEvent| {
                                    if e.key() == "Enter" {
                                        e.prevent_default();
                                        do_rename();
                                    }
                                }
                            />
                            <span id="netRenameError" role="status">{move || trawl_core::saved_name::normalize(&name_buf.get()).err().unwrap_or("")}</span>
                        }
                    }
                </Show>
            }.into_any())
            actions=Box::new(move || view! {
                <Btn
                    variant=Variant::Secondary
                    on_click=on_run_click
                    attr:title="Open query in search"
                >
                    "▶ Search"
                </Btn>
                {move || manual_run_allowed().then(|| view! {
                    <Btn
                        variant=Variant::Secondary
                        on_click=on_trigger_click
                        attr:title="Trigger a scheduled run now"
                    >
                        "⏱ Run"
                    </Btn>
                })}
            }.into_any())
        >
            {move || missing.get().then(|| view! { <p role="status">"This net no longer exists. Your draft is retained, but it cannot be saved." " " <a href="/jobs/nets">"Back to Nets"</a></p> })}
            <div hidden=move || eff_tab.get() != "runs">
                <RunsPane net_id=net_for_runs.id bus=bus on_search=on_search mutation=mutation active=Signal::derive(move || eff_tab.get() == "runs")/>
            </div>
            <div hidden=move || eff_tab.get() == "runs">
                <QuerySchedulePane net=net_for_query saved=saved bus=bus on_refresh=on_refresh/>
            </div>
        </Drawer>
    }
}

// ---------------------------------------------------------------------------
// Tab 1: Query + Schedule
// ---------------------------------------------------------------------------

#[component]
fn QuerySchedulePane(
    net: SavedQueryResponse,
    saved: Signal<Option<SavedQueryResponse>>,
    bus: ToastBus,
    on_refresh: Callback<()>,
) -> impl IntoView {
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let net_id = net.id;
    let missing = Signal::derive(move || saved.get().is_none());

    // -- query editing --
    let editing = RwSignal::new(false);
    let query_buf = RwSignal::new(net.query.clone());
    let saving_query = RwSignal::new(false);
    let original_query = net.query.clone();

    let do_save_query = {
        move || {
            if missing.get_untracked() {
                return;
            }
            saving_query.set(true);
            let q = query_buf.get_untracked();
            spawn_local(async move {
                match api::update_saved(net_id, &q).await {
                    Ok(_) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        bus.push(ToastKind::Success, "Query updated", None);
                        editing.set(false);
                        on_refresh.run(());
                    }
                    Err(e) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        bus.push(ToastKind::Error, "Update failed", Some(e.to_string()));
                    }
                }
                saving_query.set(false);
            });
        }
    };

    // -- schedule --
    let has_schedule = net.schedule.is_some();
    let interval_buf = RwSignal::new(
        net.schedule
            .as_ref()
            .map_or_else(|| "1h".to_string(), |s| s.interval.clone()),
    );
    let max_runs_buf: RwSignal<String> = RwSignal::new(
        net.schedule
            .as_ref()
            .and_then(|s| s.max_runs)
            .map_or_else(String::new, |n| n.to_string()),
    );
    let enabled_buf = RwSignal::new(net.schedule.as_ref().is_none_or(|s| s.enabled));
    let saving_schedule = RwSignal::new(false);
    let show_schedule_form = RwSignal::new(has_schedule);

    // The window is an edit like any other, and the PUT carries the
    // schedule's whole shape, so the form holds a draft rather than a
    // pass-through: `seed` is what the server reported and `window_draft`
    // is what Save will send. The span and lag buffers are fields of the
    // one draft, so a look at query mode and back does not erase typing.
    let seed = WindowDraft::from_schedule(net.schedule.as_ref());
    let window_draft = RwSignal::new(seed.clone());
    let saved_window =
        Signal::derive(move || saved.get().and_then(|n| n.schedule).and_then(|s| s.window));
    let saved_covered = Signal::derive(move || {
        saved
            .get()
            .and_then(|n| n.schedule)
            .and_then(|s| s.covered_through)
    });
    let saved_interval =
        Signal::derive(move || saved.get().and_then(|n| n.schedule).map(|s| s.interval));
    // A local refusal is not a failed request, so it stays in the form
    // next to the Save button instead of flying past as a toast.
    let save_error: RwSignal<Option<String>> = RwSignal::new(None);

    // Compare each field with the previous server seed. Remote updates only
    // replace clean fields; edits in another field do not freeze this one.
    let previous = StoredValue::new(net.clone());
    Effect::new(move |_| {
        let Some(current) = saved.get() else {
            return;
        };
        let old = previous.get_value();
        if query_buf.get_untracked() == old.query {
            query_buf.set(current.query.clone());
        }
        let old_interval = old
            .schedule
            .as_ref()
            .map_or_else(|| "1h".to_string(), |s| s.interval.clone());
        let max = |n: &SavedQueryResponse| {
            n.schedule
                .as_ref()
                .and_then(|s| s.max_runs)
                .map_or_else(String::new, |n| n.to_string())
        };
        let old_window = WindowDraft::from_schedule(old.schedule.as_ref());
        let schedule_dirty = interval_buf.get_untracked() != old_interval
            || max_runs_buf.get_untracked() != max(&old)
            || enabled_buf.get_untracked() != old.schedule.as_ref().is_none_or(|s| s.enabled)
            || window_draft.get_untracked() != old_window;
        if interval_buf.get_untracked() == old_interval {
            interval_buf.set(
                current
                    .schedule
                    .as_ref()
                    .map_or_else(|| "1h".to_string(), |s| s.interval.clone()),
            );
        }
        if max_runs_buf.get_untracked() == max(&old) {
            max_runs_buf.set(max(&current));
        }
        if enabled_buf.get_untracked() == old.schedule.as_ref().is_none_or(|s| s.enabled) {
            enabled_buf.set(current.schedule.as_ref().is_none_or(|s| s.enabled));
        }
        let new_window = WindowDraft::from_schedule(current.schedule.as_ref());
        window_draft.update(|draft| draft.refresh_clean_from(&old_window, new_window));
        if !schedule_dirty && old.schedule.is_some() != current.schedule.is_some() {
            show_schedule_form.set(current.schedule.is_some());
        }
        previous.set_value(current);
    });

    let do_save_schedule = {
        move || {
            if missing.get_untracked() {
                return;
            }
            save_error.set(None);
            let max_runs = match validate_max_runs(&max_runs_buf.get_untracked()) {
                Ok(max_runs) => max_runs,
                Err(e) => {
                    if alive.try_get_value() != Some(true) {
                        return;
                    }
                    save_error.set(Some(e.to_string()));
                    return;
                }
            };
            let (window, lag) = match window_draft.get_untracked().to_request() {
                Ok(pair) => pair,
                Err(e) => {
                    if alive.try_get_value() != Some(true) {
                        return;
                    }
                    save_error.set(Some(e.to_string()));
                    return;
                }
            };
            saving_schedule.set(true);
            let interval = interval_buf.get_untracked();
            let enabled = enabled_buf.get_untracked();
            spawn_local(async move {
                match api::set_schedule(
                    net_id,
                    &interval,
                    max_runs,
                    enabled,
                    window.as_deref(),
                    lag.as_deref(),
                )
                .await
                {
                    Ok(_) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        bus.push(ToastKind::Success, "Schedule saved", None);
                        on_refresh.run(());
                    }
                    Err(e) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        // The draft survives a refusal: the operator's
                        // next move is to fix what the server named.
                        save_error.set(Some(e.to_string()));
                    }
                }
                saving_schedule.set(false);
            });
        }
    };

    let do_delete_schedule = {
        move || {
            if missing.get_untracked() {
                return;
            }
            spawn_local(async move {
                match api::delete_schedule(net_id).await {
                    Ok(_) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        bus.push(ToastKind::Success, "Schedule removed", None);
                        show_schedule_form.set(false);
                        on_refresh.run(());
                    }
                    Err(e) => {
                        if alive.try_get_value() != Some(true) {
                            return;
                        }
                        bus.push(ToastKind::Error, "Remove failed", Some(e.to_string()));
                    }
                }
            });
        }
    };

    view! {
        <div class="net-sched">
            // -- query section --
            <div class="sd-card">
                <div class="sd-card-hd">
                    <span class="ttl" id="net-query-label">"Query"</span>
                    <Show when=move || !editing.get()>
                        <Btn
                            variant=Variant::Secondary
                            size=Size::Xs
                            on_click=Callback::new(move |()| editing.set(true))
                        >"Edit"</Btn>
                    </Show>
                </div>
                <Show
                    when=move || editing.get()
                    fallback={
                        let q = saved.get_untracked().map_or_else(|| original_query.clone(), |n| n.query);
                        move || { let q = q.clone(); view! { <pre class="preview mono">{move || saved.get().map_or_else(|| q.clone(), |n| n.query)}</pre> } }
                    }
                >
                    <textarea
                        aria-labelledby="net-query-label"
                        class="mono"
                        rows="4"
                        style="width:100%; resize:vertical; font-size:12px; padding:8px; background:var(--fill); border:1px solid var(--line); border-radius:var(--radius-ctl); color:var(--ink)"
                        prop:value=move || query_buf.get()
                        on:input=move |e| query_buf.set(event_target_value(&e))
                    ></textarea>
                    <div style="display:flex; gap:6px; margin-top:6px">
                        <Btn
                            variant=Variant::Primary
                            size=Size::Xs
                            disabled=Signal::derive(move || saving_query.get() || missing.get())
                            on_click=Callback::new(move |()| do_save_query())
                        >{move || if saving_query.get() { "Saving…" } else { "Save" }}</Btn>
                        <Btn variant=Variant::Secondary size=Size::Xs on_click={
                            let reset_q = original_query.clone();
                            Callback::new(move |()| {
                                editing.set(false);
                                query_buf.set(saved.get_untracked().map_or_else(|| reset_q.clone(), |n| n.query));
                            })
                        }>"Cancel"</Btn>
                    </div>
                </Show>
            </div>

            // -- schedule section --
            <div class="sd-card">
                <div class="sd-card-hd">
                    <span class="ttl">"Schedule"</span>
                </div>
                <Show
                    when=move || show_schedule_form.get()
                    fallback=move || view! {
                        <p style="color:var(--ink-3); font-size:12px; margin:0">"No schedule attached."</p>
                        <Btn
                            variant=Variant::Secondary
                            size=Size::Xs
                            attr:style="margin-top:8px"
                            on_click=Callback::new(move |()| show_schedule_form.set(true))
                        >"+ Add Schedule"</Btn>
                    }
                >
                    <div style="display:flex; flex-direction:column; gap:10px">
                        <DurationChips
                            id="net-interval"
                            label="Interval"
                            value=Signal::derive(move || interval_buf.get())
                            on_set=Callback::new(move |v| interval_buf.set(v))
                        />

                        // The window decides what each run reads, so it
                        // is a control here rather than a line of prose
                        // about the TUI. The strip is a set of toggle
                        // buttons, not a radio group, hence the explicit
                        // group role and label.
                        <div role="group" aria-labelledby="net-window-label">
                            <span class="field-label" id="net-window-label">"Window"</span>
                            <Segmented
                                size=Size::Xs
                                options=vec![
                                    SegmentedOption::new("query", "Query text"),
                                    SegmentedOption::new("since_last", "Since last run"),
                                    SegmentedOption::new("fixed", "Fixed span"),
                                ]
                                active=Signal::derive(move || window_draft.get().mode.id().to_string())
                                on_change=Callback::new(move |id: String| {
                                    if let Some(mode) = WindowMode::from_id(&id) {
                                        window_draft.update(|d| d.mode = mode);
                                    }
                                })
                            />
                            {move || match window_draft.get().mode {
                                WindowMode::Query => view! {
                                    <p style="color:var(--ink-3); font-size:11px; margin:6px 0 0">
                                        "Runs the saved text as written. Put the time range in the query, for example "
                                        <code>"last=1h"</code>
                                        "."
                                    </p>
                                }.into_any(),
                                WindowMode::SinceLast => view! {
                                    <p style="color:var(--ink-3); font-size:11px; margin:6px 0 0">
                                        "Each run covers from the previous run's covered point to the run time."
                                    </p>
                                }.into_any(),
                                WindowMode::Fixed => view! {
                                    <p style="color:var(--ink-3); font-size:11px; margin:6px 0 0">
                                        "Each run covers the trailing span below, measured from the run time."
                                    </p>
                                }.into_any(),
                            }}
                        </div>

                        // One grammar and one floor for every duration in
                        // this form, so the span reuses the interval's
                        // chips rather than growing a second spelling.
                        <Show when=move || window_draft.get().mode == WindowMode::Fixed>
                            <DurationChips
                                id="net-window-span"
                                label="Span"
                                helper="At least 60s."
                                value=Signal::derive(move || window_draft.get().span)
                                on_set=Callback::new(move |v| window_draft.update(|d| d.span = v))
                            />
                        </Show>

                        <Show when=move || window_draft.get().mode != WindowMode::Query>
                            <div>
                                <label class="field-label" for="net-lag">"Lag"</label>
                                <input
                                    class="mono"
                                    id="net-lag"
                                    aria-describedby="net-lag-help"
                                    placeholder="0s"
                                    style="width:80px; font-size:12px; padding:4px 6px; background:var(--fill); border:1px solid var(--line); border-radius:var(--radius-ctl); color:var(--ink)"
                                    prop:value=move || window_draft.get().lag
                                    on:input=move |e| {
                                        let v = event_target_value(&e);
                                        window_draft.update(|d| d.lag = v);
                                    }
                                />
                                <p id="net-lag-help" style="color:var(--ink-3); font-size:11px; margin:4px 0 0">
                                    "Late-arrival allowance. Both window bounds move back by this much. Blank is none."
                                </p>
                            </div>
                        </Show>

                        <div>
                            <label class="field-label" for="net-max-runs">"Max runs "</label>
                            <span id="net-max-runs-help" style="color:var(--ink-3); font-size:11px">"(blank = unlimited)"</span>
                            <input
                                class="mono"
                                // Text, not `number`: a browser reports
                                // malformed numeric text (`1e`, a lone
                                // `-`) as an empty value, which reads as
                                // "blank = unlimited" and silently clears
                                // an existing cap. Text keeps what was
                                // typed visible so `validate_max_runs`
                                // can refuse it by name.
                                type="text"
                                inputmode="numeric"
                                id="net-max-runs"
                                aria-describedby="net-max-runs-help"
                                style="display:block; margin-top:4px; width:80px; font-size:12px; padding:4px 6px; background:var(--fill); border:1px solid var(--line); border-radius:var(--radius-ctl); color:var(--ink)"
                                prop:value=move || max_runs_buf.get()
                                on:input=move |e| max_runs_buf.set(event_target_value(&e))
                            />
                        </div>

                        <div style="display:flex; align-items:center; gap:8px">
                            <Toggle
                                label="Schedule enabled"
                                checked=enabled_buf
                                on_change=Callback::new(move |v| enabled_buf.set(v))
                            />
                            <span style="font-size:12px; color:var(--ink-2)">
                                {move || if enabled_buf.get() { "Active" } else { "Paused" }}
                            </span>
                        </div>

                        // What a save is about to cost, said before it
                        // costs it: dropping a window is not an edit the
                        // schedule can undo by itself.
                        {
                            let had_window = move || saved_window.get().is_some();

                            move || {
                                (had_window() && window_draft.get().mode == WindowMode::Query).then(|| {
                                    let coverage = saved_covered.get().map(|t| format!(
                                        " Coverage stops at {t}; switching back resumes from there, within the server's catch-up limit."
                                    ));
                                    view! {
                                        <p style="color:var(--ink-3); font-size:11px; margin:0">
                                            "Saving removes the window and lag. Without a time clause in the query, the schedule imposes no time limit."
                                            {coverage}
                                        </p>
                                    }
                                })
                            }
                        }

                        // A window or interval change can make the next
                        // run due already, which surprises an operator
                        // who expected to wait for the next tick.
                        {
                            move || {
                                let seed = WindowDraft::from_schedule(saved.get().as_ref().and_then(|n| n.schedule.as_ref()));
                                let draft = window_draft.get();
                                // Only request-effective fields count: the span
                                // rides on the PUT in fixed mode alone, and
                                // `to_request` trims it.
                                let moved = draft.mode != seed.mode
                                    || (draft.mode == WindowMode::Fixed
                                        && draft.span.trim() != seed.span.trim())
                                    || saved_interval.get().as_ref().is_some_and(|i| *i != interval_buf.get());
                                moved.then(|| view! {
                                    <p style="color:var(--ink-3); font-size:11px; margin:0">
                                        "Changing the window or interval may run the schedule immediately."
                                    </p>
                                })
                            }
                        }

                        <div style="display:flex; gap:6px; align-items:center">
                            <Btn
                                variant=Variant::Primary
                                size=Size::Xs
                                disabled=Signal::derive(move || saving_schedule.get() || missing.get())
                                on_click=Callback::new(move |()| do_save_schedule())
                            >{move || if saving_schedule.get() { "Saving…" } else { "Save schedule" }}</Btn>
                            {move || saved.get().is_some_and(|n| n.schedule.is_some()).then(|| {
                                view! {
                                    <Btn
                                        variant=Variant::Secondary
                                        size=Size::Xs
                                        attr:style="color:var(--red)"
                                        on_click=Callback::new(move |()| do_delete_schedule())
                                    >"Remove Schedule"</Btn>
                                }
                            })}
                        </div>

                        // Reuses fleet_ui::Field's error paragraph, since
                        // this refusal reads like any other field's.
                        {move || save_error.get().map(|e| view! {
                            <p class="error-banner" role="alert">{e}</p>
                        })}
                    </div>
                </Show>
            </div>
        </div>
    }
}

/// One duration field: the preset chips plus the custom box behind them.
///
/// The schedule form has two of these (interval and fixed span) and they
/// share a grammar and a 60s floor, so they share markup as well. The id
/// is a parameter because two strips on one form cannot both be
/// `net-interval`.
///
/// The custom box reads blank while a preset is pressed, and typing in
/// it clears the pressed chip, because the two are one value shown two
/// ways rather than two values.
#[component]
fn DurationChips(
    id: &'static str,
    label: &'static str,
    #[prop(into)] value: Signal<String>,
    on_set: Callback<String>,
    #[prop(optional)] helper: Option<&'static str>,
) -> impl IntoView {
    let helper_id = helper.map(|_| format!("{id}-help"));

    view! {
        <div>
            <label class="field-label" for=id>{label}</label>
            <div class="interval-chips">
                {INTERVAL_PRESETS.iter().map(|preset| {
                    let p = *preset;
                    let is_on = move || value.get() == p;
                    view! {
                        <button
                            type="button"
                            class=move || if is_on() { "interval-chip on" } else { "interval-chip" }
                            aria-pressed=move || is_on().to_string()
                            on:click=move |_| on_set.run(p.to_string())
                        >{p}</button>
                    }
                }).collect_view()}
            </div>
            <input
                class="mono"
                style="margin-top:6px; width:80px; font-size:12px; padding:4px 6px; background:var(--fill); border:1px solid var(--line); border-radius:var(--radius-ctl); color:var(--ink)"
                id=id
                aria-describedby=helper_id.clone()
                placeholder="Custom…"
                prop:value=move || {
                    let v = value.get();
                    if INTERVAL_PRESETS.contains(&v.as_str()) { String::new() } else { v }
                }
                on:input=move |e| on_set.run(event_target_value(&e))
            />
            {helper.map(|text| view! {
                <p id=helper_id style="color:var(--ink-3); font-size:11px; margin:4px 0 0">{text}</p>
            })}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Tab 2: Runs
// ---------------------------------------------------------------------------

#[component]
fn RunsPane(
    net_id: i64,
    bus: ToastBus,
    on_search: Callback<String>,
    mutation: RwSignal<u64>,
    active: Signal<bool>,
) -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let page = RwSignal::new(0usize);
    let expanded_run: RwSignal<Option<i64>> = RwSignal::new(None);
    let expanded_summary = RwSignal::new(None::<trawl_api::ReportRunSummary>);

    let (runs, refresh_error, retry) = super::job_refresh::job_refresh(active, move || {
        mutation.track();
        let p = page.get();
        async move {
            let offset = PageWindow::checked_offset(p, RUNS_PAGE_SIZE)
                .map_err(|_| api::ApiError::Refused("This runs page is too large to request."))?;
            let response = api::list_runs(net_id, RUNS_PAGE_SIZE.get(), offset).await;
            response.map(|resp| (p, resp))
        }
    });

    let now = fleet_ui::time::clock::now_ms();
    let window = Signal::derive(move || {
        let (fetched, returned, total) = runs
            .get()
            .and_then(Result::ok)
            .map_or((0, 0, 0), |(p, r)| (p, r.runs.len(), r.total));
        PageWindow::new(
            fetched,
            RUNS_PAGE_SIZE,
            returned,
            PageTotal::Known(total),
            !matches!(runs.get(), Some(Ok(_))) || page.get() != fetched,
        )
        .expect("checked runs page")
    });
    view! {
        <div>
            {move || refresh_error.get().map(|e| view! { <p role="status">{e}</p> })}
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(runs.get().map(|result| result.map(|_| ()))))
                label="runs"
                retry=retry
                render=Box::new(|()| ().into_any())
            />
            {move || {
                let data = runs.get().and_then(Result::ok).map(|(_, r)| r.runs.iter().rev()
                    .map(|r| r.row_count.unwrap_or(0) as u64).collect::<Vec<_>>()).unwrap_or_default();
                (data.len() >= 2).then(|| view! {
                    <div style="padding:8px 12px; display:flex; align-items:center; gap:8px">
                        <span style="font-size:11px; color:var(--ink-3)">"Row count trend"</span>
                        <Sparkline data=data color="var(--blue)".to_string() w=180 h=28/>
                    </div>
                })
            }}
            <fleet_ui::OverflowHint viewport=table_viewport/>
            <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Net runs" tabindex="0" style="--list-min-width:480px">
                <table class="fleet-table run-preview-table" aria-label="Net runs"><thead><tr>
                    <th scope="col">"When"</th><th scope="col">"Status"</th><th scope="col">"Duration"</th>
                    <th scope="col">"Rows"</th><th scope="col">"Error"</th>
                </tr></thead><tbody>
                    <For each=move || {
                        let mut rows = runs.get().and_then(Result::ok).map(|(_, r)| r.runs).unwrap_or_default();
                        if let Some(expanded) = expanded_summary.get()
                            && expanded_run.get() == Some(expanded.id)
                            && !rows.iter().any(|r| r.id == expanded.id) {
                            rows.push(expanded);
                        }
                        rows
                    }
                        key=|run| run.id children=move |initial| {
                        let run_id = initial.id;
                        let run = Signal::derive(move || runs.get().and_then(Result::ok)
                            .and_then(|(_, r)| r.runs.into_iter().find(|r| r.id == run_id)).or_else(|| expanded_summary.get().filter(|r| r.id == run_id)).unwrap_or_else(|| initial.clone()));
                        let is_expanded = move || expanded_run.get() == Some(run_id);
                        view! {
                            <tr class="tbl-row">
                                <td class="mono"><button type="button" class="row-stretch"
                                    aria-expanded=move || is_expanded().to_string()
                                    on:click=move |_| {
                                        expanded_summary.set(Some(run.get_untracked()));
                                        expanded_run.update(|v| *v = if *v == Some(run_id) { None } else { Some(run_id) });
                                    }
                                >{move || time_ago(&run.get().started_at, now.get())}</button>
                                    {move || (is_expanded() && runs.get().and_then(Result::ok).is_some_and(|(_, r)| !r.runs.iter().any(|r| r.id == run_id)))
                                        .then_some("Expanded run outside this page")}
                                </td>
                                <td>{move || view! { <StatusDot tone=super::run_status_tone(&run.get().status)/>{run.get().status} }}</td>
                                <td class="mono">{move || run.get().duration_ms.map_or_else(|| "—".to_string(), format_duration)}</td>
                                <td class="mono">{move || run.get().row_count.map_or_else(|| "—".to_string(), |n| n.to_string())}</td>
                                <td class="path">{move || run.get().error_message.unwrap_or_default()}</td>
                            </tr>
                            <Show when=is_expanded><tr><td colspan="5"><RunResultPreview net_id=net_id run_id=run_id bus=bus on_search=on_search summary=expanded_summary active=active/></td></tr></Show>
                        }
                    }/>
                </tbody></table>
                {move || runs.get().and_then(Result::ok).and_then(|(p, r)| r.runs.is_empty().then(|| view! {
                    <div class="tbl-empty">{if p == 0 && r.total == 0 { "No runs yet — attach a schedule to start." } else { "No runs on this page" }}</div>
                }))}
                <OffsetPager window=window on_page=Callback::new(move |p| page.set(p))/>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Expanded run result preview
// ---------------------------------------------------------------------------

#[component]
#[allow(unused_variables)]
fn RunResultPreview(
    active: Signal<bool>,
    summary: RwSignal<Option<trawl_api::ReportRunSummary>>,
    net_id: i64,
    run_id: i64,
    bus: ToastBus,
    on_search: Callback<String>,
) -> impl IntoView {
    let terminal = RwSignal::new(false);
    let (result, refresh_error, retry) = super::job_refresh::job_refresh(
        Signal::derive(move || active.get() && !terminal.get()),
        move || async move { api::get_run(net_id, run_id).await },
    );
    let preview_page = RwSignal::new(0usize);
    Effect::new(move |_| {
        if let Some(Ok(response)) = result.get() {
            // A running response must not write false back into the poll
            // gate: that notification would start another read immediately.
            if matches!(
                response.summary.status.as_str(),
                "success" | "error" | "timeout"
            ) {
                terminal.set(true);
            }
            summary.set(Some(response.summary));
        }
    });

    view! {
        <div class="run-preview">
            {move || refresh_error.get().map(|e| view! { <p role="status">{e}</p> })}
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(result.get()))
                label="result"
                retry=Callback::new(move |()| { retry.run(()); })
                render=Box::new(move |resp: trawl_api::ReportRunResponse| {
                    match resp.result {
                        None => view! {
                            <span style="color:var(--ink-3)">"No result data (error or still running)"</span>
                        }.into_any(),
                        Some(qr) => {
                            let query = resp.summary.query.clone();
                            let row_count = resp.summary.row_count;
                            view! {
                                <ResultPreviewTable result=qr row_count=row_count page=preview_page/>
                                <Btn
                                    variant=Variant::Secondary
                                    size=Size::Xs
                                    attr:style="margin-top:6px"
                                    on_click=Callback::new(move |()| on_search.run(query.clone()))
                                >"Run query again"</Btn>
                            }.into_any()
                        }
                    }
                })
            />
        </div>
    }
}

#[component]
fn ResultPreviewTable(
    result: QueryResult,
    row_count: Option<usize>,
    page: RwSignal<usize>,
) -> impl IntoView {
    let cols = result.columns.clone();
    let rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect();
    let total = rows.len();
    // Local to this expansion, so collapsing a run and opening it again
    // starts at page 1 rather than on a page the operator left behind.
    let cap = preview_cap(row_count, total);

    let window = Signal::derive(move || {
        let offset = PageWindow::checked_offset(page.get(), PREVIEW_PAGE_SIZE).unwrap_or(0);
        let returned = total.saturating_sub(offset).min(PREVIEW_PAGE_SIZE.get());
        PageWindow::new(
            page.get(),
            PREVIEW_PAGE_SIZE,
            returned,
            PageTotal::Known(total),
            false,
        )
        .expect("preview page comes from checked pager navigation")
    });

    view! {
        <table>
            <thead>
                <tr>
                    {cols.iter().map(|c| view! { <th scope="col">{c.name.clone()}</th> }).collect_view()}
                </tr>
            </thead>
            <tbody>
                {move || {
                    let offset = PageWindow::checked_offset(page.get(), PREVIEW_PAGE_SIZE).unwrap_or(0);
                    let end = offset.saturating_add(PREVIEW_PAGE_SIZE.get()).min(total);
                    rows.get(offset..end).unwrap_or_default().iter().map(|row| {
                        view! {
                            <tr>
                                {row.iter().map(|cell| view! { <td>{cell.clone()}</td> }).collect_view()}
                            </tr>
                        }
                    }).collect_view()
                }}
            </tbody>
        </table>
        <OffsetPager
            window=window
            on_page=Callback::new(move |p| page.set(p))
        />
        // Outside the paged slice: the gap between what the run stored
        // and what this response carries is a property of the response,
        // not of the page being read.
        {cap.map(|text| view! { <p class="preview-cap">{text}</p> })}
    }
}
