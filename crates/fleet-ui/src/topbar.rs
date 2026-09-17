// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<TopBar/>` — the application command bar.
//!
//! nav toggle · page title · spacer · command palette · account menu.
//! Navigation itself belongs to [`crate::sidebar`] (ADR-0032); the bar
//! names the current page and carries the two chrome affordances that
//! are not destinations. The theme choices read the `UiPrefs` context
//! the consumer provides from `fleet_ui::install()`. The bar knows
//! nothing about auth, `/me`, or app-specific endpoints: `on_logout` is
//! a callback the consumer wires to its own logout flow.
//!
//! The nav toggle is the compact-width opener for the sidebar overlay.
//! It is always rendered (CSS shows it below 900px) and always reports
//! whether the overlay is open, so its state never disagrees with the
//! shell's.
//!
//! The account menu is a native trigger plus the shared menu panel
//! ([`crate::menu`]), the same contract `ActionsMenu` mounts: the panel
//! is an overlay layer, so Escape closes it only while it is topmost, a
//! modal opened from it arbitrates properly, exactly one item is
//! tabbable, and focus returns to the trigger on every cause but an
//! outside press. Its trigger's accessible name is the visible user
//! name — the avatar and the chevron are `aria-hidden`, so nothing
//! reads "JD" aloud — and it is `disabled` until the consumer's `user`
//! signal resolves. One predicate decides both whether the panel is on
//! the page and what `aria-expanded` reports, and losing the identity
//! closes the menu rather than leaving it open behind an unmounted
//! panel.
//!
//! ADR-0025 and ADR-0028 retired the notifications bell, disabled Profile
//! and API tokens rows, and theme shortcut hint. No theme shortcut is bound.
//! The Theme group offers explicit Light, Dark and System preferences, with
//! initial focus on the checked choice. Shell owns the command palette and
//! supplies its trigger callback, availability and open state here.

use leptos::html::{Button, Div};
use leptos::prelude::*;

use crate::command_palette::platform_kbd_hint;
use crate::icon::{Icon, IconView};
use crate::kbd::Kbd;
use crate::menu::{MenuEntry, MenuItem, MenuPanel, MenuRadioItem};
use crate::theme::{ThemePreference, UiPrefs};

/// User identity for the avatar + dropdown header. `detail` is the
/// app-supplied secondary line (trawl uses role, coastwatch may use
/// email — fleet-ui doesn't care).
#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    pub name: String,
    pub detail: String,
}

#[component]
pub fn TopBar(
    /// The active destination's label, rendered as the page crumb.
    #[prop(into)]
    page_title: Signal<String>,
    /// Whether the compact sidebar overlay is open, reported by the toggle.
    #[prop(into)]
    nav_open: Signal<bool>,
    on_toggle_nav: Callback<()>,
    /// Focus returns here when the sidebar overlay closes.
    nav_toggle: NodeRef<Button>,
    #[prop(into)] user: Signal<Option<UserInfo>>,
    on_logout: Callback<()>,
    /// Shell owns the dialog. Required props prevent a decorative dead trigger.
    on_open_palette: Callback<()>,
    #[prop(into)] palette_open: Signal<bool>,
    #[prop(into)] palette_available: Signal<bool>,
    palette_trigger: NodeRef<Button>,
) -> impl IntoView {
    let hint = platform_kbd_hint(&window().navigator().user_agent().unwrap_or_default());
    let prefs = use_context::<UiPrefs>();

    view! {
        <header class="topbar">
            <button
                type="button"
                class="nav-toggle"
                aria-label="Open navigation"
                aria-controls="fleet-sidebar"
                aria-expanded=move || nav_open.get().to_string()
                node_ref=nav_toggle
                on:click=move |_| on_toggle_nav.run(())
            >
                <IconView icon=Icon::Menu size=16 stroke_width=1.5/>
            </button>

            <h2 class="crumb" aria-live="polite">{move || page_title.get()}</h2>

            <div class="sp"></div>

            <Show when=move || palette_available.get()>
                <button
                    type="button"
                    class="jump"
                    title="Command palette"
                    aria-label="Go to… Command palette"
                    aria-haspopup="dialog"
                    aria-expanded=move || palette_open.get().to_string()
                    aria-keyshortcuts=hint.aria_keyshortcuts
                    node_ref=palette_trigger
                    on:click=move |_| on_open_palette.run(())
                >
                    <IconView icon=Icon::Search size=12 stroke_width=1.5/>
                    <span class="gh">"Go to…"</span>
                    <Kbd>{hint.label}</Kbd>
                </button>
            </Show>

            {account_menu(user, prefs, on_logout)}
        </header>
    }
}

/// The account menu: native trigger plus the shared menu panel.
///
/// Split out of [`TopBar`] as a unit because it is the one part of the
/// bar with state of its own — the open flag, the wrapper the outside
/// press is measured against, and the trigger focus returns to.
fn account_menu(
    user: Signal<Option<UserInfo>>,
    prefs: Option<UiPrefs>,
    on_logout: Callback<()>,
) -> impl IntoView {
    let menu_open = RwSignal::new(false);
    let wrap_ref = NodeRef::<Div>::new();
    let trigger_ref = NodeRef::<Button>::new();

    // The one predicate: the panel mounts under it and the trigger
    // reports it. Deriving `aria-expanded` from `menu_open` alone let
    // the two disagree, because the panel also needs an identity to
    // render a header for: losing the user while the menu was open
    // unmounted the panel (disposing its layer and its focused item)
    // and left a disabled trigger claiming aria-expanded="true".
    let panel_open = Signal::derive(move || menu_open.get() && user.get().is_some());

    // Losing the identity closes the menu for good. Without this the
    // stale open flag survives the unmount, so the next identity
    // (a re-login, a `/me` refetch) remounts the panel and runs its
    // initial-focus effect with no user activation behind it.
    Effect::new(move |_| {
        if user.get().is_none() {
            menu_open.set(false);
        }
    });

    if prefs.is_none() {
        leptos::logging::warn!(
            "fleet-ui TopBar: UiPrefs context missing — did you call fleet_ui::install()?"
        );
    }

    // Checked state names the stored preference, even when System resolves to
    // the same appearance as a fixed choice. Opening does not change it.
    let entries = move || {
        let mut entries = Vec::new();
        if let Some(prefs) = prefs {
            let items = [
                ("Light", ThemePreference::Light),
                ("Dark", ThemePreference::Dark),
                ("System", ThemePreference::System),
            ]
            .into_iter()
            .map(|(label, preference)| MenuRadioItem {
                label,
                checked: Signal::derive(move || prefs.theme_preference().get() == preference),
                on_activate: Callback::new(move |()| prefs.select_theme(preference)),
            })
            .collect();
            entries.push(MenuEntry::RadioGroup {
                label: "Theme",
                items,
            });
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Item(MenuItem {
            label: Signal::stored("Sign Out".to_string()),
            danger: true,
            on_activate: on_logout,
        }));
        entries
    };

    view! {
        <div class="user-wrap" node_ref=wrap_ref>
            <button
                class="user"
                type="button"
                node_ref=trigger_ref
                aria-haspopup="menu"
                // Rendered unconditionally, including while disabled,
                // where it reads false: a trigger that only sometimes
                // reports its state is worse than one that always does.
                aria-expanded=move || panel_open.get().to_string()
                disabled=move || user.get().is_none()
                on:click=move |_| menu_open.update(|v| *v = !*v)
            >
                <span class="avatar" aria-hidden="true">
                    {move || avatar_initials(user.get().as_ref())}
                </span>
                <span class="who">
                    {move || user.get().map_or_else(|| "…".to_string(), |u| u.name)}
                </span>
                <IconView icon=Icon::Chevron size=10 stroke_width=1.5 attr:aria-hidden="true"/>
            </button>
            <Show when=move || panel_open.get()>
                <MenuPanel
                    panel_class="user-menu"
                    menu_label="Account"
                    entries=entries()
                    header=Box::new(move || view! {
                        <div class="hdr">
                            <div class="name">
                                {move || user.get().map(|u| u.name).unwrap_or_default()}
                            </div>
                            <div class="mail">
                                {move || user.get().map(|u| u.detail).unwrap_or_default()}
                            </div>
                        </div>
                    }.into_any())
                    open=menu_open
                    wrap_ref=wrap_ref
                    trigger_ref=trigger_ref
                />
            </Show>
        </div>
    }
}

fn avatar_initials(user: Option<&UserInfo>) -> String {
    user.map_or_else(
        || "··".into(),
        |u| {
            let s = u.name.trim();
            if s.is_empty() {
                return "··".into();
            }
            let mut chars = s.chars().filter(|c| c.is_alphanumeric());
            let first = chars.next().unwrap_or('·');
            let second = chars.next().unwrap_or(first);
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                second.to_ascii_uppercase()
            )
        },
    )
}
