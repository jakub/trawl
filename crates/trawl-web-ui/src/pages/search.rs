// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/search` — query workspace.
//!
//! Layout (top to bottom inside `.search-col`):
//! editor wrap (header + `DslEditor` + date range + run button)
//! → meta strip (count · duration · chips · save/export)
//! → tabs (Events / Visualization)
//! → tab body (Events: histogram + results table | Visualization: chart)
//!
//! State split:
//! - `query_text` — in-progress editor buffer (not URL-synced).
//! - `executed_q` / `filters` / `range` — URL-driven memos (canonical).
//! - `effective_q` — derived from the triple; what actually hits the
//!   server. Filter chips in the meta strip and the date-range popover
//!   mutate state by navigating; URL drives memos drives resource.

use leptos::prelude::*;
use trawl_api::value::QueryResult;

use crate::components::chart::Chart;
use crate::components::editor_wrap::EditorWrap;
use crate::components::export_modal::ExportModal;
use crate::components::facet_sidebar::FacetSidebar;
use crate::components::histogram::Histogram;
use crate::components::meta_strip::MetaStrip;
use crate::components::results_table::ResultsTable;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::components::status_bar::StatusKind;
use crate::components::tabs::{ResultsTab, Tabs};
use crate::components::toast::ToastBus;
use crate::pages::layout::ShellStatus;
use crate::state::query::{
    Filter, Mode, RangeSpec, UrlSignals, effective_query, navigator, url_signals,
};
use crate::state::search_session::rows_resource;
use crate::state::stream_session::{
    LiveSignals, RingBuffer, StreamLifecycle, ring_to_result, start_stream,
};

#[component]
pub fn Search() -> impl IntoView {
    let bus = use_context::<ToastBus>().expect("ToastBus context");
    let shell_status = use_context::<ShellStatus>().expect("ShellStatus context");

    let query_text = RwSignal::new(String::new());

    let UrlSignals {
        executed_q,
        page,
        mode,
        filters,
        range,
    } = url_signals();

    Effect::new(move |_| {
        query_text.set(executed_q.get());
    });

    let effective_q = Memo::new(move |_| {
        let base = executed_q.get();
        let fs = filters.get();
        let r = range.get();
        effective_query(&base, &fs, &r)
    });

    let rows = rows_resource(effective_q, page);

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
    on_cleanup(move || {
        stream_handle.update_value(|slot| {
            *slot = None;
        });
    });

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

    // --- sync search status to shell status bar -----------------------
    let active_tab = RwSignal::new(ResultsTab::Events);

    let is_chart_query = Memo::new(move |_| {
        let q = effective_q.get();
        if q.trim().is_empty() {
            return false;
        }
        trawl_core::parser::parse(&q).is_ok_and(|ast| ast.has_aggregation())
    });

    let ring_result = Memo::new(move |_| ring_to_result(&ring.read()));

    let loading =
        Signal::derive(move || !effective_q.get().trim().is_empty() && rows.get().is_none());

    // Drive the shell's status bar from search-specific state.
    Effect::new(move |_| {
        shell_status.kind.set(match (mode.get(), loading.get()) {
            (Mode::Live, _) => StatusKind::Live,
            (_, true) => StatusKind::Hauling,
            _ => StatusKind::Connected,
        });
    });
    Effect::new(move |_| {
        shell_status.count.set(
            rows.get()
                .and_then(Result::ok)
                .map(|r| r.pagination.returned),
        );
    });
    Effect::new(move |_| {
        shell_status.range.set(range.get());
    });
    Effect::new(move |_| {
        shell_status.lagged.set(lagged.get());
    });

    on_cleanup(move || {
        shell_status.kind.set(StatusKind::Connected);
        shell_status.count.set(None);
        shell_status.range.set(RangeSpec::default());
        shell_status.lagged.set(None);
    });

    let truncated =
        Signal::derive(move || rows.get().and_then(Result::ok).is_some_and(|r| r.truncated));
    let last_count = Signal::derive(move || {
        rows.get()
            .and_then(Result::ok)
            .map(|r| r.pagination.returned)
    });

    let show_save_modal = RwSignal::new(false);
    let on_save = Callback::new(move |()| show_save_modal.set(true));
    let show_export_modal = RwSignal::new(false);
    let on_export = Callback::new(move |()| show_export_modal.set(true));
    let running = loading;

    let filters_sig = Signal::derive(move || filters.get());
    let range_sig = Signal::derive(move || range.get());

    view! {
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
                    on_export=on_export
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
        <Show when=move || show_save_modal.get()>
            <SaveAsNetModal
                query=effective_q.get_untracked()
                bus=bus
                on_close=Callback::new(move |_| show_save_modal.set(false))
            />
        </Show>
        <Show when=move || show_export_modal.get()>
            <ExportModal
                query=effective_q.get_untracked()
                bus=bus
                on_close=Callback::new(move |_| show_export_modal.set(false))
            />
        </Show>
    }
}

/// Simple table rendering for the ring-buffered raw-event live feed.
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
