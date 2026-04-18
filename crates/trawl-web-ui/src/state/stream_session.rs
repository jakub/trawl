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
//! If the browser permanently loses the stream, the UI freezes on the
//! last snapshot — acceptable for v1.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use leptos::prelude::*;
use serde::Deserialize;
use trawl_api::value::{Column, QueryResult, Value};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{EventSource, MessageEvent};

/// Max raw events retained in the ring — older events roll off.
pub const LIVE_RING_CAPACITY: usize = 5000;

/// Bundle of the `EventSource` handle + all its registered closures.
/// Dropping this calls `close()` on the `EventSource` first (so no more
/// callbacks fire) and then releases the closures.
#[allow(dead_code)] // fields held for drop ordering, not read
pub struct StreamLifecycle {
    source: EventSource,
    on_data: Closure<dyn FnMut(MessageEvent)>,
    on_snapshot: Closure<dyn FnMut(MessageEvent)>,
    on_lagged: Closure<dyn FnMut(MessageEvent)>,
}

impl Drop for StreamLifecycle {
    fn drop(&mut self) {
        self.source.close();
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
}

/// Bounded, append-only-from-the-tail ring of raw events.
#[derive(Clone, Default)]
pub struct RingBuffer {
    /// Insertion-ordered ring of event objects.
    pub events: VecDeque<serde_json::Map<String, serde_json::Value>>,
    /// Monotonic counter — bumped on every push; lets `Memo`s key off a
    /// cheap `u64` instead of cloning the `VecDeque` for change detection.
    pub epoch: u64,
}

impl RingBuffer {
    fn push(&mut self, event: serde_json::Map<String, serde_json::Value>) {
        if self.events.len() >= LIVE_RING_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(event);
        self.epoch = self.epoch.wrapping_add(1);
    }
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
    } = signals;

    // Lagged signal auto-clear: set(Some(n)) then schedule a set(None)
    // ~3s later so the badge fades from the UI without the user having
    // to do anything.
    let lagged_clear_guard: Rc<RefCell<Option<gloo_timers::callback::Timeout>>> =
        Rc::new(RefCell::new(None));

    let on_data = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let Some(data_str) = ev.data().as_string() else {
            return;
        };
        let Ok(map) = serde_json::from_str::<serde_json::Map<_, _>>(&data_str) else {
            return;
        };
        ring.update(|r| r.push(map));
    });

    let on_snapshot = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let Some(data_str) = ev.data().as_string() else {
            return;
        };
        let Ok(wire) = serde_json::from_str::<SnapshotWire>(&data_str) else {
            return;
        };
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
    });

    let on_lagged = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
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

    // Attach listeners. `add_event_listener_with_callback` returns
    // Result but the only failure mode is invalid UTF-8 event names,
    // which are compile-time strings here.
    let _ = source.add_event_listener_with_callback("data", on_data.as_ref().unchecked_ref());
    let _ =
        source.add_event_listener_with_callback("snapshot", on_snapshot.as_ref().unchecked_ref());
    let _ = source.add_event_listener_with_callback("lagged", on_lagged.as_ref().unchecked_ref());

    Some(StreamLifecycle {
        source,
        on_data,
        on_snapshot,
        on_lagged,
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

/// Convert a JSON value into a `trawl_api::value::Value`.
fn json_to_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(s),
        other => Value::String(other.to_string()),
    }
}

/// Convert a ring buffer into a `QueryResult` suitable for
/// rendering via `<ResultsTable/>`. Column order is first-seen stable.
#[must_use]
pub fn ring_to_result(ring: &RingBuffer) -> QueryResult {
    // Collect column names in first-seen order.
    let mut col_order: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ev in &ring.events {
        for k in ev.keys() {
            if seen.insert(k.clone()) {
                col_order.push(k.clone());
            }
        }
    }

    let rows: Vec<Vec<Value>> = ring
        .events
        .iter()
        .map(|ev| {
            col_order
                .iter()
                .map(|col| ev.get(col).cloned().map_or(Value::Null, json_to_value))
                .collect()
        })
        .collect();

    QueryResult {
        columns: col_order.into_iter().map(|name| Column { name }).collect(),
        rows,
    }
}
