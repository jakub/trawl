// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<StatusBar/>` — 26px footer with status, last-search summary,
//! theme toggle, and version.
//!
//! Sources count + indexed total + ingest rate are stubbed (`—`)
//! until `/api/v1/stats` gets wired through. Theme toggle calls
//! `UiPrefs::theme.update()` and the `state::theme` effect
//! re-projects to `<html data-theme>`.

use leptos::prelude::*;

use crate::state::theme::{Theme, UiPrefs};

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
    /// Currently selected range label (e.g. "15m").
    #[prop(into)]
    range: Signal<&'static str>,
    /// Currently lagged events count, if the live stream emitted a
    /// back-pressure notification.
    #[prop(into)]
    lagged: Signal<Option<u64>>,
) -> impl IntoView {
    let prefs = use_context::<UiPrefs>();

    let toggle_theme = move |_| {
        if let Some(p) = prefs {
            p.theme.update(|t| *t = t.toggled());
        }
    };

    let theme_label = move || {
        prefs.map_or("light", |p| match p.theme.get() {
            Theme::Light => "light",
            Theme::Dark => "dark",
        })
    };

    let status_class = move || match status.get() {
        StatusKind::Connected => "dot",
        StatusKind::Hauling | StatusKind::Live => "dot live",
        StatusKind::Error => "dot err",
    };

    let status_label = move || match status.get() {
        StatusKind::Connected => "CONNECTED",
        StatusKind::Hauling => "HAULING",
        StatusKind::Live => "LIVE",
        StatusKind::Error => "ERROR",
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
                        <span class="strong">{format!("LAGGED {n}")}</span>
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
                <span class="amber">{move || range.get()}</span>
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
