// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ResourceNotice/>` — what the results region shows in place of the
//! table or chart when the server answered a snapshot with
//! `query_resource_exhausted` (ADR-0056).

use leptos::prelude::*;

#[component]
pub fn ResourceNotice(message: String) -> impl IntoView {
    view! {
        <div class="resource-notice" role="alert">
            <p class="resource-notice-title">"Query ran out of resources"</p>
            <p class="resource-notice-text">{message}</p>
        </div>
    }
}
