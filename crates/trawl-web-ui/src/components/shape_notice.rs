// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ShapeNotice/>` — the SPA's rendering of `QueryResponse.notices`
//! (ADR-0013 §7).
//!
//! A fact about ONE execution, exactly like `<DegradedNotice/>` beside
//! it: the server said this query's SHAPE was confusing, the query ran
//! with pure semantics anyway, and the next query is the next answer.
//!
//! No dismissal machinery, deliberately. A degraded pin is a condition
//! the operator cannot fix from the search page, so its notice earns a
//! dismiss button; a shape advisory is fixed by editing the query that is
//! already in the box above it.
//!
//! The strings are server-authored constants, never client text — but
//! they still render through `sanitize_display_text`, because "the server
//! wrote it" is a property of today's code, not of the wire.

use leptos::prelude::*;
use trawl_core::sanitize::sanitize_display_text;

/// One line per advisory, over the results.
#[component]
pub fn ShapeNotice(
    /// `QueryResponse.notices`, verbatim.
    #[prop(into)]
    notices: Signal<Vec<String>>,
) -> impl IntoView {
    view! {
        // Same live-region discipline as the degraded notice: the region
        // outlives its content so it is reliably announced.
        <div class="deg-live" aria-live="polite">
            <For
                each=move || notices.get()
                key=std::clone::Clone::clone
                let:notice
            >
                <div class="deg-notice">
                    <span class="deg-text">{sanitize_display_text(&notice)}</span>
                </div>
            </For>
        </div>
    }
}
