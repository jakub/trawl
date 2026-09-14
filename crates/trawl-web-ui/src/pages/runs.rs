// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RunsPage/>` — global runs dashboard for the Jobs mode.
//!
//! Shows aggregate stats (recorded runs, success rate, average
//! duration), a paginated table of recent runs across all saved
//! queries, and the selected run's stored result beside its execution
//! receipt.
//!
//! URL params: `run=<id>&net=<id>` names the selected run. Both are
//! required and both must parse — a run id means nothing without the net
//! whose result file it names. Selection REPLACES, like schema's `svc`
//! and nets' `net`: a list you are reading through is one history entry,
//! not one per row you glance at. The receipt's "Open net" anchor is the
//! one control here that pushes, so Back from the net drawer returns to
//! the runs page with this run still selected.

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api::{self, RUNS_PAGE_SIZE};
use crate::components::net_drawer::RunResultPreview;
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::time::{format_duration, time_ago};
use fleet_ui::{
    Badge, Icon, IconView, LoadState, Loaded, OffsetPager, PageTotal, PageWindow, SearchInput,
    StatusDot, StatusTone, ToastBus, Tone,
};

/// The `(net_id, run_id)` a `?run=&net=` pair names, or `None`.
///
/// Both halves are required: a run id alone cannot address a stored
/// result, and a hand-edited URL that drops one of them selects nothing
/// rather than half a run.
fn parse_run_selection(net: Option<&str>, run: Option<&str>) -> Option<(i64, i64)> {
    Some((net?.parse().ok()?, run?.parse().ok()?))
}

/// The receipt's outcome badge, off the same status vocabulary the
/// list's dot reads: one mapping of run statuses, two presentations.
fn run_badge_tone(status: &str) -> Tone {
    match crate::components::run_status_tone(status) {
        StatusTone::Success => Tone::Success,
        StatusTone::Error => Tone::Danger,
        StatusTone::Running => Tone::Info,
        StatusTone::Neutral => Tone::Neutral,
    }
}

#[component]
#[allow(clippy::too_many_lines)]
pub fn RunsPage() -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let page = RwSignal::new(0usize);
    let filter = RwSignal::new(String::new());
    let bus = expect_context::<ToastBus>();
    let qm = use_query_map();
    let run_selected: Memo<Option<(i64, i64)>> = Memo::new(move |_| {
        let params = qm.get();
        parse_run_selection(params.get("net").as_deref(), params.get("run").as_deref())
    });

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

    let (stats, stats_error, retry_stats) =
        crate::components::job_refresh::job_refresh(Signal::stored(true), || async move {
            api::runs_stats().await
        });

    // Captured up-front — `use_navigate()` panics outside the router's
    // reactive context, which is where the close callback runs.
    let nav = use_navigate();
    let push_run = move |selection: Option<(i64, i64)>| {
        let url = match selection {
            Some((net_id, run_id)) => format!("/jobs/runs?run={run_id}&net={net_id}"),
            None => "/jobs/runs".to_string(),
        };
        nav(
            &url,
            NavigateOptions {
                replace: true,
                ..Default::default()
            },
        );
    };
    let on_close: Callback<()> = {
        let push = push_run.clone();
        Callback::new(move |()| push(None))
    };

    let goto_search = navigator();
    let on_search: Callback<String> = Callback::new(move |q: String| {
        report_refusal(
            bus,
            goto_search(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false),
        );
    });

    // The selected run's net name, off the page the list already
    // carries. Read as a signal so the detail mounts once per selection
    // rather than once per list refresh.
    let selected_net_name = Signal::derive(move || {
        let (net_id, run_id) = run_selected.get()?;
        let (_, resp) = runs.get()?.ok()?;
        resp.runs
            .iter()
            .find(|gr| gr.net_id == net_id && gr.run.id == run_id)
            .map(|gr| gr.net_name.clone())
    });

    // The sheet header counts what the table shows, off the same
    // predicate the rows are filtered by. It also keeps the sheet's
    // name distinct from the scroll region's, which is "Recent runs"
    // on its own.
    let visible_count = Signal::derive(move || {
        let needle = filter.get().to_lowercase();
        match runs.get() {
            Some(Ok((_, resp))) => resp
                .runs
                .iter()
                .filter(|r| needle.is_empty() || r.net_name.to_lowercase().contains(&needle))
                .count(),
            _ => 0,
        }
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
            </div>

            {move || refresh_error.get().or(stats_error.get()).map(|e| view! { <p role="status">{e}</p> })}
            <div class="stats-row">
                <div class="stat-card">
                    <div class="label">"Recorded runs"</div>
                    <div class="value">
                        <Loaded
                            state=Signal::derive(move || LoadState::from_resource(stats.get()))
                            label="recorded runs"
                            retry=Callback::new(move |()| { retry_stats.run(()); })
                            render=Box::new(|s: trawl_api::RunsStatsResponse| {
                                s.total_runs.to_string().into_any()
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
                    <div class="label">"Average duration"</div>
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

            <div class="page-split" class:has-panel=move || run_selected.get().is_some()>
            <section class="list-sheet" aria-labelledby="runs-sheet-title">
                <div class="list-sheet-hd">
                    <h2 id="runs-sheet-title" class="list-sheet-ttl">
                        "Recent runs"<span class="cnt">{move || format!(" {}", visible_count.get())}</span>
                    </h2>
                    <SearchInput value=filter placeholder="Filter by net…"/>
                </div>
            <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Recent runs" tabindex="0" style="--list-min-width:560px">
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
                            // The selected run is a place with a URL, and selecting
                            // one REPLACES: reading down a list is one history entry,
                            // not one per row. The row link keeps focus while its
                            // cells update in place.
                            let href = format!("/jobs/runs?run={run_id}&net={net_id}");
                            view! {
                                <tr class="tbl-row" class:active=move || run_selected.get() == Some((net_id, run_id))>
                                    <td class="mono"><a class="row-stretch" href=href prop:replace=true>{move || run.get().net_name}</a></td>
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
            </section>

            // A fresh mount per selection: a late response for the run
            // before this one lands in a disposed resource and never
            // paints over the run now on screen.
            {move || run_selected.get().map(|(net_id, run_id)| view! {
                <RunDetail
                    net_id=net_id
                    run_id=run_id
                    net_name=selected_net_name
                    on_close=on_close
                    on_search=on_search
                />
            })}
            </div>
        </div>
    }
}

/// The selected run: its stored result beside the receipt of how it was
/// produced.
#[component]
fn RunDetail(
    net_id: i64,
    run_id: i64,
    /// The net's name off the list, which the run summary does not
    /// carry.
    net_name: Signal<Option<String>>,
    on_close: Callback<()>,
    on_search: Callback<String>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    // Filled by the preview below out of the one `get_run` response the
    // two of them share; the preview keeps it current while the run is
    // still going.
    let summary = RwSignal::new(None::<trawl_api::ReportRunSummary>);

    let title = move || net_name.get().unwrap_or_else(|| format!("Run {run_id}"));
    let status = move || summary.get().map(|s| s.status);
    let duration = move || {
        summary
            .get()
            .and_then(|s| s.duration_ms)
            .map_or_else(|| "—".to_string(), format_duration)
    };
    let row_count = move || {
        summary
            .get()
            .and_then(|s| s.row_count)
            .map_or_else(|| "—".to_string(), |n| n.to_string())
    };

    view! {
        <section class="list-sheet run-detail" aria-labelledby="run-detail-title">
            <div class="list-sheet-hd">
                <h2 id="run-detail-title" class="list-sheet-ttl">{title}</h2>
                {move || status().map(|s| {
                    let tone = run_badge_tone(&s);
                    view! { <Badge tone=tone>{s}</Badge> }
                })}
                <span class="cnt">{move || format!("{} recorded rows", row_count())}</span>
                // The one control here that PUSHES: the net drawer is
                // somewhere else, so Back comes home to this run.
                <a class="open-net btn-sec" href=format!("/jobs/nets?net={net_id}&ntab=runs")>
                    "Open net"
                </a>
                <button
                    type="button"
                    class="sd-x"
                    aria-label="Close"
                    on:click=move |_| on_close.run(())
                >
                    <IconView icon=Icon::Close size=12 stroke_width=1.5/>
                </button>
            </div>
            <div class="result-grid">
                <div class="data-area">
                    <RunResultPreview
                        active=Signal::stored(true)
                        summary=summary
                        net_id=net_id
                        run_id=run_id
                        bus=bus
                        on_search=on_search
                    />
                </div>
                <aside class="receipt" aria-labelledby="receipt-title">
                    <h3 id="receipt-title">"Execution receipt"</h3>
                    <dl class="fieldlist">
                        <div><dt>"Net"</dt><dd>{title}</dd></div>
                        <div><dt>"Outcome"</dt><dd>{move || status().unwrap_or_else(|| "—".to_string())}</dd></div>
                        <div><dt>"Duration"</dt><dd>{duration}</dd></div>
                        <div><dt>"Rows recorded"</dt><dd>{row_count}</dd></div>
                        <div><dt>"Query"</dt><dd class="mono">
                            {move || summary.get().map_or_else(|| "—".to_string(), |s| s.query)}
                        </dd></div>
                    </dl>
                    <p class="small">
                        "Rerunning uses the recorded query against data available now. It will not replace this result."
                    </p>
                </aside>
            </div>
        </section>
    }
}
