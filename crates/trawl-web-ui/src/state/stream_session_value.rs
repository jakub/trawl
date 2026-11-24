// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SSE lane's live ring, kept out of the wasm-only session module.
//!
//! On native only the tests call them.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::{HashMap, VecDeque};

use trawl_api::value::{Column, ColumnType, QueryResult, TypeFold, Value};

pub const LIVE_RING_CAPACITY: usize = 5000;

#[derive(Clone, Default)]
pub struct RingBuffer {
    pub events: VecDeque<QueryResult>,
    /// Monotonic counter — bumped on every push; lets `Memo`s key off a
    /// cheap `u64` instead of cloning the `VecDeque` for change detection.
    pub epoch: u64,
}

impl RingBuffer {
    pub(crate) fn push(&mut self, event: QueryResult) {
        if self.events.len() >= LIVE_RING_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(event);
        self.epoch = self.epoch.wrapping_add(1);
    }
}

#[must_use]
pub fn event_field<'e>(event: &'e QueryResult, name: &str) -> Option<&'e Value> {
    let idx = event.columns.iter().position(|c| c.name == name)?;
    event.rows.first()?.get(idx)
}

/// An event frame as the plain JSON object a reader expects to see:
/// field → value, numbers as JSON numbers.
#[must_use]
pub fn event_object(event: &QueryResult) -> serde_json::Map<String, serde_json::Value> {
    let Some(cells) = event.rows.first() else {
        return serde_json::Map::new();
    };
    event
        .columns
        .iter()
        .zip(cells)
        .map(|(column, cell)| (column.name.clone(), cell.to_plain_json()))
        .collect()
}

/// Each column merges the types its frames gave it, over the cells that
/// hold a value ([`TypeFold::observe_typed`]): one type stays, frames that
/// disagree make the column `mixed`, and a null cell (which its one-row
/// frame types `string` for want of evidence) adds nothing. A column no
/// frame gave a value is `string`.
#[must_use]
pub fn ring_to_result(ring: &RingBuffer) -> QueryResult {
    let mut names: Vec<&str> = Vec::new();
    let mut places: HashMap<&str, usize> = HashMap::new();
    for frame in &ring.events {
        for column in &frame.columns {
            places.entry(column.name.as_str()).or_insert_with(|| {
                names.push(column.name.as_str());
                names.len() - 1
            });
        }
    }

    let mut folds = vec![TypeFold::new(); names.len()];
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for frame in &ring.events {
        for cells in &frame.rows {
            let mut row = vec![Value::Null; names.len()];
            for (column, cell) in frame.columns.iter().zip(cells) {
                let place = places[column.name.as_str()];
                folds[place].observe_typed(&column.ty, cell);
                row[place].clone_from(cell);
            }
            rows.push(row);
        }
    }

    let columns = names
        .into_iter()
        .zip(folds)
        .map(|(name, fold)| Column::new(name, fold.finish().unwrap_or(ColumnType::String)))
        .collect();
    QueryResult { columns, rows }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::String(s.to_owned())
    }

    fn frame(wire: &serde_json::Value) -> QueryResult {
        serde_json::from_value(wire.clone()).expect("a valid data frame")
    }

    #[test]
    fn large_unsigned_is_exact_uint() {
        let mut ring = RingBuffer::default();
        ring.push(frame(&serde_json::json!({
            "columns": [{"name": "request_id", "type": "integer"}],
            "rows": [["18446744073709551615"]],
        })));
        let result = ring_to_result(&ring);
        assert_eq!(
            result.columns,
            vec![Column::new("request_id", ColumnType::Integer)]
        );
        assert_eq!(result.rows, vec![vec![Value::UInt(u64::MAX)]]);
    }

    #[test]
    fn frames_merge_by_name() {
        let mut ring = RingBuffer::default();
        ring.push(frame(&serde_json::json!({
            "columns": [
                {"name": "_time", "type": "timestamp"},
                {"name": "code", "type": "integer"},
                {"name": "tag", "type": "string"},
            ],
            "rows": [["2026-01-15T09:00:00.000000Z", "404", null]],
        })));
        ring.push(frame(&serde_json::json!({
            "columns": [
                {"name": "_time", "type": "timestamp"},
                {"name": "code", "type": "string"},
                {"name": "tag", "type": "float"},
                {"name": "late", "type": "boolean"},
            ],
            "rows": [["2026-01-15T09:00:01.000000Z", "E_TIMEOUT", 1.5, true]],
        })));

        let result = ring_to_result(&ring);
        assert_eq!(
            result.columns,
            vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("code", ColumnType::Mixed),
                Column::new("tag", ColumnType::Float),
                Column::new("late", ColumnType::Boolean),
            ]
        );
        assert_eq!(
            result.rows,
            vec![
                vec![
                    text("2026-01-15T09:00:00.000000Z"),
                    Value::Integer(404),
                    Value::Null,
                    Value::Null,
                ],
                vec![
                    text("2026-01-15T09:00:01.000000Z"),
                    text("E_TIMEOUT"),
                    Value::Float(1.5),
                    Value::Boolean(true),
                ],
            ]
        );
        let wire = serde_json::to_string(&result).expect("the merge encodes");
        assert_eq!(serde_json::from_str::<QueryResult>(&wire).unwrap(), result);
    }

    #[test]
    fn an_all_null_column_is_string() {
        let mut ring = RingBuffer::default();
        for _ in 0..2 {
            ring.push(frame(&serde_json::json!({
                "columns": [{"name": "gone", "type": "string"}],
                "rows": [[null]],
            })));
        }
        assert_eq!(
            ring_to_result(&ring).columns,
            vec![Column::new("gone", ColumnType::String)]
        );
    }

    #[test]
    fn event_fields_read_by_name() {
        let event = frame(&serde_json::json!({
            "columns": [
                {"name": "_time", "type": "timestamp"},
                {"name": "status", "type": "integer"},
            ],
            "rows": [["2026-01-15T09:00:00.000000Z", "18446744073709551615"]],
        }));
        assert_eq!(
            event_field(&event, "_time"),
            Some(&text("2026-01-15T09:00:00.000000Z"))
        );
        assert_eq!(event_field(&event, "message"), None);
        assert_eq!(
            serde_json::to_string(&event_object(&event)).unwrap(),
            r#"{"_time":"2026-01-15T09:00:00.000000Z","status":18446744073709551615}"#
        );
    }
}
