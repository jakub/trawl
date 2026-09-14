// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<MetaStrip/>` — the executed-scope strip at the foot of the query
//! console.
//!
//! It describes the query the LINK ran, never the editor buffer: the
//! window comes from `effective_window(&executed_q, &range)`, the chips
//! from the URL's filters, the mode from `?mode=`, and the count from
//! the active result source. Typing changes none of them, which is the
//! executed-query vs editor-buffer distinction ADR-0027 draws, stated in
//! words instead of left for the reader to infer.
//!
//! While the link cannot be read the strip states nothing at all beyond
//! the "filters unreadable" chip: no window, no badge, no count and no
//! remove controls, because nothing ran and removing a chip navigates
//! (ADR-0027). The truncation notice lives in the result header, beside
//! the row count it qualifies.

use leptos::prelude::*;

use crate::state::query::{EffectiveWindow, Filter, FilterOp, window_caption};
use fleet_ui::{Badge, Tone};

#[component]
pub fn MetaStrip(
    /// The window the executed query ran under — the same phrase the
    /// histogram caption carries.
    #[prop(into)]
    window: Signal<EffectiveWindow>,
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
    /// Called with the index of a filter to remove.
    on_remove: Callback<usize>,
) -> impl IntoView {
    view! {
        <div class="scope" class:blocked=move || blocked.get()>
            <Show when=move || !blocked.get()>
                <span class="scope-lb">"Executed scope"</span>
                <span class="scope-window">{move || window_caption(&window.get())}</span>
            </Show>
            <div class="meta-chips">
                {move || filters.get().into_iter().enumerate().map(|(i, f)| {
                    let is_excl = f.op == FilterOp::Exclude;
                    let label = format!(
                        "{}{} = {}",
                        if is_excl { "⊘ " } else { "◆ " },
                        f.field,
                        f.value,
                    );
                    // Copy handle: the `<Show>` below re-renders its
                    // children, so the name cannot be moved into them.
                    let remove_label =
                        StoredValue::new(format!("Remove filter {} = {}", f.field, f.value));
                    view! {
                        <span class="chip" class:excl=move || is_excl>
                            <span>{label}</span>
                            <Show when=move || !blocked.get()>
                                <button
                                    type="button"
                                    class="x"
                                    aria-label=move || remove_label.get_value()
                                    on:click=move |_| on_remove.run(i)
                                ><span aria-hidden="true">"×"</span></button>
                            </Show>
                        </span>
                    }
                }).collect::<Vec<_>>()}
                <Show when=move || filters_unreadable.get()>
                    <span class="chip bad">"filters unreadable"</span>
                </Show>
            </div>
            <Show when=move || !blocked.get()>
                <span class="mode">
                    {move || if live.get() {
                        view! {
                            <Badge tone=Tone::Info>
                                <span class="live-dot" aria-hidden="true"></span>
                                "Live"
                            </Badge>
                        }.into_any()
                    } else {
                        view! { <Badge tone=Tone::Neutral>"Snapshot"</Badge> }.into_any()
                    }}
                </span>
                <span class="scope-count">
                    {move || count.get().map_or_else(
                        || "—".to_string(),
                        |n| format!("{n} rows"),
                    )}
                </span>
            </Show>
        </div>
    }
}
