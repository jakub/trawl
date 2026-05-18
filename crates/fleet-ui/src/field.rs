// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Field/>` — labelled form wrapper around an `<input>`/`<select>`/
//! `<textarea>` slot.
//!
//! Owns the `.field` wrapper, the `<label>` text, the optional hint
//! paragraph, and the optional error display. The actual control is
//! passed via `children` so the caller controls `id`, `type`,
//! `value`, event handlers, and any control-specific attributes.

use leptos::prelude::*;

/// Form-field wrapper. `children` is the underlying input/select/
/// textarea.
///
/// `aria-describedby` wiring isn't generated here in step 2 — the
/// consumers that will adopt `<Field>` (the login form, modal-internal
/// inputs) live in trawl-web-ui and migrate in ADR-0030 step 4. When
/// they do, the right call is to add an `id: &'static str` prop and
/// have callers thread it onto their inner `<input>`.
#[component]
pub fn Field(
    label: &'static str,
    #[prop(optional)] hint: Option<&'static str>,
    #[prop(optional)] error: Option<String>,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="field">
            <label>{label}</label>
            {children()}
            {hint.map(|h| view! { <p class="field-hint">{h}</p> })}
            {error.map(|e| view! { <p class="error">{e}</p> })}
        </div>
    }
}
