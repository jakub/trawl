// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Shell/>` — authenticated layout wrapping all non-login routes.
//!
//! Renders topbar, left rail, status bar, and toast bus. Child routes
//! mount into `<Outlet/>` inside `<main>`. Auth is checked on mount;
//! unauthenticated users are redirected to `/login`.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::Outlet;

use crate::api;
use crate::components::rail::Rail;
use crate::components::status_bar::{StatusBar, StatusKind};
use crate::components::toast::{ToastBus, Toasts};
use crate::components::topbar::TopBar;
use crate::state::app_mode;
use crate::state::query::RangeSpec;
use crate::state::section;

/// Shared status signals that the Shell owns and the StatusBar reads.
/// Search (or any page that wants to drive the status bar) writes to
/// these via `use_context::<ShellStatus>()`.
#[derive(Clone, Copy)]
pub struct ShellStatus {
    pub kind: RwSignal<StatusKind>,
    pub count: RwSignal<Option<usize>>,
    pub range: RwSignal<RangeSpec>,
    pub lagged: RwSignal<Option<u64>>,
}

#[component]
pub fn Shell() -> impl IntoView {
    let me = RwSignal::new(None::<api::MeResponse>);
    let redirect_to_login = RwSignal::new(false);

    Effect::new(move |_| {
        spawn_local(async move {
            match api::me().await {
                Ok(resp) => me.set(Some(resp)),
                Err(_) => redirect_to_login.set(true),
            }
        });
    });

    Effect::new(move |_| {
        if redirect_to_login.get()
            && let Some(win) = web_sys::window()
        {
            let _ = win.location().set_href("/login");
        }
    });

    let current_app = app_mode::from_url();
    let current_section = section::from_url(current_app);

    let bus = ToastBus::new();
    provide_context(bus);

    let shell_status = ShellStatus {
        kind: RwSignal::new(StatusKind::Connected),
        count: RwSignal::new(None),
        range: RwSignal::new(RangeSpec::default()),
        lagged: RwSignal::new(None),
    };
    provide_context(shell_status);
    provide_context(me);

    view! {
        <div class="shell">
            <TopBar mode=current_app me=Signal::derive(move || me.get())/>
            <div class="body">
                <Rail mode=current_app section=Signal::derive(move || current_section.get())/>
                <Show when=move || me.get().is_some() fallback=|| view! { <main class="main"></main> }>
                    <main class="main">
                        <Outlet/>
                    </main>
                </Show>
            </div>
            <StatusBar
                status=Signal::derive(move || shell_status.kind.get())
                count=Signal::derive(move || shell_status.count.get())
                range=Signal::derive(move || shell_status.range.get())
                lagged=Signal::derive(move || shell_status.lagged.get())
            />
            <Toasts bus=bus/>
        </div>
    }
}

/// Generic redirect component. Navigates to `path` on mount.
#[component]
pub fn RedirectTo(#[prop(into)] path: String) -> impl IntoView {
    let path = path.clone();
    Effect::new(move |_| {
        if let Some(win) = web_sys::window() {
            let _ = win.location().set_href(&path);
        }
    });
    view! { <p>"redirecting…"</p> }
}

/// 404 page — rendered by the router fallback.
#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <div class="login-shell">
            <div class="login-card">
                <h1>"404"</h1>
                <p class="subtitle">"that page does not exist."</p>
            </div>
        </div>
    }
}
