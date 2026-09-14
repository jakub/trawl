// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ErrorBanner/>` — `role="alert"` message strip.
//!
//! `role="alert"` is the point: a screen reader announces the message
//! when it appears, which a plain `<div class="error">` does not.
//!
//! Renders nothing while the signal is `None`, so the
//! conditional-render idiom (`{move || error.get().map(|msg| …)}`)
//! collapses into a single component invocation. The class is
//! `.error-banner`, not bare `.error`, because a bare `.error` selector
//! leaks padding/border onto every element that uses `error` as a state
//! token (`.status-dot.error`, `.load-hint.error`, …).
//!
//! `id` exists so a control can point `aria-describedby` at the banner:
//! `role="alert"` announces the message once, the association is what
//! lets a screen-reader user read it again from the invalid field.

use leptos::prelude::*;

#[component]
pub fn ErrorBanner(
    #[prop(into)] error: Signal<Option<String>>,
    /// Optional stable target for a field's `aria-describedby`. The
    /// caller must only point at it while the error is `Some`, since
    /// nothing renders otherwise.
    #[prop(optional, into)]
    id: Option<String>,
) -> impl IntoView {
    move || {
        error
            .get()
            .map(|msg| view! { <div id=id.clone() class="error-banner" role="alert">{msg}</div> })
    }
}
