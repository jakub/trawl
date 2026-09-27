// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<AuthShell/>` — authenticated layout wrapping all non-login routes.
//!
//! A thin app-specific wrapper over [`fleet_ui::Shell`]: keeps the
//! `/me` fetch, the Unauthorized redirect to sign-in carrying the requested
//! URL, and the
//! `ShellStatus` + `me` context provision; maps trawl's `AppMode` /
//! `section` state onto fleet-ui's `SidebarGroup` / `RailItem` props.
//! The toast bus and `<Toasts/>` host are owned by `fleet_ui::Shell`
//! (pages reach the bus via `expect_context::<ToastBus>()`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::Outlet;

use fleet_ui::{Icon, IconView, RailItem, Shell, SidebarGroup, UserInfo};

use crate::api;
use crate::components::status_bar::StatusBar;
use crate::search_status::{FooterCount, StatusKind};
use crate::state::app_mode;
use crate::state::filter_rail::FilterRailChoice;
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
    let auth_error = RwSignal::new(None::<&'static str>);
    let checking_session = RwSignal::new(false);
    let session_attempt = RwSignal::new(0u64);

    Effect::new(move |_| {
        let _ = session_attempt.get();
        checking_session.set(true);
        auth_error.set(None);
        spawn_local(async move {
            match api::me().await {
                Ok(resp) => {
                    me.try_set(Some(resp));
                }
                Err(api::ApiError::Unauthorized) => {
                    redirect_to_login.try_set(true);
                }
                Err(err) => {
                    auth_error.try_set(Some(if err.http_status() == Some(403) {
                        "This account does not have access to Trawl."
                    } else {
                        "Unable to check your session. Try again when the service is available."
                    }));
                }
            }
            checking_session.try_set(false);
        });
    });
    let retry_session = move |_| {
        if !checking_session.get_untracked() {
            session_attempt.update(|attempt| *attempt += 1);
        }
    };

    Effect::new(move |_| {
        if redirect_to_login.get() {
            crate::auth_return::redirect_to_login();
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
    // The wide filter rail's hand choice lives here, above the Search
    // route, so it survives route changes and drops on sign-out: `/login`
    // is outside this shell (ADR-0044).
    provide_context(FilterRailChoice::new());

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

    let sidebar_groups = Signal::derive(|| {
        section::groups()
            .iter()
            .map(|group| SidebarGroup {
                label: group.label.map(String::from),
                items: group
                    .items
                    .iter()
                    .map(|item| RailItem {
                        id: item.id.to_string(),
                        label: item.label.to_string(),
                        icon: item.icon,
                        path: item.path.to_string(),
                        badge: None,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>()
    });

    let user = Signal::derive(move || {
        me.get().map(|m| UserInfo {
            name: m.name,
            detail: m.roles.join(", "),
        })
    });

    let signing_out = RwSignal::new(false);
    let logout_error = RwSignal::new(false);
    let on_logout = Callback::new(move |()| {
        if signing_out.get_untracked() || !crate::auth_return::begin_explicit_logout() {
            return;
        }
        signing_out.set(true);
        logout_error.set(false);
        spawn_local(async move {
            if api::logout().await.is_err() || !crate::auth_return::finish_explicit_logout() {
                crate::auth_return::cancel_explicit_logout();
                logout_error.try_set(true);
            }
            signing_out.try_set(false);
        });
    });

    view! {
        <Shell
            brand="trawl"
            brand_accent="_"
            sidebar_groups=sidebar_groups
            sidebar_active=Signal::derive(move || current_section.get())
            user=user
            on_logout=on_logout
            // `Shell` calls the footer once, so the session gate is a
            // reactive `Show`, not an `if`. The status bar probes
            // `/api/v1/health`, which the trawl-web proxy answers 401 without
            // a session: mount it only once `/me` confirms one, as `Outlet` does.
            footer=Box::new(move || view! {
                <Show when=move || me.get().is_some()>
                    <StatusBar
                        status=Signal::derive(move || shell_status.kind.get())
                        count=Signal::derive(move || shell_status.count.get())
                        lagged=Signal::derive(move || shell_status.lagged.get())
                        admin=admin_stats
                    />
                </Show>
            }.into_any())
            sidebar_bottom=ViewFn::from(|| view! {
                <a class="it" title="Help" href="https://trawl.sh" target="_blank" rel="noopener noreferrer">
                    <IconView icon=Icon::Question size=18 stroke_width=1.5/>
                    <span class="lb">"Help"</span>
                </a>
            })
        >
            <Show when=move || logout_error.get()>
                <div class="auth-notice">
                    <p role="alert">"Sign out was not confirmed. Your session may still be active."</p>
                    <button type="button" class="btn-sec" disabled=move || signing_out.get() on:click=move |_| on_logout.run(())>"Retry sign out"</button>
                </div>
            </Show>
            <Show when=move || me.get().is_none()>
                <div class="auth-notice">
                    {move || auth_error.get().map(|message| view! { <p role="alert">{message}</p> })}
                    <Show when=move || checking_session.get()><p role="status">"Checking your session…"</p></Show>
                    <Show when=move || auth_error.get().is_some()>
                        <button type="button" class="btn-sec" disabled=move || checking_session.get() on:click=retry_session>"Retry session check"</button>
                    </Show>
                </div>
            </Show>
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
        <main class="login-shell">
            <div class="login-card">
                <h1>"404"</h1>
                <p class="subtitle">"That page does not exist."</p>
                <a href="/search">"Go to Search"</a>
            </div>
        </main>
    }
}
