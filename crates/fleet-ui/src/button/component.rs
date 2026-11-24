// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use super::variant::{Size, Variant, btn_class};

/// `on_click` is optional: a submit button inside a `<form>` needs no click
/// handler (an attribute-less `<button>` in a form defaults to
/// `type=submit`, so the form's `on:submit` fires).
#[component]
pub fn Btn(
    variant: Variant,
    #[prop(optional)] size: Size,
    #[prop(into, optional)] on_click: Option<Callback<()>>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(default = false)] full: bool,
    #[prop(default = false)] stop_propagation: bool,
    children: Children,
) -> impl IntoView {
    let class = btn_class(variant, size, full);

    view! {
        <button
            class=class
            disabled=move || disabled.get()
            on:click=move |e: leptos::web_sys::MouseEvent| {
                if stop_propagation {
                    e.stop_propagation();
                }
                if let Some(cb) = on_click {
                    cb.run(());
                }
            }
        >
            {children()}
        </button>
    }
}
