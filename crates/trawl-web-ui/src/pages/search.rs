// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/search` — query workspace.
//!
//! Layout (top to bottom inside `.search-col`, the page's one sheet):
//! `h1` → query console (header + draft state + `DslEditor` + date
//! range + Haul + tools + the executed-scope strip)
//! → tabs (Events / Visualization · trailing Truncated / Stop live /
//!   Save / Export)
//! → degraded-field notice (hidden unless the execution reported one)
//! → tab body (Events: histogram + results table | Visualization: chart)
//!
//! Two skip links open the page ahead of the filter rail, because the
//! rail's value controls stay in the tab order on purpose: one focuses
//! the editor, one the results region (audit finding A03).
//!
//! State split:
//! - `query_text` — in-progress editor buffer (not URL-synced).
//! - `executed_q` / `filters` / `range` — URL-driven memos (canonical).
//! - `effective_q` — derived from the triple AND the mode; what actually
//!   hits the server. A snapshot folds the range in, a live stream never
//!   does (ADR-0027, amended 2026-09-21), and `mode_query` is the one
//!   place that decides. `export_q` is its always-snapshot twin, because a
//!   download from a live page is still asking for the URL's range.
//!   Filter chips in the scope strip and the date-range popover mutate
//!   state by navigating; URL drives memos drives resource.
//! - `draft_dirty` — the console header's own comparison of the two.
//!   It reads both and writes neither, so saying "unsent changes"
//!   cannot itself become a navigation (ADR-0027).
//! - `selected` / `result_gen` — the docked inspector's selection, a
//!   `(response generation, ORIGINAL row index)` pair. Sorting re-orders
//!   the rows on screen but never the pair, so a re-sort keeps the same
//!   event open; a new response, a page turn or a new effective query
//!   bumps the generation, which closes the panel.
//!
//! Both reading modes (`View`) default off and apply to SNAPSHOT raw
//! results only: a live ring evicts rows every frame, so a selection in
//! it would be cleared continuously (ADR-0032).

use leptos::prelude::*;
use leptos::web_sys;
use trawl_api::value::QueryResult;
use wasm_bindgen::JsCast;

use crate::categorical::{CatShape, detect as detect_categorical};
use crate::components::cat_chart::CatChart;
use crate::components::chart::Chart;
use crate::components::degraded_notice::DegradedNotice;
use crate::components::editor_wrap::EditorWrap;
use crate::components::exact_table::ExactTable;
use crate::components::export_modal::ExportModal;
use crate::components::facet_sidebar::FacetSidebar;
use crate::components::histogram::Histogram;
use crate::components::malformed_notice::MalformedNotice;
use crate::components::meta_strip::MetaStrip;
use crate::components::results_table::ResultsTable;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::components::search_quick_start::SearchQuickStart;
use crate::facets::is_aggregation_shape;
use crate::fetch_plan::FetchPlan;
use crate::pages::layout::ShellStatus;
use crate::result_actions::sorted_page;
use crate::search_status::{CountSource, FooterCount, StatusInputs, StatusKind, search_status};
use crate::search_url::{PAGE_SIZE, Param, admit_filters, refusal_copy};
use crate::state::query::{
    Filter, Mode, UrlSignals, mode_query, navigator, replace_navigator, report_refusal, url_signals,
};
use crate::state::search_session::rows_resource;
use fleet_ui::overlay::use_overlay_layer;
use fleet_ui::{
    Details, LoadState, Rows, Segmented, SegmentedOption, Size, TabItem, Tabs, ToastBus, ToastKind,
    UiPrefs,
};
use leptos::ev;
use leptos_use::{use_event_listener, use_window};

use crate::state::stream_session::{
    LiveSignals, RingBuffer, StreamLifecycle, ring_to_result, start_stream,
};

/// Move keyboard focus to the first element matching `selector`, and
/// report whether anything took it.
///
/// Both skip links are real anchors, so the browser already moves the
/// document to the target; neither target takes focus from an `href`
/// alone — `.cm-content` is `CodeMirror`'s own contenteditable, and the
/// results tab is a `tabindex="0"` control — so the handler supplies it.
///
/// The return value is what keeps the link honest. A missing target is
/// a real case: the results header is absent while the malformed banner
/// is up (ADR-0027). Swallowing the default there left the link inert —
/// it prevented the navigation AND focused nothing — so the caller
/// prevents the default only when the focus landed, and otherwise lets
/// the browser move the document the way an anchor always does.
fn focus_search_control(selector: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.query_selector(selector).ok().flatten())
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
        .is_some_and(|element| element.focus().is_ok())
}

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

    // The console header's draft state. Trimmed on both sides: trailing
    // whitespace the editor adds is not an unsent change, and `Haul`
    // would produce the same link.
    let draft_dirty = Memo::new(move |_| query_text.get().trim() != executed_q.get().trim());

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

    // The DSL this MODE runs. A snapshot folds the URL's range in; a live
    // stream never does (ADR-0027, amended 2026-09-21): the stream lane
    // evaluates `last=` per event against the server clock, so a folded
    // range silently dropped every event older than the window while the
    // page still said Live. `mode.get()` is read here, not outside: the
    // mode flip alone is what changes this value when q, f and r all stand.
    let effective_q = Memo::new(move |_| {
        if unreadable.get() {
            return String::new();
        }
        mode_query(mode.get(), &executed_q.get(), &filters.get(), &range.get())
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
    // still drives the stream and the notice; the export modal reads
    // `export_q`, which is the snapshot DSL in either mode.
    let snapshot_q = Memo::new(move |_| {
        if live.get() {
            String::new()
        } else {
            effective_q.get()
        }
    });

    // How much of the result one request asks for (ADR-0037): one page
    // of a raw-event query, the whole of an aggregation. The page is an
    // input, so a page turn under `Whole` produces the same plan and the
    // resource below does not re-fire.
    let fetch_plan = Memo::new(move |_| FetchPlan::for_query(&snapshot_q.get(), page.get()));

    let (query_pending, set_query_pending) = signal(false);
    let request_generation = RwSignal::new(0_u64);
    let (rows, request_intent) = rows_resource(
        snapshot_q,
        fetch_plan,
        set_query_pending,
        request_generation,
        executed_q,
        filters,
        range,
    );

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
            // Plan equality, not `page == 0`: the navigation below goes
            // to page 0, and the resource re-fires only when that moves
            // the request. It does not when the plan is already the
            // page-0 plan — either the page is 0, or the query is an
            // aggregation whose plan is `Whole` on every page — and
            // those are exactly the cases this Haul must refetch.
            let rerun = !live.get_untracked()
                && fetch_plan.get_untracked()
                    == FetchPlan::for_query(&snapshot_q.get_untracked(), 0)
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
        Callback::new(move |nav: crate::context_query::SearchNavigation| {
            let new_q = nav.query;
            if unreadable.get_untracked() {
                return;
            }
            // Buffer after navigation, not before it: a refused link
            // must leave the editor exactly as the reader left it.
            let outcome = goto(&new_q, 0, Mode::Snapshot, &[], &nav.range, false);
            if outcome.is_ok() {
                query_text.set(new_q);
            }
            report_refusal(bus, outcome);
        })
    };

    let on_result_filter = {
        let goto = goto.clone();
        Callback::new(
            move |(query, filter): (crate::state::search_session::ExecutedQuery, Filter)| {
                if unreadable.get_untracked() {
                    return;
                }
                let mut filters = query.filters;
                if filters.contains(&filter) {
                    return;
                }
                filters.push(filter);
                if let Err(reason) = admit_filters(&filters) {
                    bus.push(ToastKind::Error, refusal_copy(reason), None);
                    return;
                }
                report_refusal(
                    bus,
                    goto(
                        &query.base,
                        0,
                        Mode::Snapshot,
                        &filters,
                        &query.range,
                        false,
                    ),
                );
            },
        )
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

    // The exact table's sort column and direction, owned here rather
    // than inside the table: the bars beside it slice the SAME sorted
    // list, and a sort that lived privately in the table would put the
    // two on different orders the moment a page turned. A page turn
    // under a fetched-whole aggregation keeps it, since the rows on
    // screen are still that result.
    //
    // Reset on the RESPONSE's identity, not on the pending query. The
    // rows on screen during a round trip belong to the previous
    // response, and resetting when the next query is submitted made
    // them visibly shuffle back to server order for the whole flight,
    // under a sort control that still claimed the old column. They hold
    // their order until their replacement lands.
    //
    // The executed query alone, not the response generation: a
    // same-query Haul re-runs the identical query, so its rows are the
    // same rows and the sort survives it (the C4 contract). Keying on
    // the generation would reset on every refetch instead.
    let sort = RwSignal::new(None::<(usize, bool)>);
    Effect::new(move |prev: Option<Option<String>>| {
        let previous = prev.flatten();
        // Nothing landed — a first request in flight, or a failure.
        // Hold the last answer rather than reading the gap as a change.
        let Some(landed) = rows.get().and_then(Result::ok).map(|r| r.query.effective) else {
            return previous;
        };
        if previous.is_some_and(|before| before != landed) {
            sort.set(None);
        }
        Some(landed)
    });

    let is_chart_query = Memo::new(move |_| is_aggregation_shape(&effective_q.get()));

    let ring_result = Memo::new(move |_| ring_to_result(&ring.read()));

    let loading = Signal::derive(move || {
        !snapshot_q.get().trim().is_empty()
            && (query_pending.get()
                || match rows.get() {
                    None => true,
                    Some(Ok(response)) => {
                        // The plan, not the page: under `Whole` the
                        // response answers every page of this query, so
                        // comparing pages would leave the spinner up for
                        // a page turn that posts nothing.
                        response.query.effective != snapshot_q.get()
                            || response.plan != fetch_plan.get()
                            || response.generation != request_generation.get()
                            || response.intent != request_intent.get()
                    }
                    Some(Err(_)) => false,
                })
    });
    // Only an accepted response can supply execution facts. The resource
    // retains its previous value during reloads, including same-query Hauls.
    let accepted_snapshot = Signal::derive(move || {
        if unreadable.get() || live.get() || snapshot_q.get().trim().is_empty() || loading.get() {
            return None;
        }
        rows.get().and_then(Result::ok)
    });
    let execution = Signal::derive(move || {
        accepted_snapshot
            .get()
            .and_then(|response| response.response.execution)
    });

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
        if loading.get() {
            return LoadState::Loading;
        }
        LoadState::from_resource(rows.get().map(|r| r.map(|resp| resp.response.result)))
    });
    // A resource holds its previous response while the next request is
    // in flight, so a count taken straight off `active_rows` would put
    // the old page's row count under the new executed scope and on the
    // Events tab beside it. No count until the answer matches.
    let active_row_count = Signal::derive(move || match active_rows.get() {
        LoadState::Ready(result) if live.get() || accepted_snapshot.get().is_some() => {
            Some(result.rows.len())
        }
        _ => None,
    });

    // A resource keeps its previous response while the next one loads,
    // so "did a snapshot run" is `snapshot_q`, not the mode: the strip
    // and the notice below must not describe the page live replaced for
    // the frame it takes the empty query to resolve.
    let snapshot_ran = Signal::derive(move || !snapshot_q.get().trim().is_empty());

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
                accepted_snapshot
                    .get()
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

    // The degraded fields this execution reported. Read off the
    // response, never re-derived and never refreshed from the catalog:
    // it describes the answer already on screen.
    let degraded_fields = Signal::derive(move || {
        if unreadable.get() || !snapshot_ran.get() {
            return Vec::new();
        }
        accepted_snapshot
            .get()
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
    // Export is a bounded one-shot download, never the stream, so it runs
    // the snapshot DSL in BOTH modes: the range the URL carries is exactly
    // what a download from a live page is asking for, and a range-free
    // export would pull the whole corpus into a file, the door ADR-0027's
    // empty-query rule guards.
    let export_q = Memo::new(move |_| {
        if unreadable.get() {
            return String::new();
        }
        mode_query(
            Mode::Snapshot,
            &executed_q.get(),
            &filters.get(),
            &range.get(),
        )
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
    // Field provenance follows the active result source, never the editor.
    let facet_capabilities = Signal::derive(move || {
        let query = if live.get() {
            effective_q.get()
        } else {
            rows.get()
                .and_then(Result::ok)
                .map_or_else(String::new, |r| r.query.effective)
        };
        crate::result_actions::Capabilities::for_query(&query)
    });
    // A pending URL can hide stale row-derived groups, but its filter count
    // and Clear all are URL-owned and remain safe to use.
    let facet_rows_suppressed = Signal::derive(move || {
        !facet_capabilities.get().raw_facets()
            || (!live.get()
                && rows
                    .get()
                    .and_then(Result::ok)
                    .is_some_and(|r| r.query.effective != effective_q.get()))
    });

    // --- reading modes ------------------------------------------------
    // Both default off (ADR-0032): with the defaults in force the
    // results DOM is exactly what it was before this slice.
    let prefs = use_context::<UiPrefs>();
    let details = Signal::derive(move || prefs.map_or(Details::Inline, |p| p.details().get()));
    let rows_mode = Signal::derive(move || prefs.map_or(Rows::Compact, |p| p.rows().get()));
    let set_details = Callback::new(move |id: String| {
        if let Some(p) = prefs {
            p.details().set(if id == "inspector" {
                Details::Inspector
            } else {
                Details::Inline
            });
        }
    });
    let set_rows = Callback::new(move |id: String| {
        if let Some(p) = prefs {
            p.rows().set(if id == "message-first" {
                Rows::MessageFirst
            } else {
                Rows::Compact
            });
        }
    });

    let view_open = RwSignal::new(false);
    let view_wrap = NodeRef::<leptos::html::Div>::new();
    let view_btn = NodeRef::<leptos::html::Button>::new();
    let close_view = Callback::new(move |()| view_open.set(false));

    // The inspector's selection. Owned here rather than in the table,
    // because only this component sees the three things that invalidate
    // it: a fresh response, a page turn and a new effective query.
    let selected = RwSignal::new(None::<(u64, usize)>);
    let result_gen = RwSignal::new(0_u64);
    Effect::new(move |_| {
        let _ = rows.get();
        result_gen.update(|g| *g = g.wrapping_add(1));
    });
    Effect::new(move |_| {
        snapshot_q.track();
        page.track();
        selected.set(None);
    });
    let generation = Signal::derive(move || result_gen.get());

    // The categorical shape of the snapshot aggregate on screen, if it
    // has one. Read off the executed query and the response together:
    // only the `stats … by <field>` stage knows which column is the
    // group, and a chart drawn without it would label the wrong axis.
    let cat_shape: Memo<Option<CatShape>> = Memo::new(move |_| {
        // The resource holds the PREVIOUS response while the next
        // request is in flight, and `effective_q` follows the URL at
        // once, so detection would read an old result through a new
        // query and label the wrong axis. No chart while pending — the
        // same rule the histogram caption follows.
        if loading.get() {
            return None;
        }
        rows.get()
            .and_then(Result::ok)
            .and_then(|resp| detect_categorical(&effective_q.get(), &resp.result))
    });

    view! {
        <div class="search-layout">
            // Ahead of the rail, whose value controls stay in the tab
            // order by design: the two bypasses are what keeps that from
            // costing 110 tab stops to reach the editor (A03).
            <a class="skip-link" href="#search-query" on:click=move |event: web_sys::MouseEvent| {
                if focus_search_control(".dsl-editor [contenteditable=true]") {
                    event.prevent_default();
                }
            }>"Skip to query editor"</a>
            <a class="skip-link" href="#search-results" on:click=move |event: web_sys::MouseEvent| {
                if focus_search_control("[role=tablist][aria-label=Results] [role=tab][tabindex='0']") {
                    event.prevent_default();
                }
            }>"Skip to results"</a>
            <FacetSidebar
                state=active_rows
                filters=filters_sig
                suppressed=unreadable
                rows_suppressed=facet_rows_suppressed
                capabilities=facet_capabilities
                on_add=on_add_filter
                on_clear=on_clear_filters
            />
            <div class="search-col">
                <h1 class="search-heading">"Search"</h1>
                <div class="console">
                    <EditorWrap
                        query=query_text
                        on_submit=on_submit
                        range=range_sig
                        on_range_change=on_range_change
                        reset_key=raw_search
                        running=running
                        blocked=unreadable
                        draft_dirty=draft_dirty
                        on_save=on_save
                        on_live=on_live
                    />
                    // The strip under the editor describes the EXECUTED
                    // query, not the buffer above it. Timing and row count
                    // belong to the accepted response; chips and mode come
                    // from the URL (ADR-0027).
                    <MetaStrip
                        execution=execution
                        filters=filters_sig
                        filters_unreadable=filters_unreadable
                        blocked=unreadable
                        live=live
                        count=active_row_count
                        pending=loading
                        on_remove=on_remove_filter
                    />
                </div>
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
                        // The reading-mode disclosure. A button plus a
                        // popover rather than two segmented strips in the
                        // header: the header is a 36-40px row and the
                        // modes are read rarely, so they are one press
                        // away instead of permanently spending its width.
                        <div class="view-wrap" node_ref=view_wrap>
                            <button
                                type="button"
                                class="action view"
                                aria-expanded=move || view_open.get().to_string()
                                aria-controls="search-view"
                                node_ref=view_btn
                                on:click=move |_| view_open.update(|open| *open = !*open)
                            >"View"</button>
                            <Show when=move || view_open.get()>
                                <ViewPanel
                                    details=details
                                    rows_mode=rows_mode
                                    aggregate=is_chart_query
                                    on_details=set_details
                                    on_rows=set_rows
                                    on_close=close_view
                                    wrap=view_wrap
                                    trigger=view_btn
                                />
                            </Show>
                        </div>
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
                    && (active_tab.get() == ResultsTab::Events || !is_chart_query.get())
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
                } else if mode.get() == Mode::Snapshot && !snapshot_ran.get() {
                    // Both tabs start here. Examples use the picker and filters
                    // just like Haul, and only change the editor after admission.
                    let goto = goto.clone();
                    view! { <SearchQuickStart on_run=Callback::new(move |query: &'static str| {
                        if unreadable.get_untracked() {
                            return;
                        }
                        let outcome = goto(query, 0, Mode::Snapshot, &filters.get_untracked(), &range.get_untracked(), false);
                        if outcome.is_ok() {
                            query_text.set(query.to_string());
                            active_tab.set(ResultsTab::Events);
                            // The Run button leaves the DOM with the guide.
                            // Focus the replacement region after navigation renders.
                            request_animation_frame(move || {
                                focus_search_control("#search-results");
                            });
                        }
                        report_refusal(bus, outcome);
                    })/> }.into_any()
                } else {
                    match (active_tab.get(), mode.get()) {
                    // An aggregation answers in exact numbers, so the
                    // table drops the expansion column and offers a
                    // search only on the fields the query grouped by
                    // (F02). The chart beside it is decoration over the
                    // same numbers, which is why it is aria-hidden.
                    (ResultsTab::Events, Mode::Snapshot) if is_chart_query.get() => view! {
                        <>
                            <div class="agg-split" class:has-chart=move || cat_shape.get().is_some()>
                                <ExactTable
                                    busy=running
                                    page=page
                                    rows=rows
                                    sort=sort
                                    on_paginate=on_paginate
                                    on_add_filter=on_result_filter
                                />
                                {move || {
                                    // The shape — and with it the scale every
                                    // bar is measured against — is detected
                                    // over the WHOLE fetched result, so a bar
                                    // keeps its width from page to page. The
                                    // bars themselves draw the page the table
                                    // is showing, through the same sorted
                                    // index list the table cut it with.
                                    let shape = cat_shape.get()?;
                                    let resp = rows.get()?.ok()?;
                                    let whole = resp.response.result;
                                    let indices = sorted_page(&whole.rows, sort.get(), page.get(), PAGE_SIZE);
                                    let drawn = QueryResult {
                                        columns: whole.columns.clone(),
                                        rows: indices
                                            .into_iter()
                                            .filter_map(|i| whole.rows.get(i).cloned())
                                            .collect(),
                                    };
                                    Some(view! { <CatChart shape=shape result=drawn/> })
                                }}
                            </div>
                        </>
                    }.into_any(),
                    (ResultsTab::Events, Mode::Snapshot) => view! {
                        <>
                            <Histogram rows=rows/>
                            <ResultsTable
                                busy=running
                                page=page
                                rows=rows
                                on_paginate=on_paginate
                                on_add_filter=on_result_filter
                                on_navigate=on_navigate_q
                                details=details
                                rows_mode=rows_mode
                                selected=selected
                                generation=generation
                            />
                        </>
                    }.into_any(),
                    (ResultsTab::Events, Mode::Live) if is_chart_query.get() => view! {
                        <LiveRawTable result=Signal::derive(move || live_snapshot.get().unwrap_or_else(QueryResult::empty)) failure=stream_failure waiting="Waiting for the first live aggregation snapshot."/>
                    }.into_any(),
                    (ResultsTab::Events, Mode::Live) => view! {
                        <LiveRawTable result=ring_result failure=stream_failure/>
                    }.into_any(),
                    // The Visualization pane IS the results region while
                    // it is the active tab: only one tab is mounted, so
                    // the id stays unique and "Skip to results" reaches
                    // the chart the same way it reaches a table.
                    (ResultsTab::Visualization, Mode::Snapshot) => {
                        let inner = if loading.get() {
                            view! { <p class="results-empty" role="status">"Loading snapshot visualization…"</p> }.into_any()
                        } else {
                            match rows.get() {
                                Some(Ok(resp)) => {
                                    // The measured window this response
                                    // arrived in. The chart draws a whole
                                    // result or says why not (ADR-0037),
                                    // and only these three numbers can
                                    // tell it which it has.
                                    let coverage = resp.pagination.clone();
                                    let result = resp.result.clone();
                                    view! { <Chart snapshot=Signal::derive(move || Some(result.clone())) query=effective_q coverage=Signal::derive(move || Some(coverage.clone()))/> }.into_any()
                                }
                                Some(Err(_)) => view! { <div class="results-empty"><p role="alert">"Snapshot query failed. Open Events for the query error."</p><button type="button" class="btn-sec" on:click=move |_| rows.refetch()>"Retry snapshot"</button></div> }.into_any(),
                                None => view! { <p class="results-empty">"Run a query to visualize its snapshot."</p> }.into_any(),
                            }
                        };
                        view! { <div id="search-results" class="results" role="region" aria-label="Search results" tabindex="0">{inner}</div> }.into_any()
                    },
                    (ResultsTab::Visualization, Mode::Live) if is_chart_query.get() => view! {
                        <div id="search-results" class="results" role="region" aria-label="Search results" tabindex="0">
                            // A live stream asks for no window, so there
                            // is no coverage to judge: every frame is the
                            // whole of what the server aggregated.
                            <Chart snapshot=live_snapshot query=effective_q coverage=Signal::derive(|| None) failure=stream_failure on_retry=retry_stream/>
                        </div>
                    }.into_any(),
                    (ResultsTab::Visualization, Mode::Live) => view! {
                        <div id="search-results" class="results" role="region" aria-label="Search results" tabindex="0">
                            <Show when=move || stream_failure.get().is_none()>
                                <p class="results-empty">"Live event queries appear in Events. Use timechart with a count metric for a live visualization."</p>
                            </Show>
                        </div>
                    }.into_any(),
                }}}
            </div>
        </div>
        {move || save_query.get().map(|query| view! {
            <SaveAsNetModal
                from_editor=true
                query=query
                on_close=Callback::new(move |_| save_query.set(None))
            />
        })}
        <Show when=move || show_export_modal.get()>
            <ExportModal
                query=export_q.get_untracked()
                on_close=Callback::new(move |_| show_export_modal.set(false))
            />
        </Show>
    }
}

/// The reading-mode popover behind the result header's "View" button.
///
/// A `FocusPolicy::None` overlay layer (`fleet_ui::overlay`), so it
/// arbitrates Escape against whatever else is open without taking focus.
/// The chord IS silenced while it is open: `has_layers()` counts layers
/// and ignores their focus policy, so the command palette treats this
/// popover like any other overlay. It closes on Escape while topmost —
/// returning focus to the trigger — and on any mousedown outside its
/// wrapper.
#[component]
fn ViewPanel(
    #[prop(into)] details: Signal<Details>,
    #[prop(into)] rows_mode: Signal<Rows>,
    /// Aggregation-shaped results always render the plain table, so the
    /// Rows group states that instead of offering a choice it would not
    /// honour (ADR-0025: nothing advertises a capability that does not
    /// exist).
    #[prop(into)]
    aggregate: Signal<bool>,
    on_details: Callback<String>,
    on_rows: Callback<String>,
    on_close: Callback<()>,
    wrap: NodeRef<leptos::html::Div>,
    trigger: NodeRef<leptos::html::Button>,
) -> impl IntoView {
    let layer = use_overlay_layer();

    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if e.key() != "Escape" || !layer.is_topmost() {
            return;
        }
        e.prevent_default();
        on_close.run(());
        if let Some(btn) = trigger.get_untracked() {
            let _ = btn.focus();
        }
    });
    // Identity through `contains`, the same rule the modal and drawer
    // scrims use: a press on the trigger is inside the wrapper, so the
    // button's own click still toggles instead of re-opening.
    let _ = use_event_listener(use_window(), ev::mousedown, move |e| {
        let Some(host) = wrap.get_untracked() else {
            return;
        };
        let inside = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Node>().ok())
            .is_some_and(|node| host.contains(Some(&node)));
        if !inside {
            on_close.run(());
        }
    });

    view! {
        <div id="search-view" class="view-pop">
            <div role="group" aria-labelledby="view-details-lb">
                <span id="view-details-lb" class="view-lb">"Details"</span>
                <Segmented
                    size=Size::Xs
                    options=vec![
                        SegmentedOption::new("inline", "Inline"),
                        SegmentedOption::new("inspector", "Inspector"),
                    ]
                    active=Signal::derive(move || details.get().as_attr().to_string())
                    on_change=on_details
                />
            </div>
            <div role="group" aria-labelledby="view-rows-lb">
                <span id="view-rows-lb" class="view-lb">"Rows"</span>
                <Show
                    when=move || !aggregate.get()
                    fallback=move || view! {
                        <p class="view-note">"Aggregation results always use the plain table"</p>
                    }
                >
                    <Segmented
                        size=Size::Xs
                        options=vec![
                            SegmentedOption::new("compact", "Compact"),
                            SegmentedOption::new("message-first", "Message first"),
                        ]
                        active=Signal::derive(move || rows_mode.get().as_attr().to_string())
                        on_change=on_rows
                    />
                </Show>
            </div>
        </div>
    }
}

/// Simple table rendering for the ring-buffered raw-event live feed.
#[component]
fn LiveRawTable(
    #[prop(default = "Streaming — waiting for first event…")] waiting: &'static str,
    #[prop(into)] result: Signal<QueryResult>,
    #[prop(into)] failure: Signal<Option<&'static str>>,
) -> impl IntoView {
    view! {
        // Same region contract as the snapshot tables: "Skip to results"
        // reaches live mode too, so the container must be able to hold
        // focus and name itself.
        <div id="search-results" class="results" role="region" aria-label="Search results" tabindex="0">
            {move || {
                let r = result.get();
                if r.columns.is_empty() {
                    view! {
                        <Show when=move || failure.get().is_none()>
                            <div class="results-empty">{waiting}</div>
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
