// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/search` — query workspace.
//!
//! Layout (top to bottom inside `.search-col`):
//! editor wrap (header + `DslEditor` + date range + run button)
//! → meta strip (filter chips — hidden while empty)
//! → tabs (Events / Visualization · trailing Save/Export actions)
//! → degraded-field notice (hidden unless the execution reported one)
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
use crate::components::degraded_notice::DegradedNotice;
use crate::components::editor_wrap::EditorWrap;
use crate::components::export_modal::ExportModal;
use crate::components::facet_sidebar::FacetSidebar;
use crate::components::histogram::Histogram;
use crate::components::meta_strip::MetaStrip;
use crate::components::results_table::ResultsTable;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::components::status_bar::StatusKind;
use crate::pages::layout::ShellStatus;
use crate::state::query::{
    Filter, Mode, RangeSpec, UrlSignals, effective_query, navigator, url_signals,
};
use crate::state::search_session::rows_resource;
use fleet_ui::{TabItem, Tabs, ToastBus, ToastKind};

use crate::state::stream_session::{
    LiveSignals, RingBuffer, StreamLifecycle, ring_to_result, start_stream,
};

/// Results-area tab. The typed enum is search-page semantics rather than
/// design-system chrome, so it stays app-side while the strip itself is
/// `fleet_ui::Tabs`.
///
/// Patterns and Statistics from the design are deferred — we don't
/// have pattern detection or pre-aggregated stats yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultsTab {
    Events,
    Visualization,
}

impl ResultsTab {
    fn id(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Visualization => "viz",
        }
    }

    fn from_id(id: &str) -> Self {
        if id == "viz" {
            Self::Visualization
        } else {
            Self::Events
        }
    }
}

#[component]
pub fn Search() -> impl IntoView {
    let shell_status = use_context::<ShellStatus>().expect("ShellStatus context");
    let bus = expect_context::<ToastBus>();

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

    // "Live Tail" in the date-range popover: same query, SSE mode. Reads
    // the editor buffer (not `executed_q`) so an unsubmitted edit streams
    // rather than silently tailing the previously-run query.
    let on_live = {
        let goto = goto.clone();
        Callback::new(move |()| {
            goto(
                &query_text.get_untracked(),
                0,
                Mode::Live,
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
        // Keep the push counter monotonic so a new query cannot reuse row keys.
        ring.update(|r| r.events.clear());
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
    // fleet_ui::Tabs speaks &'static str ids; the typed ResultsTab enum
    // stays app-side (ADR-0002) with a two-line id <-> enum map here.
    let tabs_active = Signal::derive(move || active_tab.get().id().to_string());
    let on_tab_change = Callback::new(move |id: String| active_tab.set(ResultsTab::from_id(&id)));

    let is_chart_query = Memo::new(move |_| {
        let q = effective_q.get();
        if q.trim().is_empty() {
            return false;
        }
        trawl_core::parser::parse(&q).is_ok_and(|ast| ast.has_aggregation())
    });

    let ring_result = Memo::new(move |_| {
        let ring = ring.read();
        (ring.epoch, ring_to_result(&ring))
    });

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
        shell_status.lagged.set(lagged.get());
    });

    on_cleanup(move || {
        shell_status.kind.set(StatusKind::Connected);
        shell_status.count.set(None);
        shell_status.lagged.set(None);
    });

    let truncated =
        Signal::derive(move || rows.get().and_then(Result::ok).is_some_and(|r| r.truncated));
    let last_count = Signal::derive(move || {
        rows.get()
            .and_then(Result::ok)
            .map(|r| r.pagination.returned)
    });

    // The degraded fields this execution reported. Read off the
    // response, never re-derived and never refreshed from the catalog:
    // it describes the answer already on screen.
    //
    // Empty in live mode by construction — the snapshot resource keeps
    // running behind the live tail, and SSE carries no notice, so a
    // stale snapshot's fields must not be shown over streamed rows.
    let degraded_fields = Signal::derive(move || {
        if mode.get() == Mode::Live {
            return Vec::new();
        }
        rows.get()
            .and_then(Result::ok)
            .map_or_else(Vec::new, |r| r.degraded_fields.clone())
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
                    on_live=on_live
                />
                <MetaStrip
                    truncated=truncated
                    filters=filters_sig
                    on_remove=on_remove_filter
                />
                <Tabs
                    items=vec![
                        TabItem::with_count(ResultsTab::Events.id(), "Events", last_count),
                        TabItem::new(ResultsTab::Visualization.id(), "Visualization"),
                    ]
                    active=tabs_active
                    on_change=on_tab_change
                    trailing=Box::new(move || view! {
                        <span
                            class="action"
                            on:click=move |_| bus.push(
                                ToastKind::Info,
                                "Save",
                                Some("Net saving is landing soon — use the history page for now.".into()),
                            )
                        >"Save"</span>
                        <span
                            class="action"
                            on:click=move |_| on_export.run(())
                        >"Export"</span>
                    }.into_any())
                />
                // Above the results body, not inside it: a zero-row
                // answer is exactly when "some values are missing" is
                // worth reading, and the Visualization tab is drawn from
                // the same incomplete rows.
                <DegradedNotice query=effective_q fields=degraded_fields/>
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
                on_close=Callback::new(move |_| show_save_modal.set(false))
            />
        </Show>
        <Show when=move || show_export_modal.get()>
            <ExportModal
                query=effective_q.get_untracked()
                on_close=Callback::new(move |_| show_export_modal.set(false))
            />
        </Show>
    }
}

/// Simple table rendering for the ring-buffered raw-event live feed.
#[component]
fn LiveRawTable(#[prop(into)] result: Signal<(u64, QueryResult)>) -> impl IntoView {
    // Rows hold immutable cells. A column change gives them new keys so their
    // cells are rebuilt in the current first-seen column order.
    let columns = Memo::new(move |previous: Option<&(u64, Vec<String>)>| {
        let names =
            result.with(|(_, r)| r.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
        let version = previous.map_or(0, |(version, old)| {
            version.wrapping_add(u64::from(old != &names))
        });
        (version, names)
    });
    view! {
        <div class="results">
            <Show
                when=move || result.with(|(_, r)| !r.columns.is_empty())
                fallback=|| view! {
                    <div class="results-empty">"Streaming — waiting for first event…"</div>
                }
            >
                <div class="results-table-wrap">
                    <table class="results-table">
                        <thead>
                            <tr>
                                {move || columns.with(|(_, names)| names.iter().map(|name| view! {
                                    <th>{name.clone()}</th>
                                }).collect::<Vec<_>>())}
                            </tr>
                        </thead>
                        <tbody>
                            <For
                                each=move || {
                                    let version = columns.read().0;
                                    result.with(|(epoch, r)| {
                                        let first = epoch.wrapping_sub(r.rows.len() as u64);
                                        r.rows.iter().enumerate().map(|(index, row)| {
                                            (first.wrapping_add(index as u64), version, row.clone())
                                        }).collect::<Vec<_>>()
                                    })
                                }
                                key=|(id, version, _)| (*id, *version)
                                children=|(_, _, row)| view! {
                                    <tr>
                                        {row.iter().map(|v| {
                                            let s = trawl_api::display::value_to_string(v);
                                            view! { <td>{s}</td> }
                                        }).collect::<Vec<_>>()}
                                    </tr>
                                }
                            />
                        </tbody>
                    </table>
                </div>
            </Show>
        </div>
    }
}
