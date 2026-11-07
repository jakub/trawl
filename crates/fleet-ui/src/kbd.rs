// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Kbd/>` — keyboard shortcut chip.
//!
//! Two treatments, both shipped in fleet-ui.css:
//! - default `.kbd` — the bordered chip (modal footer hints, menus).
//! - `inline` `.kbd-inline` — borderless dimmed text for shortcuts
//!   embedded inside colored buttons (trawl's Haul button).

#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;

/// Whether a user agent's primary shortcut modifier is ⌘ rather than
/// Ctrl: macOS and iPad/iPhone/iPod. A desktop-mode iPad UA that reports
/// only Macintosh reads as macOS, which gives the same answer.
#[must_use]
pub fn apple_keyboard(ua: &str) -> bool {
    let ua = ua.to_ascii_lowercase();
    ["ipad", "iphone", "ipod", "macintosh", "mac os x"]
        .iter()
        .any(|name| ua.contains(name))
}

/// Keyboard shortcut label, e.g. `<Kbd>"⌘⏎"</Kbd>`.
#[cfg(target_arch = "wasm32")]
#[component]
pub fn Kbd(#[prop(default = false)] inline: bool, children: Children) -> impl IntoView {
    let class = if inline { "kbd-inline" } else { "kbd" };
    view! {
        <span class=class>{children()}</span>
    }
}

#[cfg(test)]
mod tests {
    use super::apple_keyboard;

    #[test]
    fn apple_keyboard_reads_macos_and_ios_user_agents() {
        for (ua, apple) in [
            ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)", true),
            ("Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)", true),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)",
                true,
            ),
            ("MACINTOSH", true),
            ("Mozilla/5.0 (X11; Linux x86_64)", false),
            ("Mozilla/5.0 (Windows NT 10.0; Win64; x64)", false),
            ("Mozilla/5.0 (Linux; Android 14)", false),
            ("", false),
        ] {
            assert_eq!(apple_keyboard(ua), apple, "{ua}");
        }
    }
}
