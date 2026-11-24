// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Field/>` — labelled form wrapper around an `<input>`/`<select>`/
//! `<textarea>` slot.

use leptos::prelude::*;

#[derive(Debug, Clone, Default)]
pub enum Helper {
    #[default]
    None,
    Hint(&'static str),
    Error(String),
}

/// Form-field wrapper. `children` is the underlying input/select/
/// textarea. In default mode the caller should set `id` on that control
/// element so the emitted `<label for=id>` actually associates; in
/// `wrap` mode nesting associates implicitly and `id` is unused for the
/// label (it still namespaces the helper paragraph).
#[component]
pub fn Field(
    #[prop(into, optional)] id: Option<&'static str>,
    label: &'static str,
    #[prop(default = Helper::None)] helper: Helper,
    #[prop(default = false)] wrap: bool,
    #[prop(default = "field")] class: &'static str,
    children: Children,
) -> impl IntoView {
    let helper_id = format!("{}-help", id.unwrap_or("field"));
    let described_by =
        matches!(helper, Helper::Hint(_) | Helper::Error(_)).then(|| helper_id.clone());

    let helper_view = match helper {
        Helper::None => None,
        Helper::Hint(h) => {
            Some(view! { <p id=helper_id.clone() class="field-hint">{h}</p> }.into_any())
        }
        Helper::Error(e) => {
            Some(view! { <p id=helper_id.clone() class="error-banner">{e}</p> }.into_any())
        }
    };

    if wrap {
        view! {
            <label class=class aria-describedby=described_by>
                <span>{label}</span>
                {children()}
                {helper_view}
            </label>
        }
        .into_any()
    } else {
        view! {
            <div class=class aria-describedby=described_by>
                <label for=id>{label}</label>
                {children()}
                {helper_view}
            </div>
        }
        .into_any()
    }
}
