// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sortable table header cell, shared by the page-level `.tbl` tables
//! (schema services, nets). Clicking the active header flips direction;
//! a fresh key starts at its natural direction.

use leptos::prelude::*;

/// One sortable header cell. Not a `#[component]` — the flex sizing
/// `style` is per-column and threading it through props buys nothing.
/// `default_desc` is the direction a freshly-selected key starts in.
pub fn sort_th<K>(
    sort: RwSignal<(K, bool)>,
    key: K,
    default_desc: bool,
    label: &'static str,
    style: &'static str,
) -> impl IntoView
where
    K: Copy + PartialEq + Send + Sync + 'static,
{
    let arrow = move || {
        let (k, desc) = sort.get();
        (k == key).then_some(if desc { "↓" } else { "↑" })
    };
    view! {
        <div
            class="th sortable"
            class:active=move || sort.get().0 == key
            style=style
            on:click=move |_| {
                sort.update(|s| {
                    if s.0 == key {
                        s.1 = !s.1;
                    } else {
                        *s = (key, default_desc);
                    }
                });
            }
        >
            {label}
            <span class="dir">{arrow}</span>
        </div>
    }
}
