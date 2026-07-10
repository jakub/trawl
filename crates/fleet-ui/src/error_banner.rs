// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ErrorBanner/>` — `role="alert"` message strip.
//!
//! Both apps hand-rolled page-namespaced `<div class="error">` strips;
//! this is that strip with the alert role screen readers need. Renders
//! nothing while the signal is `None`, so the conditional-render idiom
//! (`{move || error.get().map(|msg| …)}`) collapses into a single
//! component invocation. The `.error` class already ships in
//! fleet-ui.css — adopting this component changes exactly one thing in
//! the DOM: the `role="alert"` attribute.

use leptos::prelude::*;

#[component]
pub fn ErrorBanner(#[prop(into)] error: Signal<Option<String>>) -> impl IntoView {
    move || {
        error
            .get()
            .map(|msg| view! { <div class="error" role="alert">{msg}</div> })
    }
}
