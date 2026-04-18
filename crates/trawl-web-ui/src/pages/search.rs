// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/search` — hero screen (editor, results, live-tail).

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::value::QueryResult;

use crate::api;
use crate::components::chart::Chart;
use crate::components::editor::DslEditor;
use crate::components::facet_sidebar::FacetSidebar;
use crate::components::live_badge::LiveBadge;
use crate::components::results_table::ResultsTable;
use crate::state::query::{Mode, build_search_url, go_to, url_signals};
use crate::state::search_session::rows_resource;
use crate::state::stream_session::{
    LiveSignals, RingBuffer, StreamLifecycle, ring_to_result, start_stream,
};

#[component]
#[allow(clippy::too_many_lines)] // page-level component wires many signals
pub fn Search() -> impl IntoView {
    let me = RwSignal::new(None::<api::MeResponse>);
    let redirect_to_login = RwSignal::new(false);

    // In-progress editor buffer — never touches the URL.
    let query_text = RwSignal::new(String::new());

    // URL-driven: the executed query, page, and mode.
    let (executed_q, page, mode) = url_signals();

    // Keep editor in sync with URL on first load and back/forward —
    // but don't clobber in-progress edits.
    Effect::new(move |_| {
        let url_q = executed_q.get();
        let buf = query_text.get_untracked();
        if buf.is_empty() || buf == url_q {
            query_text.set(url_q);
        }
    });

    let rows = rows_resource(executed_q, page);

    let on_submit = Callback::new(move |()| {
        // Submit preserves current mode — live stays live, snapshot
        // stays snapshot. Reset page to 0 for snapshot.
        go_to(&query_text.get_untracked(), 0, mode.get_untracked(), false);
    });

    // --- live-tail state ----------------------------------------------
    let ring = RwSignal::new(RingBuffer::default());
    let live_snapshot = RwSignal::new(None::<QueryResult>);
    let lagged = RwSignal::new(None::<u64>);
    let stream_handle: StoredValue<Option<StreamLifecycle>, LocalStorage> =
        StoredValue::new_local(None);

    // Start/stop the SSE stream based on (mode, executed_q). Each change
    // tears down the previous handle (which close()s the EventSource
    // via Drop) before opening a new one.
    Effect::new(move |_| {
        let current_mode = mode.get();
        let q = executed_q.get();
        // Drop any existing stream first.
        stream_handle.update_value(|slot| {
            *slot = None;
        });
        // Reset transient state on every start/stop so stale events from
        // a previous query don't bleed into the next session.
        ring.set(RingBuffer::default());
        live_snapshot.set(None);
        lagged.set(None);

        if current_mode != Mode::Live || q.trim().is_empty() {
            return;
        }

        let signals = LiveSignals {
            ring,
            snapshot: live_snapshot,
            lagged,
        };
        if let Some(handle) = start_stream(&q, signals) {
            stream_handle.update_value(|slot| {
                *slot = Some(handle);
            });
        }
    });

    // On mount: fetch /me. On 401, redirect to /login.
    Effect::new(move |_| {
        spawn_local(async move {
            match api::me().await {
                Ok(resp) => me.set(Some(resp)),
                Err(_) => redirect_to_login.set(true),
            }
        });
    });

    Effect::new(move |_| {
        if redirect_to_login.get()
            && let Some(win) = web_sys::window()
        {
            let _ = win.location().set_href("/login");
        }
    });

    let on_logout = move |_| {
        spawn_local(async move {
            let _ = api::logout().await;
            if let Some(win) = web_sys::window() {
                let _ = win.location().set_href("/login");
            }
        });
    };

    // Toggle live-tail mode. In snapshot→live, we also decide whether
    // the query is "chart-shaped" (has aggregation) so the UI can pick
    // the right renderer; that's a simple parse call.
    let toggle_live = move |_| {
        let q = query_text.get_untracked();
        let new_mode = match mode.get_untracked() {
            Mode::Snapshot => Mode::Live,
            Mode::Live => Mode::Snapshot,
        };
        let nav = leptos_router::hooks::use_navigate();
        nav(
            &build_search_url(&q, 0, new_mode),
            leptos_router::NavigateOptions::default(),
        );
    };

    // Whether the live stream is aggregation-shaped (→ chart) vs
    // raw-event-shaped (→ scrolling table). Derived from a parse of
    // the executed query. Falls back to raw on parse error.
    let is_chart_query = Memo::new(move |_| {
        let q = executed_q.get();
        if q.trim().is_empty() {
            return false;
        }
        trawl_core::parser::parse(&q).is_ok_and(|ast| ast.has_aggregation())
    });

    // Materialize the raw-event ring into a QueryResult for the live
    // table. `Signal::derive` (not `Memo`) because QueryResult doesn't
    // impl PartialEq — ring_to_result is cheap enough to recompute on
    // render, and the containing signal reads only fire when the ring
    // itself changes.
    let ring_result = Signal::derive(move || ring_to_result(&ring.read()));

    view! {
        <div class="shell">
            <header class="topbar">
                <h1>"trawl"</h1>
                <div class="spacer"></div>
                <LiveBadge
                    active=Signal::derive(move || mode.get() == Mode::Live)
                    lagged=Signal::derive(move || lagged.get())
                />
                <button class="btn-link" on:click=toggle_live>
                    {move || if mode.get() == Mode::Live { "stop live" } else { "live tail" }}
                </button>
                <span class="user">
                    {move || me.get().map(|m| format!("{} · {}", m.name, m.role))}
                </span>
                <button class="btn-link" on:click=on_logout>"logout"</button>
            </header>
            <main class="main">
                <Show when=move || me.get().is_some() fallback=|| ()>
                    <div class="search-layout">
                        <FacetSidebar rows=rows/>
                        <div class="search-col">
                            <DslEditor query=query_text on_submit=on_submit/>
                            {move || match mode.get() {
                                Mode::Snapshot => view! {
                                    <ResultsTable
                                        executed_q=Signal::derive(move || executed_q.get())
                                        page=Signal::derive(move || page.get())
                                        rows=rows
                                    />
                                }.into_any(),
                                Mode::Live if is_chart_query.get() => view! {
                                    <Chart snapshot=Signal::derive(move || live_snapshot.get())/>
                                }.into_any(),
                                Mode::Live => view! {
                                    <LiveRawTable result=ring_result/>
                                }.into_any(),
                            }}
                        </div>
                    </div>
                </Show>
            </main>
        </div>
    }
}

/// Simple table rendering for the ring-buffered raw-event live feed.
/// Stripped-down vs `<ResultsTable/>` — no pagination, no suspense.
#[component]
fn LiveRawTable(#[prop(into)] result: Signal<QueryResult>) -> impl IntoView {
    view! {
        <div class="results">
            {move || {
                let r = result.get();
                if r.columns.is_empty() {
                    view! {
                        <div class="results-empty">"streaming — waiting for first event…"</div>
                    }.into_any()
                } else {
                    let columns: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
                    let rows = r.rows.clone();
                    view! {
                        <div class="results-table-wrap">
                            <table class="results-table">
                                <thead>
                                    <tr>
                                        {columns.iter().cloned().map(|name| view! {
                                            <th>{name}</th>
                                        }).collect::<Vec<_>>()}
                                    </tr>
                                </thead>
                                <tbody>
                                    {rows.iter().map(|row| {
                                        let cells = row.iter().map(|v| {
                                            let s = trawl_api::display::value_to_string(v);
                                            view! { <td>{s}</td> }
                                        }).collect::<Vec<_>>();
                                        view! { <tr>{cells}</tr> }
                                    }).collect::<Vec<_>>()}
                                </tbody>
                            </table>
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}
