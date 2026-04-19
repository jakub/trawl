// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<LiveBadge/>` — streaming status pill.
//!
//! Shows `● LIVE` while streaming; flips to `⚠ LAGGED: N missed` for
//! ~3s on every back-pressure event, then returns to LIVE. The lagged
//! clear is scheduled by `state::stream_session` — this component just
//! reads the signals.

// Module-level allow: the design replaces the standalone topbar badge
// with a HAULING indicator inside the status bar (commit 4). Until
// then this component sits unused — keeping the implementation around
// rather than deleting and rewriting from scratch.
#![allow(dead_code)]

use leptos::prelude::*;

#[component]
pub fn LiveBadge(
    #[prop(into)] active: Signal<bool>,
    #[prop(into)] lagged: Signal<Option<u64>>,
) -> impl IntoView {
    view! {
        {move || if !active.get() {
            ().into_any()
        } else if let Some(n) = lagged.get() {
            view! {
                <span class="live-badge lagged">
                    {format!("⚠ LAGGED: {n} missed")}
                </span>
            }.into_any()
        } else {
            view! {
                <span class="live-badge live">"● LIVE"</span>
            }.into_any()
        }}
    }
}
