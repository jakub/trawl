// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<TopBar/>` — generic application chrome.
//!
//! brand · mode tabs · spacer · command-palette stub · app links ·
//! account menu. The theme toggle reads the `UiPrefs` context the
//! consumer provides from `fleet_ui::install()`. The bar knows nothing
//! about auth, `/me`, or app-specific endpoints: `on_logout` is a
//! callback the consumer wires to its own logout flow.
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
//! Three affordances left with ADR-0025 and ADR-0028: the notifications
//! bell (never wired), the disabled Profile and API tokens rows, and
//! the theme item's `⌘⇧L` hint chip (the chord collides with
//! Bitwarden's autofill and Safari's own binding, so it is not bound
//! and the hint would be a lie). The `⌘K` command-palette stub stays as
//! it is until the palette slice.

use leptos::html::{Button, Div};
use leptos::prelude::*;
use leptos_router::components::A;

use crate::icon::{Icon, IconView};
use crate::menu::{MenuEntry, MenuItem, MenuPanel};
use crate::theme::UiPrefs;

/// A top-bar mode tab. Active state is baked into the struct so the
/// caller can rebuild the Vec reactively from its routing state
/// without any wiring inside fleet-ui.
#[derive(Debug, Clone)]
pub struct ModeTab {
    pub id: String,
    pub label: String,
    pub path: String,
    pub active: bool,
}

/// A cross-app navigation link rendered in the topbar's app-switcher
/// area. Coastwatch and trawl each ship a small slice so users can hop
/// between fleet apps without re-authenticating.
#[derive(Debug, Clone)]
pub struct AppLink {
    pub label: String,
    pub href: String,
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
    #[prop(into)] brand: String,
    #[prop(into)] brand_accent: String,
    #[prop(into)] modes: Signal<Vec<ModeTab>>,
    #[prop(into, optional)] app_links: Signal<Vec<AppLink>>,
    #[prop(into)] user: Signal<Option<UserInfo>>,
    on_logout: Callback<()>,
) -> impl IntoView {
    let prefs = use_context::<UiPrefs>();

    view! {
        <div class="topbar">
            <div class="brand">
                <span>{brand}</span><span class="accent">{brand_accent}</span>
            </div>

            <div class="modes">
                {move || modes.get().into_iter().map(|tab| {
                    // Outer move || rebuilds the whole tab list whenever `modes`
                    // changes, so `tab.active` is fresh per render. The inner
                    // closure pattern used by Rail (where `active` is a real
                    // Signal) would silently break here because `tab.active` is
                    // a plain bool captured by value — there's nothing for a
                    // reactive re-run to re-read. Keep the class static.
                    let class = tab_class(tab.active);
                    view! {
                        <A href=tab.path attr:class=class>
                            <span>{tab.label}</span>
                        </A>
                    }
                }).collect::<Vec<_>>()}
            </div>

            <div class="sp"></div>

            <div class="jump" title="Command palette — coming soon">
                <IconView icon=Icon::Search size=12 stroke_width=1.5/>
                <span class="gh">"Search…"</span>
                <span class="kbd">"⌘K"</span>
            </div>

            {move || {
                let links = app_links.get();
                (!links.is_empty()).then(|| view! {
                    <div class="app-links">
                        {links.into_iter().map(|link| {
                            let class = if link.active { "app-link active" } else { "app-link" };
                            view! {
                                <a href=link.href class=class>{link.label}</a>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                })
            }}

            {account_menu(user, prefs, on_logout)}
        </div>
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

    let toggle_theme = Callback::new(move |()| {
        if let Some(p) = prefs {
            p.theme.update(|t| *t = t.toggled());
        } else {
            // Developer-facing: consumer mounted <TopBar/> without calling
            // `fleet_ui::install()`, so the theme toggle silently does
            // nothing. Surface it so it's caught in dev, not QA.
            leptos::logging::warn!(
                "fleet-ui TopBar: UiPrefs context missing — did you call fleet_ui::install()?"
            );
        }
    });

    // The theme item renames itself with the theme it would switch to,
    // so its label is a derived signal rather than a snapshot string.
    let entries = move || {
        vec![
            MenuEntry::Item(MenuItem {
                label: Signal::derive(move || theme_label(prefs)),
                danger: false,
                on_activate: toggle_theme,
            }),
            MenuEntry::Separator,
            MenuEntry::Item(MenuItem {
                label: Signal::stored("Sign Out".to_string()),
                danger: true,
                on_activate: on_logout,
            }),
        ]
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
        Some(Theme::Light) => "Switch to dark theme".into(),
        None => {
            // Same root cause as the toggle_theme warn above: consumer
            // forgot `fleet_ui::install()`. The label is meaningless without
            // prefs, but we still render something so the UI doesn't break.
            leptos::logging::warn!(
                "fleet-ui TopBar: UiPrefs context missing — did you call fleet_ui::install()?"
            );
            "Theme (unavailable)".into()
        }
    }
}
