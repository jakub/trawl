// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Toggle/>` — checkbox-backed slide switch over the `.toggle` /
//! `.toggle-slider` CSS pair.

use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::JsCast;

/// Slide switch. `checked` drives the knob position reactively;
/// `on_change` fires with the new state on every flip. `label` names the
/// control independently of its checked state.
#[component]
pub fn Toggle(
    label: &'static str,
    #[prop(into)] checked: Signal<bool>,
    on_change: Callback<bool>,
) -> impl IntoView {
    view! {
        <label class="toggle">
            <input
                type="checkbox"
                aria-label=label
                prop:checked=move || checked.get()
                on:change=move |e| {
                    let Some(el) = e.target()
                        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                    else { return };
                    on_change.run(el.checked());
                }
            />
            <span class="toggle-slider"></span>
        </label>
    }
}
