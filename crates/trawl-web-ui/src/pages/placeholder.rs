// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Coming-soon placeholders for routes without real UIs yet.

use leptos::prelude::*;

#[component]
pub fn IntelPlaceholder() -> impl IntoView {
    view! {
        <div class="placeholder">
            <div class="placeholder-card">
                <div class="placeholder-eyebrow">"Intel"</div>
                <h2>"Intel — coming soon"</h2>
                <p>
                    "Threat intel feeds, IoCs, and external enrichment \
                     — surfaced alongside your event store."
                </p>
            </div>
        </div>
    }
}

#[component]
pub fn SettingsPlaceholder() -> impl IntoView {
    view! {
        <div class="placeholder">
            <div class="placeholder-card">
                <div class="placeholder-eyebrow">"Settings"</div>
                <h2>"Settings — coming soon"</h2>
                <p>"Sources, schema, retention, users, and API tokens."</p>
            </div>
        </div>
    }
}
