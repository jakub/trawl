// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `<Badge>` component. Wasm-only (pulls leptos) — the pure
//! [`Tone`] enum it renders lives in [`super::tone`] so its CSS-class
//! contract is native-testable.

use leptos::prelude::*;

use super::tone::{Tone, badge_class};

/// Small uppercase pill for domain-kind labels (story states, TLP
/// markings, claim relationships, run outcomes, …).
///
/// The API is deliberately tone-only: apps map their domain kinds onto
/// [`Tone`] at the call site, and there is NO color/style passthrough
/// prop (ADR-0003). A mis-fitting palette is a cheap in-place API
/// change under lockstep versioning.
#[component]
pub fn Badge(#[prop(optional)] tone: Tone, children: Children) -> impl IntoView {
    view! {
        <span class=badge_class(tone)>{children()}</span>
    }
}
