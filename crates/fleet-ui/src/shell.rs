// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Shell/>` — chrome-only application layout: body(sidebar +
//! content(command bar + main(children))) + footer + toast host.
//!
//! Owns the [`ToastBus`] via `provide_context` and the route command palette.
//! Does NOT do auth, fetch `/me`, render a status bar, or know what
//! routes exist beyond the sidebar groups it is handed. Consumers wrap
//! `Shell` with whatever app-specific concerns they need — trawl's
//! `AuthShell` gates on `/me`, owns its `StatusBar`, passes that as the
//! `footer` prop, and renders its router `<Outlet/>` as `children`.
//!
//! # Compact navigation
//!
//! Below 900px the sidebar is not docked: the command bar's toggle opens
//! it as a `FocusPolicy::Capture` overlay layer over a scrim, the same
//! posture [`crate::drawer`] has. While it is open `has_layers()` is
//! true, so the palette chord is inert, Escape closes it only while it
//! is topmost, a route change closes it, and focus returns to the
//! toggle through the layer's opener restore.
//!
//! # `ToastBus` contract
//!
//! Shell is the single owner of the toast stack: one bus, one
//! `<Toasts/>` host, provided via context. `children` and `footer`
//! closures execute inside Shell's body, so `<Outlet/>` page content
//! reaches the bus with `expect_context::<ToastBus>()`. Consumers must
//! not create their own bus or mount their own `<Toasts/>` inside a
//! Shell — see [`crate::toast`] for the full contract.

use leptos::ev;
use leptos::html::{Button, Div};
use leptos::prelude::*;
use leptos::web_sys;
use leptos_router::hooks::use_location;
use leptos_use::{
    UseEventListenerOptions, use_event_listener, use_event_listener_with_options, use_media_query,
    use_window,
};
use wasm_bindgen::JsCast;

use crate::command_palette::{
    ChordFacts, CommandInput, CommandPalette, commands_from, editable_target, is_palette_chord,
    palette_available, platform_kbd_hint,
};
use crate::overlay::{FocusPolicy, OverlayLayer, has_layers, use_overlay_layer_with};

use crate::sidebar::{Sidebar, SidebarGroup};
use crate::theme::{Sidebar as SidebarPref, UiPrefs};
use crate::toast::{ToastBus, Toasts};
use crate::topbar::{TopBar, UserInfo};

#[component]
#[allow(clippy::too_many_lines)] // the chrome is one cohesive view tree
pub fn Shell(
    #[prop(into)] brand: String,
    #[prop(into)] brand_accent: String,
    #[prop(into)] sidebar_groups: Signal<Vec<SidebarGroup>>,
    #[prop(into)] sidebar_active: Signal<String>,
    #[prop(into)] user: Signal<Option<UserInfo>>,
    on_logout: Callback<()>,
    /// App footer (status bar). Optional — footer-less apps omit it and
    /// the shell grid's `auto` row collapses to zero height.
    #[prop(optional)]
    footer: Option<Children>,
    /// Bottom-pinned sidebar slot, passed through to [`Sidebar`]'s
    /// `bottom` prop. A [`ViewFn`], not `Children`: the sidebar is
    /// mounted docked or as the compact overlay, so the slot has to
    /// render more than once.
    #[prop(optional, into)]
    sidebar_bottom: Option<ViewFn>,
    children: Children,
) -> impl IntoView {
    let bus = ToastBus::new();
    provide_context(bus);

    let commands = Memo::new(move |_| {
        let groups = sidebar_groups.get();
        commands_from(
            std::iter::empty::<CommandInput<'_>>(),
            groups
                .iter()
                .flat_map(|group| group.items.iter())
                .map(|item| CommandInput {
                    label: &item.label,
                    path: &item.path,
                }),
        )
    });
    let available = Signal::derive(move || commands.with(|items| palette_available(items)));
    let palette_open = RwSignal::new(false);
    let palette_mounted = RwSignal::new(false);
    let palette_trigger = NodeRef::<Button>::new();
    let main_ref = NodeRef::<leptos::html::Main>::new();
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

    // The breakpoint the CSS uses for the same switch. 899.98px rather
    // than 900px so exactly one of the two presentations is live at
    // every width, including the fractional widths a zoomed viewport
    // reports.
    let compact = use_media_query("(max-width: 899.98px)");
    let nav_open = RwSignal::new(false);
    let nav_toggle = NodeRef::<Button>::new();
    let close_nav = Callback::new(move |()| nav_open.set(false));
    let toggle_nav = Callback::new(move |()| nav_open.update(|open| *open = !*open));

    // The crumb names the active destination; before the consumer's
    // routing state resolves, the brand is the honest fallback.
    let brand_title = brand.clone();
    let page_title = Signal::derive(move || {
        let active = sidebar_active.get();
        sidebar_groups
            .with(|groups| {
                groups
                    .iter()
                    .flat_map(|group| group.items.iter())
                    .find(|item| item.id == active)
                    .map(|item| item.label.clone())
            })
            .unwrap_or_else(|| brand_title.clone())
    });

    // Collapse is a persisted preference, so the control exists only
    // where `fleet_ui::install()` was called: without prefs there is
    // nowhere to write the state and the sidebar stays expanded.
    let prefs = use_context::<UiPrefs>();
    let collapsed =
        prefs.map(|p| Signal::derive(move || p.sidebar().get() == SidebarPref::Collapsed));
    let on_toggle_collapse = prefs
        .map(|p| Callback::new(move |()| p.sidebar().update(|state| *state = state.toggled())));

    let overlay_bottom = sidebar_bottom.clone();
    let brand_overlay = brand.clone();
    let accent_overlay = brand_accent.clone();

    view! {
        <div class="shell">
            <a class="skip-link" href="#fleet-main-content" on:click=move |event| {
                event.prevent_default();
                if let Some(main) = main_ref.get_untracked() {
                    let _ = main.focus();
                }
            }>"Skip to main content"</a>
            <div class="body">
                {move || (compact.get() && nav_open.get()).then(|| view! {
                    <NavOverlay
                        brand=brand_overlay.clone()
                        brand_accent=accent_overlay.clone()
                        groups=sidebar_groups
                        active=sidebar_active
                        bottom=overlay_bottom.clone()
                        on_close=close_nav
                    />
                })}
                <Show when=move || !compact.get()>
                    <Sidebar
                        brand=brand.clone()
                        brand_accent=brand_accent.clone()
                        groups=sidebar_groups
                        active=sidebar_active
                        collapsed=collapsed
                        on_toggle_collapse=on_toggle_collapse
                        bottom=sidebar_bottom.clone()
                    />
                </Show>
                <div class="shell-content">
                    <TopBar
                        page_title=page_title
                        nav_open=nav_open
                        on_toggle_nav=toggle_nav
                        nav_toggle=nav_toggle
                        user=user
                        on_logout=on_logout
                        on_open_palette=open_palette
                        palette_open=palette_open
                        palette_available=available
                        palette_trigger=palette_trigger
                    />
                    <main class="main" id="fleet-main-content" tabindex="-1" node_ref=main_ref>
                        {children()}
                    </main>
                </div>
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

/// The compact-width sidebar: a scrim plus the sidebar itself, registered
/// with the overlay stack so the rest of the chrome arbitrates against it.
///
/// `FocusPolicy::Capture` — initial focus moves into the panel and
/// returns to the opener on close, but nothing is Tab-trapped and the
/// background stays interactive, exactly as [`crate::drawer`] behaves.
#[component]
fn NavOverlay(
    brand: String,
    brand_accent: String,
    groups: Signal<Vec<SidebarGroup>>,
    active: Signal<String>,
    bottom: Option<ViewFn>,
    on_close: Callback<()>,
) -> impl IntoView {
    let scrim_ref = NodeRef::<Div>::new();
    // The sidebar owns its own id; reading the panel back by id keeps the
    // overlay plumbing out of the component's prop surface.
    let layer = use_overlay_layer_with(FocusPolicy::Capture, || {
        document().get_element_by_id("fleet-sidebar")
    });

    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if e.key() == "Escape" && layer.is_topmost() {
            e.prevent_default();
            on_close.run(());
        }
    });

    // Navigating is what the overlay is for, so the first route change
    // after it opens closes it. Comparing against the pathname at mount
    // keeps the effect from firing on its own first run.
    let pathname = use_location().pathname;
    let opened_at = pathname.get_untracked();
    Effect::new(move |_| {
        if pathname.get() != opened_at {
            on_close.run(());
        }
    });

    let on_scrim_mousedown = move |e: web_sys::MouseEvent| {
        // Identity comparison, same idiom as the Drawer: a press that
        // started inside the panel must not dismiss it.
        let Some(scrim) = scrim_ref.get() else {
            return;
        };
        let Some(target) = e.target() else { return };
        let Some(el) = target.dyn_ref::<web_sys::Element>() else {
            return;
        };
        if el.is_same_node(Some(scrim.as_ref())) {
            on_close.run(());
        }
    };

    view! {
        <div class="nav-scrim" node_ref=scrim_ref on:mousedown=on_scrim_mousedown></div>
        <Sidebar
            brand=brand
            brand_accent=brand_accent
            groups=groups
            active=active
            overlay=true
            bottom=bottom
        />
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
            let mut facts = ChordFacts {
                key: &key,
                ctrl: event.ctrl_key(),
                meta: event.meta_key(),
                alt: event.alt_key(),
                shift: event.shift_key(),
                repeat: event.repeat(),
                default_prevented: event.default_prevented(),
                composing: event.is_composing(),
                editable: false,
            };
            if !available.get_untracked() || !is_palette_chord(facts, is_macos) {
                return;
            }
            // Ordinary typing never needs a composed-path walk or DOM queries.
            // Reuse the predicate after classifying a possible chord's target.
            facts.editable = editable_target(&event);
            if !is_palette_chord(facts, is_macos) {
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
