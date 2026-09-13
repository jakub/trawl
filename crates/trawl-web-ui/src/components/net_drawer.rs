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

use crate::schedule_edit::{WindowDraft, WindowMode, validate_max_runs};

use crate::api::RUNS_PAGE_SIZE;
const RESULT_PREVIEW_ROWS: usize = 20;

const INTERVAL_PRESETS: &[&str] = &["5m", "15m", "1h", "6h", "24h", "1w"];

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn NetDrawer(
    net: SavedQueryResponse,
    tab: Signal<String>,
    on_close: Callback<()>,
    on_tab_change: Callback<String>,
    on_search: Callback<String>,
    on_refresh: Callback<()>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let net_for_query = net.clone();
    let net_for_runs = net.clone();
    let query_for_run = net.query.clone();
    let net_id_for_trigger = net.id;
    let name_for_trigger = net.name.clone();
    // A windowed schedule computes each run's bounds from the last one
    // it covered, so a manual run out of band moves that point and
    // leaves a hole the schedule will not revisit. The offer is withdrawn
    // from the SAVED state, never from an unsaved draft of it.
    let manual_run_allowed = net
        .schedule
        .as_ref()
        .and_then(|s| s.window.as_ref())
        .is_none();

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

    let do_rename = {
        let net_id = net.id;
        let original_query = net.query.clone();
        let orig = original_name.clone();
        move || {
            let new_name = name_buf.get_untracked().trim().to_string();
            if new_name.is_empty() || new_name == orig {
                editing_name.set(false);
                name_buf.set(orig.clone());
                return;
            }
            let q = original_query.clone();
            spawn_local(async move {
                match api::update_saved_full(net_id, &q, Some(&new_name)).await {
                    Ok(_) => {
                        bus.push(ToastKind::Success, "Renamed", None);
                        editing_name.set(false);
                        on_refresh.run(());
                    }
                    Err(e) => {
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
                name_buf.set(orig.clone());
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
        Callback::new(move |()| cb.run(q.clone()))
    };

    let on_trigger_click = Callback::new(move |()| {
        let name = name_for_trigger.clone();
        let id = net_id_for_trigger;
        spawn_local(async move {
            match api::trigger_run(id).await {
                Ok(_) => {
                    bus.push(
                        ToastKind::Success,
                        "Run triggered",
                        Some(format!("'{name}' is executing.")),
                    );
                    on_refresh.run(());
                }
                Err(e) => {
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
                        let name = net.name.clone();
                        move || view! {
                            <button
                                type="button"
                                class="name"
                                node_ref=name_btn_ref
                                aria-label=format!("Rename {name}")
                                // The title looks like the drawer's
                                // heading, so nothing but the tooltip
                                // tells a pointer user it can be edited.
                                title="Rename"
                                on:click=move |_| editing_name.set(true)
                            >{name.clone()}</button>
                        }
                    }
                >
                    {
                        let do_rename = do_rename.clone();
                        let do_rename_blur = do_rename.clone();
                        // The input REPLACES the heading it edits, so the
                        // name it is editing is nowhere on screen to label
                        // it: without this the field is an unnamed textbox.
                        let name = net.name.clone();
                        view! {
                            <input
                                class="name-edit"
                                node_ref=name_input_ref
                                aria-label=format!("New name for {name}")
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
                {manual_run_allowed.then(|| view! {
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
            {move || {
                if eff_tab.get() == "runs" {
                    view! {
                        <RunsPane
                            net_id=net_for_runs.id
                            bus=bus
                            on_search=on_search
                        />
                    }.into_any()
                } else {
                    view! {
                        <QuerySchedulePane
                            net=net_for_query.clone()
                            bus=bus
                            on_refresh=on_refresh
                        />
                    }.into_any()
                }
            }}
        </Drawer>
    }
}

// ---------------------------------------------------------------------------
// Tab 1: Query + Schedule
// ---------------------------------------------------------------------------

#[component]
fn QuerySchedulePane(
    net: SavedQueryResponse,
    bus: ToastBus,
    on_refresh: Callback<()>,
) -> impl IntoView {
    let net_id = net.id;

    // -- query editing --
    let editing = RwSignal::new(false);
    let query_buf = RwSignal::new(net.query.clone());
    let saving_query = RwSignal::new(false);
    let original_query = net.query.clone();

    let do_save_query = {
        move || {
            saving_query.set(true);
            let q = query_buf.get_untracked();
            spawn_local(async move {
                match api::update_saved(net_id, &q).await {
                    Ok(_) => {
                        bus.push(ToastKind::Success, "Query updated", None);
                        editing.set(false);
                        on_refresh.run(());
                    }
                    Err(e) => {
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
    let saved_window = net.schedule.as_ref().and_then(|s| s.window.clone());
    let saved_covered = net
        .schedule
        .as_ref()
        .and_then(|s| s.covered_through.clone());
    let saved_interval = net.schedule.as_ref().map(|s| s.interval.clone());
    // A local refusal is not a failed request, so it stays in the form
    // next to the Save button instead of flying past as a toast.
    let save_error: RwSignal<Option<String>> = RwSignal::new(None);

    let do_save_schedule = {
        move || {
            save_error.set(None);
            let max_runs = match validate_max_runs(&max_runs_buf.get_untracked()) {
                Ok(max_runs) => max_runs,
                Err(e) => {
                    save_error.set(Some(e.to_string()));
                    return;
                }
            };
            let (window, lag) = match window_draft.get_untracked().to_request() {
                Ok(pair) => pair,
                Err(e) => {
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
                        bus.push(ToastKind::Success, "Schedule saved", None);
                        on_refresh.run(());
                    }
                    Err(e) => {
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
            spawn_local(async move {
                match api::delete_schedule(net_id).await {
                    Ok(_) => {
                        bus.push(ToastKind::Success, "Schedule removed", None);
                        show_schedule_form.set(false);
                        on_refresh.run(());
                    }
                    Err(e) => {
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
                        let q = original_query.clone();
                        move || view! { <pre class="preview mono">{q.clone()}</pre> }
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
                            disabled=saving_query
                            on_click=Callback::new(move |()| do_save_query())
                        >{move || if saving_query.get() { "Saving…" } else { "Save" }}</Btn>
                        <Btn variant=Variant::Secondary size=Size::Xs on_click={
                            let reset_q = original_query.clone();
                            Callback::new(move |()| {
                                editing.set(false);
                                query_buf.set(reset_q.clone());
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
                                type="number"
                                id="net-max-runs"
                                aria-describedby="net-max-runs-help"
                                min="1"
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
                            let had_window = saved_window.is_some();
                            let saved_covered = saved_covered.clone();
                            move || {
                                (had_window && window_draft.get().mode == WindowMode::Query).then(|| {
                                    let coverage = saved_covered.clone().map(|t| format!(
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
                            let seed = seed.clone();
                            let saved_interval = saved_interval.clone();
                            move || {
                                let draft = window_draft.get();
                                let moved = draft.mode != seed.mode
                                    || draft.span != seed.span
                                    || saved_interval.as_ref().is_some_and(|i| *i != interval_buf.get());
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
                                disabled=saving_schedule
                                on_click=Callback::new(move |()| do_save_schedule())
                            >{move || if saving_schedule.get() { "Saving…" } else { "Save schedule" }}</Btn>
                            {has_schedule.then(|| {
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
fn RunsPane(net_id: i64, bus: ToastBus, on_search: Callback<String>) -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let page = RwSignal::new(0usize);
    let expanded_run: RwSignal<Option<i64>> = RwSignal::new(None);

    let pending = RwSignal::new(false);
    let runs = LocalResource::new(move || {
        let p = page.get();
        async move {
            let offset = PageWindow::checked_offset(p, RUNS_PAGE_SIZE)
                .map_err(|_| api::ApiError::Refused("This runs page is too large to request."))?;
            let _ = pending.try_set(true);
            let response = api::list_runs(net_id, RUNS_PAGE_SIZE.get(), offset).await;
            let _ = pending.try_set(false);
            response.map(|resp| (p, resp))
        }
    });

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = move || js_sys::Date::now() as i64;

    view! {
        <div>
            {move || {
                let data = runs.get()
                    .and_then(Result::ok)
                    .map(|(_, resp)| {
                        resp.runs.iter()
                            .rev()
                            .map(|r| r.row_count.unwrap_or(0) as u64)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if data.len() >= 2 {
                    view! {
                        <div style="padding:8px 12px; display:flex; align-items:center; gap:8px">
                            <span style="font-size:11px; color:var(--ink-3)">"Row count trend"</span>
                            <Sparkline data=data color="var(--blue)".to_string() w=180 h=28/>
                        </div>
                    }.into_any()
                } else {
                    view! { <div></div> }.into_any()
                }
            }}

            <Loaded
                state=Signal::derive(move || LoadState::from_resource(runs.get()))
                label="runs"
                retry=Callback::new(move |()| { runs.set(None); runs.refetch(); })
                render=Box::new(move |(fetched_page, resp): (usize, trawl_api::ListReportRunsResponse)| {
                        let now = now_ms();
                        let returned = resp.runs.len();
                        let total = resp.total;
                        let window = Signal::derive(move || PageWindow::new(
                            fetched_page, RUNS_PAGE_SIZE, returned, PageTotal::Known(total),
                            pending.get() || page.get() != fetched_page,
                        ).expect("runs page comes from checked pager navigation"));

                        let rows = resp.runs.iter().map(|run| {
                            let run_id = run.id;
                            let when = time_ago(&run.started_at, now);
                            let dur = run.duration_ms.map_or_else(|| "—".to_string(), format_duration);
                            let row_ct = run.row_count.map_or_else(|| "—".to_string(), |n| n.to_string());
                            let status = run.status.clone();
                            let tone = super::run_status_tone(&status);
                            let err_msg = run.error_message.clone().unwrap_or_default();
                            let is_expanded = move || expanded_run.get() == Some(run_id);

                            view! {
                                <tr class="tbl-row">
                                    <td class="mono">
                                        // The row's one control (ADR-0029),
                                        // stretched over the row: a pointer
                                        // anywhere on it toggles the run's
                                        // result preview exactly once.
                                        <button
                                            type="button"
                                            class="row-stretch"
                                            aria-expanded=move || is_expanded().to_string()
                                            on:click=move |_| {
                                                expanded_run.update(|v| {
                                                    *v = if *v == Some(run_id) { None } else { Some(run_id) };
                                                });
                                            }
                                        >{when}</button>
                                    </td>
                                    <td>
                                        <StatusDot tone=tone/>
                                        " "
                                        <span style="font-size:11px">{status}</span>
                                    </td>
                                    <td class="mono">{dur}</td>
                                    <td style="text-align:right" class="mono">{row_ct}</td>
                                    <td style="min-width:0; color:var(--red); font-size:11px" class="path">{err_msg}</td>
                                </tr>
                                <Show when=is_expanded>
                                    <tr><td colspan="5"><RunResultPreview
                                        net_id=net_id
                                        run_id=run_id
                                        bus=bus
                                        on_search=on_search
                                    /></td></tr>
                                </Show>
                            }
                        }).collect_view();

                        view! {
                            <fleet_ui::OverflowHint viewport=table_viewport/>
                            <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Net runs" tabindex="0" style="--list-min-width:480px">
                                <table class="fleet-table run-preview-table" aria-label="Net runs"><thead><tr>
                                    <th scope="col" style="width:100px">"When"</th>
                                    <th scope="col" style="width:90px">"Status"</th>
                                    <th scope="col" style="width:80px">"Duration"</th>
                                    <th scope="col" style="width:70px; text-align:right">"Rows"</th>
                                    <th scope="col">"Error"</th>
                                </tr></thead><tbody>
                                    {rows}
                                </tbody></table>
                                {if returned == 0 { Some(view! {
                                    <div class="tbl-empty">{if total == 0 && fetched_page == 0 {
                                            "No runs yet — attach a schedule to start."
                                        } else { "No runs on this page" }}</div>
                                }) } else { None }}
                                <OffsetPager
                                    window=window
                                    on_page=Callback::new(move |p| page.set(p))
                                />
                            </div>
                        }.into_any()
                })
            />
        </div>
    }
}

// ---------------------------------------------------------------------------
// Expanded run result preview
// ---------------------------------------------------------------------------

#[component]
#[allow(unused_variables)]
fn RunResultPreview(
    net_id: i64,
    run_id: i64,
    bus: ToastBus,
    on_search: Callback<String>,
) -> impl IntoView {
    let result = LocalResource::new(move || async move { api::get_run(net_id, run_id).await });

    view! {
        <div class="run-preview">
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(result.get()))
                label="result"
                retry=Callback::new(move |()| { result.set(None); result.refetch(); })
                render=Box::new(move |resp: trawl_api::ReportRunResponse| {
                    match resp.result {
                        None => view! {
                            <span style="color:var(--ink-3)">"No result data (error or still running)"</span>
                        }.into_any(),
                        Some(qr) => {
                            let query = resp.summary.query.clone();
                            view! {
                                <ResultPreviewTable result=qr/>
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
fn ResultPreviewTable(result: QueryResult) -> impl IntoView {
    let cols = result.columns.clone();
    let rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .take(RESULT_PREVIEW_ROWS)
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect();
    let total_rows = result.rows.len();
    let truncated = total_rows > RESULT_PREVIEW_ROWS;

    view! {
        <table>
            <thead>
                <tr>
                    {cols.iter().map(|c| view! { <th scope="col">{c.name.clone()}</th> }).collect_view()}
                </tr>
            </thead>
            <tbody>
                {rows.into_iter().map(|row| {
                    view! {
                        <tr>
                            {row.into_iter().map(|cell| view! { <td>{cell}</td> }).collect_view()}
                        </tr>
                    }
                }).collect_view()}
            </tbody>
        </table>
        {truncated.then(|| view! {
            <div style="font-size:11px; color:var(--ink-3); margin-top:4px">
                {format!("Showing {RESULT_PREVIEW_ROWS} of {total_rows} rows")}
            </div>
        })}
    }
}
