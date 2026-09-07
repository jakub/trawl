// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<MalformedNotice/>` — what a search link that cannot be read shows
//! instead of results (ADR-0027).
//!
//! The URL is the document. When one of its structured parameters does
//! not parse, the page shows the parameter's name and the raw value the
//! link actually carries, and runs nothing: a query that quietly dropped
//! a filter would answer a question nobody asked. The URL is left
//! exactly as it arrived so it can be sent back to whoever shared it,
//! and the one repair button rewrites it only when clicked.
//!
//! The raw value is attacker-supplied text from the address bar. It
//! renders in a leptos text position (never as markup) and truncated,
//! because a 4 KiB `f` payload is not a sentence.

use leptos::prelude::*;

use crate::search_url::{Malformed, Repair};

/// One line over the results: which parameter could not be read, what it
/// says, and the single repair on offer.
#[component]
pub fn MalformedNotice(
    /// The parameter that could not be read, or `None` when the link is
    /// readable — which is the ordinary case and renders nothing.
    #[prop(into)]
    malformed: Signal<Option<Malformed>>,
    /// The repair on offer, already admitted by `search_url::plan_repair`
    /// — usually the named parameter's default, and "Start over" when
    /// that one would build a link this app cannot read back. The button
    /// reads its label, so what it says is a verdict rather than a hope
    /// about what the click will do.
    #[prop(into)]
    repair: Signal<Option<Repair>>,
    /// Navigates to that repair, replacing the current history entry.
    /// Nothing else in the app navigates on a decode.
    on_repair: Callback<()>,
) -> impl IntoView {
    view! {
        // The live region outlives the notice inside it, same as the
        // degraded notice: a region that mounts with its content is not
        // reliably announced.
        <div class="url-live" aria-live="polite">
            {move || malformed.get().map(|m| {
                // The whole-link verdict retains none of the raw value —
                // it is tens of kilobytes of address bar and its message
                // introduces nothing — so the echo is rendered only when
                // there is something to echo.
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
