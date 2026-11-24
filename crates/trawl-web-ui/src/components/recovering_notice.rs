// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RecoveringNotice/>` — what the results region shows in place of the
//! table or chart when the server refused a snapshot as
//! `corpus_recovering` (ADR-0041).

use fleet_ui::{Btn, Variant};
use leptos::prelude::*;

#[component]
pub fn RecoveringNotice(message: String, on_retry: Callback<()>) -> impl IntoView {
    view! {
        <div class="recovering-notice" role="alert">
            <p class="recovering-notice-title">"Search is recovering"</p>
            <p class="recovering-notice-text">{message}</p>
            <div class="load-recovery">
                <Btn variant=Variant::Secondary on_click=on_retry>"Retry"</Btn>
            </div>
        </div>
    }
}
