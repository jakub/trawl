// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Pager/>` — table footer with summary + optional prev/next
//! controls (issue #31).
//!
//! Unifies trawl's two footer families — `.results-footer` (results
//! table) and `.tbl-foot` (history/runs/nets/drawer, literally
//! commented "mirror of .results-footer") — onto the `.results-footer`
//! look (ADR-0003). Three shapes:
//!
//! - **summary-only** — pass just `summary` (nets' "N nets" count row);
//!   no pagination semantics are invented for unpaginated tables.
//! - **prev/next** — pass `on_prev`/`on_next` (+ `can_prev`/`can_next`)
//!   to render the compact pager buttons.
//! - **custom trailing slot** — `children` render on the right (the
//!   intel pages' cursor-based "load more" button).

use leptos::prelude::*;

use crate::button::{Btn, Size, Variant};

/// Table footer. `summary` is the left-hand count/range text
/// (`"1–50 of 213"`, `"7 nets"`); the pager controls render only when
/// the corresponding callbacks are provided.
#[component]
pub fn Pager(
    #[prop(into)] summary: Signal<String>,
    #[prop(into, optional)] can_prev: Option<Signal<bool>>,
    #[prop(into, optional)] can_next: Option<Signal<bool>>,
    #[prop(into, optional)] on_prev: Option<Callback<()>>,
    #[prop(into, optional)] on_next: Option<Callback<()>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let prev = on_prev.map(|cb| {
        let enabled = can_prev.unwrap_or_else(|| Signal::derive(|| true));
        view! {
            <Btn
                variant=Variant::Secondary
                size=Size::Sm
                disabled=Signal::derive(move || !enabled.get())
                on_click=cb
            >"← prev"</Btn>
        }
    });
    let next = on_next.map(|cb| {
        let enabled = can_next.unwrap_or_else(|| Signal::derive(|| true));
        view! {
            <Btn
                variant=Variant::Secondary
                size=Size::Sm
                disabled=Signal::derive(move || !enabled.get())
                on_click=cb
            >"next →"</Btn>
        }
    });
    let has_controls = prev.is_some() || next.is_some();

    view! {
        <footer class="results-footer">
            <span class="results-summary">{move || summary.get()}</span>
            {children.map(|c| c())}
            {has_controls.then(|| view! {
                <div class="results-pager">
                    {prev}
                    {next}
                </div>
            })}
        </footer>
    }
}
