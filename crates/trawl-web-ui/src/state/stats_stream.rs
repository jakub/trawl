// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shell owns one dashboard stream and its one-shot bootstrap.
use crate::dashboard_state::DashboardState;
use leptos::prelude::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use trawl_api::DashboardSnapshot;
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{EventSource, MessageEvent};

pub type SharedDashboard = RwSignal<DashboardState<DashboardSnapshot>>;

/// Closing invalidates queued callbacks and the outstanding bootstrap first.
#[allow(dead_code)]
pub struct StatsLifecycle {
    alive: Arc<AtomicBool>,
    source: EventSource,
    on_stats: Closure<dyn FnMut(MessageEvent)>,
    on_error: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for StatsLifecycle {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        self.source.set_onerror(None);
        let _ = self
            .source
            .remove_event_listener_with_callback("stats", self.on_stats.as_ref().unchecked_ref());
        self.source.close();
    }
}

pub fn start_stats_stream(stats: SharedDashboard) -> Option<StatsLifecycle> {
    let Ok(source) = EventSource::new("/api/v1/dashboard/stream") else {
        stats.update(|s| s.stream_error(false));
        return None;
    };
    let alive = Arc::new(AtomicBool::new(true));
    let stats_alive = alive.clone();
    let on_stats = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        if !stats_alive.load(Ordering::SeqCst) {
            return;
        }
        let snapshot = ev
            .data()
            .as_string()
            .and_then(|s| serde_json::from_str::<DashboardSnapshot>(&s).ok());
        if let Some(snapshot) = snapshot {
            stats.update(|s| s.stream_snapshot(snapshot));
        } else {
            stats.update(|s| s.stream_error(false));
        }
    });
    let err_source = source.clone();
    let err_alive = alive.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        if err_alive.load(Ordering::SeqCst) {
            stats.update(|s| s.stream_error(err_source.ready_state() != EventSource::CLOSED));
        }
    });
    let _ = source.add_event_listener_with_callback("stats", on_stats.as_ref().unchecked_ref());
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    let bootstrap_alive = alive.clone();
    leptos::task::spawn_local(async move {
        let result = crate::api::dashboard().await.map_err(|e| e.http_status());
        if bootstrap_alive.load(Ordering::SeqCst) {
            stats.update(|s| s.bootstrap(result));
        }
    });
    Some(StatsLifecycle {
        alive,
        source,
        on_stats,
        on_error,
    })
}
