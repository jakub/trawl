// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Tabs/>` — Events / Visualization for the search workspace.
//!
//! Patterns and Statistics from the design are deferred — we don't
//! have pattern detection or pre-aggregated stats yet.

use leptos::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultsTab {
    Events,
    Visualization,
}

#[component]
pub fn Tabs(
    active: RwSignal<ResultsTab>,
    /// Optional row count rendered next to the Events tab label.
    #[prop(into)]
    count: Signal<Option<usize>>,
) -> impl IntoView {
    view! {
        <div class="tabs">
            <div
                class="t"
                class:active=move || active.get() == ResultsTab::Events
                on:click=move |_| active.set(ResultsTab::Events)
            >
                <span>"Events"</span>
                {move || count.get().map(|c| view! { <span class="c">{c}</span> })}
            </div>
            <div
                class="t"
                class:active=move || active.get() == ResultsTab::Visualization
                on:click=move |_| active.set(ResultsTab::Visualization)
            >
                <span>"Visualization"</span>
            </div>
            <div class="sp"></div>
        </div>
    }
}
