// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<NetsPage/>` — saved query ("net") management for the Jobs mode.
//!
//! URL params:
//! - `net=<id>` — opens the detail drawer for that net
//! - `ntab=query|runs` — active drawer tab (default: query)

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::web_sys;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use trawl_api::SavedQueryResponse;

use crate::api;
use crate::components::net_drawer::NetDrawer;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::components::toast::{ToastBus, ToastKind};
use crate::state::query::{Mode, RangeSpec, navigator};
use crate::time_fmt::time_ago;

#[component]
#[allow(clippy::too_many_lines)]
pub fn NetsPage(bus: ToastBus) -> impl IntoView {
    let qm = use_query_map();
    let net_selected: Memo<Option<i64>> =
        Memo::new(move |_| qm.get().get("net").and_then(|s| s.parse::<i64>().ok()));
    let tab_param = Memo::new(move |_| {
        qm.get()
            .get("ntab")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "query".to_string())
    });
    let tab_sig: Signal<String> = Signal::derive(move || tab_param.get());

    let filter = RwSignal::new(String::new());
    let refresh = RwSignal::new(0u64);
    let show_create_modal = RwSignal::new(false);
    let actions_open: RwSignal<Option<i64>> = RwSignal::new(None);

    let nets = LocalResource::new(move || {
        let _ = refresh.get();
        async move { api::list_saved().await }
    });

    let nav = use_navigate();
    let goto_search = navigator();

    let push_net = {
        let nav = nav.clone();
        move |id: Option<i64>, ntab: &str| {
            let url = match id {
                Some(n) => format!("/search?app=jobs&section=nets&net={n}&ntab={ntab}"),
                None => "/search?app=jobs&section=nets".to_string(),
            };
            nav(
                &url,
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
        }
    };

    let on_open: Callback<i64> = {
        let push = push_net.clone();
        Callback::new(move |id: i64| push(Some(id), "query"))
    };
    let on_tab_change: Callback<String> = {
        let push = push_net.clone();
        Callback::new(move |ntab: String| {
            let id = net_selected.get_untracked();
            push(id, &ntab);
        })
    };
    let on_close: Callback<()> = {
        let push = push_net.clone();
        Callback::new(move |()| push(None, "query"))
    };
    let on_refresh: Callback<()> = Callback::new(move |()| {
        refresh.update(|n| *n += 1);
    });
    let on_search: Callback<String> = {
        let goto = goto_search.clone();
        Callback::new(move |q: String| {
            goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false);
        })
    };

    let on_delete = {
        let bus = bus.clone();
        move |id: i64, name: String| {
            let bus = bus.clone();
            spawn_local(async move {
                match api::delete_saved(id).await {
                    Ok(_) => {
                        bus.push(
                            ToastKind::Success,
                            "Net deleted",
                            Some(format!("'{name}' has been removed.")),
                        );
                        refresh.update(|n| *n += 1);
                    }
                    Err(e) => {
                        bus.push(ToastKind::Error, "Delete failed", Some(e.to_string()));
                    }
                }
            });
        }
    };

    let on_run_now = {
        let goto = goto_search.clone();
        move |q: String| {
            goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false);
        }
    };

    let now_ms = move || js_sys::Date::now() as i64;

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Nets · Saved Queries"</h1>
                    <p class="sub">"Manage saved queries, attach schedules, and inspect run history."</p>
                </div>
                <div class="actions">
                    <div class="inp-wrap">
                        <SearchIcon/>
                        <input
                            placeholder="filter nets…"
                            prop:value=move || filter.get()
                            on:input=move |e| filter.set(event_target_value(&e))
                        />
                    </div>
                    <button class="btn-pri" on:click=move |_| show_create_modal.set(true)>
                        "+ New net"
                    </button>
                </div>
            </div>

            <div class="tbl">
                <div class="tbl-hd">
                    <div style="flex:2">"Name"</div>
                    <div style="flex:3">"Query"</div>
                    <div style="flex:0 0 80px">"Schedule"</div>
                    <div style="flex:0 0 100px">"Last Run"</div>
                    <div style="flex:0 0 40px"></div>
                </div>
                <div class="tbl-body">
                    {move || {
                        let now = now_ms();
                        match nets.get() {
                            None => view! {
                                <div class="tbl-empty">"loading nets…"</div>
                            }.into_any(),
                            Some(Err(e)) => {
                                let msg = e.to_string();
                                view! {
                                    <div class="tbl-empty" style="color:var(--red)">
                                        {format!("couldn't load nets: {msg}")}
                                    </div>
                                }.into_any()
                            }
                            Some(Ok(resp)) => {
                                let needle = filter.get().to_lowercase();
                                let visible: Vec<&SavedQueryResponse> = resp.queries.iter()
                                    .filter(|q| {
                                        if needle.is_empty() { return true; }
                                        q.name.to_lowercase().contains(&needle)
                                            || q.query.to_lowercase().contains(&needle)
                                    })
                                    .collect();
                                if visible.is_empty() {
                                    return view! {
                                        <div class="tbl-empty">
                                            {if resp.queries.is_empty() {
                                                "no nets yet — save a query from the search page to get started"
                                            } else {
                                                "no nets match that filter"
                                            }}
                                        </div>
                                    }.into_any();
                                }
                                let count = visible.len();
                                let on_open = on_open.clone();
                                let rows = visible.into_iter().map(|net| {
                                    let id = net.id;
                                    let name = net.name.clone();
                                    let query_text = net.query.clone();
                                    let name_for_delete = net.name.clone();
                                    let query_for_run = net.query.clone();
                                    let on_delete = on_delete.clone();
                                    let on_run_now = on_run_now.clone();

                                    let sched_badge = match &net.schedule {
                                        Some(s) if s.enabled => {
                                            view! {
                                                <span class="sched-badge active">{format!("⏰ {}", s.interval)}</span>
                                            }.into_any()
                                        }
                                        Some(s) => {
                                            view! {
                                                <span class="sched-badge disabled">{format!("⏸ {}", s.interval)}</span>
                                            }.into_any()
                                        }
                                        None => view! {
                                            <span style="color:var(--ink-3)">"—"</span>
                                        }.into_any(),
                                    };

                                    let last_run_view = match net.schedule.as_ref().and_then(|s| s.last_run.as_ref()) {
                                        Some(run) => {
                                            let when = time_ago(&run.started_at, now);
                                            let dot_class = match run.status.as_str() {
                                                "success" => "status-dot success",
                                                "error" | "timeout" => "status-dot error",
                                                "running" => "status-dot running",
                                                _ => "status-dot",
                                            };
                                            view! {
                                                <span>
                                                    <span class=dot_class></span>
                                                    " "
                                                    <span class="mono" style="color:var(--ink-2)">{when}</span>
                                                </span>
                                            }.into_any()
                                        }
                                        None => view! {
                                            <span style="color:var(--ink-3)">"—"</span>
                                        }.into_any(),
                                    };

                                    view! {
                                        <div
                                            class="tbl-row"
                                            on:click=move |_| on_open.run(id)
                                        >
                                            <div style="flex:2" class="mono">{name.clone()}</div>
                                            <div style="flex:3; min-width:0" class="mono path">{query_text}</div>
                                            <div style="flex:0 0 80px">{sched_badge}</div>
                                            <div style="flex:0 0 100px">{last_run_view}</div>
                                            <div style="flex:0 0 40px; position:relative">
                                                <button
                                                    class="btn-icon"
                                                    on:click=move |e: web_sys::MouseEvent| {
                                                        e.stop_propagation();
                                                        actions_open.update(|v| {
                                                            *v = if *v == Some(id) { None } else { Some(id) };
                                                        });
                                                    }
                                                >"⋯"</button>
                                                <Show when=move || actions_open.get() == Some(id)>
                                                    <div class="actions-menu">
                                                        <div
                                                            class="item"
                                                            on:click={
                                                                let q = query_for_run.clone();
                                                                let run = on_run_now.clone();
                                                                move |e: web_sys::MouseEvent| {
                                                                    e.stop_propagation();
                                                                    actions_open.set(None);
                                                                    run(q.clone());
                                                                }
                                                            }
                                                        >"▶ Run now"</div>
                                                        <div
                                                            class="item danger"
                                                            on:click={
                                                                let name = name_for_delete.clone();
                                                                let del = on_delete.clone();
                                                                move |e: web_sys::MouseEvent| {
                                                                    e.stop_propagation();
                                                                    actions_open.set(None);
                                                                    del(id, name.clone());
                                                                }
                                                            }
                                                        >"Delete"</div>
                                                    </div>
                                                </Show>
                                            </div>
                                        </div>
                                    }
                                }).collect_view();
                                view! {
                                    {rows}
                                    <div class="tbl-foot">
                                        <span>{format!("{count} net{}", if count == 1 { "" } else { "s" })}</span>
                                    </div>
                                }.into_any()
                            }
                        }
                    }}
                </div>
            </div>

            // Drawer — rendered when ?net=<id> is present
            {move || {
                let id = net_selected.get()?;
                let resp = nets.get()?.ok()?;
                let net = resp.queries.iter().find(|q| q.id == id)?.clone();
                Some(view! {
                    <NetDrawer
                        net=net
                        tab=tab_sig
                        bus=bus.clone()
                        on_close=on_close
                        on_tab_change=on_tab_change
                        on_search=on_search.clone()
                        on_refresh=on_refresh
                    />
                })
            }}

            // Create modal
            <Show when=move || show_create_modal.get()>
                <SaveAsNetModal
                    query=Signal::derive(|| String::new())
                    bus=bus.clone()
                    on_close=Callback::new(move |saved: bool| {
                        show_create_modal.set(false);
                        if saved { refresh.update(|n| *n += 1); }
                    })
                />
            </Show>
        </div>
    }
}

#[component]
fn SearchIcon() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <circle cx="7" cy="7" r="4.5"/>
            <path d="m10.5 10.5 3 3"/>
        </svg>
    }
}
