// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SSE-backed live-tail session: raw events ring buffer, aggregation
//! snapshot, and lagged indicator — all Leptos signals driven by a
//! single `EventSource` whose lifetime is bounded by a `StoredValue`.
//!
//! The server emits three distinct SSE event names:
//! - `data`  → JSON object for one log event
//! - `snapshot` → `{columns: [...], rows: [{...}, ...]}` aggregation result
//! - `lagged` → `{missed: N}` back-pressure notification
//!
//! `EventSource` auto-reconnects on network blips (cookie rides along).
//! Search reports failed connections and malformed snapshots, and clears
//! stale chart data. A valid event or reopened connection clears the failure.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use leptos::prelude::*;
use serde::Deserialize;
use trawl_api::value::{Column, QueryResult, Value};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{EventSource, MessageEvent};

use super::stream_session_value::json_to_value;
pub use super::stream_session_value::{RingBuffer, ring_to_result};

/// The failure copy for a stream the browser has given up on: closed,
/// not reconnecting. Search reads it back to tell this failure from the
/// others, so every writer of it names this constant.
pub const STREAM_UNAVAILABLE: &str = "Live stream unavailable. Retry or switch to Snapshot.";

/// Which stream session last proved it opened: the connection opened or
/// the server delivered an event on it.
///
/// A refused live query closes before it opens (ADR-0039), so "never
/// opened" is what lets Search read a closed stream as a possible query
/// error rather than a dropped connection. The mark only ever moves
/// forward, so a callback from an older session cannot claim, or
/// retract, an opening for the current one.
#[derive(Clone, Copy)]
pub struct OpenedMark {
    /// The latest session that opened, shared by every session.
    pub latest: RwSignal<Option<u64>>,
    /// This stream's session, higher than every earlier one.
    pub session: u64,
}

impl OpenedMark {
    fn mark(self) {
        // Untracked read, and a write only when the mark moves: a stream
        // delivering events must not notify on every one of them. The
        // signal may be gone after route teardown.
        let moves = self
            .latest
            .try_get_untracked()
            .is_some_and(|latest| latest.is_none_or(|seen| seen < self.session));
        if moves {
            let _ = self.latest.try_set(Some(self.session));
        }
    }
}

/// Bundle of the `EventSource` handle + all its registered closures.
/// Dropping this calls `close()` on the `EventSource` first (so no more
/// callbacks fire) and then releases the closures.
#[allow(dead_code)] // fields held for drop ordering, not read
pub struct StreamLifecycle {
    source: EventSource,
    on_data: Closure<dyn FnMut(MessageEvent)>,
    on_snapshot: Closure<dyn FnMut(MessageEvent)>,
    on_lagged: Closure<dyn FnMut(MessageEvent)>,
    on_open: Closure<dyn FnMut(web_sys::Event)>,
    on_error: Closure<dyn FnMut(web_sys::Event)>,
    render_tick: Option<gloo_timers::callback::Interval>,
    ring: RwSignal<RingBuffer>,
    dirty: Rc<Cell<bool>>,
}

impl Drop for StreamLifecycle {
    fn drop(&mut self) {
        self.source.close();
        // Cancel before releasing the listener closures. On pause, publish the
        // final received events; on unmount, the signal may already be gone.
        self.render_tick.take();
        if self.dirty.replace(false) {
            self.ring.try_update(|_| {});
        }
    }
}

/// Raw-event buffer shared across the ring, chart-snapshot, and badge
/// signals. Signals live in the caller — this module only wires events
/// to them.
#[derive(Clone, Copy)]
pub struct LiveSignals {
    pub ring: RwSignal<RingBuffer>,
    pub snapshot: RwSignal<Option<QueryResult>>,
    pub lagged: RwSignal<Option<u64>>,
    /// Search displays failures; callers without a status view may omit it.
    pub failure: Option<RwSignal<Option<&'static str>>>,
    /// Aggregation frames accepted since the stream opened — the
    /// footer's `Updates` count. Bumped only for a frame that passed
    /// validation, so a malformed one raises the failure without
    /// claiming an update. Callers without a count may omit it.
    pub frames: Option<RwSignal<u64>>,
    /// Records that this stream opened. Callers that never ask may omit it.
    pub opened: Option<OpenedMark>,
}

/// Open an SSE stream for the given query, wiring its three event types
/// into the provided signals. Returns `None` if the browser doesn't
/// expose `EventSource` (shouldn't happen in modern browsers) or the
/// query string is empty.
pub fn start_stream(query: &str, signals: LiveSignals) -> Option<StreamLifecycle> {
    if query.trim().is_empty() {
        return None;
    }
    let url = format!(
        "/api/v1/stream?query={}",
        js_sys::encode_uri_component(query)
            .as_string()
            .unwrap_or_default()
    );

    let source = EventSource::new(&url).ok()?;

    // Wire each named SSE event to a dedicated closure. Using `FnMut`
    // gives the callback a mutable environment without `Cell`-dance on
    // the hot path.
    let LiveSignals {
        ring,
        snapshot,
        lagged,
        failure,
        opened,
        ..
    } = signals;
    let mark_opened = move || {
        if let Some(opened) = opened {
            opened.mark();
        }
    };

    // Lagged signal auto-clear: set(Some(n)) then schedule a set(None)
    // ~3s later so the badge fades from the UI without the user having
    // to do anything.
    let lagged_clear_guard: Rc<RefCell<Option<gloo_timers::callback::Timeout>>> =
        Rc::new(RefCell::new(None));

    // Store every event immediately in the bounded ring, but notify the view
    // once per frame interval instead of rebuilding its result per SSE event.
    let dirty = Rc::new(Cell::new(false));
    let tick_dirty = Rc::clone(&dirty);
    let render_tick = gloo_timers::callback::Interval::new(16, move || {
        if tick_dirty.replace(false) {
            ring.try_update(|_| {});
        }
    });
    let event_dirty = Rc::clone(&dirty);
    let on_data = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        mark_opened();
        let Some(data_str) = ev.data().as_string() else {
            return;
        };
        let Ok(map) = serde_json::from_str::<serde_json::Map<_, _>>(&data_str) else {
            return;
        };
        if ring.try_update_untracked(|r| r.push(map)).is_some() {
            event_dirty.set(true);
        }
    });

    let on_snapshot = snapshot_listener(signals);

    let on_lagged = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        mark_opened();
        let Some(data_str) = ev.data().as_string() else {
            return;
        };
        let Ok(wire) = serde_json::from_str::<LaggedWire>(&data_str) else {
            return;
        };
        lagged.set(Some(wire.missed));
        // Schedule a clear after 3s. Dropping the previous Timeout
        // cancels it, so rapid-fire lagged events don't stack.
        let lagged_sig = lagged;
        let guard = lagged_clear_guard.clone();
        let timeout = gloo_timers::callback::Timeout::new(3_000, move || {
            lagged_sig.set(None);
        });
        *guard.borrow_mut() = Some(timeout);
    });

    let on_open = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        mark_opened();
        if let Some(failure) = failure {
            failure.set(None);
        }
    });
    let error_source = source.clone();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
        snapshot.set(None);
        if let Some(failure) = failure {
            let message = if error_source.ready_state() == EventSource::CONNECTING {
                "Live stream disconnected. Reconnecting; retry or switch to Snapshot if this persists."
            } else {
                STREAM_UNAVAILABLE
            };
            failure.set(Some(message));
        }
    });

    // Attach listeners. The `Result` carries only an exception thrown by
    // the JS call itself, and the event names are compile-time strings,
    // so there is nothing here to act on.
    let _ = source.add_event_listener_with_callback("data", on_data.as_ref().unchecked_ref());
    let _ =
        source.add_event_listener_with_callback("snapshot", on_snapshot.as_ref().unchecked_ref());
    let _ = source.add_event_listener_with_callback("lagged", on_lagged.as_ref().unchecked_ref());

    let _ = source.add_event_listener_with_callback("open", on_open.as_ref().unchecked_ref());
    let _ = source.add_event_listener_with_callback("error", on_error.as_ref().unchecked_ref());

    Some(StreamLifecycle {
        source,
        on_data,
        on_snapshot,
        on_lagged,
        on_open,
        on_error,
        render_tick: Some(render_tick),
        ring,
        dirty,
    })
}

fn snapshot_listener(signals: LiveSignals) -> Closure<dyn FnMut(MessageEvent)> {
    let LiveSignals {
        snapshot,
        failure,
        frames,
        opened,
        ..
    } = signals;
    Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        // Any frame, readable or not, means the stream opened.
        if let Some(opened) = opened {
            opened.mark();
        }
        let wire = ev
            .data()
            .as_string()
            .and_then(|data| serde_json::from_str::<SnapshotWire>(&data).ok());
        let Some(wire) = wire else {
            snapshot.set(None);
            if let Some(failure) = failure {
                failure.set(Some(
                    "Live stream returned an unreadable snapshot. Retry or switch to Snapshot.",
                ));
            }
            return;
        };
        if (wire.columns.is_empty() && !wire.rows.is_empty())
            || wire
                .columns
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != wire.columns.len()
        {
            snapshot.set(None);
            if let Some(failure) = failure {
                failure.set(Some(
                    "Live stream returned an unreadable snapshot. Retry or switch to Snapshot.",
                ));
            }
            return;
        }
        if let Some(failure) = failure {
            failure.set(None);
        }
        if let Some(frames) = frames {
            frames.update(|n| *n = n.wrapping_add(1));
        }
        // Rehydrate rows in declared column order. Missing keys land as
        // Null (consistent with how the server materializes sparse rows).
        let rows: Vec<Vec<Value>> = wire
            .rows
            .iter()
            .map(|row_map| {
                wire.columns
                    .iter()
                    .map(|col| row_map.get(col).cloned().map_or(Value::Null, json_to_value))
                    .collect()
            })
            .collect();
        let result = QueryResult {
            columns: wire
                .columns
                .into_iter()
                .map(|name| Column { name })
                .collect(),
            rows,
        };
        snapshot.set(Some(result));
    })
}

#[derive(Deserialize)]
struct SnapshotWire {
    columns: Vec<String>,
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct LaggedWire {
    missed: u64,
}
