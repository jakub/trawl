// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure layout decisions for the snapshot results table's reading modes.
//!
//! At the crate root rather than under `components/`, which is wasm-gated:
//! these answers are asked on every render of the results table, and
//! wrong ones address the wrong event, so their tests run on native
//! `cargo nextest run -p trawl-web-ui`.
//!
//! On native, only the tests consume these items — `#[allow(dead_code)]`
//! at module scope silences the bin-crate dead-code warning. Matches the
//! `facets.rs` / `query_merge.rs` pattern.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::value::Value;

/// One event's detail rows, split for the null-field disclosure: column
/// indices, each list in wire order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DetailFields {
    /// Fields with a value, shown while the disclosure is closed.
    pub shown: Vec<usize>,
    /// Null fields, folded behind "Show N null fields".
    pub null: Vec<usize>,
}

impl DetailFields {
    /// The rows to render, in wire order: the fields with a value while
    /// the disclosure is closed, every field in its place once it opens.
    #[must_use]
    pub fn visible(&self, open: bool) -> Vec<usize> {
        if open {
            (0..self.shown.len() + self.null.len()).collect()
        } else {
            self.shown.clone()
        }
    }
}

/// The null-field disclosure's label: "Show 3 null fields", "Hide 1 null
/// field".
#[must_use]
pub fn null_fields_label(open: bool, count: usize) -> String {
    let verb = if open { "Hide" } else { "Show" };
    let noun = if count == 1 { "field" } else { "fields" };
    format!("{verb} {count} null {noun}")
}

/// Partition a row's cells into the fields it holds and its null fields.
///
/// Both detail presentations (the inline expansion and the docked
/// inspector) ask here, so they agree on N. Only [`Value::Null`] is a
/// null field: an empty string, `0`, `false`, an empty array and the
/// text `"NULL"` are values the event carries. The display text is never
/// consulted, because it cannot tell `"NULL"` the string from a NULL.
#[must_use]
pub fn partition_detail_fields(row: &[Value]) -> DetailFields {
    let mut out = DetailFields::default();
    for (i, cell) in row.iter().enumerate() {
        if matches!(cell, Value::Null) {
            out.null.push(i);
        } else {
            out.shown.push(i);
        }
    }
    out
}

/// The row index the docked inspector describes, if any.
///
/// A selection is a `(generation, original row index)` pair, and this is
/// the arbitration between the two halves. The index alone is not enough:
/// a new response, a page turn or a new effective query bumps the
/// generation, and index 2 of the page that just arrived is a different
/// event from index 2 of the page that was on screen when the reader
/// clicked. A stale generation reads as "nothing selected", which closes
/// the inspector rather than repointing it.
#[must_use]
pub fn inspector_selection(generation: u64, selected: Option<(u64, usize)>) -> Option<usize> {
    selected
        .filter(|&(generation_id, _)| generation_id == generation)
        .map(|(_, idx)| idx)
}

/// The columns a message-first row shows, by their ORIGINAL index into
/// the response's column list.
///
/// Everything not named here is still in the event — it is read in the
/// inline detail or the docked inspector — but it is off the row, which
/// is the whole point of the mode: one wide message instead of a dozen
/// truncated cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFirst {
    pub time: Option<usize>,
    pub severity: Option<usize>,
    pub message: usize,
    pub meta: Vec<usize>,
}

impl MessageFirst {
    /// The original column indices this layout renders, left to right:
    /// time, severity, message. The header row and the data cells read
    /// the same list, so the two cannot drift apart.
    #[must_use]
    pub fn header_indices(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(3);
        out.extend(self.time);
        out.extend(self.severity);
        out.push(self.message);
        out
    }
}

/// The columns `message` / `msg` may be called, lower-cased.
const MESSAGE_NAMES: [&str; 2] = ["message", "msg"];

/// The secondary line's fields, in the order they are read.
const META_NAMES: [&str; 3] = ["service", "host", "latency_ms"];

/// Plan a message-first layout for these columns, or `None` when the
/// response has no message to lead with.
///
/// `None` is not a failure: the table falls back to the compact column
/// layout, which is the only honest rendering of a result whose rows are
/// not events. Aggregation-shaped results take that path for the same
/// reason, and never reach here at all.
#[must_use]
pub fn message_first(columns: &[String], severity_cols: &[usize]) -> Option<MessageFirst> {
    let message = columns
        .iter()
        .position(|c| MESSAGE_NAMES.contains(&c.to_ascii_lowercase().as_str()))?;
    let time = columns.iter().position(|c| c == "_time");
    // The response declares which columns render as severity; the first
    // is the one the row shows as its pill.
    let severity = severity_cols.first().copied();
    let meta = META_NAMES
        .iter()
        .filter_map(|want| columns.iter().position(|c| c == want))
        .collect();
    Some(MessageFirst {
        time,
        severity,
        message,
        meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_detail_fields_splits_only_null_preserving_order() {
        let row = vec![
            Value::String("2026-01-01T00:00:00Z".into()),
            Value::Null,
            Value::String(String::new()),
            Value::Integer(0),
            Value::Null,
            Value::Boolean(false),
            Value::Array(vec![]),
            Value::String("NULL".into()),
            Value::Float(0.0),
            Value::Null,
        ];
        assert_eq!(
            partition_detail_fields(&row),
            DetailFields {
                shown: vec![0, 2, 3, 5, 6, 7, 8],
                null: vec![1, 4, 9],
            }
        );
        let parts = partition_detail_fields(&row);
        assert_eq!(parts.visible(false), vec![0, 2, 3, 5, 6, 7, 8]);
        assert_eq!(parts.visible(true), (0..row.len()).collect::<Vec<_>>());
        assert_eq!(partition_detail_fields(&[]), DetailFields::default());
        assert!(
            partition_detail_fields(&[Value::Integer(1), Value::UInt(2)])
                .null
                .is_empty()
        );
    }

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn null_fields_label_counts_and_names_the_action() {
        assert_eq!(null_fields_label(false, 3), "Show 3 null fields");
        assert_eq!(null_fields_label(true, 3), "Hide 3 null fields");
        assert_eq!(null_fields_label(false, 1), "Show 1 null field");
        assert_eq!(null_fields_label(true, 1), "Hide 1 null field");
    }

    #[test]
    fn message_first_needs_a_message_column() {
        // No message to lead with: the table stays compact rather than
        // promoting an arbitrary column to the wide slot.
        assert_eq!(
            message_first(&cols(&["_time", "host", "status"]), &[]),
            None
        );
        assert_eq!(message_first(&[], &[]), None);
    }

    #[test]
    fn message_first_accepts_the_msg_alias_in_any_case() {
        for name in ["message", "msg", "Message", "MSG"] {
            let plan = message_first(&cols(&["_time", name]), &[]).expect(name);
            assert_eq!(plan.message, 1, "{name}");
        }
    }

    #[test]
    fn message_first_maps_time_and_the_first_severity_column() {
        let plan = message_first(&cols(&["_time", "_severity", "message"]), &[1, 2]).unwrap();
        assert_eq!(plan.time, Some(0));
        assert_eq!(plan.severity, Some(1), "the FIRST declared severity column");
        assert_eq!(plan.message, 2);
    }

    #[test]
    fn message_first_leaves_time_and_severity_unset_when_absent() {
        let plan = message_first(&cols(&["host", "message"]), &[]).unwrap();
        assert_eq!(plan.time, None);
        assert_eq!(plan.severity, None);
        assert_eq!(plan.message, 1);
    }

    #[test]
    fn header_indices_read_time_then_severity_then_message() {
        let full = message_first(&cols(&["_time", "_severity", "message"]), &[1, 2]).unwrap();
        assert_eq!(full.header_indices(), vec![0, 1, 2]);

        // The corpus shape: a time and a message, no declared severity.
        let plain = message_first(&cols(&["_time", "host", "status", "message"]), &[]).unwrap();
        assert_eq!(plain.header_indices(), vec![0, 3]);
    }

    #[test]
    fn message_first_meta_is_the_subset_that_exists_in_a_fixed_order() {
        // Declaration order in the response is irrelevant: the secondary
        // line always reads service, host, latency.
        let plan = message_first(
            &cols(&["message", "latency_ms", "host", "status", "service"]),
            &[],
        )
        .unwrap();
        assert_eq!(plan.meta, vec![4, 2, 1]);

        let partial = message_first(&cols(&["message", "host"]), &[]).unwrap();
        assert_eq!(partial.meta, vec![1]);

        let none = message_first(&cols(&["message", "status"]), &[]).unwrap();
        assert!(none.meta.is_empty());
    }

    #[test]
    fn inspector_selection_is_none_without_a_selection() {
        assert_eq!(inspector_selection(7, None), None);
    }

    #[test]
    fn inspector_selection_keeps_the_index_on_a_matching_generation() {
        assert_eq!(inspector_selection(7, Some((7, 3))), Some(3));
        assert_eq!(inspector_selection(0, Some((0, 0))), Some(0));
    }

    #[test]
    fn inspector_selection_drops_a_stale_generation() {
        // The row the reader clicked is gone; index 3 of the new page is
        // a different event, so the inspector closes instead of moving.
        assert_eq!(inspector_selection(8, Some((7, 3))), None);
        assert_eq!(inspector_selection(7, Some((8, 3))), None);
    }
}
