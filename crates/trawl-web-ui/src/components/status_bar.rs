// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<StatusBar/>` — 26px footer with status, last-search summary,
//! admin stats, and corner theme + density toggles.
//!
//! The stats cluster (hot buffer / WAL backlog / active queries /
//! uptime) renders only while the `stats` signal carries a
//! [`DashboardSnapshot`] — AuthShell feeds it from the admin-only
//! `/api/v1/dashboard/stream` SSE stream, so non-admins never see the
//! group. Theme/density toggles call
//! `UiPrefs::theme()`/`UiPrefs::density()` `.update()` and fleet-ui's
//! install effect re-projects to `<html data-theme>` /
//! `<html data-density>`.

use fleet_ui::{Density, Theme, UiPrefs};
use leptos::prelude::*;
use leptos::web_sys;
use trawl_api::DashboardSnapshot;

use crate::api;
use crate::components::service_card_fmt::{format_bytes, format_count, format_uptime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// Connected to trawld; idle, ready to run.
    Connected,
    /// Snapshot query in flight.
    Hauling,
    /// Live SSE stream open.
    Live,
    /// Last query errored — wired in a follow-up commit; rendering
    /// switches the status dot to red.
    #[allow(dead_code)]
    Error,
}

#[component]
pub fn StatusBar(
    #[prop(into)] status: Signal<StatusKind>,
    /// Last-search row count (`None` if nothing has run yet).
    #[prop(into)]
    count: Signal<Option<usize>>,
    /// Currently lagged events count, if the live stream emitted a
    /// back-pressure notification.
    #[prop(into)]
    lagged: Signal<Option<u64>>,
    /// Live admin stats from `/api/v1/dashboard/stream`; `None` for
    /// non-admin sessions (the stats cluster is hidden entirely).
    #[prop(into)]
    stats: Signal<Option<DashboardSnapshot>>,
) -> impl IntoView {
    let prefs = use_context::<UiPrefs>();

    let toggle_theme = move |_| {
        if let Some(p) = prefs {
            p.theme().update(|t| *t = t.toggled());
        }
    };

    let theme_label = move || {
        prefs.map_or("light", |p| match p.theme().get() {
            Theme::Light => "light",
            Theme::Dark => "dark",
        })
    };

    let toggle_density = move |_| {
        if let Some(p) = prefs {
            p.density().update(|d| *d = d.toggled());
        }
    };

    let density_label = move || {
        prefs.map_or("compact", |p| match p.density().get() {
            Density::Compact => "compact",
            Density::Comfortable => "comfortable",
        })
    };

    let status_class = move || match status.get() {
        StatusKind::Connected => "dot",
        StatusKind::Hauling | StatusKind::Live => "dot live",
        StatusKind::Error => "dot err",
    };

    // Host the browser is talking to — shown in the connected-state label
    // next to the server version from /api/v1/health.
    let host = web_sys::window()
        .and_then(|w| w.location().host().ok())
        .unwrap_or_default();
    let server = LocalResource::new(api::health);

    let status_label = move || match status.get() {
        StatusKind::Connected => {
            let v = server
                .get()
                .and_then(Result::ok)
                .and_then(|h| h.version)
                .unwrap_or_else(|| "?".to_string());
            format!("Connected ({host} v{v})")
        }
        StatusKind::Hauling => "Hauling".to_string(),
        StatusKind::Live => "Live".to_string(),
        StatusKind::Error => "Error".to_string(),
    };

    view! {
        <div class="statusbar">
            <div class="grp">
                <span class=status_class></span>
                <span class="strong">{status_label}</span>
            </div>
            {move || lagged.get().map(|n| view! {
                <>
                    <span class="divider">"·"</span>
                    <div class="grp lagged">
                        <span class="strong">{format!("Lagged {n}")}</span>
                    </div>
                </>
            })}
            {move || stats.get().map(|s| view! {
                <>
                    <span class="divider">"·"</span>
                    <div class="grp" title="Hot buffer (events / bytes)">
                        <span>"hot "</span>
                        <span class="strong">
                            {format_count(u64::try_from(s.hot_buffer_events).unwrap_or_default())}
                        </span>
                        <span>
                            {format!(" / {}", format_bytes(u64::try_from(s.hot_buffer_bytes).unwrap_or_default()))}
                        </span>
                    </div>
                    <span class="divider">"·"</span>
                    <div class="grp" title="WAL backlog (files / bytes)">
                        <span>"wal "</span>
                        <span class="strong">{s.wal_files.to_string()}</span>
                        <span>{format!(" / {}", format_bytes(s.wal_bytes))}</span>
                    </div>
                    <span class="divider">"·"</span>
                    <div class="grp" title="Active queries">
                        <span>"queries "</span>
                        <span class="strong">{s.active_queries.len().to_string()}</span>
                    </div>
                    <span class="divider">"·"</span>
                    <div class="grp" title="Server uptime">
                        <span>"up "</span>
                        <span class="accent">{format_uptime(s.uptime_secs)}</span>
                    </div>
                </>
            })}
            <span class="divider">"·"</span>
            <div class="grp">
                <span>
                    "last "
                    <span class="strong">
                        {move || count.get().map_or_else(|| "—".to_string(), |c| c.to_string())}
                    </span>
                </span>
            </div>
            <div class="sp"></div>
            <div class="grp clickable" on:click=toggle_theme title="Switch theme">
                <span>{theme_label}</span>
            </div>
            <span class="divider">"·"</span>
            <div class="grp clickable" on:click=toggle_density title="Switch density">
                <span>{density_label}</span>
            </div>
        </div>
    }
}
