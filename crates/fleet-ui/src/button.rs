// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Typed `<Btn variant=Variant>` replacing the stringly-typed
//! `class="btn-pri"` / `"btn-sec"` / `"btn-danger"` pattern.
//!
//! Adding a new variant is a one-line enum change visible to every
//! consumer — no more drift between callers that misspell or forget a
//! class name.

use leptos::prelude::*;

/// Visual variant. Maps onto the CSS classes shipped in
/// `styles/fleet-ui.css`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Danger,
    /// The large amber form primary (bare `.btn`) — the login page's
    /// submit button. Distinct from [`Variant::Primary`], which is the
    /// compact `.btn-pri` used in modal footers and toolbars.
    Form,
}

impl Variant {
    fn css_class(self) -> &'static str {
        match self {
            Self::Primary => "btn-pri",
            Self::Secondary => "btn-sec",
            Self::Danger => "btn-danger",
            Self::Form => "btn",
        }
    }
}

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
/// `disabled` is a reactive `Signal<bool>` (`#[prop(into)]`, so plain
/// `disabled=true` still compiles); `full` opts into `btn-full`
/// (width: 100%), used on the login form's large submit button.
#[component]
pub fn Btn(
    variant: Variant,
    #[prop(into, optional)] on_click: Option<Callback<()>>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(default = false)] full: bool,
    children: Children,
) -> impl IntoView {
    let class = if full {
        format!("{} btn-full", variant.css_class())
    } else {
        variant.css_class().to_string()
    };

    view! {
        <button
            class=class
            disabled=move || disabled.get()
            on:click=move |_| {
                if let Some(cb) = on_click {
                    cb.run(());
                }
            }
        >
            {children()}
        </button>
    }
}
