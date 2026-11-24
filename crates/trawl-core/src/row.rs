// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The typed row the pipeline stages pass between them.
//!
//! A row carries [`crate::eval::EvalValue`] cells end to end, and JSON
//! appears only at the edges, through the two doors here. JSON has no
//! spelling for a non-finite double, so crossing a JSON boundary once per
//! stage would turn NaN into NULL live while batch keeps it.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::compare;
use crate::eval::EvalValue;

pub type Row = BTreeMap<String, EvalValue>;

#[must_use]
pub fn from_json(event: &Map<String, Value>) -> Row {
    event
        .iter()
        .map(|(key, value)| (key.clone(), EvalValue::from(value)))
        .collect()
}

#[must_use]
pub fn to_json(row: Row) -> Map<String, Value> {
    row.into_iter()
        .map(|(key, cell)| (key, Value::from(cell)))
        .collect()
}

#[must_use]
pub fn cell_text(cell: &EvalValue) -> String {
    match cell {
        EvalValue::Null => "null".to_owned(),
        EvalValue::Bool(b) => b.to_string(),
        EvalValue::Int(n) => n.to_string(),
        EvalValue::UInt(n) => n.to_string(),
        EvalValue::Float(f) => {
            let text = compare::canonical_double_text(*f);
            // The two zeros and the two NaN signs compare equal
            // (`compare::double_total_cmp`), so each pair is one key: a signed
            // key would split a group `DuckDB` does not split. The scalar cast
            // domain (`duckdb_double_to_string`) keeps its sign.
            match text.as_str() {
                "-0.0" => "0.0".to_owned(),
                "-nan" => "nan".to_owned(),
                _ => text,
            }
        }
        EvalValue::Str(s) => s.clone(),
        EvalValue::Timestamp(instant) => instant.cast_text(),
        EvalValue::Array(_) => Value::from(cell.clone()).to_string(),
    }
}

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
        assert_eq!(
            cell_key(&EvalValue::Timestamp(instant)),
            cell_key(&EvalValue::Str(text))
        );
        assert_eq!(
            cell_key(&EvalValue::Timestamp(crate::compare::Instant::Infinity)),
            cell_key(&EvalValue::Str("infinity".into()))
        );
    }

    #[test]
    fn cell_key_tags_the_kind_json_quoting_used_to_carry() {
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
        assert_eq!(
            cell_key(&EvalValue::Float(f64::NAN)),
            cell_key(&EvalValue::Float(f64::NAN))
        );
        assert_eq!(
            cell_key(&EvalValue::Float(-0.0)),
            cell_key(&EvalValue::Float(0.0))
        );
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
    fn the_json_door_writes_a_special_as_null() {
        let mut row = Row::new();
        row.insert("x".into(), EvalValue::Float(f64::INFINITY));
        row.insert("y".into(), EvalValue::Float(f64::NAN));
        row.insert("z".into(), EvalValue::Float(1.5));
        let frame = serde_json::to_string(&to_json(row)).unwrap();
        assert_eq!(frame, r#"{"x":null,"y":null,"z":1.5}"#);
    }
}
