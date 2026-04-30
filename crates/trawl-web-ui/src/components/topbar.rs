// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<TopBar/>` — application chrome.
//!
//! Brand · mode tabs (Search/Intel/Jobs/Settings) · spacer · command
//! palette stub · session pill · history/notif/settings icons · user
//! avatar dropdown (Profile / API tokens / theme switch / Sign out).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::state::app_mode::AppMode;
use crate::state::theme::UiPrefs;

#[component]
pub fn TopBar(
    /// Currently active mode — drives the tab-active styling.
    #[prop(into)]
    mode: Signal<AppMode>,
    /// User identity for the avatar + dropdown header. Optional so the
    /// bar can render before `/me` resolves.
    #[prop(into, optional)]
    me: Signal<Option<api::MeResponse>>,
) -> impl IntoView {
    // use_navigate() must be captured during component setup — calling
    // it inside a click handler would panic (no <Router> context at
    // dispatch time).
    let nav = use_navigate();
    let prefs = use_context::<UiPrefs>();

    let go = {
        let nav = nav.clone();
        move |target: AppMode| {
            nav(target.default_path(), NavigateOptions::default());
        }
    };

    let menu_open = RwSignal::new(false);

    let on_logout = move |_| {
        menu_open.set(false);
        spawn_local(async move {
            let _ = api::logout().await;
            if let Some(win) = web_sys::window() {
                let _ = win.location().set_href("/login");
            }
        });
    };

    let toggle_theme = move |_| {
        if let Some(p) = prefs {
            p.theme.update(|t| *t = t.toggled());
        }
    };

    view! {
        <div class="topbar">
            <div class="brand">
                <span>"trawl"</span><span class="amber">"_"</span>
            </div>

            <div class="modes">
                {AppMode::ALL.iter().copied().map(|m| {
                    let go = go.clone();
                    view! {
                        <div
                            class="mode"
                            class:active=move || mode.get() == m
                            on:click=move |_| go(m)
                        >
                            <span class="dot"></span>
                            <span>{m.label()}</span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>

            <div class="sp"></div>

            <div class="jump" title="Command palette — coming soon">
                <IconSearch/>
                <span class="gh">"Jump to query, source, dashboard…"</span>
                <span class="kbd">"⌘K"</span>
            </div>

            <div class="env" title="session">
                <span class="pulse"></span>
                <span>"session"</span>
            </div>

            <div class="iconbtn" title="Notifications — coming soon">
                <IconBell/>
            </div>

            <div class="user-wrap">
                <div class="user" on:click=move |_| menu_open.update(|v| *v = !*v)>
                    <div class="avatar">{move || avatar_initials(me.get().as_ref())}</div>
                    <span class="who">{move || me.get().map_or_else(|| "…".to_string(), |m| m.name)}</span>
                    <IconChevron/>
                </div>
                <Show when=move || menu_open.get()>
                    <div class="overlay" on:click=move |_| menu_open.set(false)></div>
                    <div class="user-menu">
                        <div class="hdr">
                            <div class="name">{move || me.get().map(|m| m.name).unwrap_or_default()}</div>
                            <div class="mail">{move || me.get().map(|m| m.role).unwrap_or_default()}</div>
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
                        <div class="item danger" on:click=on_logout>
                            <span>"Sign Out"</span>
                        </div>
                    </div>
                </Show>
            </div>
        </div>
    }
}

fn avatar_initials(me: Option<&api::MeResponse>) -> String {
    me.map_or_else(
        || "··".into(),
        |m| {
            let s = m.name.trim();
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
    use crate::state::theme::Theme;
    match prefs.map(|p| p.theme.get()) {
        Some(Theme::Dark) => "Switch to light theme".into(),
        Some(Theme::Light) | None => "Switch to dark theme".into(),
    }
}

// ─── inline icons (12–14px, monoline, currentColor) ──────────────────

#[component]
fn IconSearch() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <circle cx="7" cy="7" r="4.5"/>
            <path d="m10.5 10.5 3 3"/>
        </svg>
    }
}

#[component]
fn IconChevron() -> impl IntoView {
    view! {
        <svg width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="m4 6 4 4 4-4"/>
        </svg>
    }
}

#[component]
fn IconBell() -> impl IntoView {
    view! {
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="M3.5 12V7a4.5 4.5 0 1 1 9 0v5l1 1.5h-11l1-1.5z"/>
            <path d="M7 14a1 1 0 0 0 2 0"/>
        </svg>
    }
}
