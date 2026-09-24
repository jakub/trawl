// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SSE lane's cell decoder and live ring, kept out of the wasm-only
//! session module.
//!
//! [`stream_session`](super::stream_session) is `wasm32`-gated, so a test
//! written beside it never runs. None of this has a browser dependency,
//! so it lives here and builds on every target. The decoder's rule —
//! which JSON number becomes which [`Value`] — is the same rule the
//! server's executor and `trawl_api`'s deserializer apply, and a decoder
//! that drifts from them shows the reader a different number. The ring
//! and [`ring_to_result`] are here so the filter rail's live lane can be
//! tested against its snapshot lane; the session module re-exports them.
//!
//! On native only the tests call them — the same shape every other
//! pure module in this crate carries.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::VecDeque;

use trawl_api::value::{Column, QueryResult, Value, land_u64};

/// Max raw events retained in the ring — older events roll off.
pub const LIVE_RING_CAPACITY: usize = 5000;

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
    pub(crate) fn push(&mut self, event: serde_json::Map<String, serde_json::Value>) {
        if self.events.len() >= LIVE_RING_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(event);
        self.epoch = self.epoch.wrapping_add(1);
    }
}

/// Convert a JSON value into a `trawl_api::value::Value`.
///
/// A number that fits `i64` is an integer; above that [`land_u64`] gives
/// it the unsigned variant, which is still a number the results table can
/// sort and chart, rather than a rounded double. Anything with a
/// fractional or out-of-range magnitude is a float; a number `serde_json`
/// can describe as none of those is null.
pub(crate) fn json_to_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(u) = n.as_u64() {
                land_u64(u)
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

/// A live event's oversized unsigned reaches the results table intact.
///
/// Ingest keeps such a value as an unsigned integer and the SSE lane
/// re-serializes it as a raw JSON number, so this decoder is the last
/// place it can silently become a float.
#[cfg(test)]
#[test]
fn large_unsigned_is_exact_uint() {
    assert_eq!(
        json_to_value(serde_json::json!(18_446_744_073_709_551_615_u64)),
        Value::UInt(u64::MAX)
    );
    assert_eq!(
        json_to_value(serde_json::json!(9_223_372_036_854_775_808_u64)),
        Value::UInt(9_223_372_036_854_775_808)
    );
    assert_eq!(json_to_value(serde_json::json!(42)), Value::Integer(42));
    assert_eq!(json_to_value(serde_json::json!(-42)), Value::Integer(-42));
    assert_eq!(json_to_value(serde_json::json!(1.5)), Value::Float(1.5));
    assert_eq!(json_to_value(serde_json::json!(null)), Value::Null);
    assert_eq!(json_to_value(serde_json::json!(true)), Value::Boolean(true));
    assert_eq!(
        json_to_value(serde_json::json!("x")),
        Value::String("x".to_owned())
    );
    // An object keeps its stringified shape: the results table has no cell
    // type for one.
    assert_eq!(
        json_to_value(serde_json::json!({"a": 1})),
        Value::String(r#"{"a":1}"#.to_owned())
    );
}
