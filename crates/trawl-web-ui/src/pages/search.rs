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
use crate::pages::layout::ShellStatus;
use crate::search_status::{CountSource, FooterCount, StatusInputs, StatusKind, search_status};
use crate::search_url::{Param, admit_filters, refusal_copy};
use crate::state::query::{
    Filter, Mode, RangeSpec, UrlSignals, effective_query, navigator, replace_navigator,
    report_refusal, url_signals,
};
use crate::state::search_session::rows_resource;
use fleet_ui::{LoadState, TabItem, Tabs, ToastBus, ToastKind};

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

    // Whether the stream, not the snapshot resource, is the active
    // result source. `mode=live` is a claim the page keeps true
    // (ADR-0027, amended 2026-09-12).
    let live = Signal::derive(move || mode.get() == Mode::Live);

    // What the snapshot resource runs: nothing at all while live. The
    // resource short-circuits an empty query without a round trip, so
    // no `/api/v1/query` leaves the page behind a stream, and every
    // reader of `rows` below reads the "no query yet" placeholder
    // instead of the page the previous mode left behind. `effective_q`
    // still drives the stream, the export modal and the notice.
    let snapshot_q = Memo::new(move |_| {
        if live.get() {
            String::new()
        } else {
            effective_q.get()
        }
    });

    let (query_pending, set_query_pending) = signal(false);
    let rows = rows_resource(snapshot_q, page, set_query_pending);

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
            // Hauling the query the URL already carries writes the same
            // link, and the memos behind the resource do not notify on an
            // unchanged value — so a snapshot Haul would be silent just
            // when it is the only way to retry a page that failed. Ask
            // the resource itself in that case. In live the same Haul is
            // a no-op: no snapshot runs, and the stream's own key
            // (retry, mode, effective query) has not moved.
            let rerun = !live.get_untracked()
                && page.get_untracked() == 0
                && query_text.get_untracked() == executed_q.get_untracked();
            let outcome = goto(
                &query_text.get_untracked(),
                0,
                mode.get_untracked(),
                &filters.get_untracked(),
                &range.get_untracked(),
                false,
            );
            if rerun && outcome.is_ok() {
                rows.refetch();
            }
            report_refusal(bus, outcome);
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
            // Committing a range is a bounded question, so it leaves
            // live even when it names the range already selected — the
            // short-circuit is for a selection that changes nothing at
            // all, and in live the mode is the change.
            if range.get_untracked() == new_range && !live.get_untracked() {
                return Ok(());
            }
            goto(
                &executed_q.get_untracked(),
                0,
                Mode::Snapshot,
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

    // Leaving live is a navigation, not a local pause: the same query,
    // page 0 and the URL's filters and range, with `mode` elided. Push,
    // so Back returns to the stream.
    let on_stop_live = {
        let goto = goto.clone();
        Callback::new(move |()| {
            if unreadable.get_untracked() {
                return;
            }
            report_refusal(
                bus,
                goto(
                    &executed_q.get_untracked(),
                    0,
                    Mode::Snapshot,
                    &filters.get_untracked(),
                    &range.get_untracked(),
                    false,
                ),
            );
        })
    };

    // --- live-tail state ----------------------------------------------
    let ring = RwSignal::new(RingBuffer::default());
    let live_snapshot = RwSignal::new(None::<QueryResult>);
    let lagged = RwSignal::new(None::<u64>);
    let stream_failure = RwSignal::new(None::<&'static str>);
    // Aggregation frames this session accepted — the footer's `Updates`
    // count. The ring's own `epoch` is the raw twin (`Received`).
    let frames = RwSignal::new(0_u64);
    let stream_retry = RwSignal::new(0_u64);
    let retry_stream = Callback::new(move |()| stream_retry.update(|n| *n = n.wrapping_add(1)));
    let stream_handle: StoredValue<Option<StreamLifecycle>, LocalStorage> =
        StoredValue::new_local(None);
    on_cleanup(move || {
        stream_handle.update_value(|slot| {
            *slot = None;
        });
    });

    Effect::new(move |_| {
        stream_retry.get();
        let current_mode = mode.get();
        let q = effective_q.get();
        stream_handle.update_value(|slot| *slot = None);
        ring.set(RingBuffer::default());
        live_snapshot.set(None);
        lagged.set(None);
        stream_failure.set(None);
        // A new session counts from zero. An automatic EventSource
        // reconnect does not re-run this effect, so a blip keeps the
        // rows and both counts.
        frames.set(0);

        if current_mode != Mode::Live || q.trim().is_empty() {
            return;
        }

        let signals = LiveSignals {
            ring,
            snapshot: live_snapshot,
            lagged,
            failure: Some(stream_failure),
            frames: Some(frames),
        };
        if let Some(handle) = start_stream(&q, signals) {
            stream_handle.update_value(|slot| *slot = Some(handle));
        } else {
            stream_failure.set(Some(
                "Live stream unavailable. Retry or switch to Snapshot.",
            ));
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

    // The rows on screen, from whichever source the mode makes active:
    // the snapshot page, the live ring, or the latest aggregation frame.
    // The tab count, the footer count and the filter rail all read this
    // and nothing else (ADR-0027, amended 2026-09-12).
    let active_rows = Memo::new(move |_| {
        if unreadable.get() {
            // Nothing ran, and the banner is the results pane: no source
            // to describe, so no count either.
            return LoadState::Loading;
        }
        if live.get() {
            return if is_chart_query.get() {
                // Before the first frame the stream has delivered no
                // rows, which is a count of zero rather than a pending
                // request.
                LoadState::Ready(live_snapshot.get().unwrap_or_else(QueryResult::empty))
            } else {
                LoadState::Ready(ring_result.get())
            };
        }
        LoadState::from_resource(rows.get().map(|r| r.map(|resp| resp.result)))
    });
    let active_row_count = Signal::derive(move || match active_rows.get() {
        LoadState::Ready(result) => Some(result.rows.len()),
        _ => None,
    });

    // A resource keeps its previous response while the next one loads,
    // so "did a snapshot run" is `snapshot_q`, not the mode: the strip
    // and the notice below must not describe the page live replaced for
    // the frame it takes the empty query to resolve.
    let snapshot_ran = Signal::derive(move || !snapshot_q.get().trim().is_empty());

    let loading = Signal::derive(move || {
        !snapshot_q.get().trim().is_empty() && (query_pending.get() || rows.get().is_none())
    });

    // Drive the shell's status bar from search-specific state. Both
    // derivations are pure (`search_status.rs`): the footer describes
    // the active result source, and nothing else on the page.
    let snapshot_failed = Signal::derive(move || rows.get().is_some_and(|r| r.is_err()));
    Effect::new(move |_| {
        shell_status.kind.set(search_status(StatusInputs {
            unreadable: unreadable.get(),
            live: live.get(),
            stream_failed: stream_failure.get().is_some(),
            snapshot_failed: snapshot_failed.get(),
            snapshot_pending: loading.get(),
        }));
    });
    Effect::new(move |_| {
        // Each mode's count names its own source: the rows the last
        // snapshot returned, the events delivered since the stream
        // opened (including those that rolled off the ring), or the
        // aggregation frames it accepted.
        let count = if !unreadable.get() && live.get() {
            if is_chart_query.get() {
                FooterCount {
                    source: CountSource::Updates,
                    value: Some(frames.get()),
                }
            } else {
                FooterCount {
                    source: CountSource::Received,
                    value: Some(ring.read().epoch),
                }
            }
        } else {
            FooterCount::last(if unreadable.get() || !snapshot_ran.get() {
                None
            } else {
                rows.get()
                    .and_then(Result::ok)
                    .map(|r| u64::try_from(r.pagination.returned).unwrap_or(u64::MAX))
            })
        };
        shell_status.count.set(count);
    });
    Effect::new(move |_| {
        shell_status.lagged.set(lagged.get());
    });

    on_cleanup(move || {
        shell_status.kind.set(StatusKind::Connected);
        shell_status.count.set(FooterCount::last(None));
        shell_status.lagged.set(None);
    });

    let truncated = Signal::derive(move || {
        !unreadable.get()
            && snapshot_ran.get()
            && rows.get().and_then(Result::ok).is_some_and(|r| r.truncated)
    });
    // The degraded fields this execution reported. Read off the
    // response, never re-derived and never refreshed from the catalog:
    // it describes the answer already on screen.
    let degraded_fields = Signal::derive(move || {
        if unreadable.get() || !snapshot_ran.get() {
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
                state=active_rows
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
                        TabItem::with_count(ResultsTab::Events.id(), "Events", active_row_count),
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
                        <Show when=move || live.get()>
                            <button
                                type="button"
                                class="action stop-live"
                                title="Stop the live stream and run this query once"
                                disabled=move || unreadable.get()
                                on:click=move |_| on_stop_live.run(())
                            >"Stop live"</button>
                        </Show>
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
                <Show when=move || !unreadable.get()
                    && mode.get() == Mode::Live
                    && !is_chart_query.get()
                    && stream_failure.get().is_some()
                >
                    <div class="results-empty">
                        <p role="alert">{move || stream_failure.get()}</p>
                        <button type="button" class="btn-sec" on:click=move |_| retry_stream.run(())>"Retry live stream"</button>
                    </div>
                </Show>
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
                        <Chart snapshot=live_snapshot query=effective_q failure=stream_failure on_retry=retry_stream/>
                    }.into_any(),
                    (ResultsTab::Events, Mode::Live) => view! {
                        <LiveRawTable result=ring_result failure=stream_failure/>
                    }.into_any(),
                    (ResultsTab::Visualization, Mode::Snapshot) => {
                        if loading.get() {
                            view! { <p class="results-empty" role="status">"Loading snapshot visualization…"</p> }.into_any()
                        } else {
                            match rows.get() {
                                Some(Ok(resp)) => view! { <Chart snapshot=Signal::derive(move || Some(resp.result.clone())) query=effective_q/> }.into_any(),
                                Some(Err(_)) => view! { <div class="results-empty"><p role="alert">"Snapshot query failed. Open Events for the query error."</p><button type="button" class="btn-sec" on:click=move |_| rows.refetch()>"Retry snapshot"</button></div> }.into_any(),
                                None => view! { <p class="results-empty">"Run a query to visualize its snapshot."</p> }.into_any(),
                            }
                        }
                    },
                    (ResultsTab::Visualization, Mode::Live) if is_chart_query.get() => view! {
                        <Chart snapshot=live_snapshot query=effective_q failure=stream_failure on_retry=retry_stream/>
                    }.into_any(),
                    (ResultsTab::Visualization, Mode::Live) => view! {
                        <Show when=move || stream_failure.get().is_none()>
                            <p class="results-empty">"Live event queries appear in Events. Use timechart with a count metric for a live visualization."</p>
                        </Show>
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
fn LiveRawTable(
    #[prop(into)] result: Signal<QueryResult>,
    #[prop(into)] failure: Signal<Option<&'static str>>,
) -> impl IntoView {
    view! {
        <div class="results">
            {move || {
                let r = result.get();
                if r.columns.is_empty() {
                    view! {
                        <Show when=move || failure.get().is_none()>
                            <div class="results-empty">"Streaming — waiting for first event…"</div>
                        </Show>
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
