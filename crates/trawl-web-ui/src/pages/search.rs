// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/search` — hero screen.
//!
//! Layout (top to bottom inside `.search-col`):
//! editor wrap (header + `DslEditor` + date range + run button)
//! → meta strip (count · duration · chips · save/export)
//! → tabs (Events / Visualization)
//! → tab body (Events: histogram + results table | Visualization: chart)
//! Status bar pinned at the bottom of the shell.
//!
//! State split:
//! - `query_text` — in-progress editor buffer (not URL-synced).
//! - `executed_q` / `filters` / `range` — URL-driven memos (canonical).
//! - `effective_q` — derived from the triple; what actually hits the
//!   server. Filter chips in the meta strip and the date-range popover
//!   mutate state by navigating; URL drives memos drives resource.

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::value::QueryResult;

use crate::api;
use crate::components::chart::Chart;
use crate::components::editor_wrap::EditorWrap;
use crate::components::facet_sidebar::FacetSidebar;
use crate::components::histogram::Histogram;
use crate::components::meta_strip::MetaStrip;
use crate::components::rail::Rail;
use crate::components::results_table::ResultsTable;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::components::status_bar::{StatusBar, StatusKind};
use crate::components::tabs::{ResultsTab, Tabs};
use crate::components::toast::{ToastBus, Toasts};
use crate::components::topbar::TopBar;
use crate::pages::history::HistoryPage;
use crate::pages::nets::NetsPage;
use crate::pages::placeholder::ModePlaceholder;
use crate::pages::runs::RunsPage;
use crate::pages::schema::SchemaPage;
use crate::state::app_mode;
use crate::state::app_mode::AppMode;
use crate::state::query::{
    Filter, Mode, RangeSpec, UrlSignals, effective_query, navigator, url_signals,
};
use crate::state::search_session::rows_resource;
use crate::state::section;
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

    let UrlSignals {
        executed_q,
        page,
        mode,
        filters,
        range,
    } = url_signals();

    // Keep editor in sync with URL on first load and back/forward —
    // but don't clobber in-progress edits.
    Effect::new(move |_| {
        let url_q = executed_q.get();
        let buf = query_text.get_untracked();
        if buf.is_empty() || buf == url_q {
            query_text.set(url_q);
        }
    });

    // Effective query = base + filters + range clauses. This is what
    // the rows resource is keyed on, NOT the raw editor buffer.
    let effective_q = Memo::new(move |_| {
        let base = executed_q.get();
        let fs = filters.get();
        let r = range.get();
        effective_query(&base, &fs, &r)
    });

    let rows = rows_resource(effective_q, page);

    // Capture the router navigator ONCE here, during component setup —
    // `use_navigate()` panics outside the `<Router>` reactive context,
    // and our callbacks (CodeMirror submit, button clicks, EventSource
    // handlers) all run after that context is gone. Cloned per-callback
    // since we hand it to multiple closures below.
    let goto = navigator();

    let on_submit = {
        let goto = goto.clone();
        Callback::new(move |()| {
            goto(
                &query_text.get_untracked(),
                0,
                mode.get_untracked(),
                &filters.get_untracked(),
                &range.get_untracked(),
                false,
            );
        })
    };

    let on_paginate = {
        let goto = goto.clone();
        Callback::new(move |new_page: usize| {
            goto(
                &executed_q.get_untracked(),
                new_page,
                Mode::Snapshot,
                &filters.get_untracked(),
                &range.get_untracked(),
                true,
            );
        })
    };

    // Filter mutation — appends (dedup'd) to the current set and
    // navigates. Page resets to 0 since the result set changed.
    let on_add_filter = {
        let goto = goto.clone();
        Callback::new(move |f: Filter| {
            let mut current = filters.get_untracked();
            if current.iter().any(|existing| existing == &f) {
                return;
            }
            current.push(f);
            goto(
                &executed_q.get_untracked(),
                0,
                mode.get_untracked(),
                &current,
                &range.get_untracked(),
                false,
            );
        })
    };

    let on_remove_filter = {
        let goto = goto.clone();
        Callback::new(move |idx: usize| {
            let mut current = filters.get_untracked();
            if idx >= current.len() {
                return;
            }
            current.remove(idx);
            goto(
                &executed_q.get_untracked(),
                0,
                mode.get_untracked(),
                &current,
                &range.get_untracked(),
                false,
            );
        })
    };

    let on_clear_filters = {
        let goto = goto.clone();
        Callback::new(move |()| {
            if filters.get_untracked().is_empty() {
                return;
            }
            goto(
                &executed_q.get_untracked(),
                0,
                mode.get_untracked(),
                &[],
                &range.get_untracked(),
                false,
            );
        })
    };

    let on_range_change = {
        let goto = goto.clone();
        Callback::new(move |new_range: RangeSpec| {
            if range.get_untracked() == new_range {
                return;
            }
            goto(
                &executed_q.get_untracked(),
                0,
                mode.get_untracked(),
                &filters.get_untracked(),
                &new_range,
                false,
            );
        })
    };

    // Free-form navigation used by detail-row "Show context" / "Find
    // similar" buttons — resets filters and range to defaults so the
    // new query runs cleanly.
    let on_navigate_q = {
        let goto = goto.clone();
        Callback::new(move |new_q: String| {
            query_text.set(new_q.clone());
            goto(&new_q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false);
        })
    };

    // --- live-tail state ----------------------------------------------
    let ring = RwSignal::new(RingBuffer::default());
    let live_snapshot = RwSignal::new(None::<QueryResult>);
    let lagged = RwSignal::new(None::<u64>);
    let stream_handle: StoredValue<Option<StreamLifecycle>, LocalStorage> =
        StoredValue::new_local(None);

    Effect::new(move |_| {
        let current_mode = mode.get();
        let q = effective_q.get();
        stream_handle.update_value(|slot| *slot = None);
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
            stream_handle.update_value(|slot| *slot = Some(handle));
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

    let current_app = app_mode::from_url();
    let current_section = section::from_url(current_app);
    let bus = ToastBus::new();

    // Active results tab.
    let active_tab = RwSignal::new(ResultsTab::Events);

    let is_chart_query = Memo::new(move |_| {
        let q = effective_q.get();
        if q.trim().is_empty() {
            return false;
        }
        trawl_core::parser::parse(&q).is_ok_and(|ast| ast.has_aggregation())
    });

    let ring_result = Signal::derive(move || ring_to_result(&ring.read()));

    // "Loading" is derived from the resource state: a non-empty
    // effective query that hasn't produced a result yet means a query
    // is in flight.
    let loading =
        Signal::derive(move || !effective_q.get().trim().is_empty() && rows.get().is_none());

    // Status bar inputs.
    let status = Signal::derive(move || match (mode.get(), loading.get()) {
        (Mode::Live, _) => StatusKind::Live,
        (_, true) => StatusKind::Hauling,
        _ => StatusKind::Connected,
    });
    let last_count = Signal::derive(move || {
        rows.get()
            .and_then(Result::ok)
            .map(|r| r.pagination.returned)
    });
    let truncated =
        Signal::derive(move || rows.get().and_then(Result::ok).is_some_and(|r| r.truncated));

    let show_save_modal = RwSignal::new(false);
    let on_save = Callback::new(move |()| show_save_modal.set(true));
    let running = loading;

    // Signal wrappers so child components get `Signal<T>` props rather
    // than memos directly.
    let filters_sig = Signal::derive(move || filters.get());
    let range_sig = Signal::derive(move || range.get());

    view! {
        <div class="shell">
            <TopBar mode=current_app me=Signal::derive(move || me.get())/>
            <div class="body">
                <Rail mode=current_app section=Signal::derive(move || current_section.get())/>
                <Show when=move || me.get().is_some() fallback=|| view! { <main class="main"></main> }>
                    <main class="main">
                        <Show
                            when=move || current_app.get() == AppMode::Search
                            fallback=move || match current_app.get() {
                                AppMode::Jobs => match current_section.get().as_str() {
                                    "nets" => view! { <NetsPage bus=bus/> }.into_any(),
                                    "runs" => view! { <RunsPage bus=bus/> }.into_any(),
                                    _ => view! { <ModePlaceholder mode=Signal::derive(move || current_app.get())/> }.into_any(),
                                },
                                _ => view! { <ModePlaceholder mode=Signal::derive(move || current_app.get())/> }.into_any(),
                            }
                        >
                            <Show
                                when=move || current_section.get() == "search"
                                fallback=move || match current_section.get().as_str() {
                                    "history" => view! { <HistoryPage bus=bus/> }.into_any(),
                                    "schema" => view! { <SchemaPage bus=bus/> }.into_any(),
                                    _ => view! { <SectionPlaceholder section=current_section/> }.into_any(),
                                }
                            >
                                <div class="search-layout">
                                    <FacetSidebar
                                        rows=rows
                                        filters=filters_sig
                                        on_add=on_add_filter
                                        on_clear=on_clear_filters
                                    />
                                    <div class="search-col">
                                        <EditorWrap
                                            query=query_text
                                            on_submit=on_submit
                                            range=range_sig
                                            on_range_change=on_range_change
                                            running=running
                                            on_save=on_save
                                            bus=bus
                                        />
                                        <MetaStrip
                                            count=last_count
                                            truncated=truncated
                                            filters=filters_sig
                                            on_remove=on_remove_filter
                                            bus=bus
                                        />
                                        <Tabs active=active_tab count=last_count/>
                                        {move || match (active_tab.get(), mode.get()) {
                                            (ResultsTab::Events, Mode::Snapshot) => view! {
                                                <>
                                                    <Histogram rows=rows range=range_sig/>
                                                    <ResultsTable
                                                        page=page
                                                        rows=rows
                                                        on_paginate=on_paginate
                                                        on_add_filter=on_add_filter
                                                        on_navigate=on_navigate_q
                                                        bus=bus
                                                    />
                                                </>
                                            }.into_any(),
                                            (ResultsTab::Events, Mode::Live) if is_chart_query.get() => view! {
                                                <Chart snapshot=live_snapshot/>
                                            }.into_any(),
                                            (ResultsTab::Events, Mode::Live) => view! {
                                                <LiveRawTable result=ring_result/>
                                            }.into_any(),
                                            (ResultsTab::Visualization, _) => view! {
                                                <Chart snapshot=live_snapshot/>
                                            }.into_any(),
                                        }}
                                    </div>
                                </div>
                            </Show>
                        </Show>
                    </main>
                </Show>
            </div>
            <StatusBar
                status=status
                count=last_count
                range=range_sig
                lagged=Signal::derive(move || lagged.get())
            />
            <Toasts bus=bus/>
            <Show when=move || show_save_modal.get()>
                <SaveAsNetModal
                    query=effective_q.get_untracked()
                    bus=bus
                    on_close=Callback::new(move |_| show_save_modal.set(false))
                />
            </Show>
        </div>
    }
}

/// Placeholder body for non-search rail sections (History, Schema)
/// until each gets its own page. Renders inside the shell so the
/// topbar + rail stay visible.
#[component]
fn SectionPlaceholder(section: Memo<String>) -> impl IntoView {
    view! {
        <div class="placeholder">
            <div class="placeholder-card">
                <div class="placeholder-eyebrow">{move || section.get()}</div>
                <h2>"Coming soon"</h2>
                <p>"This section is part of the v1 design but isn't backed by a UI yet."</p>
            </div>
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
