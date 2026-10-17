// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<LiveCapNotice/>` — what Search shows above a live aggregate whose
//! snapshot reports `capped` (ADR-0001).
//!
//! The stream's aggregation dropped input past one of its caps, so the
//! rows on screen leave something out: groups past the group cap, or
//! values past a `dc()`, `values()`, `median` or percentile cap. The
//! rows are still the live answer, so they stay, and the notice sits
//! above them rather than in their place, as `ResourceNotice` sits for a
//! query that produced nothing. It is persistent: every later snapshot of the stream carries the same
//! report, and only a new stream clears it.
//!
//! A status, not an alert: nothing failed. The `role="status"` wrapper
//! stays in the tree while the notice inside it comes and goes, because
//! a live region that mounts with its content is not reliably announced.

use leptos::prelude::*;
use trawl_api::stream::SnapshotCap;

use crate::live_cap_copy::{LIVE_CAP_ADVICE, LIVE_CAP_TITLE, cap_sentence};

/// The notice for the caps the latest snapshot reported. Nothing renders
/// while the list is empty, which is the ordinary case.
#[component]
pub fn LiveCapNotice(
    /// The latest accepted snapshot's `capped`, verbatim.
    #[prop(into)]
    capped: Signal<Vec<SnapshotCap>>,
) -> impl IntoView {
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
