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

    let (runs, refresh_error, retry_runs) =
        crate::components::job_refresh::job_refresh(Signal::stored(true), move || {
            let p = page.get();
            async move {
                let offset = PageWindow::checked_offset(p, RUNS_PAGE_SIZE).map_err(|_| {
                    api::ApiError::Refused("This runs page is too large to request.")
                })?;
                let response = api::list_all_runs(RUNS_PAGE_SIZE.get(), offset).await;
                response.map(|resp| (p, resp))
            }
        });

    let (nets_for_stats, nets_error, retry_nets) =
        crate::components::job_refresh::job_refresh(Signal::stored(true), || async move {
            api::list_saved().await
        });
    let (stats, stats_error, retry_stats) =
        crate::components::job_refresh::job_refresh(Signal::stored(true), || async move {
            api::runs_stats().await
        });

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = fleet_ui::time::clock::now_ms();

    let visible_runs = Signal::derive(move || {
        let needle = filter.get().to_lowercase();
        runs.get()
            .and_then(Result::ok)
            .map(|(_, r)| {
                r.runs
                    .into_iter()
                    .filter(|r| needle.is_empty() || r.net_name.to_lowercase().contains(&needle))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let window = Signal::derive(move || {
        let data = runs.get().and_then(Result::ok);
        let (fetched, returned, total) = data
            .as_ref()
            .map_or((0, 0, 0), |(p, r)| (*p, r.runs.len(), r.total));
        PageWindow::new(
            fetched,
            RUNS_PAGE_SIZE,
            returned,
            PageTotal::Known(total),
            data.is_none() || page.get() != fetched,
        )
        .expect("runs page comes from checked pager navigation")
    });
    let suffix = Signal::derive(move || {
        if filter.get().is_empty() {
            String::new()
        } else {
            format!(" · {} matches on this page", visible_runs.get().len())
        }
    });

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

            {move || refresh_error.get().or(nets_error.get()).or(stats_error.get()).map(|e| view! { <p role="status">{e}</p> })}
            <div class="stats-row">
                <div class="stat-card">
                    <div class="label">"Active nets"</div>
                    <div class="value">
                        <Loaded
                            state=Signal::derive(move || LoadState::from_resource(nets_for_stats.get()))
                            label="active nets"
                            retry=Callback::new(move |()| { retry_nets.run(()); })
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
                            retry=Callback::new(move |()| { retry_stats.run(()); })
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
                            retry=Callback::new(move |()| { retry_stats.run(()); })
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
                        state=Signal::derive(move || LoadState::from_resource(runs.get().map(|r| r.map(|_| ()))))
                        label="runs" retry=retry_runs render=Box::new(|()| ().into_any())
                    />
                    <table class="fleet-table runs-table" aria-label="Recent runs">
                        <thead><tr>
                            <th scope="col">"Net"</th><th scope="col" style="width:90px">"Status"</th>
                            <th scope="col" style="width:100px">"When"</th><th scope="col" style="width:80px">"Duration"</th>
                            <th scope="col" style="width:70px; text-align:right">"Rows"</th>
                        </tr></thead>
                        <tbody><For each=move || visible_runs.get() key=|r| (r.net_id, r.run.id) children=move |initial| {
                            let net_id = initial.net_id;
                            let run_id = initial.run.id;
                            let run = Signal::derive(move || runs.get().and_then(Result::ok)
                                .and_then(|(_, r)| r.runs.into_iter().find(|r| r.net_id == net_id && r.run.id == run_id))
                                .unwrap_or_else(|| initial.clone()));
                            // The row link keeps focus while cells update in place.
                            let href = format!("/jobs/nets?net={net_id}&ntab=runs");
                            view! {
                                <tr class="tbl-row">
                                    <td class="mono"><a class="row-stretch" href=href>{move || run.get().net_name}</a></td>
                                    <td>{move || view! { <StatusDot tone=crate::components::run_status_tone(&run.get().run.status)/>
                                        " " <span style="font-size:11px">{run.get().run.status}</span> }}</td>
                                    <td class="mono">{move || time_ago(&run.get().run.started_at, now_ms.get())}</td>
                                    <td class="mono">{move || run.get().run.duration_ms.map_or_else(|| "—".to_string(), format_duration)}</td>
                                    <td style="text-align:right" class="mono">{move || run.get().run.row_count.map_or_else(|| "—".to_string(), |n| n.to_string())}</td>
                                </tr>
                            }
                        }/></tbody>
                    </table>
                    {move || runs.get().and_then(Result::ok).and_then(|(p, r)| r.runs.is_empty().then(|| view! {
                        <div class="tbl-empty">{if r.total == 0 && p == 0 { "No runs yet — attach a schedule to a net to get started" }
                            else { "No runs on this page" }}</div>
                    }))}
                    <OffsetPager window=window suffix=suffix on_page=Callback::new(move |p| page.set(p))/>
                </div>
            </div>
        </div>
    }
}
