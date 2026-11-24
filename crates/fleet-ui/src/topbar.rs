// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::html::{Button, Div};
use leptos::prelude::*;

use crate::command_palette::platform_kbd_hint;
use crate::icon::{Icon, IconView};
use crate::kbd::Kbd;
use crate::menu::{MenuEntry, MenuItem, MenuPanel, MenuRadioItem};
use crate::theme::{ThemePreference, UiPrefs};

#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    pub name: String,
    pub detail: String,
}

#[component]
pub fn TopBar(
    #[prop(into)] page_title: Signal<String>,
    #[prop(into)] nav_open: Signal<bool>,
    on_toggle_nav: Callback<()>,
    /// Focus returns here when the sidebar overlay closes.
    nav_toggle: NodeRef<Button>,
    #[prop(into)] user: Signal<Option<UserInfo>>,
    on_logout: Callback<()>,
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

fn account_menu(
    user: Signal<Option<UserInfo>>,
    prefs: Option<UiPrefs>,
    on_logout: Callback<()>,
) -> impl IntoView {
    let menu_open = RwSignal::new(false);
    let wrap_ref = NodeRef::<Div>::new();
    let trigger_ref = NodeRef::<Button>::new();

    // The one predicate: the panel mounts under it and the trigger
    // reports it, because the panel also needs an identity to render a header for.
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
