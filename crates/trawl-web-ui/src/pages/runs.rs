// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RunsPage/>` — global runs dashboard for the Jobs mode.
//!
//! Shows aggregate stats (recorded runs, success rate, average
//! duration), a paginated table of recent runs across all saved
//! queries, and the selected run's stored result above its execution
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
use leptos_use::use_media_query;
use trawl_api::{RunsSortDir, RunsSortKey};

use crate::api::{self, RUNS_PAGE_SIZE};
use crate::components::net_drawer::RunResultPreview;
use crate::components::sort_th::table_sort_th;
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::time::{format_duration, time_ago};
use fleet_ui::{
    Badge, Drawer, LoadState, Loaded, PageTotal, PageWindow, Pager, SearchInput, ToastBus,
};

/// A server page under one ordering intent. The revision prevents returning
/// to an earlier sort from restoring that sort's old page or response.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RunsOwner {
    page: usize,
    key: RunsSortKey,
    dir: RunsSortDir,
    order_revision: u64,
}

#[derive(Clone)]
struct RunsAttempt {
    generation: u64,
    owner: RunsOwner,
    pending: bool,
    error: Option<String>,
}

/// The `(net_id, run_id)` a `?run=&net=` pair names, or `None`.
///
/// Both halves are required: a run id alone cannot address a stored
/// result, and a hand-edited URL that drops one of them selects nothing
/// rather than half a run.
fn parse_run_selection(net: Option<&str>, run: Option<&str>) -> Option<(i64, i64)> {
    Some((net?.parse().ok()?, run?.parse().ok()?))
}

#[component]
#[allow(clippy::too_many_lines)]
pub fn RunsPage() -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let sort = RwSignal::new((RunsSortKey::Started, true));
    let order = Memo::new(move |previous: Option<&((RunsSortKey, bool), u64)>| {
        let current = sort.get();
        let revision = previous.map_or(0, |(old, revision)| {
            if *old == current {
                *revision
            } else {
                revision.wrapping_add(1)
            }
        });
        (current, revision)
    });
    // A page belongs to the ordering revision under which it was selected.
    // Deriving zero here avoids an old-page/new-sort request from an effect
    // that resets the page after the sort has already changed.
    let page = RwSignal::new((0u64, 0usize));
    let requested = Memo::new(move |_| {
        let ((key, descending), order_revision) = order.get();
        let (page_revision, page) = page.get();
        RunsOwner {
            page: if page_revision == order_revision {
                page
            } else {
                0
            },
            key,
            dir: if descending {
                RunsSortDir::Desc
            } else {
                RunsSortDir::Asc
            },
            order_revision,
        }
    });
    let attempt = RwSignal::new(None::<RunsAttempt>);
    let wide = use_media_query("(min-width: 1100px)");
    let filter = RwSignal::new(String::new());
    let bus = expect_context::<ToastBus>();
    let qm = use_query_map();
    let run_selected: Memo<Option<(i64, i64)>> = Memo::new(move |_| {
        let params = qm.get();
        parse_run_selection(params.get("net").as_deref(), params.get("run").as_deref())
    });

    let (runs, refresh_error, refresh_runs) =
        crate::components::job_refresh::job_refresh(Signal::stored(true), move || {
            let owner = requested.get();
            let generation = attempt
                .get_untracked()
                .map_or(0, |a| a.generation.wrapping_add(1));
            // The helper calls fetch synchronously even when a previous read
            // must finish first. That queued intent already owns the loading
            // state; the obsolete read cannot complete it.
            attempt.set(Some(RunsAttempt {
                generation,
                owner,
                pending: true,
                error: None,
            }));
            async move {
                let response = match PageWindow::checked_offset(owner.page, RUNS_PAGE_SIZE) {
                    Ok(offset) => {
                        api::list_all_runs(RUNS_PAGE_SIZE.get(), offset, owner.key, owner.dir).await
                    }
                    Err(_) => Err(api::ApiError::Refused(
                        "This runs page is too large to request.",
                    )),
                };
                if requested.try_get_untracked() == Some(owner)
                    && attempt
                        .try_get_untracked()
                        .flatten()
                        .is_some_and(|a| a.generation == generation)
                {
                    attempt.set(Some(RunsAttempt {
                        generation,
                        owner,
                        pending: false,
                        error: response
                            .as_ref()
                            .err()
                            .map(std::string::ToString::to_string),
                    }));
                }
                response.map(|resp| (owner, resp))
            }
        });
    let retry_runs = Callback::new(move |()| {
        // refresh_error remains set in job_refresh while it retains old
        // success. Clear our completed failure synchronously so Retry shows
        // loading for the requested owner immediately.
        attempt.update(|a| {
            if let Some(a) = a {
                a.pending = true;
                a.error = None;
            }
        });
        refresh_runs.run(());
    });
    let current_runs = Signal::derive(move || {
        runs.get()
            .and_then(Result::ok)
            .filter(|(owner, _)| *owner == requested.get())
    });
    let list_state = Signal::derive(move || {
        if current_runs.get().is_some() {
            return LoadState::Ready(());
        }
        match attempt.get().filter(|a| a.owner == requested.get()) {
            Some(a) if !a.pending => a.error.map_or(LoadState::Loading, LoadState::Error),
            _ => LoadState::Loading,
        }
    });
    let list_busy = Signal::derive(move || matches!(list_state.get(), LoadState::Loading));

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

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = fleet_ui::time::clock::now_ms();

    let visible_runs = Signal::derive(move || {
        let needle = filter.get().to_lowercase();
        current_runs
            .get()
            .map(|(_, r)| {
                r.runs
                    .into_iter()
                    .filter(|r| needle.is_empty() || r.net_name.to_lowercase().contains(&needle))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let window = Signal::derive(move || {
        let data = current_runs.get();
        let (fetched, returned, total) = data
            .as_ref()
            .map_or((requested.get().page, 0, 0), |(owner, r)| {
                (owner.page, r.runs.len(), r.total)
            });
        PageWindow::new(
            fetched,
            RUNS_PAGE_SIZE,
            returned,
            PageTotal::Known(total),
            data.is_none(),
        )
        .expect("runs page comes from checked pager navigation")
    });
    let summary = Signal::derive(move || {
        if current_runs.get().is_none() {
            return match list_state.get() {
                LoadState::Error(_) => "Runs unavailable".to_string(),
                _ => "Loading…".to_string(),
            };
        }
        let suffix = if filter.get().is_empty() {
            String::new()
        } else {
            format!(" · {} matches on this page", visible_runs.get().len())
        };
        format!("{}{}", window.get().summary(), suffix)
    });

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Runs"</h1>
                    <p class="sub">"Recent scheduled runs across all nets."</p>
                </div>
            </div>

            {move || stats_error.get().or_else(|| current_runs.get().and_then(|_| refresh_error.get()))
                .map(|e| view! { <p role="status">{e}</p> })}
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
                        "Recent runs"
                    </h2>
                    <SearchInput value=filter placeholder="Filter by net…"/>
                </div>
            <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" aria-busy=move || list_busy.get().to_string() role="region" aria-label="Recent runs table" tabindex="0" style="--list-min-width:560px">
                <div class="tbl-body">
                    <Loaded
                        state=list_state
                        label="runs" retry=retry_runs render=Box::new(|()| ().into_any())
                    />
                    <table class="fleet-table runs-table" aria-label="Recent runs">
                        <thead><tr>
                            {table_sort_th(sort, RunsSortKey::Net, false, "Net", "")}
                            {table_sort_th(sort, RunsSortKey::Status, false, "Status", "width:90px")}
                            {table_sort_th(sort, RunsSortKey::Started, true, "When", "width:100px")}
                            {table_sort_th(sort, RunsSortKey::Duration, true, "Duration", "width:80px")}
                            {table_sort_th(sort, RunsSortKey::Rows, true, "Rows", "width:70px; text-align:right")}
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
                                    <td><a class="row-stretch" href=href prop:replace=true>{move || run.get().net_name}</a></td>
                                    <td><span style="font-size:11px">{move || {
                                        crate::tone_vocab::run_status_label(&run.get().run.status).to_owned()
                                    }}</span></td>
                                    <td>{move || time_ago(&run.get().run.started_at, now_ms.get())}</td>
                                    <td>{move || run.get().run.duration_ms.map_or_else(|| "—".to_string(), format_duration)}</td>
                                    <td style="text-align:right">{move || run.get().run.row_count.map_or_else(|| "—".to_string(), |n| n.to_string())}</td>
                                </tr>
                            }
                        }/></tbody>
                    </table>
                    {move || current_runs.get().and_then(|(owner, r)| r.runs.is_empty().then(|| view! {
                        <div class="tbl-empty">{if r.total == 0 && owner.page == 0 { "No runs yet — attach a schedule to a net to get started" }
                            else { "No runs on this page" }}</div>
                    }))}
                    <Pager
                        summary=summary
                        can_prev=Signal::derive(move || window.get().can_prev())
                        can_next=Signal::derive(move || window.get().can_next())
                        on_prev=Callback::new(move |()| {
                            if let Some(p) = window.get_untracked().prev_page() {
                                page.set((order.get_untracked().1, p));
                            }
                        })
                        on_next=Callback::new(move |()| {
                            if let Some(p) = window.get_untracked().next_page() {
                                page.set((order.get_untracked().1, p));
                            }
                        })
                    />
                </div>
            </div>
            </section>

            // A fresh mount per selection: a late response for the run
            // before this one lands in a disposed resource and never
            // paints over the run now on screen.
            <For each=move || { run_selected.get().into_iter().collect::<Vec<_>>() } key=|identity| *identity
                children=move |(net_id, run_id)| {
                    let net_name = Signal::derive(move || runs.get().and_then(Result::ok)
                        .and_then(|(_, response)| response.runs.into_iter()
                            .find(|run| run.net_id == net_id && run.run.id == run_id)
                            .map(|run| run.net_name)));
                    view! {
                        <RunDetail net_id=net_id run_id=run_id net_name=net_name
                            on_close=on_close on_search=on_search docked=wide/>
                    }
                }
            />
            </div>
        </div>
    }
}

/// The selected run: its stored result above the receipt of how it was
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
    docked: Signal<bool>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    // Filled by the preview below out of the one `get_run` response the
    // two of them share; the preview keeps it current while the run is
    // still going.
    let summary = RwSignal::new(None::<trawl_api::ReportRunSummary>);

    // This memo lives under the selected identity's keyed owner. Paging
    // away retains a correct name, but another selection cannot inherit it.
    let known_name = Memo::new(move |previous: Option<&Option<String>>| {
        net_name.get().or_else(|| previous.cloned().flatten())
    });
    let title = move || known_name.get().unwrap_or_else(|| format!("Run {run_id}"));
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
        <Drawer tabs=vec![] tabs_label="Run details" active_tab=Signal::stored(String::new())
            on_tab_change=Callback::new(|_| {}) on_close=on_close docked=docked
            panel_class="run-detail" close_size=12
            title=Box::new(move || view! {
                <span class="name">{title}</span>
                {move || status().map(|s| {
                    let tone = crate::tone_vocab::run_badge_tone(&s);
                    let label = crate::tone_vocab::run_status_label(&s).to_owned();
                    view! { <Badge tone=tone>{label}</Badge> }
                })}
            }.into_any())
            actions=Box::new(move || view! {
                // The one control here that PUSHES: Back from the net
                // returns to this selected run.
                <a class="open-net btn-sec" href=format!("/jobs/nets?net={net_id}&ntab=runs")>
                    "Open net"
                </a>
            }.into_any())
        >
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
        </Drawer>
    }
}
