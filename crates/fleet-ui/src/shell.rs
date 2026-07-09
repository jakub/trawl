// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Shell/>` — chrome-only application layout: topbar + body(rail +
//! main(children)) + footer + toast host.
//!
//! Owns the [`ToastBus`] via `provide_context`. Owns nothing else.
//! Does NOT do auth, fetch `/me`, render a status bar, or know what
//! routes the app has. Consumers wrap `Shell` with whatever
//! app-specific concerns they need — trawl's `AuthShell` gates on
//! `/me`, owns its `StatusBar`, passes that as the `footer` prop, and
//! renders its router `<Outlet/>` as `children`.
//!
//! # ToastBus contract
//!
//! Shell is the single owner of the toast stack: one bus, one
//! `<Toasts/>` host, provided via context. `children` and `footer`
//! closures execute inside Shell's body, so `<Outlet/>` page content
//! reaches the bus with `expect_context::<ToastBus>()`. Consumers must
//! not create their own bus or mount their own `<Toasts/>` inside a
//! Shell — see [`crate::toast`] for the full contract.

use leptos::prelude::*;

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

    view! {
        <div class="shell">
            <TopBar
                brand=brand
                brand_accent=brand_accent
                modes=modes
                app_links=app_links
                user=user
                on_logout=on_logout
            />
            <div class="body">
                {match rail_bottom {
                    Some(b) => view! { <Rail items=rail_items active=rail_active bottom=b/> }.into_any(),
                    None => view! { <Rail items=rail_items active=rail_active/> }.into_any(),
                }}
                <main class="main">
                    {children()}
                </main>
            </div>
            {footer.map(|f| f())}
            <Toasts bus=bus/>
        </div>
    }
}
