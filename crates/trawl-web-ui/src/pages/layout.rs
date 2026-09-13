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
use crate::components::status_bar::StatusBar;
use crate::search_status::{FooterCount, StatusKind};
use crate::state::app_mode::{self, AppMode};
use crate::state::section;
use crate::state::stats_stream::{StatsLifecycle, start_stats_stream};

/// Shared status signals that the shell owns and the `StatusBar` reads.
/// Search (or any page that wants to drive the status bar) writes to
/// these via `use_context::<ShellStatus>()`.
#[derive(Clone, Copy)]
pub struct ShellStatus {
    pub kind: RwSignal<StatusKind>,
    pub count: RwSignal<FooterCount>,
    pub lagged: RwSignal<Option<u64>>,
}

#[component]
#[allow(clippy::too_many_lines)] // shell chrome is one cohesive view tree
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
        // Pages that never run a search keep the shell's own default:
        // the snapshot source with nothing counted yet.
        count: RwSignal::new(FooterCount::last(None)),
        lagged: RwSignal::new(None),
    };
    provide_context(shell_status);
    provide_context(me);

    // The Health page and footer share this report. Only server_manage
    // sessions issue the bootstrap GET or open the stream. The footer
    // receives live snapshots only, so reconnecting numbers disappear.
    // Dropping the handle closes the stream and invalidates its callbacks.
    let dashboard = RwSignal::new(crate::dashboard_state::DashboardState::<
        trawl_api::DashboardSnapshot,
    >::default());
    provide_context(dashboard);
    let admin_stats = RwSignal::new(None::<trawl_api::DashboardSnapshot>);
    Effect::new(move |_| {
        let state = dashboard.get();
        admin_stats.set(
            if state.phase == crate::dashboard_state::DashboardPhase::Live {
                state.snapshot
            } else {
                None
            },
        );
    });
    let stats_handle: StoredValue<Option<StatsLifecycle>, LocalStorage> =
        StoredValue::new_local(None);
    on_cleanup(move || stats_handle.update_value(|s| *s = None));
    Effect::new(move |_| {
        let is_admin = me
            .get()
            .is_some_and(|m| crate::perms::is_trawl_admin(&m.permissions));
        // Drop any previous stream first — this Effect re-runs whenever
        // `me` changes, and two live EventSources would double-push.
        stats_handle.update_value(|s| *s = None);
        dashboard.set(crate::dashboard_state::DashboardState::default());
        if is_admin {
            stats_handle.update_value(|s| *s = start_stats_stream(dashboard));
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
                    admin=admin_stats
                />
            }.into_any())
            rail_bottom=Box::new(|| view! {
                <a class="it" title="Help" href="https://trawl.sh" target="_blank" rel="noopener noreferrer">
                    <IconView icon=Icon::Question size=20 stroke_width=1.4/>
                    <span class="lb">"Help"</span>
                </a>
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
#[allow(clippy::needless_pass_by_value)] // Leptos component props: easier to pass owned
pub fn RedirectTo(#[prop(into)] path: String) -> impl IntoView {
    let path = path.clone();
    Effect::new(move |_| {
        if let Some(win) = web_sys::window() {
            let _ = win.location().set_href(&path);
        }
    });
    view! { <p>"Redirecting…"</p> }
}

/// 404 page — rendered by the router fallback.
#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <div class="login-shell">
            <div class="login-card">
                <h1>"404"</h1>
                <p class="subtitle">"That page does not exist."</p>
                <a href="/search">"Go to Search"</a>
            </div>
        </div>
    }
}
