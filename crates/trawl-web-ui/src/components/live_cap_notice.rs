// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<LiveCapNotice/>` — what Search shows above a live aggregate whose
//! snapshot reports `capped` (ADR-0001).
//!
//! A status, not an alert: nothing failed. The `role="status"` wrapper
//! stays in the tree while the notice inside it comes and goes, because
//! a live region that mounts with its content is not reliably announced.

use leptos::prelude::*;
use trawl_api::stream::SnapshotCap;

use crate::live_cap_copy::{LIVE_CAP_ADVICE, LIVE_CAP_TITLE, cap_sentence};

#[component]
pub fn LiveCapNotice(#[prop(into)] capped: Signal<Vec<SnapshotCap>>) -> impl IntoView {
    view! {
        <div class="live-cap-live" role="status">
            <Show when=move || capped.with(|c| !c.is_empty())>
                <div class="live-cap-notice">
                    <p class="live-cap-notice-title">{LIVE_CAP_TITLE}</p>
                    <p class="live-cap-notice-text">
                        {move || {
                            capped.with(|caps| {
                                caps.iter()
                                    .map(cap_sentence)
                                    .chain(std::iter::once(LIVE_CAP_ADVICE.to_owned()))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                        }}
                    </p>
                </div>
            </Show>
        </div>
    }
}
