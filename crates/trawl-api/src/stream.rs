// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for the live tail, `GET /api/v1/stream`.
//!
//! An aggregate stream answers with `snapshot` events, each of which
//! replaces the whole result: `{"columns":[...],"rows":[...]}`. A snapshot
//! whose aggregation dropped input past a cap also carries `capped`, one
//! [`SnapshotCap`] per cap that dropped something since the stream started
//! (ADR-0001). The field is omitted when nothing was dropped, and reaching
//! a cap exactly drops nothing. A cap stays on every later snapshot of the
//! stream, because the dropped input never comes back.

use serde::{Deserialize, Serialize};

/// Which bound of a live aggregation dropped input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCapKind {
    /// A new group arrived while the group map was full. A `timechart`
    /// holds one map per bucket.
    Groups,
    /// A `dc()` or `values()` accumulator was full and the value was unseen.
    DistinctValues,
    /// A `median` or percentile accumulator was full and the event carried a
    /// number.
    ExactValues,
}

/// One cap a snapshot's aggregation passed, with the bound it passed.
///
/// The limit travels with the name so a client never hard-codes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotCap {
    /// The cap that dropped input.
    pub cap: SnapshotCapKind,
    /// Its bound: groups per aggregation (per bucket for a `timechart`), or
    /// values per accumulator.
    pub limit: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_cap_wire_spelling() {
        let caps = vec![
            SnapshotCap {
                cap: SnapshotCapKind::Groups,
                limit: 10_000,
            },
            SnapshotCap {
                cap: SnapshotCapKind::DistinctValues,
                limit: 10_000,
            },
            SnapshotCap {
                cap: SnapshotCapKind::ExactValues,
                limit: 100_000,
            },
        ];
        let wire = serde_json::to_value(&caps).unwrap();
        assert_eq!(
            wire,
            serde_json::json!([
                {"cap": "groups", "limit": 10_000},
                {"cap": "distinct_values", "limit": 10_000},
                {"cap": "exact_values", "limit": 100_000},
            ])
        );
        let back: Vec<SnapshotCap> = serde_json::from_value(wire).unwrap();
        assert_eq!(back, caps);
    }
}
