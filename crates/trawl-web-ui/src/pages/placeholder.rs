// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Coming-soon placeholder for settings routes without real UIs yet.

use leptos::prelude::*;

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
