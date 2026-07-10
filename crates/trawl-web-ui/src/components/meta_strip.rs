// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<MetaStrip/>` — count · duration · scanned · chips · save/export.

use leptos::prelude::*;

use crate::state::query::{Filter, FilterOp};
use fleet_ui::{ToastBus, ToastKind};

#[component]
pub fn MetaStrip(
    /// Row count for the current page; `None` while loading.
    #[prop(into)]
    count: Signal<Option<usize>>,
    /// Whether the result has been truncated server-side.
    #[prop(into)]
    truncated: Signal<bool>,
    /// Active filters — rendered as chips. Each chip has an `×` that
    /// calls `on_remove` with its index.
    #[prop(into)]
    filters: Signal<Vec<Filter>>,
    /// Called with the index of a filter to remove.
    on_remove: Callback<usize>,
    /// Called when the user clicks the export action.
    on_export: Callback<()>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    view! {
        <div class="meta">
            <div class="meta-count">
                <span class="num">
                    {move || count.get().map_or_else(|| "—".to_string(), |c| c.to_string())}
                </span>
                " events"
                <Show when=move || truncated.get()>
                    <span class="divider">"·"</span>
                    <span class="dim">"truncated"</span>
                </Show>
            </div>
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
            <div class="meta-actions">
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
            </div>
        </div>
    }
}
