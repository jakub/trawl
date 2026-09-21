// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SSE lane's cell decoder, kept out of the wasm-only session module.
//!
//! [`stream_session`](super::stream_session) is `wasm32`-gated, so a test
//! written beside it never runs. This function has no browser dependency,
//! so it lives here and builds on every target: the rule it applies —
//! which JSON number becomes which [`Value`] — is the same rule the
//! server's executor and `trawl_api`'s deserializer apply, and a decoder
//! that drifts from them shows the reader a different number.
//!
//! On native only the test below calls it — the same shape every other
//! pure module in this crate carries.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::value::{Value, land_u64};

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

/// A live event's oversized unsigned reaches the results table intact.
///
/// Ingest keeps such a value as an unsigned integer and the SSE lane
/// re-serializes it as a raw JSON number, so this decoder is the last
/// place it can silently become a float.
#[cfg(test)]
#[test]
fn large_unsigned_is_exact_string() {
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
