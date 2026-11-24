// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<MalformedNotice/>` — what a search link that cannot be read shows
//! instead of results (ADR-0027).
//!
//! The raw value is attacker-supplied text from the address bar. It
//! renders in a leptos text position (never as markup) and truncated,
//! because a 4 KiB `f` payload is not a sentence.

use leptos::prelude::*;

use crate::search_url::{Malformed, Repair};

#[component]
pub fn MalformedNotice(
    #[prop(into)] malformed: Signal<Option<Malformed>>,
    #[prop(into)] repair: Signal<Option<Repair>>,
    on_repair: Callback<()>,
) -> impl IntoView {
    view! {
        // The live region outlives the notice inside it, same as the
        // degraded notice: a region that mounts with its content is not
        // reliably announced.
        <div class="url-live" aria-live="polite">
            {move || malformed.get().map(|m| {
                // The whole-link verdict retains none of the raw value, so the
                // echo is rendered only when there is something to echo.
                let raw = m.truncated_raw();
                view! {
                <div class="url-notice">
                    <span class="url-notice-text">
                        {m.message()}
                        {(!raw.is_empty()).then(|| view! {
                            " "
                            <code class="url-notice-raw">{raw}</code>
                        })}
                    </span>
                    {move || repair.get().map(|r| view! {
                        <button
                            type="button"
                            class="url-notice-repair"
                            on:click=move |_| on_repair.run(())
                        >
                            {r.label()}
                        </button>
                    })}
                </div>
            }})}
        </div>
    }
}
