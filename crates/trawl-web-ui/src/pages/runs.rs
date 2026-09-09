// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RunsPage/>` — global runs dashboard for the Jobs mode.
//!
//! Shows aggregate stats (active nets, success rate, avg duration) and a
//! paginated table of recent runs across all saved queries.

use leptos::prelude::*;

use crate::api::{self, RUNS_PAGE_SIZE};
use fleet_ui::time::{format_duration, time_ago};
use fleet_ui::{LoadState, Loaded, OffsetPager, PageTotal, PageWindow, SearchInput, StatusDot};

#[component]
#[allow(clippy::too_many_lines)]
pub fn RunsPage() -> impl IntoView {
    let page = RwSignal::new(0usize);
    let filter = RwSignal::new(String::new());

    let pending = RwSignal::new(false);
    let runs = LocalResource::new(move || {
        let p = page.get();
        async move {
            let offset = PageWindow::checked_offset(p, RUNS_PAGE_SIZE)
                .map_err(|_| api::ApiError::Refused("This runs page is too large to request."))?;
            let _ = pending.try_set(true);
            let response = api::list_all_runs(RUNS_PAGE_SIZE.get(), offset).await;
            let _ = pending.try_set(false);
            response.map(|resp| (p, resp))
        }
    });

    let nets_for_stats = LocalResource::new(|| async move { api::list_saved().await });
    let stats = LocalResource::new(|| async move { api::runs_stats().await });

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
                            let rate = (s.success_count * 100)
                                .checked_div(total)
                                .map_or_else(|| "—".to_string(), |pct| format!("{pct}%"));
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
                        render=Box::new(move |(fetched_page, resp): (usize, trawl_api::ListAllRunsResponse)| {
                                let now = now_ms();
                                let needle = filter.get().to_lowercase();
                                let visible: Vec<_> = resp.runs.iter()
                                    .filter(|r| {
                                        needle.is_empty() || r.net_name.to_lowercase().contains(&needle)
                                    })
                                    .collect();
                                let returned = resp.runs.len();
                                let total = resp.total;
                                let matches = visible.len();
                                let suffix = if needle.is_empty() { String::new() }
                                    else { format!(" · {matches} matches on this page") };
                                let window = Signal::derive(move || PageWindow::new(
                                    fetched_page, RUNS_PAGE_SIZE, returned, PageTotal::Known(total),
                                    pending.get() || page.get() != fetched_page,
                                ).expect("runs page comes from checked pager navigation"));

                                let rows = visible.into_iter().map(|gr| {
                                    let net_id = gr.net_id;
                                    let net_name = gr.net_name.clone();
                                    let when = time_ago(&gr.run.started_at, now);
                                    let dur = gr.run.duration_ms.map_or_else(|| "—".to_string(), format_duration);
                                    let row_ct = gr.run.row_count.map_or_else(|| "—".to_string(), |n| n.to_string());
                                    let run_status = gr.run.status.clone();
                                    let tone = crate::components::run_status_tone(&run_status);
                                    // The net's Runs drawer is a place with a
                                    // URL, and this one pushes: browser Back
                                    // returns to the runs list.
                                    let href = format!("/jobs/nets?net={net_id}&ntab=runs");

                                    view! {
                                        <div class="tbl-row">
                                            <div style="flex:1" class="mono">
                                                <a class="row-stretch" href=href>{net_name}</a>
                                            </div>
                                            <div style="flex:0 0 70px">
                                                <StatusDot tone=tone/>
                                                " "
                                                <span style="font-size:11px">{run_status}</span>
                                            </div>
                                            <div style="flex:0 0 80px" class="mono">{when}</div>
                                            <div style="flex:0 0 60px" class="mono">{dur}</div>
                                            <div style="flex:0 0 50px; text-align:right" class="mono">{row_ct}</div>
                                        </div>
                                    }
                                }).collect_view();

                                view! {
                                    {rows}
                                    {if returned == 0 { Some(view! {
                                        <div class="tbl-empty">{if total == 0 && fetched_page == 0 {
                                            "No runs yet — attach a schedule to a net to get started"
                                        } else { "No runs on this page" }}</div>
                                    }) } else { None }}
                                    <OffsetPager
                                        window=window
                                        suffix=suffix
                                        on_page=Callback::new(move |p| page.set(p))
                                    />
                                }.into_any()
                        })
                    />
                </div>
            </div>
        </div>
    }
}
