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
use wasm_bindgen::JsCast;

use crate::api;
use crate::components::sparkline::Sparkline;
use crate::time_fmt::{format_duration, time_ago};
use fleet_ui::{ToastBus, ToastKind};

const RUNS_PAGE_SIZE: usize = 20;
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

    // -- inline rename --
    let editing_name = RwSignal::new(false);
    let name_buf = RwSignal::new(net.name.clone());
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

    let close = move || on_close.run(());
    let close_key = close;
    let close_scrim = close;
    let close_x = close;

    let on_keydown = move |e: web_sys::KeyboardEvent| {
        if e.key() == "Escape" {
            e.prevent_default();
            if editing_name.get_untracked() {
                editing_name.set(false);
                name_buf.set(original_name.clone());
            } else {
                close_key();
            }
        }
    };

    let tab_click = move |t: &'static str| {
        let cb = on_tab_change;
        move |_| cb.run(t.to_string())
    };
    let tab_query_click = tab_click("query");
    let tab_runs_click = tab_click("runs");

    let is_tab = move |want: &'static str| -> bool {
        let t = tab.get();
        let eff = if t.is_empty() { "query" } else { t.as_str() };
        eff == want
    };

    let on_run_click = {
        let q = query_for_run;
        let cb = on_search;
        move |_| cb.run(q.clone())
    };

    let on_trigger_click = move |_| {
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
    };

    view! {
        <div
            class="sd-scrim"
            tabindex="-1"
            on:mousedown=move |e: web_sys::MouseEvent| {
                if let Some(target) = e.target()
                    && let Some(el) = target.dyn_ref::<web_sys::Element>()
                    && el.class_name().contains("sd-scrim")
                {
                    close_scrim();
                }
            }
            on:keydown=on_keydown
        >
            <aside class="sd-drawer" role="dialog" aria-modal="true">
                <div class="sd-hd">
                    <div class="sd-ttl">
                        <Show
                            when=move || editing_name.get()
                            fallback={
                                let name = net.name.clone();
                                move || view! {
                                    <span
                                        class="name"
                                        title="Click to rename"
                                        style="cursor:pointer"
                                        on:click=move |_| editing_name.set(true)
                                    >{name.clone()}</span>
                                }
                            }
                        >
                            {
                                let do_rename = do_rename.clone();
                                let do_rename_blur = do_rename.clone();
                                view! {
                                    <input
                                        class="name-edit"
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
                    </div>
                    <div class="sd-actions">
                        <button class="btn-sec" on:click=on_run_click title="Open query in search">
                            "▶ Search"
                        </button>
                        <button class="btn-sec" on:click=on_trigger_click title="Trigger a scheduled run now">
                            "⏱ Run"
                        </button>
                        <span class="sd-x" title="Close (Esc)" on:click=move |_| close_x()>
                            <CloseIcon/>
                        </span>
                    </div>
                </div>

                <div class="sd-tabs">
                    <span
                        class=move || if is_tab("query") { "tb on" } else { "tb" }
                        on:click=tab_query_click
                    >"Query + Schedule"</span>
                    <span
                        class=move || if is_tab("runs") { "tb on" } else { "tb" }
                        on:click=tab_runs_click
                    >"Runs"</span>
                    <span class="sp"></span>
                </div>

                <div class="sd-body">
                    {move || {
                        if is_tab("runs") {
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
                </div>
            </aside>
        </div>
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

    let do_save_schedule = {
        move || {
            saving_schedule.set(true);
            let interval = interval_buf.get_untracked();
            let max_runs_str = max_runs_buf.get_untracked();
            let max_runs = max_runs_str.trim().parse::<u64>().ok();
            let enabled = enabled_buf.get_untracked();
            spawn_local(async move {
                match api::set_schedule(net_id, &interval, max_runs, enabled).await {
                    Ok(_) => {
                        bus.push(ToastKind::Success, "Schedule saved", None);
                        on_refresh.run(());
                    }
                    Err(e) => {
                        bus.push(ToastKind::Error, "Schedule failed", Some(e.to_string()));
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
                    <span class="ttl">"Query"</span>
                    <Show when=move || !editing.get()>
                        <button class="btn-sec btn-xs" on:click=move |_| editing.set(true)>"Edit"</button>
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
                        class="mono"
                        rows="4"
                        style="width:100%; resize:vertical; font-size:12px; padding:8px; background:var(--panel-2); border:1px solid var(--line); border-radius:3px; color:var(--ink)"
                        prop:value=move || query_buf.get()
                        on:input=move |e| query_buf.set(event_target_value(&e))
                    ></textarea>
                    <div style="display:flex; gap:6px; margin-top:6px">
                        <button
                            class="btn-pri btn-xs"
                            prop:disabled=move || saving_query.get()
                            on:click=move |_| do_save_query()
                        >{move || if saving_query.get() { "Saving…" } else { "Save" }}</button>
                        <button class="btn-sec btn-xs" on:click={
                            let reset_q = original_query.clone();
                            move |_| {
                                editing.set(false);
                                query_buf.set(reset_q.clone());
                            }
                        }>"Cancel"</button>
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
                        <button
                            class="btn-sec btn-xs"
                            style="margin-top:8px"
                            on:click=move |_| show_schedule_form.set(true)
                        >"+ Add Schedule"</button>
                    }
                >
                    <div style="display:flex; flex-direction:column; gap:10px">
                        // interval chips
                        <div>
                            <label class="field-label">"Interval"</label>
                            <div class="interval-chips">
                                {INTERVAL_PRESETS.iter().map(|preset| {
                                    let p = *preset;
                                    view! {
                                        <span
                                            class=move || if interval_buf.get() == p { "interval-chip on" } else { "interval-chip" }
                                            on:click=move |_| interval_buf.set(p.to_string())
                                        >{p}</span>
                                    }
                                }).collect_view()}
                            </div>
                            <input
                                class="mono"
                                style="margin-top:6px; width:80px; font-size:12px; padding:4px 6px; background:var(--panel-2); border:1px solid var(--line); border-radius:3px; color:var(--ink)"
                                placeholder="custom…"
                                prop:value=move || {
                                    let v = interval_buf.get();
                                    if INTERVAL_PRESETS.contains(&v.as_str()) { String::new() } else { v }
                                }
                                on:input=move |e| interval_buf.set(event_target_value(&e))
                            />
                        </div>

                        // max runs
                        <div>
                            <label class="field-label">"Max runs "</label>
                            <span style="color:var(--ink-3); font-size:11px">"(blank = unlimited)"</span>
                            <input
                                class="mono"
                                type="number"
                                min="1"
                                style="display:block; margin-top:4px; width:80px; font-size:12px; padding:4px 6px; background:var(--panel-2); border:1px solid var(--line); border-radius:3px; color:var(--ink)"
                                prop:value=move || max_runs_buf.get()
                                on:input=move |e| max_runs_buf.set(event_target_value(&e))
                            />
                        </div>

                        // enabled toggle
                        <div style="display:flex; align-items:center; gap:8px">
                            <label class="toggle">
                                <input
                                    type="checkbox"
                                    prop:checked=move || enabled_buf.get()
                                    on:change=move |e| {
                                        let Some(el) = e.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
                                        enabled_buf.set(el.checked());
                                    }
                                />
                                <span class="toggle-slider"></span>
                            </label>
                            <span style="font-size:12px; color:var(--ink-2)">
                                {move || if enabled_buf.get() { "Active" } else { "Paused" }}
                            </span>
                        </div>

                        // actions
                        <div style="display:flex; gap:6px; align-items:center">
                            <button
                                class="btn-pri btn-xs"
                                prop:disabled=move || saving_schedule.get()
                                on:click=move |_| do_save_schedule()
                            >{move || if saving_schedule.get() { "Saving…" } else { "Save Schedule" }}</button>
                            {has_schedule.then(|| {
                                view! {
                                    <button
                                        class="btn-sec btn-xs"
                                        style="color:var(--red)"
                                        on:click=move |_| do_delete_schedule()
                                    >"Remove Schedule"</button>
                                }
                            })}
                        </div>
                    </div>
                </Show>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Tab 2: Runs
// ---------------------------------------------------------------------------

#[component]
fn RunsPane(net_id: i64, bus: ToastBus, on_search: Callback<String>) -> impl IntoView {
    let page = RwSignal::new(0usize);
    let expanded_run: RwSignal<Option<i64>> = RwSignal::new(None);

    let runs = LocalResource::new(move || {
        let p = page.get();
        let offset = p * RUNS_PAGE_SIZE;
        async move { api::list_runs(net_id, RUNS_PAGE_SIZE, offset).await }
    });

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = move || js_sys::Date::now() as i64;

    view! {
        <div>
            // Row count sparkline (trend over recent runs)
            {move || {
                let data = runs.get()
                    .and_then(Result::ok)
                    .map(|resp| {
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

            {move || {
                let now = now_ms();
                match runs.get() {
                    None => view! { <div style="padding:12px; color:var(--ink-3)">"loading runs…"</div> }.into_any(),
                    Some(Err(e)) => {
                        let msg = e.to_string();
                        view! {
                            <div style="padding:12px; color:var(--red)">{format!("couldn't load runs: {msg}")}</div>
                        }.into_any()
                    }
                    Some(Ok(resp)) => {
                        if resp.runs.is_empty() {
                            return view! {
                                <div style="padding:12px; color:var(--ink-3)">"no runs yet — attach a schedule to start."</div>
                            }.into_any();
                        }
                        let total = resp.total;
                        let p = page.get();
                        let first = p * RUNS_PAGE_SIZE + 1;
                        let last = (first - 1 + resp.runs.len()).min(total);

                        let rows = resp.runs.iter().map(|run| {
                            let run_id = run.id;
                            let when = time_ago(&run.started_at, now);
                            let dur = run.duration_ms.map_or_else(|| "—".to_string(), format_duration);
                            let row_ct = run.row_count.map_or_else(|| "—".to_string(), |n| n.to_string());
                            let status = run.status.clone();
                            let dot_class = match status.as_str() {
                                "success" => "status-dot success",
                                "error" | "timeout" => "status-dot error",
                                "running" => "status-dot running",
                                _ => "status-dot",
                            };
                            let err_msg = run.error_message.clone().unwrap_or_default();
                            let is_expanded = move || expanded_run.get() == Some(run_id);

                            view! {
                                <div
                                    class="tbl-row"
                                    on:click=move |_| {
                                        expanded_run.update(|v| {
                                            *v = if *v == Some(run_id) { None } else { Some(run_id) };
                                        });
                                    }
                                >
                                    <div style="flex:0 0 80px" class="mono">{when}</div>
                                    <div style="flex:0 0 70px">
                                        <span class=dot_class></span>
                                        " "
                                        <span style="font-size:11px">{status}</span>
                                    </div>
                                    <div style="flex:0 0 60px" class="mono">{dur}</div>
                                    <div style="flex:0 0 50px; text-align:right" class="mono">{row_ct}</div>
                                    <div style="flex:1; min-width:0; color:var(--red); font-size:11px" class="path">{err_msg}</div>
                                </div>
                                <Show when=is_expanded>
                                    <RunResultPreview
                                        net_id=net_id
                                        run_id=run_id
                                        bus=bus
                                        on_search=on_search
                                    />
                                </Show>
                            }
                        }).collect_view();

                        view! {
                            <div class="tbl">
                                <div class="tbl-hd" style="font-size:11px">
                                    <div style="flex:0 0 80px">"When"</div>
                                    <div style="flex:0 0 70px">"Status"</div>
                                    <div style="flex:0 0 60px">"Duration"</div>
                                    <div style="flex:0 0 50px; text-align:right">"Rows"</div>
                                    <div style="flex:1">"Error"</div>
                                </div>
                                <div class="tbl-body">
                                    {rows}
                                </div>
                                <div class="tbl-foot">
                                    <span>{format!("{first}–{last} of {total}")}</span>
                                    <span style="display:flex; gap:4px">
                                        <button
                                            class="btn-sec btn-xs"
                                            prop:disabled=move || page.get() == 0
                                            on:click=move |_| page.update(|p| *p = p.saturating_sub(1))
                                        >"← prev"</button>
                                        <button
                                            class="btn-sec btn-xs"
                                            prop:disabled=move || last >= total
                                            on:click=move |_| page.update(|p| *p += 1)
                                        >"next →"</button>
                                    </span>
                                </div>
                            </div>
                        }.into_any()
                    }
                }
            }}
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
            {move || match result.get() {
                None => view! { <span style="color:var(--ink-3)">"loading result…"</span> }.into_any(),
                Some(Err(e)) => {
                    let msg = e.to_string();
                    view! { <span style="color:var(--red)">{msg}</span> }.into_any()
                }
                Some(Ok(resp)) => {
                    match resp.result {
                        None => view! {
                            <span style="color:var(--ink-3)">"no result data (error or still running)"</span>
                        }.into_any(),
                        Some(qr) => {
                            let query = resp.summary.query.clone();
                            view! {
                                <ResultPreviewTable result=qr/>
                                <button
                                    class="btn-sec btn-xs"
                                    style="margin-top:6px"
                                    on:click=move |_| on_search.run(query.clone())
                                >"View full results →"</button>
                            }.into_any()
                        }
                    }
                }
            }}
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
                    {cols.iter().map(|c| view! { <th>{c.name.clone()}</th> }).collect_view()}
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
                {format!("showing {RESULT_PREVIEW_ROWS} of {total_rows} rows")}
            </div>
        })}
    }
}

#[component]
fn CloseIcon() -> impl IntoView {
    view! {
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="M4 4l8 8M12 4l-8 8"/>
        </svg>
    }
}
