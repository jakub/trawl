// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use super::{clock, parse_timestamp, time_ago};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhenMode {
    #[default]
    Relative,
    Absolute,
}

/// `ts` is an RFC 3339 (or `DuckDB` space-separated) timestamp string.
#[component]
pub fn When(#[prop(into)] ts: Signal<String>, #[prop(optional)] mode: WhenMode) -> impl IntoView {
    let title = move || {
        let raw = ts.get();
        parse_timestamp(&raw).map_or(raw, |dt| dt.to_rfc3339())
    };
    let label = move || {
        let raw = ts.get();
        match mode {
            WhenMode::Relative => time_ago(&raw, clock::now_ms().get()),
            WhenMode::Absolute => {
                parse_timestamp(&raw).map_or(raw, |dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            }
        }
    };

    view! {
        <span class="when" title=title>{label}</span>
    }
}
