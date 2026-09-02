// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The typed row the pipeline stages pass between them.
//!
//! A row carries [`crate::eval::EvalValue`] cells end to end, and JSON
//! appears only at the wire, through the two doors here. Crossing a JSON
//! boundary once per stage would lose every non-finite double — JSON has
//! no spelling for one, so `serde_json::Number::from_f64` yields NULL — and
//! `* | let x = 0/0 | where x == x` would keep the row in batch
//! (`NaN = NaN` is TRUE in `DuckDB`, ADR-0011) while dropping it live, for
//! no reason a query author could see. On the wire a non-finite still
//! serializes as `null`, matching what `trawl-api`'s `serialize_f64` sends
//! on the batch path, but that happens once, at the edge.
//!
//! [`Row`] is a type alias for the same `BTreeMap` `serde_json::Map` is
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

/// The one JSON → row door: the event bus's wire JSON, or a batch result
/// cell, read into typed cells.
///
/// Per-cell reading is `EvalValue::from(&Value)` by identity. A JSON
/// integer above `i64::MAX` becomes `UInt`, keeping its exact digits
/// through the row (identity, egress, keys, `pin_read`) and widening to
/// `Float` only when a numeric expression reads it, so the wire hands back
/// the exact integer it was given. A JSON object cannot appear here:
/// ingest stringifies nested values before they reach the bus
/// (`envelope::canonicalize`).
#[must_use]
pub fn from_json(event: &Map<String, Value>) -> Row {
    event
        .iter()
        .map(|(key, value)| (key.clone(), EvalValue::from(value)))
        .collect()
}

/// The one row → JSON door: the SSE wire, and the batch bridge's
/// compatibility path.
///
/// This is the live lane's single nulling site — `Float(inf)` has no JSON
/// spelling and serializes as `null` — and it is deliberately never
/// called from a stage. A stage that needed JSON would re-introduce the
/// per-stage boundary this module exists to avoid.
#[must_use]
pub fn to_json(row: Row) -> Map<String, Value> {
    row.into_iter()
        .map(|(key, cell)| (key, Value::from(cell)))
        .collect()
}

/// The text a cell contributes to an identity or a display: group-by
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
/// - `-0.0` is normalized to `0.0`, so it groups with positive zero —
///   the two compare equal ([`compare::double_total_cmp`]), and a key
///   that split them would contradict the comparison;
/// - a NaN renders `nan` (not JSON `null`), so all NaNs land in one
///   group, which is what `DuckDB`'s `NaN = NaN` does.
///
/// A JSON `null` cell renders `null`; an absent field is the caller's
/// empty string, not this function's.
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
        // Exact digits, never the double it computes as: a group key and
        // a `dedup` key are identities, and two ids one apart must not
        // collapse into one.
        EvalValue::UInt(n) => n.to_string(),
        EvalValue::Float(f) => {
            let text = compare::canonical_double_text(*f);
            // The identity path normalizes what the order ties: the two
            // zeros compare equal and so do the two NaN signs
            // (`compare::double_total_cmp`), so each pair is one key and
            // one group — a signed key would split a group `DuckDB` does
            // not split, make `dc()` say two and let `dedup` keep both
            // rows.
            //
            // The scalar cast domain keeps its sign:
            // `duckdb_double_to_string` (hence `tostring()`, `concat()`
            // and the DOUBLE pin's glob text) renders `-0.0` and `-nan`,
            // because that is the text the engine prints. Caveat, parked:
            // `DuckDB` displays whichever representative row it kept, so a
            // group key it prints may carry a sign this one does not.
            match text.as_str() {
                "-0.0" => "0.0".to_owned(),
                "-nan" => "nan".to_owned(),
                _ => text,
            }
        }
        EvalValue::Str(s) => s.clone(),
        // The instant's own cast text, not the JSON string it becomes on
        // the wire: the wire door would wrap it in quote characters, which
        // land inside a group key (`stats count() by t` giving
        // `"2026-01-15 09:00:00"` live against `2026-01-15 09:00:00` in
        // batch) and split a `Timestamp` cell from a `Str` cell holding
        // the same text. An infinity renders as its word, so
        // `Timestamp(Infinity)` and `Str("infinity")` share one identity
        // under the `s:` tag — the rule the wire imposes, where this cell
        // is a JSON string.
        EvalValue::Timestamp(instant) => instant.cast_text(),
        // A list stays JSON text (`values()` produces a string array).
        EvalValue::Array(_) => Value::from(cell.clone()).to_string(),
    }
}

/// The identity of a cell inside a whole-row `dedup` key.
///
/// Typed cells carry no quotes, so the kind is tagged explicitly: a string
/// `1` and an integer `1` stay different keys, as they are on the wire
/// where JSON quotes one and not the other. The key bytes are internal to
/// a live dedup's memory; what matters is the equivalence classes, which
/// follow [`cell_text`] — the two float rulings included.
///
/// A `Timestamp` tags as a string because that is what it is on the wire:
/// a JSON string in `DuckDB`'s timestamp text.
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

/// A `u64` counter as a cell: an integer while it fits, and the unsigned
/// cell past that, digits intact.
///
/// `count()`, `count(field)` and `dc()` all produce one. No stream
/// reaches the second arm (it would need `i64::MAX` events), but the
/// counter is a `u64` and this is the reading that does not invent a
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
            // The ruling, twice over: an identity normalizes what the
            // total order TIES, so both NaN signs read `nan` and both
            // zeros read `0.0`. The scalar cast text keeps its sign —
            // that is `duckdb_double_to_string`'s job, not this one.
            (EvalValue::Float(f64::NAN), "nan"),
            (EvalValue::Float(-f64::NAN), "nan"),
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
        let instant = crate::compare::Instant::At(ts);
        let text = crate::eval::timestamp_to_duckdb_text(&ts);
        assert_eq!(cell_text(&EvalValue::Timestamp(instant)), text);
        assert!(!text.contains('"'), "no quote characters: {text:?}");
        // …so an instant and its text are ONE identity, which is what the
        // shared `s:` tag claims.
        assert_eq!(
            cell_key(&EvalValue::Timestamp(instant)),
            cell_key(&EvalValue::Str(text))
        );
        // An INFINITY keys as its word, for the same reason: on the wire
        // this cell was a JSON string, and that is the identity it kept.
        assert_eq!(
            cell_key(&EvalValue::Timestamp(crate::compare::Instant::Infinity)),
            cell_key(&EvalValue::Str("infinity".into()))
        );
    }

    #[test]
    fn cell_key_tags_the_kind_json_quoting_used_to_carry() {
        // A string `1` and an integer `1` are different dedup keys, as
        // they are on the wire (`"1"` vs `1`).
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
        // …and so do the two NaN signs, which the total order ties too.
        assert_eq!(
            cell_key(&EvalValue::Float(-f64::NAN)),
            cell_key(&EvalValue::Float(f64::NAN))
        );
        assert_eq!(cell_text(&EvalValue::Float(-f64::NAN)), "nan");
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
    /// in every identity: a rounded double would make `dedup`,
    /// `stats … by` and `dc()` merge ids that differ.
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

    /// Both doors ORDER by key, whatever order the input arrived in —
    /// the property an SSE frame's bytes and a result-column list rest
    /// on, pinned with deliberately UNSORTED input keys.
    ///
    /// What this does NOT pin, stated so it is not mistaken for it: a
    /// crate added anywhere in the graph can turn on `serde_json`'s
    /// `preserve_order` feature (cargo unifies features) and swap
    /// `Map`'s backing to `IndexMap`. [`to_json`] would survive that —
    /// it collects in [`Row`] order, and `Row` is a `BTreeMap` in its
    /// own right, not `serde_json::Map`'s alias — so this case would
    /// stay green. The case that catches the flip is
    /// `an_sse_frame_keeps_its_pre_change_bytes` in `stage_parity`,
    /// which compares a WIRE map's own serialization against the
    /// round trip: under `preserve_order` the wire side would keep its
    /// insertion order while this side stays sorted.
    #[test]
    fn both_doors_order_by_key_whatever_the_input_order() {
        let event = json!({"zeta": 1, "alpha": 2, "mid": 3});
        let object = event.as_object().unwrap();
        let row = from_json(object);
        assert_eq!(
            row.keys().collect::<Vec<_>>(),
            vec!["alpha", "mid", "zeta"],
            "the row door must order by key"
        );
        assert_eq!(
            serde_json::to_string(&to_json(row)).unwrap(),
            r#"{"alpha":2,"mid":3,"zeta":1}"#,
            "the wire door must order by key"
        );
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
