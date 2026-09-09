// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Shell/>` — chrome-only application layout: topbar + body(rail +
//! main(children)) + footer + toast host.
//!
//! Owns the [`ToastBus`] via `provide_context` and the route command palette.
//! Does NOT do auth, fetch `/me`, render a status bar, or know what
//! routes exist beyond its mode tabs and rail items. Consumers wrap `Shell`
//! with whatever app-specific concerns they need — trawl's `AuthShell` gates on
//! `/me`, owns its `StatusBar`, passes that as the `footer` prop, and
//! renders its router `<Outlet/>` as `children`.
//!
//! # `ToastBus` contract
//!
//! Shell is the single owner of the toast stack: one bus, one
//! `<Toasts/>` host, provided via context. `children` and `footer`
//! closures execute inside Shell's body, so `<Outlet/>` page content
//! reaches the bus with `expect_context::<ToastBus>()`. Consumers must
//! not create their own bus or mount their own `<Toasts/>` inside a
//! Shell — see [`crate::toast`] for the full contract.

use leptos::html::Button;
use leptos::prelude::*;
use leptos_use::{UseEventListenerOptions, use_event_listener_with_options, use_window};

use crate::command_palette::{
    ChordFacts, CommandInput, CommandPalette, commands_from, editable_target, is_palette_chord,
    palette_available, platform_kbd_hint,
};
use crate::overlay::{OverlayLayer, has_layers};

use crate::rail::{Rail, RailItem};
use crate::toast::{ToastBus, Toasts};
use crate::topbar::{AppLink, ModeTab, TopBar, UserInfo};

#[component]
pub fn Shell(
    #[prop(into)] brand: String,
    #[prop(into)] brand_accent: String,
    #[prop(into)] rail_items: Signal<Vec<RailItem>>,
    #[prop(into)] rail_active: Signal<String>,
    #[prop(into)] modes: Signal<Vec<ModeTab>>,
    #[prop(into)] user: Signal<Option<UserInfo>>,
    #[prop(into, optional)] app_links: Signal<Vec<AppLink>>,
    on_logout: Callback<()>,
    /// App footer (status bar). Optional — footer-less apps omit it and
    /// the shell grid's `auto` row collapses to zero height.
    #[prop(optional)]
    footer: Option<Children>,
    /// Bottom-pinned rail slot, passed through to [`Rail`]'s `bottom`
    /// prop (rendered inside `<div class="bot">`).
    #[prop(optional)]
    rail_bottom: Option<Children>,
    children: Children,
) -> impl IntoView {
    let bus = ToastBus::new();
    provide_context(bus);

    let commands = Memo::new(move |_| {
        let modes = modes.get();
        let rail = rail_items.get();
        commands_from(
            modes.iter().map(|mode| CommandInput {
                label: &mode.label,
                path: &mode.path,
            }),
            rail.iter().map(|item| CommandInput {
                label: &item.label,
                path: &item.path,
            }),
        )
    });
    let available = Signal::derive(move || commands.with(|items| palette_available(items)));
    let palette_open = RwSignal::new(false);
    let palette_mounted = RwSignal::new(false);
    let palette_trigger = NodeRef::<Button>::new();
    let palette_layer = StoredValue::<Option<OverlayLayer>>::new(None);
    let hint = platform_kbd_hint(&window().navigator().user_agent().unwrap_or_default());

    let close_palette = Callback::new(move |()| {
        palette_open.set(false);
        // Keep the real anchor mounted through event dispatch. The router's
        // window click listener still needs its href and composed path.
        // Closing state changes before navigation, disposal follows dispatch.
        queue_microtask(move || {
            if palette_open.try_get_untracked() == Some(false) {
                palette_mounted.try_set(false);
            }
        });
    });
    let open_palette = Callback::new(move |()| {
        if !available.get_untracked() || has_layers() {
            return;
        }
        // A shortcut from the editor must restore to the visible trigger too.
        // Capture it synchronously before the overlay records its opener.
        if let Some(trigger) = palette_trigger.get_untracked() {
            let _ = trigger.focus();
        }
        palette_open.set(true);
        palette_mounted.set(true);
    });

    Effect::new(move |_| {
        if !available.get() {
            palette_open.set(false);
            palette_mounted.set(false);
        }
    });

    install_palette_chord(
        available,
        palette_open,
        palette_layer,
        hint.is_macos,
        open_palette,
        close_palette,
    );

    view! {
        <div class="shell">
            <TopBar
                brand=brand
                brand_accent=brand_accent
                modes=modes
                app_links=app_links
                user=user
                on_logout=on_logout
                on_open_palette=open_palette
                palette_open=palette_open
                palette_available=available
                palette_trigger=palette_trigger
            />
            <div class="body">
                <Rail items=rail_items active=rail_active bottom=rail_bottom/>
                <main class="main">
                    {children()}
                </main>
            </div>
            {footer.map(|f| f())}
            <Toasts bus=bus/>
            <Show when=move || palette_mounted.get() && available.get()>
                <CommandPalette
                    commands=commands
                    open=palette_open
                    on_close=close_palette
                    layer_slot=palette_layer
                />
            </Show>
        </div>
    }
}

fn install_palette_chord(
    available: Signal<bool>,
    palette_open: RwSignal<bool>,
    palette_layer: StoredValue<Option<OverlayLayer>>,
    is_macos: bool,
    open_palette: Callback<()>,
    close_palette: Callback<()>,
) {
    // Bubble phase lets editors consume a key first. The hook removes this
    // listener when Shell unmounts. Prevent default only after all gates pass.
    let _ = use_event_listener_with_options(
        use_window(),
        leptos::ev::keydown,
        move |event: web_sys::KeyboardEvent| {
            let key = event.key();
            let facts = ChordFacts {
                key: &key,
                ctrl: event.ctrl_key(),
                meta: event.meta_key(),
                alt: event.alt_key(),
                shift: event.shift_key(),
                repeat: event.repeat(),
                default_prevented: event.default_prevented(),
                composing: event.is_composing(),
                editable: editable_target(&event),
            };
            if !available.get_untracked() || !is_palette_chord(facts, is_macos) {
                return;
            }
            if palette_open.get_untracked() {
                if palette_layer
                    .get_value()
                    .is_some_and(OverlayLayer::is_topmost)
                {
                    event.prevent_default();
                    close_palette.run(());
                }
            } else if !has_layers() {
                event.prevent_default();
                open_palette.run(());
            }
        },
        UseEventListenerOptions::default().capture(false),
    );
}
