// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<MetaStrip/>` — count · duration · scanned · chips · save/export.

use leptos::prelude::*;

use crate::components::toast::{ToastBus, ToastKind};

#[component]
pub fn MetaStrip(
    /// Row count for the current page; `None` while loading.
    #[prop(into)]
    count: Signal<Option<usize>>,
    /// Whether the result has been truncated server-side.
    #[prop(into)]
    truncated: Signal<bool>,
    bus: ToastBus,
) -> impl IntoView {
    view! {
        <div class="meta">
            <span>
                <span class="num">
                    {move || count.get().map_or_else(|| "—".to_string(), |c| c.to_string())}
                </span>
                " events"
            </span>
            <span class="divider">"·"</span>
            <span>
                {move || if truncated.get() {
                    "truncated server-side".to_string()
                } else {
                    "scanned ".to_string() + &dash() + " / " + &dash()
                }}
            </span>
            <span class="sp"></span>
            <span
                class="action"
                on:click=move |_| bus.push(
                    ToastKind::Info,
                    "Save",
                    Some("Saving nets is coming soon.".into()),
                )
            >"save"</span>
            <span
                class="action"
                on:click=move |_| bus.push(
                    ToastKind::Info,
                    "Export",
                    Some("CSV/JSON export is coming soon.".into()),
                )
            >"export"</span>
        </div>
    }
}

fn dash() -> String {
    "—".into()
}
