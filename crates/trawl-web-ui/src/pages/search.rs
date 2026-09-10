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
use crate::components::malformed_notice::MalformedNotice;
use crate::components::meta_strip::MetaStrip;
use crate::components::results_table::ResultsTable;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::components::status_bar::StatusKind;
use crate::pages::layout::ShellStatus;
use crate::search_url::{Param, admit_filters, refusal_copy};
use crate::state::query::{
    Filter, Mode, RangeSpec, UrlSignals, effective_query, navigator, replace_navigator,
    report_refusal, url_signals,
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
#[allow(clippy::too_many_lines)] // the search page markup is one cohesive view tree
pub fn Search() -> impl IntoView {
    let shell_status = use_context::<ShellStatus>().expect("ShellStatus context");
    let bus = expect_context::<ToastBus>();

    let query_text = RwSignal::new(String::new());

    let UrlSignals {
        raw_search,
        executed_q,
        page,
        mode,
        filters,
        range,
        malformed,
        repair,
    } = url_signals();

    Effect::new(move |_| {
        query_text.set(executed_q.get());
    });

    // Whether this link's structured state could be read at all.
    //
    // A link that could not be read runs nothing and rewrites nothing
    // (ADR-0027): while this is true the banner's repair is the ONLY
    // control that navigates. That is structural, not a property of the
    // blanked query below — every callback in this component returns
    // early on it, and every control that reaches one of them is
    // rendered `disabled` (or, where a span has no disabled state, not
    // rendered). Blanking `effective_q` alone was not the gate it looked
    // like: the export modal read that empty string and posted it, and
    // an empty query is `SELECT *` with no WHERE to the server's
    // emitter, so a refused link exported the whole corpus.
    //
    // Every reader of `rows` below is gated on it too, because refusing
    // to RUN a link is not the same as refusing to PAINT one: a response
    // already in flight when the URL turned unreadable (Back,
    // mid-request) still lands in the resource, and rows under a banner
    // that says the link was not run are a straight contradiction. That
    // the resource keeps the in-flight request is a residual outside
    // this slice; what it shows is not.
    let unreadable = Signal::derive(move || malformed.with(Option::is_some));

    let effective_q = Memo::new(move |_| {
        if unreadable.get() {
            return String::new();
        }
        let base = executed_q.get();
        let fs = filters.get();
        let r = range.get();
        effective_query(&base, &fs, &r)
    });

    let (query_pending, set_query_pending) = signal(false);
    let rows = rows_resource(effective_q, page, set_query_pending);

    let goto = navigator();

    let on_submit = {
        let goto = goto.clone();
        Callback::new(move |()| {
            // The gate, at every door that navigates. The Haul button
            // carries `disabled` as well; this is what makes the
            // editor's own Ctrl+Enter obey the same rule.
            if unreadable.get_untracked() {
                return;
            }
            // The other half of the gate: a link this app cannot read
            // back is not written at all. The editor keeps its text and
            // the address bar keeps its query, so a query too long to
            // share is a toast and an edit away from working, not a
            // banner over an editor the page just emptied.
            report_refusal(
                bus,
                goto(
                    &query_text.get_untracked(),
                    0,
                    mode.get_untracked(),
                    &filters.get_untracked(),
                    &range.get_untracked(),
                    false,
                ),
            );
        })
    };

    // "Live Tail" in the date-range popover: same query, SSE mode. Reads
    // the editor buffer (not `executed_q`) so an unsubmitted edit streams
    // rather than silently tailing the previously-run query.
    let on_live = {
        let goto = goto.clone();
        Callback::new(move |()| {
            if unreadable.get_untracked() {
                return Err("This search link could not be read.".to_string());
            }
            goto(
                &query_text.get_untracked(),
                0,
                Mode::Live,
                &filters.get_untracked(),
                &range.get_untracked(),
                false,
            )
            .map_err(|reason| crate::search_url::refusal_copy(reason).to_string())
        })
    };

    let on_paginate = {
        let goto = goto.clone();
        Callback::new(move |new_page: usize| {
            if unreadable.get_untracked() {
                return;
            }
            report_refusal(
                bus,
                goto(
                    &executed_q.get_untracked(),
                    new_page,
                    Mode::Snapshot,
                    &filters.get_untracked(),
                    &range.get_untracked(),
                    true,
                ),
            );
        })
    };

    let on_add_filter = {
        let goto = goto.clone();
        Callback::new(move |f: Filter| {
            if unreadable.get_untracked() {
                return;
            }
            let mut current = filters.get_untracked();
            if current.iter().any(|existing| existing == &f) {
                return;
            }
            current.push(f);
            // The producer asks the reader's own rules before it
            // navigates: a link this app builds must be one it can read
            // back, so a set that busts a cap is refused out loud here
            // rather than becoming the malformed banner one navigation
            // later (ADR-0027).
            if let Err(reason) = admit_filters(&current) {
                bus.push(ToastKind::Error, refusal_copy(reason), None);
                return;
            }
            report_refusal(
                bus,
                goto(
                    &executed_q.get_untracked(),
                    0,
                    mode.get_untracked(),
                    &current,
                    &range.get_untracked(),
                    false,
                ),
            );
        })
    };

    let on_remove_filter = {
        let goto = goto.clone();
        Callback::new(move |idx: usize| {
            if unreadable.get_untracked() {
                return;
            }
            let mut current = filters.get_untracked();
            if idx >= current.len() {
                return;
            }
            current.remove(idx);
            report_refusal(
                bus,
                goto(
                    &executed_q.get_untracked(),
                    0,
                    mode.get_untracked(),
                    &current,
                    &range.get_untracked(),
                    false,
                ),
            );
        })
    };

    let on_clear_filters = {
        let goto = goto.clone();
        Callback::new(move |()| {
            if unreadable.get_untracked() || filters.get_untracked().is_empty() {
                return;
            }
            report_refusal(
                bus,
                goto(
                    &executed_q.get_untracked(),
                    0,
                    mode.get_untracked(),
                    &[],
                    &range.get_untracked(),
                    false,
                ),
            );
        })
    };

    let on_range_change = {
        let goto = goto.clone();
        Callback::new(move |picked: fleet_ui::RangeValue| {
            if unreadable.get_untracked() {
                return Err("This search link could not be read.".to_string());
            }
            let new_range = crate::search_url::normalize_dialog_range(picked)?;
            if range.get_untracked() == new_range {
                return Ok(());
            }
            goto(
                &executed_q.get_untracked(),
                0,
                mode.get_untracked(),
                &filters.get_untracked(),
                &new_range,
                false,
            )
            .map_err(|reason| crate::search_url::refusal_copy(reason).to_string())
        })
    };

    // The repair the banner offers: the link as it stands with ONE
    // parameter replaced, built by `search_url::repair_url` off the
    // query map. Rebuilding it from the memos instead would write every
    // parameter's fallback, so repairing an unreadable `f` also
    // silently dropped an unreadable `r` beside it and ran the default
    // window. `replace` so the broken link does not become a Back
    // destination.
    let repair_to = replace_navigator();
    let on_repair = Callback::new(move |()| {
        let Some(r) = repair.get_untracked() else {
            return;
        };
        // Every repair keeps `q` as it stands (a named one carries it
        // verbatim; "Start over" lands on an empty one, and an unread
        // link's `executed_q` is ALREADY empty), so the effect that
        // syncs the editor to the executed query never fires across a
        // repair, and whatever was typed into the still-editable editor
        // would sit above results from the query that actually ran.
        // Reset the buffer here, to the query the repaired link runs.
        query_text.set(executed_q.get_untracked());
        report_refusal(bus, repair_to(&r.href));
    });

    let on_navigate_q = {
        let goto = goto.clone();
        Callback::new(move |new_q: String| {
            if unreadable.get_untracked() {
                return;
            }
            // Buffer after navigation, not before it: a refused link
            // must leave the editor exactly as the reader left it.
            let outcome = goto(&new_q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false);
            if outcome.is_ok() {
                query_text.set(new_q);
            }
            report_refusal(bus, outcome);
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

    let ring_result = Memo::new(move |_| ring_to_result(&ring.read()));

    let loading = Signal::derive(move || {
        !effective_q.get().trim().is_empty() && (query_pending.get() || rows.get().is_none())
    });

    // Drive the shell's status bar from search-specific state.
    Effect::new(move |_| {
        shell_status.kind.set(match (mode.get(), loading.get()) {
            (Mode::Live, _) => StatusKind::Live,
            (_, true) => StatusKind::Hauling,
            _ => StatusKind::Connected,
        });
    });
    Effect::new(move |_| {
        shell_status.count.set(if unreadable.get() {
            None
        } else {
            rows.get()
                .and_then(Result::ok)
                .map(|r| r.pagination.returned)
        });
    });
    Effect::new(move |_| {
        shell_status.lagged.set(lagged.get());
    });

    on_cleanup(move || {
        shell_status.kind.set(StatusKind::Connected);
        shell_status.count.set(None);
        shell_status.lagged.set(None);
    });

    let truncated = Signal::derive(move || {
        !unreadable.get() && rows.get().and_then(Result::ok).is_some_and(|r| r.truncated)
    });
    let last_count = Signal::derive(move || {
        if unreadable.get() {
            return None;
        }
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
        if mode.get() == Mode::Live || unreadable.get() {
            return Vec::new();
        }
        rows.get()
            .and_then(Result::ok)
            .map_or_else(Vec::new, |r| r.degraded_fields.clone())
    });

    // Capture the editor once per opening. URL state cannot retarget this save.
    let save_query = RwSignal::new(None::<String>);
    let on_save = Callback::new(move |()| {
        if unreadable.get_untracked() {
            return;
        }
        save_query.set(Some(query_text.get_untracked()));
    });
    let show_export_modal = RwSignal::new(false);
    let on_export = Callback::new(move |()| {
        if unreadable.get_untracked() {
            return;
        }
        show_export_modal.set(true);
    });
    // A modal opened on a readable link must close if Back reaches a
    // malformed one. Discard the pending Save snapshot with it, so the
    // refusal banner is the only remaining action context, ADR-0027.
    Effect::new(move |_| {
        if unreadable.get() {
            save_query.set(None);
            show_export_modal.set(false);
        }
    });
    let running = loading;

    let malformed_sig = Signal::derive(move || malformed.get());
    let repair_sig = Signal::derive(move || repair.get());
    // The chip strip's own admission that the filters on screen are not
    // the filters in the link.
    let filters_unreadable = Signal::derive(move || {
        malformed.with(|m| m.as_ref().is_some_and(|m| m.param == Param::Filters))
    });

    let filters_sig = Signal::derive(move || filters.get());
    let range_sig = Signal::derive(move || range.get());

    view! {
        <div class="search-layout">
            <FacetSidebar
                rows=rows
                filters=filters_sig
                suppressed=unreadable
                on_add=on_add_filter
                on_clear=on_clear_filters
            />
            <div class="search-col">
                <EditorWrap
                    query=query_text
                    on_submit=on_submit
                    range=range_sig
                    on_range_change=on_range_change
                    reset_key=raw_search
                    running=running
                    blocked=unreadable
                    on_save=on_save
                    on_live=on_live
                />
                <MetaStrip
                    truncated=truncated
                    filters=filters_sig
                    filters_unreadable=filters_unreadable
                    blocked=unreadable
                    on_remove=on_remove_filter
                />
                <MalformedNotice malformed=malformed_sig repair=repair_sig on_repair=on_repair/>
                <Tabs
                    items=vec![
                        TabItem::with_count(ResultsTab::Events.id(), "Events", last_count),
                        TabItem::new(ResultsTab::Visualization.id(), "Visualization"),
                    ]
                    label="Results"
                    active=tabs_active
                    on_change=on_tab_change
                    // Buttons, not spans: an unreadable link disables
                    // both. Export posts the effective query, which is
                    // the blanked sentinel then, and the server reads
                    // an empty query as every row (ADR-0027).
                    trailing=Box::new(move || view! {
                        <button
                            type="button"
                            class="action save"
                            disabled=move || unreadable.get()
                            on:click=move |_| on_save.run(())
                        >"Save"</button>
                        <button
                            type="button"
                            class="action export"
                            disabled=move || unreadable.get()
                            on:click=move |_| on_export.run(())
                        >"Export"</button>
                    }.into_any())
                />
                // Above the results body, not inside it: a zero-row
                // answer is exactly when "some values are missing" is
                // worth reading, and the Visualization tab is drawn from
                // the same incomplete rows.
                <DegradedNotice query=effective_q fields=degraded_fields/>
                {move || if unreadable.get() {
                    // The banner above IS the results pane while the
                    // link cannot be read.
                    ().into_any()
                } else { match (active_tab.get(), mode.get()) {
                    (ResultsTab::Events, Mode::Snapshot) => view! {
                        <>
                            <Histogram rows=rows range=range_sig/>
                            <ResultsTable
                                busy=running
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
                }}}
            </div>
        </div>
        {move || save_query.get().map(|query| view! {
            <SaveAsNetModal
                query=query
                on_close=Callback::new(move |_| save_query.set(None))
            />
        })}
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
fn LiveRawTable(#[prop(into)] result: Signal<QueryResult>) -> impl IntoView {
    view! {
        <div class="results">
            {move || {
                let r = result.get();
                if r.columns.is_empty() {
                    view! {
                        <div class="results-empty">"Streaming — waiting for first event…"</div>
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
