// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `<Btn>` component. Wasm-only (pulls leptos) — the pure
//! [`Variant`] enum it renders lives in [`super::variant`] so its
//! CSS-class contract is native-testable.

use leptos::prelude::*;

use super::variant::{Size, Variant, btn_class};

/// Typed button used in modal footers, toolbars, and forms.
///
/// `on_click` carries `Callback<()>` — the variant encodes the
/// semantic, the underlying mouse event is unused at every existing
/// call site. It's optional: a submit button inside a `<form>` needs
/// no click handler (an attribute-less `<button>` in a form defaults
/// to `type=submit`, so the form's `on:submit` fires). Callers that
/// need richer event data can wrap a raw `<button>` or extend this
/// signature later.
///
/// `disabled` is a reactive `Signal<bool>` (`#[prop(into, optional)]`, so
/// plain `disabled=true` still compiles — or omit it entirely for an
/// always-enabled button); `full` opts into `btn-full`
/// (width: 100%), used on the login form's large submit button.
///
/// `size` selects the compact classes (`btn-sm` / `btn-xs`) — the
/// composition rules, including `Size::Sm` rendering standalone, live in
/// the natively-tested [`btn_class`].
///
/// `stop_propagation` stops the click from bubbling before `on_click`
/// runs — for buttons nested inside clickable rows (results-table quick
/// actions, lineage entries) where the raw markup called
/// `e.stop_propagation()` by hand.
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
