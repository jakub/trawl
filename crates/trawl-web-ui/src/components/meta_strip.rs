// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Execution facts at the foot of the query console, with URL filter
//! chips and mode.

use leptos::prelude::*;

use crate::search_status::execution_started;
use crate::state::query::{Filter, FilterOp};
use fleet_ui::time::format_duration;
use fleet_ui::{Badge, Tone};
use trawl_api::QueryExecution;

#[component]
pub fn MetaStrip(
    #[prop(into)] execution: Signal<Option<QueryExecution>>,
    #[prop(into)] filters: Signal<Vec<Filter>>,
    #[prop(into)] filters_unreadable: Signal<bool>,
    #[prop(into)] blocked: Signal<bool>,
    #[prop(into)] live: Signal<bool>,
    #[prop(into)] count: Signal<Option<usize>>,
    /// True while a snapshot is in flight. The count then reads as an
    /// ellipsis rather than as the rows of the response the resource is
    /// still holding, which belong to the previous query.
    #[prop(into)]
    pending: Signal<bool>,
    on_remove: Callback<usize>,
) -> impl IntoView {
    // A count of zero is a fact and keeps the strip: presence counts, not
    // the number.
    let reports = move || {
        filters_unreadable.get()
            || (!blocked.get()
                && (pending.get()
                    || count.get().is_some()
                    || execution.get().is_some()
                    || live.get()
                    || !filters.get().is_empty()))
    };
    view! {
        <div class="scope" class:blocked=move || blocked.get() class:empty=move || !reports()>
            <Show when=move || !blocked.get()>
                <div class="scope-facts">
                    <span class="scope-count">
                        {move || if pending.get() {
                            "…".to_string()
                        } else {
                            count.get().map_or_else(
                                String::new,
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
                            <span class="scope-execution">{format!("Execution in {}", format_duration(facts.duration_ms))}</span>
                            {started.map(|value| view! { <span class="scope-started">{format!("Started {value}")}</span> })}
                        }
                    })}
                </div>
            </Show>
            <div class="meta-chips">
                <Show when=move || !blocked.get()>
                    {move || filters.get().into_iter().enumerate().map(|(i, f)| {
                        let is_excl = f.op == FilterOp::Exclude;
                        let label = format!(
                            "{} {} {}",
                            f.field,
                            if is_excl { "≠" } else { "=" },
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
