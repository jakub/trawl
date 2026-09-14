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

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
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
