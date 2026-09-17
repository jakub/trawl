// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<StatusBar/>` — 26px footer with status, last-search summary,
//! and admin stats.
//!
//! The stats cluster (hot buffer / WAL measurement / active queries /
//! uptime) renders only while the `admin` signal carries a
//! [`DashboardSnapshot`] — `AuthShell` feeds it from the admin-only
//! `/api/v1/dashboard/stream` SSE stream, so non-admins never see the
//! group.

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
                <span class="strong status-label">{status_label}</span>
            </div>
            {move || lagged.get().map(|n| view! {
                <>
                    <div class="grp lagged">
                        <span class="strong">{format!("Lagged {n}")}</span>
                    </div>
                </>
            })}
            {move || admin.get().map(|s| view! {
                <>
                    <div class="grp" title="Hot buffer (events / bytes)">
                        <span>"Hot "</span>
                        <span class="strong">
                            {format_count(u64::try_from(s.hot_buffer_events).unwrap_or_default())}
                        </span>
                        <span>
                            {format!(" / {}", format_bytes(u64::try_from(s.hot_buffer_bytes).unwrap_or_default()))}
                        </span>
                    </div>
                    <div class="grp wal-measurement" title=format!("WAL {}; sample age at dashboard snapshot", wal_reading(&s))>
                        <span>"WAL "</span>
                        <span class="strong">{wal_reading(&s)}</span>
                    </div>
                    <div class="grp" title="Active queries">
                        <span>"Queries "</span>
                        <span class="strong">{s.active_queries.len().to_string()}</span>
                    </div>
                    <div class="grp" title="Server uptime">
                        <span>"Up "</span>
                        <span class="accent">{format_uptime(s.uptime_secs)}</span>
                    </div>
                </>
            })}
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
        </footer>
    }
}

/// The footer receives live dashboard snapshots, but a storage measurement can
/// still have failed. Read its metadata before presenting numeric placeholders.
fn wal_reading(snapshot: &DashboardSnapshot) -> String {
    use trawl_api::StorageMeasurementStatus as Status;
    match (
        snapshot.wal_measurement.status,
        snapshot.wal_measurement.sample_age_secs,
    ) {
        (Status::NotConfigured, _) => "not configured".into(),
        (Status::NotSampled, _) => "awaiting measurement".into(),
        (Status::Failed, None) => "failed; unavailable".into(),
        (status, Some(age)) => {
            let prefix = if status == Status::Failed {
                "failed; last "
            } else {
                ""
            };
            format!(
                "{prefix}{} / {}; age {age}s",
                snapshot.wal_files,
                format_bytes(snapshot.wal_bytes)
            )
        }
        (Status::Complete, None) => "unavailable".into(),
    }
}
