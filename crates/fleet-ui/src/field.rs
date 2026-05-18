// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Field/>` — labelled form wrapper around an `<input>`/`<select>`/
//! `<textarea>` slot.
//!
//! Owns the `.field` wrapper, the `<label>` text, and an optional
//! single helper paragraph (hint OR error — not both, by type
//! construction). The actual control is passed via `children` so the
//! caller controls `type`, `value`, and event handlers; the caller
//! MUST thread the `id` prop onto their inner control element so the
//! emitted `<label for=id>` and `aria-describedby` wiring resolve.

use leptos::prelude::*;

/// Optional helper text under the field. Hint XOR Error — having both
/// crowds the layout and dilutes the error message, so the type forces
/// the choice instead of accepting two independent `Option`s.
#[derive(Debug, Clone, Default)]
pub enum Helper {
    #[default]
    None,
    Hint(&'static str),
    Error(String),
}

/// Form-field wrapper. `children` is the underlying input/select/
/// textarea. The caller MUST set `id` on that control element so the
/// emitted `<label for=id>` actually associates.
#[component]
pub fn Field(
    id: &'static str,
    label: &'static str,
    #[prop(default = Helper::None)] helper: Helper,
    children: Children,
) -> impl IntoView {
    let helper_id = format!("{id}-help");
    let described_by =
        matches!(helper, Helper::Hint(_) | Helper::Error(_)).then(|| helper_id.clone());

    let helper_view = match helper {
        Helper::None => None,
        Helper::Hint(h) => {
            Some(view! { <p id=helper_id.clone() class="field-hint">{h}</p> }.into_any())
        }
        Helper::Error(e) => {
            Some(view! { <p id=helper_id.clone() class="error">{e}</p> }.into_any())
        }
    };

    view! {
        <div class="field" aria-describedby=described_by>
            <label for=id>{label}</label>
            {children()}
            {helper_view}
        </div>
    }
}
