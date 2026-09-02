// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<When/>` — the canonical timestamp label.
//!
//! Relative mode renders the [`time_ago`](super::time_ago)
//! buckets and re-renders off the shared 30s
//! [`clock`](super::clock) tick; absolute mode renders
//! `%Y-%m-%d %H:%M UTC` — the explicit zone marker is the point,
//! nothing else in the apps signals timezone. Both modes carry the
//! full RFC 3339 timestamp in the `title` attribute (normalized via
//! `to_rfc3339`; unparseable input passes through raw, matching
//! `time_ago`'s show-something-imperfect stance).

use leptos::prelude::*;

use super::{clock, parse_timestamp, time_ago};

/// How [`When`] renders its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhenMode {
    /// `time_ago` buckets ("just now" / "5m ago" / …), ticking off the
    /// shared clock.
    #[default]
    Relative,
    /// `%Y-%m-%d %H:%M UTC` — fixed calendar form with the zone marker.
    Absolute,
}

/// Canonical timestamp label. `ts` is an RFC 3339 (or `DuckDB`
/// space-separated) timestamp string.
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
