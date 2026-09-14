// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<StatusBar/>` — 26px footer with status, last-search summary,
//! admin stats, and a corner theme toggle.
//!
//! The stats cluster (hot buffer / WAL backlog / active queries /
//! uptime) renders only while the `admin` signal carries a
//! [`DashboardSnapshot`] — `AuthShell` feeds it from the admin-only
//! `/api/v1/dashboard/stream` SSE stream, so non-admins never see the
//! group. The theme toggle calls `UiPrefs::theme()` `.update()` and
//! fleet-ui's install effect re-projects to `<html data-theme>`.

use fleet_ui::{Theme, UiPrefs};
use leptos::prelude::*;
use leptos::web_sys;
use trawl_api::DashboardSnapshot;

use crate::api;
use crate::components::service_card_fmt::{format_bytes, format_count, format_uptime};
use crate::search_status::{FooterCount, StatusKind, footer_count_label};

#[component]
#[allow(clippy::too_many_lines)] // one footer, one markup tree
pub fn StatusBar(
    #[prop(into)] status: Signal<StatusKind>,
    /// The active result source's count and the source it names
    /// (`Last —` before anything has run).
    #[prop(into)]
    count: Signal<FooterCount>,
    /// Currently lagged events count, if the live stream emitted a
    /// back-pressure notification.
    #[prop(into)]
    lagged: Signal<Option<u64>>,
    /// Live admin stats from `/api/v1/dashboard/stream`; `None` for
    /// non-admin sessions (the stats cluster is hidden entirely).
    #[prop(into)]
    admin: Signal<Option<DashboardSnapshot>>,
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

    // The visible text is the theme in force; the accessible name says
    // what pressing the control does (ADR-0028's ruling on its menu
    // twin). The name OPENS with the visible word because WCAG 2.5.3
    // asks a name to contain its own label: a control reading "dark"
    // and named only "Switch to light theme" cannot be activated by
    // voice with the word on it.
    let next_theme_label = move || {
        prefs.map_or("dark", |p| match p.theme().get() {
            Theme::Light => "dark",
            Theme::Dark => "light",
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
        <footer class="statusbar">
            <div class="grp">
                <span class=status_class></span>
                <span class="strong status-label">{status_label}</span>
            </div>
            {move || lagged.get().map(|n| view! {
                <>
                    <span class="divider">"·"</span>
                    <div class="grp lagged">
                        <span class="strong">{format!("Lagged {n}")}</span>
                    </div>
                </>
            })}
            {move || admin.get().map(|s| view! {
                <>
                    <span class="divider">"·"</span>
                    <div class="grp" title="Hot buffer (events / bytes)">
                        <span>"Hot "</span>
                        <span class="strong">
                            {format_count(u64::try_from(s.hot_buffer_events).unwrap_or_default())}
                        </span>
                        <span>
                            {format!(" / {}", format_bytes(u64::try_from(s.hot_buffer_bytes).unwrap_or_default()))}
                        </span>
                    </div>
                    <span class="divider">"·"</span>
                    <div class="grp" title="WAL backlog (files / bytes)">
                        <span>"WAL "</span>
                        <span class="strong">{s.wal_files.to_string()}</span>
                        <span>{format!(" / {}", format_bytes(s.wal_bytes))}</span>
                    </div>
                    <span class="divider">"·"</span>
                    <div class="grp" title="Active queries">
                        <span>"Queries "</span>
                        <span class="strong">{s.active_queries.len().to_string()}</span>
                    </div>
                    <span class="divider">"·"</span>
                    <div class="grp" title="Server uptime">
                        <span>"Up "</span>
                        <span class="accent">{format_uptime(s.uptime_secs)}</span>
                    </div>
                </>
            })}
            <span class="divider">"·"</span>
            // The footer names the source it counted, so the label is
            // data, not markup: `Last` in snapshot, `Received` or
            // `Updates` while the stream is the active source.
            <div class="grp count">
                <span>
                    {move || format!("{} ", footer_count_label(&count.get()).0)}
                    <span class="strong">
                        {move || footer_count_label(&count.get()).1}
                    </span>
                </span>
            </div>
            <div class="sp"></div>
            <button
                type="button"
                class="grp clickable"
                aria-label=move || {
                    let current = theme_label();
                    let next = next_theme_label();
                    format!("Theme {current}: switch to {next} theme")
                }
                on:click=toggle_theme
            >
                <span>{theme_label}</span>
            </button>
        </footer>
    }
}
