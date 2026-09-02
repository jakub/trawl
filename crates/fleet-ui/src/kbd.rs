// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Kbd/>` — keyboard shortcut chip.
//!
//! Two treatments, both shipped in fleet-ui.css:
//! - default `.kbd` — the bordered chip (modal footer hints, menus).
//! - `inline` `.kbd-inline` — borderless dimmed text for shortcuts
//!   embedded inside colored buttons (trawl's Haul button).

use leptos::prelude::*;

/// Keyboard shortcut label, e.g. `<Kbd>"⌘⏎"</Kbd>`.
#[component]
pub fn Kbd(#[prop(default = false)] inline: bool, children: Children) -> impl IntoView {
    let class = if inline { "kbd-inline" } else { "kbd" };
    view! {
        <span class=class>{children()}</span>
    }
}
