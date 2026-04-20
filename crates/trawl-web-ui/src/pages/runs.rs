// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RunsPage/>` — global runs dashboard for the Jobs mode.
//!
//! Shows aggregate stats (active nets, success rate, avg duration) and a
//! paginated table of recent runs across all saved queries.

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::components::toast::ToastBus;
use crate::time_fmt::{format_duration, time_ago};

const RUNS_PAGE_SIZE: usize = 20;

#[component]
#[allow(clippy::too_many_lines)]
pub fn RunsPage(bus: ToastBus) -> impl IntoView {
    let _ = bus;
    let page = RwSignal::new(0usize);
    let filter = RwSignal::new(String::new());

    let runs = LocalResource::new(move || {
        let p = page.get();
        let offset = p * RUNS_PAGE_SIZE;
        async move { api::list_all_runs(RUNS_PAGE_SIZE, offset).await }
    });

    let nets_for_stats = LocalResource::new(|| async move { api::list_saved().await });

    let nav = use_navigate();

    let goto_net = {
        let nav = nav.clone();
        move |net_id: i64| {
            let url = format!("/search?app=jobs&section=nets&net={net_id}&ntab=runs");
            nav(
                &url,
                NavigateOptions {
                    replace: false,
                    ..Default::default()
                },
            );
        }
    };

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = move || js_sys::Date::now() as i64;

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Runs · Dashboard"</h1>
                    <p class="sub">"Recent scheduled runs across all nets."</p>
                </div>
                <div class="actions">
                    <div class="inp-wrap">
                        <SearchIcon/>
                        <input
                            placeholder="filter by net…"
                            prop:value=move || filter.get()
                            on:input=move |e| filter.set(event_target_value(&e))
                        />
                    </div>
                </div>
            </div>

            // Stats cards
            <div class="stats-row">
                {move || {
                    let active_nets = nets_for_stats.get()
                        .and_then(Result::ok)
                        .map_or(0, |resp| {
                            resp.queries.iter()
                                .filter(|q| q.schedule.as_ref().is_some_and(|s| s.enabled))
                                .count()
                        });

                    let (success_rate, avg_dur) = runs.get()
                        .and_then(Result::ok)
                        .map_or(("—".to_string(), "—".to_string()), |resp| {
                            if resp.runs.is_empty() {
                                return ("—".to_string(), "—".to_string());
                            }
                            let total = resp.runs.len();
                            let successes = resp.runs.iter()
                                .filter(|r| r.run.status == "success")
                                .count();
                            #[allow(clippy::manual_checked_ops)]
                            let rate = if total > 0 {
                                format!("{}%", successes * 100 / total)
                            } else {
                                "—".to_string()
                            };
                            let durations: Vec<u64> = resp.runs.iter()
                                .filter_map(|r| r.run.duration_ms)
                                .collect();
                            let avg = if durations.is_empty() {
                                "—".to_string()
                            } else {
                                format_duration(durations.iter().sum::<u64>() / durations.len() as u64)
                            };
                            (rate, avg)
                        });

                    view! {
                        <div class="stat-card">
                            <div class="label">"Active Nets"</div>
                            <div class="value">{active_nets.to_string()}</div>
                        </div>
                        <div class="stat-card">
                            <div class="label">"Success Rate"</div>
                            <div class="value">{success_rate}</div>
                        </div>
                        <div class="stat-card">
                            <div class="label">"Avg Duration"</div>
                            <div class="value">{avg_dur}</div>
                        </div>
                    }
                }}
            </div>

            // Runs table
            <div class="tbl" style="margin-top:16px">
                <div class="tbl-hd">
                    <div style="flex:1">"Net"</div>
                    <div style="flex:0 0 70px">"Status"</div>
                    <div style="flex:0 0 80px">"When"</div>
                    <div style="flex:0 0 60px">"Duration"</div>
                    <div style="flex:0 0 50px; text-align:right">"Rows"</div>
                </div>
                <div class="tbl-body">
                    {move || {
                        let now = now_ms();
                        match runs.get() {
                            None => view! {
                                <div class="tbl-empty">"loading runs…"</div>
                            }.into_any(),
                            Some(Err(e)) => {
                                let msg = e.to_string();
                                view! {
                                    <div class="tbl-empty" style="color:var(--red)">
                                        {format!("couldn't load runs: {msg}")}
                                    </div>
                                }.into_any()
                            }
                            Some(Ok(resp)) => {
                                if resp.runs.is_empty() {
                                    return view! {
                                        <div class="tbl-empty">
                                            "no runs yet — attach a schedule to a net to get started"
                                        </div>
                                    }.into_any();
                                }
                                let needle = filter.get().to_lowercase();
                                let visible: Vec<_> = resp.runs.iter()
                                    .filter(|r| {
                                        needle.is_empty() || r.net_name.to_lowercase().contains(&needle)
                                    })
                                    .collect();
                                let total = resp.total;
                                let p = page.get();
                                let first = p * RUNS_PAGE_SIZE + 1;
                                let last = (first - 1 + visible.len()).min(total);
                                let goto = goto_net.clone();

                                let rows = visible.into_iter().map(|gr| {
                                    let net_id = gr.net_id;
                                    let net_name = gr.net_name.clone();
                                    let when = time_ago(&gr.run.started_at, now);
                                    let dur = gr.run.duration_ms.map_or_else(|| "—".to_string(), format_duration);
                                    let row_ct = gr.run.row_count.map_or_else(|| "—".to_string(), |n| n.to_string());
                                    let status = gr.run.status.clone();
                                    let dot_class = match status.as_str() {
                                        "success" => "status-dot success",
                                        "error" | "timeout" => "status-dot error",
                                        "running" => "status-dot running",
                                        _ => "status-dot",
                                    };
                                    let goto = goto.clone();

                                    view! {
                                        <div class="tbl-row" on:click=move |_| goto(net_id)>
                                            <div style="flex:1" class="mono">{net_name}</div>
                                            <div style="flex:0 0 70px">
                                                <span class=dot_class></span>
                                                " "
                                                <span style="font-size:11px">{status}</span>
                                            </div>
                                            <div style="flex:0 0 80px" class="mono">{when}</div>
                                            <div style="flex:0 0 60px" class="mono">{dur}</div>
                                            <div style="flex:0 0 50px; text-align:right" class="mono">{row_ct}</div>
                                        </div>
                                    }
                                }).collect_view();

                                view! {
                                    {rows}
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
                                }.into_any()
                            }
                        }
                    }}
                </div>
            </div>
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
