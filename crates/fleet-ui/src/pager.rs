// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Pager/>` — table footer with summary + optional prev/next
//! controls.

use leptos::prelude::*;

use crate::button::{Btn, Size, Variant};

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
            >"← Prev"</Btn>
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
            >"Next →"</Btn>
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
