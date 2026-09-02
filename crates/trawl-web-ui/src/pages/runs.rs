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
use fleet_ui::time::{format_duration, time_ago};
use fleet_ui::{LoadState, Loaded, Pager, SearchInput, StatusDot};

const RUNS_PAGE_SIZE: usize = 20;

#[component]
#[allow(clippy::too_many_lines)]
pub fn RunsPage() -> impl IntoView {
    let page = RwSignal::new(0usize);
    let filter = RwSignal::new(String::new());

    let runs = LocalResource::new(move || {
        let p = page.get();
        let offset = p * RUNS_PAGE_SIZE;
        async move { api::list_all_runs(RUNS_PAGE_SIZE, offset).await }
    });

    let nets_for_stats = LocalResource::new(|| async move { api::list_saved().await });
    let stats = LocalResource::new(|| async move { api::runs_stats().await });

    let nav = use_navigate();

    let goto_net = {
        let nav = nav.clone();
        move |net_id: i64| {
            let url = format!("/jobs/nets?net={net_id}&ntab=runs");
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
                    <h1>"Runs"</h1>
                    <p class="sub">"Recent scheduled runs across all nets."</p>
                </div>
                <div class="actions">
                    <SearchInput value=filter placeholder="Filter by net…"/>
                </div>
            </div>

            <div class="stats-row">
                {move || {
                    let active_nets = nets_for_stats.get()
                        .and_then(Result::ok)
                        .map_or(0, |resp| {
                            resp.queries.iter()
                                .filter(|q| q.schedule.as_ref().is_some_and(|s| s.enabled))
                                .count()
                        });

                    let (success_rate, avg_dur) = stats.get()
                        .and_then(Result::ok)
                        .map_or(("—".to_string(), "—".to_string()), |s| {
                            let total = s.total_runs;
                            let rate = if total > 0 {
                                format!("{}%", s.success_count * 100 / total)
                            } else {
                                "—".to_string()
                            };
                            let avg = s.avg_duration_ms
                                .map_or_else(|| "—".to_string(), format_duration);
                            (rate, avg)
                        });

                    view! {
                        <div class="stat-card">
                            <div class="label">"Active nets"</div>
                            <div class="value">{active_nets.to_string()}</div>
                        </div>
                        <div class="stat-card">
                            <div class="label">"Success rate"</div>
                            <div class="value">{success_rate}</div>
                        </div>
                        <div class="stat-card">
                            <div class="label">"Avg duration"</div>
                            <div class="value">{avg_dur}</div>
                        </div>
                    }
                }}
            </div>

            <div class="tbl" style="margin-top:16px">
                <div class="tbl-hd">
                    <div style="flex:1">"Net"</div>
                    <div style="flex:0 0 70px">"Status"</div>
                    <div style="flex:0 0 80px">"When"</div>
                    <div style="flex:0 0 60px">"Duration"</div>
                    <div style="flex:0 0 50px; text-align:right">"Rows"</div>
                </div>
                <div class="tbl-body">
                    <Loaded
                        state=Signal::derive(move || LoadState::from_resource(runs.get()))
                        label="runs"
                        render=Box::new(move |resp: trawl_api::ListAllRunsResponse| {
                                let now = now_ms();
                                if resp.runs.is_empty() {
                                    return view! {
                                        <div class="tbl-empty">
                                            "No runs yet — attach a schedule to a net to get started"
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
                                    let tone = crate::components::run_status_tone(&status);
                                    let goto = goto.clone();

                                    view! {
                                        <div class="tbl-row" on:click=move |_| goto(net_id)>
                                            <div style="flex:1" class="mono">{net_name}</div>
                                            <div style="flex:0 0 70px">
                                                <StatusDot tone=tone/>
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
                                    <Pager
                                        summary=format!("{first}–{last} of {total}")
                                        can_prev=Signal::derive(move || page.get() != 0)
                                        can_next=Signal::derive(move || last < total)
                                        on_prev=Callback::new(move |()| {
                                            page.update(|p| *p = p.saturating_sub(1));
                                        })
                                        on_next=Callback::new(move |()| page.update(|p| *p += 1))
                                    />
                                }.into_any()
                        })
                    />
                </div>
            </div>
        </div>
    }
}
