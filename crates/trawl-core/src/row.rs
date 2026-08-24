// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The typed row the pipeline stages pass between them.
//!
//! Stages used to hand each other `serde_json::Map<String, Value>`, so
//! every computed value crossed a JSON boundary once per stage — and JSON
//! has no spelling for a non-finite double, so `serde_json::Number::
//! from_f64` turned one into NULL. `* | let x = 0/0 | where x == x` kept
//! the row in batch (`NaN = NaN` is TRUE in `DuckDB`, ADR-0011) and
//! dropped it live, for no reason a query author could see.
//!
//! A row now carries [`crate::eval::EvalValue`] cells end to end and JSON
//! appears only at the WIRE, through the two doors here. The wire
//! behaviour is unchanged: a non-finite still serializes as `null`,
//! because that is what both lanes have always sent (`trawl-api`'s
//! `serialize_f64` does the same for the batch path) — the difference is
//! that it now happens ONCE, at the edge, instead of at every stage
//! boundary.
//!
//! [`Row`] is a type ALIAS for the same `BTreeMap` `serde_json::Map` is
//! (this workspace does not enable serde_json's `preserve_order`, checked
//! in `Cargo.lock`: no `indexmap` dependency), so key ordering — and with
//! it SSE frame bytes and result-column order — is preserved by
//! construction rather than by care.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::compare;
use crate::eval::EvalValue;

/// One pipeline row: field name → typed cell.
///
/// An alias, not a newtype: `BTreeMap`'s own `insert`/`remove`/`retain`/
/// `keys` are exactly the operations the stages perform, and the ordering
/// they impose is the ordering the JSON map already had.
pub type Row = BTreeMap<String, EvalValue>;

/// The ONE JSON → row door: the event bus's wire JSON, or a batch result
/// cell, read into typed cells.
///
/// Per-cell reading is `EvalValue::from(&Value)` by IDENTITY — including
/// its lossy arm, where a JSON integer above `i64::MAX` becomes a
/// `Float`. That is what the evaluator already read for such a value, so
/// preserving it keeps this change a re-TYPING and not a re-reading. A
/// JSON object cannot appear here: ingest stringifies nested values
/// before they reach the bus (`envelope::canonicalize`).
#[must_use]
pub fn from_json(event: &Map<String, Value>) -> Row {
    event
        .iter()
        .map(|(key, value)| (key.clone(), EvalValue::from(value)))
        .collect()
}

/// The ONE row → JSON door: the SSE wire, and the batch bridge's
/// compatibility path.
///
/// This is the live lane's single nulling site — `Float(inf)` has no JSON
/// spelling and serializes as `null` — and it is deliberately never
/// called from a stage. A stage that needed JSON would be re-introducing
/// the boundary this module exists to remove.
#[must_use]
pub fn to_json(row: Row) -> Map<String, Value> {
    row.into_iter()
        .map(|(key, cell)| (key, Value::from(cell)))
        .collect()
}

/// The TEXT a cell contributes to an identity or a display: group-by
/// keys, `dedup <field>` keys, `top`/`rare` values, `dc`/`values`
/// members, and the group columns a snapshot row displays.
///
/// One text for identity and for display, deliberately — a group whose
/// KEY says one thing and whose printed cell says another is a bug
/// waiting to be reported. The float arm therefore goes through the one
/// probe-pinned renderer ([`compare::canonical_double_text`],
/// `DuckDB`'s own `CAST(… AS VARCHAR)`), which has two visible
/// consequences:
///
/// - `-0.0` is normalized to `0.0`, so it groups WITH positive zero —
///   the two compare equal ([`compare::double_total_cmp`]), and a key
///   that split them would contradict the comparison;
/// - a NaN renders `nan` (not JSON `null`), so all NaNs land in ONE
///   group, which is what `DuckDB`'s `NaN = NaN` does.
///
/// A JSON `null` cell renders `"null"`, as the JSON `Display` it replaces
/// did; an ABSENT field is the caller's empty string, not this
/// function's.
///
/// Public because the batch tail sorts through it too
/// (`trawl-engine`'s `post_process`), and a second renderer there would
/// be a second answer to "what does this cell say".
#[must_use]
pub fn cell_text(cell: &EvalValue) -> String {
    match cell {
        EvalValue::Null => "null".to_owned(),
        EvalValue::Bool(b) => b.to_string(),
        EvalValue::Int(n) => n.to_string(),
        // EXACT digits, never the double it computes as: a group key and
        // a `dedup` key are identities, and two ids one apart must not
        // collapse into one.
        EvalValue::UInt(n) => n.to_string(),
        EvalValue::Float(f) => {
            let text = compare::canonical_double_text(*f);
            // Only the sign of zero is normalized; `-nan` keeps its sign
            // because `DuckDB` renders it and a pattern matches it.
            if text == "-0.0" {
                "0.0".to_owned()
            } else {
                text
            }
        }
        EvalValue::Str(s) => s.clone(),
        // The instant's own text, NOT the JSON string it becomes on the
        // wire: rendering it through the wire door wrapped it in QUOTE
        // characters, which then landed inside a group key (`stats count()
        // by t` emitted `"2026-01-15 09:00:00"` live against
        // `2026-01-15 09:00:00` in batch) and split a `Timestamp` cell
        // from a `Str` cell holding the same text.
        EvalValue::Timestamp(ts) => crate::eval::timestamp_to_duckdb_text(ts),
        // A list stays JSON text, as it has always been (`values()`
        // produces a string array).
        EvalValue::Array(_) => Value::from(cell.clone()).to_string(),
    }
}

/// The identity of a cell inside a WHOLE-ROW `dedup` key.
///
/// Whole-row dedup used to format each cell through JSON `Display`, which
/// tells a string from a number by its QUOTES: `"1"` and `1` are
/// different keys. Typed cells have no quotes, so the kind is tagged
/// explicitly and the equivalence classes survive the retyping. The key
/// BYTES are new (they are internal to a live dedup's memory), the
/// classes are not — except for the two float rulings [`cell_text`]
/// documents.
///
/// A `Timestamp` tags as a string because that is what it was on the
/// wire: a JSON string in `DuckDB`'s timestamp text.
pub(crate) fn cell_key(cell: &EvalValue) -> String {
    let tag = match cell {
        EvalValue::Null => 'n',
        EvalValue::Bool(_) => 'b',
        EvalValue::Int(_) | EvalValue::UInt(_) | EvalValue::Float(_) => '#',
        EvalValue::Str(_) | EvalValue::Timestamp(_) => 's',
        EvalValue::Array(_) => 'a',
    };
    format!("{tag}:{}", cell_text(cell))
}

/// A `u64` counter as a cell: an integer while it fits, and the
/// unsigned cell past that — which is what the JSON row carried, digits
/// intact.
///
/// `count()`, `count(field)` and `dc()` all produce one. No stream
/// reaches the second arm (it would need `i64::MAX` events), but the
/// counter IS a `u64` and this is the reading that does not invent a
/// rounding.
pub(crate) fn count_value(n: u64) -> EvalValue {
    i64::try_from(n).map_or(EvalValue::UInt(n), EvalValue::Int)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cell_text_renders_each_variant() {
        for (cell, want) in [
            (EvalValue::Null, "null"),
            (EvalValue::Bool(true), "true"),
            (EvalValue::Int(-42), "-42"),
            (EvalValue::Str("nginx".into()), "nginx"),
            (EvalValue::Float(1.5), "1.5"),
            (EvalValue::Float(200.0), "200.0"),
            // DuckDB's rendering, not Rust's: signed, two-digit exponent.
            (EvalValue::Float(1e-7), "1e-07"),
            (EvalValue::Float(f64::INFINITY), "inf"),
            (EvalValue::Float(f64::NEG_INFINITY), "-inf"),
            (EvalValue::Float(f64::NAN), "nan"),
            (EvalValue::Float(-f64::NAN), "-nan"),
            // The ruling: negative zero groups WITH positive zero.
            (EvalValue::Float(-0.0), "0.0"),
            (EvalValue::Float(0.0), "0.0"),
        ] {
            assert_eq!(cell_text(&cell), want, "{cell:?}");
        }
    }

    #[test]
    fn a_timestamp_reads_as_its_own_text_not_as_json() {
        let ts = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let text = crate::eval::timestamp_to_duckdb_text(&ts);
        assert_eq!(cell_text(&EvalValue::Timestamp(ts)), text);
        assert!(!text.contains('"'), "no quote characters: {text:?}");
        // …so an instant and its text are ONE identity, which is what the
        // shared `s:` tag claims.
        assert_eq!(
            cell_key(&EvalValue::Timestamp(ts)),
            cell_key(&EvalValue::Str(text))
        );
    }

    #[test]
    fn cell_key_tags_the_kind_json_quoting_used_to_carry() {
        // A string `1` and an integer `1` were different dedup keys when
        // the key was JSON text (`"1"` vs `1`); they still are.
        assert_ne!(
            cell_key(&EvalValue::Str("1".into())),
            cell_key(&EvalValue::Int(1))
        );
        assert_ne!(
            cell_key(&EvalValue::Null),
            cell_key(&EvalValue::Str("null".into()))
        );
        assert_ne!(
            cell_key(&EvalValue::Bool(true)),
            cell_key(&EvalValue::Str("true".into()))
        );
        // …and equal cells still share one key, NaN included.
        assert_eq!(
            cell_key(&EvalValue::Float(f64::NAN)),
            cell_key(&EvalValue::Float(f64::NAN))
        );
        assert_eq!(
            cell_key(&EvalValue::Float(-0.0)),
            cell_key(&EvalValue::Float(0.0))
        );
    }

    #[test]
    fn count_value_reads_a_counter_as_the_json_round_trip_did() {
        assert_eq!(count_value(0), EvalValue::Int(0));
        assert_eq!(count_value(7), EvalValue::Int(7));
        #[allow(clippy::cast_sign_loss)]
        let max = i64::MAX as u64;
        assert_eq!(count_value(max), EvalValue::Int(i64::MAX));
        assert_eq!(count_value(max + 1), EvalValue::UInt(max + 1));
    }

    /// A number above `i64::MAX` keeps its digits through both doors and
    /// in every identity — the JSON row did, and a rounded double makes
    /// `dedup`, `stats … by` and `dc()` merge ids that differ.
    #[test]
    fn an_unsigned_number_keeps_its_digits() {
        let event = json!({ "request_id": u64::MAX, "near": u64::MAX - 1 });
        let object = event.as_object().unwrap();
        let row = from_json(object);
        assert_eq!(row.get("request_id"), Some(&EvalValue::UInt(u64::MAX)));
        assert_eq!(&to_json(row.clone()), object, "the wire keeps the digits");

        assert_eq!(
            cell_text(&EvalValue::UInt(u64::MAX)),
            "18446744073709551615"
        );
        assert_ne!(
            cell_key(row.get("request_id").unwrap()),
            cell_key(row.get("near").unwrap()),
            "two ids one apart are two keys"
        );
        // …and it is still a NUMBER to a key, not a string.
        assert_ne!(
            cell_key(&EvalValue::UInt(7)),
            cell_key(&EvalValue::Str("7".into()))
        );
    }

    #[test]
    fn json_round_trips_through_both_doors() {
        let event = json!({
            "service": "nginx",
            "status": 200,
            "rate": 1.5,
            "flag": true,
            "missing": null,
            "list": ["a", "b"],
        });
        let object = event.as_object().unwrap();
        let row = from_json(object);
        assert_eq!(&to_json(row), object);
    }

    #[test]
    fn the_wire_door_is_the_one_place_a_special_becomes_null() {
        let mut row = Row::new();
        row.insert("x".into(), EvalValue::Float(f64::INFINITY));
        row.insert("y".into(), EvalValue::Float(f64::NAN));
        row.insert("z".into(), EvalValue::Float(1.5));
        let frame = serde_json::to_string(&to_json(row)).unwrap();
        assert_eq!(frame, r#"{"x":null,"y":null,"z":1.5}"#);
    }
}
