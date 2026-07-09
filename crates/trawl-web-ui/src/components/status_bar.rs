// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<StatusBar/>` — 26px footer with status, last-search summary,
//! theme toggle, and version.
//!
//! Sources count + indexed total + ingest rate are stubbed (`—`)
//! until `/api/v1/stats` gets wired through. Theme toggle calls
//! `UiPrefs::theme().update()` and fleet-ui's install effect
//! re-projects to `<html data-theme>`.

use fleet_ui::{Theme, UiPrefs};
use leptos::prelude::*;
use leptos::web_sys;

use crate::api;
use crate::state::query::RangeSpec;

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
    /// Currently selected range spec.
    #[prop(into)]
    range: Signal<RangeSpec>,
    /// Currently lagged events count, if the live stream emitted a
    /// back-pressure notification.
    #[prop(into)]
    lagged: Signal<Option<u64>>,
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
            <span class="divider">"·"</span>
            <div class="grp"><span>"— sources"</span></div>
            <span class="divider">"·"</span>
            <div class="grp"><span>"— indexed"</span></div>
            <span class="divider">"·"</span>
            <div class="grp"><span>"ingest " <span class="amber">"—/s"</span></span></div>
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
            <div class="grp">
                <span>"range "</span>
                <span class="amber">{move || range.get().label()}</span>
            </div>
            <span class="divider">"·"</span>
            <div class="grp clickable" on:click=toggle_theme title="Switch theme">
                <span>{theme_label}</span>
            </div>
            <span class="divider">"·"</span>
            <div class="grp">
                <span>"trawl "</span>
                <span class="strong">{concat!("v", env!("CARGO_PKG_VERSION"))}</span>
            </div>
        </div>
    }
}
