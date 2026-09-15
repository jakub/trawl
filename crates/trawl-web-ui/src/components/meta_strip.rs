// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Execution facts at the foot of the query console, with URL filter
//! chips and mode. Draft edits never change these facts. The caller supplies
//! timing only for the accepted snapshot response, and counts from the active
//! result source. Unreadable links show only their unreadable-filter notice.
//! The truncation notice remains in the result header beside its row count.

use leptos::prelude::*;

use crate::search_status::execution_started;
use crate::state::query::{Filter, FilterOp};
use fleet_ui::time::format_duration;
use fleet_ui::{Badge, Tone};
use trawl_api::QueryExecution;

#[component]
pub fn MetaStrip(
    /// Server timing belonging to the accepted snapshot response.
    #[prop(into)]
    execution: Signal<Option<QueryExecution>>,
    /// Active filters — rendered as chips. Each chip has an `×` that
    /// calls `on_remove` with its index.
    #[prop(into)]
    filters: Signal<Vec<Filter>>,
    /// True when the link's `f` parameter could not be read at all. The
    /// chips beside this one are then the empty fallback, not the
    /// link's filters, so the strip says so rather than looking like a
    /// query with no filters (ADR-0027).
    #[prop(into)]
    filters_unreadable: Signal<bool>,
    /// True while ANY of the link's parameters could not be read.
    /// Removing a chip navigates, so the `×` is not rendered at all
    /// while the link is refused — a dead control the reader can still
    /// click is worse than none (ADR-0027).
    #[prop(into)]
    blocked: Signal<bool>,
    /// True while the stream, not the snapshot resource, is the active
    /// result source.
    #[prop(into)]
    live: Signal<bool>,
    /// Rows in the active result, from whichever source the mode makes
    /// active. `None` while there is no answer to count.
    #[prop(into)]
    count: Signal<Option<usize>>,
    /// True while a snapshot is in flight. The count then reads as an
    /// ellipsis rather than as the rows of the response the resource is
    /// still holding, which belong to the previous query.
    #[prop(into)]
    pending: Signal<bool>,
    /// Called with the index of a filter to remove.
    on_remove: Callback<usize>,
) -> impl IntoView {
    view! {
        <div class="scope" class:blocked=move || blocked.get()>
            <Show when=move || !blocked.get()>
                <div class="scope-facts">
                    <span class="scope-count">
                        {move || if pending.get() {
                            "…".to_string()
                        } else {
                            count.get().map_or_else(
                                || "—".to_string(),
                                |n| if live.get() {
                                    format!("{n} buffered rows")
                                } else if n == 1 {
                                    "1 row returned".to_string()
                                } else {
                                    format!("{n} rows returned")
                                },
                            )
                        }}
                    </span>
                    {move || execution.get().map(|facts| {
                        let started = execution_started(&facts.started_at);
                        view! {
                            <span class="scope-execution">{format!("Execution {}", format_duration(facts.duration_ms))}</span>
                            {started.map(|value| view! { <span class="scope-started">{format!("Started {value}")}</span> })}
                        }
                    })}
                </div>
            </Show>
            <div class="meta-chips">
                // The whole loop, not just the remove control: a chip
                // under a refused link describes a query that did not
                // run, whichever parameter the reader got wrong.
                <Show when=move || !blocked.get()>
                    {move || filters.get().into_iter().enumerate().map(|(i, f)| {
                        let is_excl = f.op == FilterOp::Exclude;
                        let label = format!(
                            "{}{} = {}",
                            if is_excl { "⊘ " } else { "◆ " },
                            f.field,
                            f.value,
                        );
                        let remove_label = format!("Remove filter {} = {}", f.field, f.value);
                        view! {
                            <span class="chip" class:excl=move || is_excl>
                                <span>{label}</span>
                                <button
                                    type="button"
                                    class="x"
                                    aria-label=remove_label
                                    on:click=move |_| on_remove.run(i)
                                ><span aria-hidden="true">"×"</span></button>
                            </span>
                        }
                    }).collect::<Vec<_>>()}
                </Show>
                <Show when=move || filters_unreadable.get()>
                    <span class="chip bad">"filters unreadable"</span>
                </Show>
            </div>
            <Show when=move || !blocked.get() && live.get()>
                <span class="mode">
                    <Badge tone=Tone::Info>
                        <span class="live-dot" aria-hidden="true"></span>
                        "Live"
                    </Badge>
                </span>
            </Show>
        </div>
    }
}
