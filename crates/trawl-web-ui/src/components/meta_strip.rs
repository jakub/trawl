// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<MetaStrip/>` — filter chips + truncation note. Renders nothing
//! while there are no chips and no truncation, so the strip doesn't
//! occupy a band of empty chrome. The result count lives on the
//! Events tab; save/export live in the tab strip's trailing slot.

use leptos::prelude::*;

use crate::state::query::{Filter, FilterOp};

#[component]
pub fn MetaStrip(
    /// Whether the result has been truncated server-side.
    #[prop(into)]
    truncated: Signal<bool>,
    /// Active filters — rendered as chips. Each chip has an `×` that
    /// calls `on_remove` with its index.
    #[prop(into)]
    filters: Signal<Vec<Filter>>,
    /// Called with the index of a filter to remove.
    on_remove: Callback<usize>,
) -> impl IntoView {
    view! {
        <Show when=move || truncated.get() || filters.with(|f| !f.is_empty())>
            <div class="meta">
                <div class="meta-chips">
                    {move || filters.get().into_iter().enumerate().map(|(i, f)| {
                        let is_excl = f.op == FilterOp::Exclude;
                        let label = format!(
                            "{}{} = {}",
                            if is_excl { "⊘ " } else { "◆ " },
                            f.field,
                            f.value,
                        );
                        view! {
                            <span class="chip" class:excl=move || is_excl>
                                <span>{label}</span>
                                <span
                                    class="x"
                                    on:click=move |e| {
                                        e.stop_propagation();
                                        on_remove.run(i);
                                    }
                                >"×"</span>
                            </span>
                        }
                    }).collect::<Vec<_>>()}
                </div>
                <Show when=move || truncated.get()>
                    <span class="dim">"truncated"</span>
                </Show>
            </div>
        </Show>
    }
}
