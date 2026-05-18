// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<TopBar/>` — generic application chrome.
//!
//! brand · mode tabs · spacer · command-palette stub · session pill ·
//! notif iconbtn · user-menu dropdown. Theme toggle reads from the
//! `UiPrefs` context provided by `fleet_ui::install()`. The bar knows
//! nothing about auth, /me, or app-specific endpoints — `on_logout`
//! is a callback the consumer wires to its own logout flow.

use leptos::prelude::*;
use leptos_router::components::A;

use crate::icon::{Icon, IconView};
use crate::theme::UiPrefs;

/// A top-bar mode tab. Active state is baked into the struct so the
/// caller can rebuild the Vec reactively from its routing state
/// without any wiring inside fleet-ui.
#[derive(Debug, Clone)]
pub struct ModeTab {
    pub id: &'static str,
    pub label: &'static str,
    pub path: &'static str,
    pub active: bool,
}

/// A cross-app navigation link rendered in the topbar's app-switcher
/// area. Coastwatch and trawl will each ship a small static slice so
/// users can hop between fleet apps without re-authenticating.
#[derive(Debug, Clone, Copy)]
pub struct AppLink {
    pub label: &'static str,
    pub href: &'static str,
    pub active: bool,
}

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
    brand: &'static str,
    brand_accent: &'static str,
    #[prop(into)] modes: Signal<Vec<ModeTab>>,
    #[prop(optional)] app_links: &'static [AppLink],
    #[prop(into)] user: Signal<Option<UserInfo>>,
    on_logout: Callback<()>,
) -> impl IntoView {
    let prefs = use_context::<UiPrefs>();
    let menu_open = RwSignal::new(false);

    let toggle_theme = move |_| {
        if let Some(p) = prefs {
            p.theme.update(|t| *t = t.toggled());
        }
    };

    let on_logout_click = move |_| {
        menu_open.set(false);
        on_logout.run(());
    };

    view! {
        <div class="topbar">
            <div class="brand">
                <span>{brand}</span><span class="amber">{brand_accent}</span>
            </div>

            <div class="modes">
                {move || modes.get().into_iter().map(|tab| {
                    view! {
                        <A
                            href=tab.path
                            attr:class=move || tab_class(tab.active)
                        >
                            <span class="dot"></span>
                            <span>{tab.label}</span>
                        </A>
                    }
                }).collect::<Vec<_>>()}
            </div>

            <div class="sp"></div>

            <div class="jump" title="Command palette — coming soon">
                <IconView icon=Icon::Search size=12 stroke_width=1.5/>
                <span class="gh">"Jump to query, source, dashboard…"</span>
                <span class="kbd">"⌘K"</span>
            </div>

            <div class="env" title="session">
                <span class="pulse"></span>
                <span>"session"</span>
            </div>

            <div class="iconbtn" title="Notifications — coming soon">
                <IconView icon=Icon::Bell size=14 stroke_width=1.5/>
            </div>

            {(!app_links.is_empty()).then(|| view! {
                <div class="app-links">
                    {app_links.iter().copied().map(|link| view! {
                        <a
                            href=link.href
                            class=if link.active { "app-link active" } else { "app-link" }
                        >{link.label}</a>
                    }).collect::<Vec<_>>()}
                </div>
            })}

            <div class="user-wrap">
                <div class="user" on:click=move |_| menu_open.update(|v| *v = !*v)>
                    <div class="avatar">{move || avatar_initials(user.get().as_ref())}</div>
                    <span class="who">{move || user.get().map_or_else(|| "…".to_string(), |u| u.name)}</span>
                    <IconView icon=Icon::Chevron size=10 stroke_width=1.5/>
                </div>
                <Show when=move || menu_open.get()>
                    <div class="overlay" on:click=move |_| menu_open.set(false)></div>
                    <div class="user-menu">
                        <div class="hdr">
                            <div class="name">{move || user.get().map(|u| u.name).unwrap_or_default()}</div>
                            <div class="mail">{move || user.get().map(|u| u.detail).unwrap_or_default()}</div>
                        </div>
                        <div class="item disabled" title="coming soon">
                            <span>"Profile"</span>
                        </div>
                        <div class="item disabled" title="coming soon">
                            <span>"API tokens"</span>
                        </div>
                        <div class="item" on:click=toggle_theme>
                            <span>{move || theme_label(prefs)}</span>
                            <span class="kbd">"⌘⇧L"</span>
                        </div>
                        <div class="sep"></div>
                        <div class="item danger" on:click=on_logout_click>
                            <span>"Sign Out"</span>
                        </div>
                    </div>
                </Show>
            </div>
        </div>
    }
}

fn tab_class(active: bool) -> &'static str {
    if active { "mode active" } else { "mode" }
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

fn theme_label(prefs: Option<UiPrefs>) -> String {
    use crate::theme::Theme;
    match prefs.map(|p| p.theme.get()) {
        Some(Theme::Dark) => "Switch to light theme".into(),
        Some(Theme::Light) | None => "Switch to dark theme".into(),
    }
}
