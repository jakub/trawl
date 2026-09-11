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
    let table_viewport = NodeRef::<leptos::html::Div>::new();
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
                <div class="stat-card">
                    <div class="label">"Active nets"</div>
                    <div class="value">
                        <Loaded
                            state=Signal::derive(move || LoadState::from_resource(nets_for_stats.get()))
                            label="active nets"
                            retry=Callback::new(move |()| { nets_for_stats.set(None); nets_for_stats.refetch(); })
                            render=Box::new(|resp: trawl_api::ListSavedResponse| {
                                resp.queries.iter().filter(|q| q.schedule.as_ref().is_some_and(|s| s.enabled))
                                    .count().to_string().into_any()
                            })
                        />
                    </div>
                </div>
                <div class="stat-card">
                    <div class="label">"Success rate"</div>
                    <div class="value">
                        <Loaded
                            state=Signal::derive(move || LoadState::from_resource(stats.get()))
                            label="success rate"
                            retry=Callback::new(move |()| { stats.set(None); stats.refetch(); })
                            render=Box::new(|s: trawl_api::RunsStatsResponse| {
                                (s.success_count * 100).checked_div(s.total_runs)
                                    .map_or_else(|| "—".to_string(), |pct| format!("{pct}%")).into_any()
                            })
                        />
                    </div>
                </div>
                <div class="stat-card">
                    <div class="label">"Avg duration"</div>
                    <div class="value">
                        <Loaded
                            state=Signal::derive(move || LoadState::from_resource(stats.get()))
                            label="average duration"
                            retry=Callback::new(move |()| { stats.set(None); stats.refetch(); })
                            render=Box::new(|s: trawl_api::RunsStatsResponse| {
                                s.avg_duration_ms.map_or_else(|| "—".to_string(), format_duration).into_any()
                            })
                        />
                    </div>
                </div>
            </div>

            <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Recent runs" tabindex="0" style="--list-min-width:560px;margin-top:16px">
                <div class="tbl-body">
                    <Loaded
                        state=Signal::derive(move || LoadState::from_resource(runs.get()))
                        label="runs"
                        retry=Callback::new(move |()| { runs.set(None); runs.refetch(); })
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
                                            <tr class="tbl-row">
                                                <td class="mono">
                                                <a class="row-stretch" href=href>{net_name}</a>
                                                </td>
                                                <td>
                                                <StatusDot tone=tone/>
                                                " "
                                                <span style="font-size:11px">{run_status}</span>
                                                </td>
                                                <td class="mono">{when}</td>
                                                <td class="mono">{dur}</td>
                                                <td style="text-align:right" class="mono">{row_ct}</td>
                                            </tr>
                                    }
                                }).collect_view();

                                view! {
                                    <table class="fleet-table runs-table" aria-label="Recent runs">
                                        <thead><tr>
                                            <th scope="col">"Net"</th>
                                            <th scope="col" style="width:90px">"Status"</th>
                                            <th scope="col" style="width:100px">"When"</th>
                                            <th scope="col" style="width:80px">"Duration"</th>
                                            <th scope="col" style="width:70px; text-align:right">"Rows"</th>
                                        </tr></thead>
                                        <tbody>{rows}</tbody></table>
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
