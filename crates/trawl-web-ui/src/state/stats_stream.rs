// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SSE-backed admin stats stream: `/api/v1/dashboard/stream` pushes the
//! server's [`DashboardSnapshot`] every ~2s as named `stats` events,
//! wired into a single signal the `<StatusBar/>` footer reads.
//!
//! The caller (`AuthShell`) only opens the stream for admin sessions —
//! the endpoint is `ServerManage`-gated upstream. `EventSource`
//! auto-reconnects on transient blips (cookie rides along); a
//! permanently CLOSED source clears the signal so the footer group
//! disappears instead of freezing on stale numbers.

use leptos::prelude::*;
use trawl_api::DashboardSnapshot;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{EventSource, MessageEvent};

/// Bundle of the `EventSource` handle + its registered closures.
/// Dropping this calls `close()` on the `EventSource` first (so no more
/// callbacks fire) and then releases the closures.
#[allow(dead_code)] // fields held for drop ordering, not read
pub struct StatsLifecycle {
    source: EventSource,
    on_stats: Closure<dyn FnMut(MessageEvent)>,
    on_error: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for StatsLifecycle {
    fn drop(&mut self) {
        self.source.close();
    }
}

/// Open the dashboard-stats stream, wiring `stats` events into the
/// provided signal. Returns `None` if the browser doesn't expose
/// `EventSource` (shouldn't happen in modern browsers).
pub fn start_stats_stream(stats: RwSignal<Option<DashboardSnapshot>>) -> Option<StatsLifecycle> {
    let source = EventSource::new("/api/v1/dashboard/stream").ok()?;

    let on_stats = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let Some(data_str) = ev.data().as_string() else {
            return;
        };
        let Ok(snapshot) = serde_json::from_str::<DashboardSnapshot>(&data_str) else {
            return;
        };
        stats.set(Some(snapshot));
    });

    // Transient errors leave readyState at CONNECTING while the browser
    // retries on its own — ignore those. Only a permanent CLOSE clears
    // the signal.
    let err_source = source.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        if err_source.ready_state() == EventSource::CLOSED {
            stats.set(None);
        }
    });

    let _ = source.add_event_listener_with_callback("stats", on_stats.as_ref().unchecked_ref());
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    Some(StatsLifecycle {
        source,
        on_stats,
        on_error,
    })
}
