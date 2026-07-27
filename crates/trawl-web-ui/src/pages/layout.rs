// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<AuthShell/>` — authenticated layout wrapping all non-login routes.
//!
//! A thin app-specific wrapper over [`fleet_ui::Shell`]: keeps the
//! `/me` fetch, the Unauthorized→`/login` redirect, and the
//! `ShellStatus` + `me` context provision; maps trawl's `AppMode` /
//! `section` state onto fleet-ui's `ModeTab` / `RailItem` props.
//! The toast bus and `<Toasts/>` host are owned by `fleet_ui::Shell`
//! (pages reach the bus via `expect_context::<ToastBus>()`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::Outlet;

use fleet_ui::{Icon, IconView, ModeTab, RailItem, Shell, UserInfo};

use crate::api;
use crate::components::status_bar::{StatusBar, StatusKind};
use crate::state::app_mode::{self, AppMode};
use crate::state::section;
use crate::state::stats_stream::{StatsLifecycle, start_stats_stream};

/// Shared status signals that the shell owns and the StatusBar reads.
/// Search (or any page that wants to drive the status bar) writes to
/// these via `use_context::<ShellStatus>()`.
#[derive(Clone, Copy)]
pub struct ShellStatus {
    pub kind: RwSignal<StatusKind>,
    pub count: RwSignal<Option<usize>>,
    pub lagged: RwSignal<Option<u64>>,
}

#[component]
pub fn AuthShell() -> impl IntoView {
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

    let shell_status = ShellStatus {
        kind: RwSignal::new(StatusKind::Connected),
        count: RwSignal::new(None),
        lagged: RwSignal::new(None),
    };
    provide_context(shell_status);
    provide_context(me);

    // Admin-only live stats for the footer. The stream opens only after
    // `/me` resolves with the `server_manage` permission — the endpoint is
    // `ServerManage`-gated upstream, so non-admins never even issue the
    // request. The `StoredValue` bounds the `EventSource` lifetime;
    // dropping it closes the connection.
    let admin_stats = RwSignal::new(None::<trawl_api::DashboardSnapshot>);
    let stats_handle: StoredValue<Option<StatsLifecycle>, LocalStorage> =
        StoredValue::new_local(None);
    on_cleanup(move || stats_handle.update_value(|s| *s = None));
    Effect::new(move |_| {
        let is_admin = me
            .get()
            .is_some_and(|m| m.permissions.iter().any(|p| p == "server_manage"));
        // Drop any previous stream first — this Effect re-runs whenever
        // `me` changes, and two live EventSources would double-push.
        stats_handle.update_value(|s| *s = None);
        if is_admin {
            stats_handle.update_value(|s| *s = start_stats_stream(admin_stats));
        } else {
            admin_stats.set(None);
        }
    });

    let rail_items = Signal::derive(move || {
        section::items_for(current_app.get())
            .iter()
            .map(|item| RailItem {
                id: item.id.to_string(),
                label: item.label.to_string(),
                icon: item.icon,
                path: item.path.to_string(),
                badge: None,
            })
            .collect::<Vec<_>>()
    });

    let modes = Signal::derive(move || {
        let cur = current_app.get();
        AppMode::ALL
            .iter()
            .copied()
            .map(|m| ModeTab {
                id: m.default_path().to_string(),
                label: m.label().to_string(),
                path: m.default_path().to_string(),
                active: m == cur,
            })
            .collect::<Vec<_>>()
    });

    let user = Signal::derive(move || {
        me.get().map(|m| UserInfo {
            name: m.name,
            detail: m.roles.join(", "),
        })
    });

    let on_logout = Callback::new(|()| {
        spawn_local(async move {
            if let Err(e) = api::logout().await {
                web_sys::console::warn_1(&format!("logout request failed: {e}").into());
            }
            if let Some(win) = web_sys::window() {
                let _ = win.location().set_href("/login");
            }
        });
    });

    view! {
        <Shell
            brand="trawl"
            brand_accent="_"
            rail_items=rail_items
            rail_active=Signal::derive(move || current_section.get())
            modes=modes
            user=user
            on_logout=on_logout
            footer=Box::new(move || view! {
                <StatusBar
                    status=Signal::derive(move || shell_status.kind.get())
                    count=Signal::derive(move || shell_status.count.get())
                    lagged=Signal::derive(move || shell_status.lagged.get())
                    stats=admin_stats
                />
            }.into_any())
            rail_bottom=Box::new(|| view! {
                <div class="it" title="Help — coming soon">
                    <IconView icon=Icon::Question size=20 stroke_width=1.4/>
                    <span class="lb">"Help"</span>
                </div>
            }.into_any())
        >
            <Show when=move || me.get().is_some() fallback=|| ()>
                <Outlet/>
            </Show>
        </Shell>
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
