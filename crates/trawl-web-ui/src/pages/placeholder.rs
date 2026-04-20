// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Coming-soon body for non-Search application modes.
//!
//! Each mode (Intel/Jobs/Settings) ships its shell — topbar + rail
//! reflect the mode's items — but the working area renders this card
//! until the corresponding APIs grow real UIs.

use leptos::prelude::*;

use crate::state::app_mode::AppMode;

#[component]
pub fn ModePlaceholder(#[prop(into)] mode: Signal<AppMode>) -> impl IntoView {
    view! {
        <div class="placeholder">
            <div class="placeholder-card">
                <div class="placeholder-eyebrow">{move || mode.get().label()}</div>
                <h2>{move || mode_headline(mode.get())}</h2>
                <p>{move || mode_blurb(mode.get())}</p>
            </div>
        </div>
    }
}

fn mode_headline(mode: AppMode) -> &'static str {
    match mode {
        AppMode::Search => "Search",
        AppMode::Intel => "Intel — coming soon",
        AppMode::Jobs => "Jobs — coming soon",
        AppMode::Settings => "Settings — coming soon",
    }
}

fn mode_blurb(mode: AppMode) -> &'static str {
    match mode {
        AppMode::Search => "Type a query and press ⌘⏎ to haul.",
        AppMode::Intel => {
            "Threat intel feeds, IoCs, and external enrichment \
             — surfaced alongside your event store."
        }
        AppMode::Jobs => {
            "Saved nets and scheduled runs — manage everything \
             from one place."
        }
        AppMode::Settings => "Sources, schema, retention, users, and API tokens.",
    }
}
