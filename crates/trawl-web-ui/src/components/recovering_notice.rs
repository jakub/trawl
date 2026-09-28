// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<RecoveringNotice/>` — what the results region shows in place of the
//! table or chart when the server refused a snapshot as
//! `corpus_recovering` (ADR-0041).
//!
//! The server is still loading data from before a restart, or finishing
//! an interrupted storage rollup, and cannot yet count every stored event
//! once. An empty table would claim there is nothing to find, and the
//! generic "Couldn't load results" would read as a fault, so the notice
//! names the state and quotes the server's sentence under it. The
//! sentence is fixed per reason and carries no counts or paths; it
//! renders as a text node all the same.
//!
//! Unlike a query error, the same request can succeed once the server
//! catches up, so the notice keeps a Retry.

use fleet_ui::{Btn, Variant};
use leptos::prelude::*;

/// The notice for one refused snapshot.
#[component]
pub fn RecoveringNotice(
    /// The server's sentence for the reason, verbatim.
    message: String,
    /// Sends the same request again.
    on_retry: Callback<()>,
) -> impl IntoView {
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
