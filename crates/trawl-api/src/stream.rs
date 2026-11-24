// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for the live tail, `GET /api/v1/stream`.
//!
//! Both result frames speak the result wire of [`crate::value`]: typed
//! columns and positional rows. A pass-through stream sends each event as
//! a `data` frame, a one-row [`QueryResult`] whose columns are the fields
//! that event carries. An aggregate stream answers with `snapshot` frames
//! ([`SnapshotFrame`]), each of which replaces the whole result. A
//! snapshot also carries `capped` (ADR-0001).

use serde::{Deserialize, Serialize};

use crate::value::QueryResult;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotFrame {
    #[serde(flatten)]
    pub result: QueryResult,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capped: Vec<SnapshotCap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCapKind {
    Groups,
    DistinctValues,
    ExactValues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotCap {
    pub cap: SnapshotCapKind,
    pub limit: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Column, ColumnType, Value};

    #[test]
    fn snapshot_frame_wire_spelling() {
        let frame = SnapshotFrame {
            result: QueryResult {
                columns: vec![
                    Column::new("host", ColumnType::String),
                    Column::new("total", ColumnType::Integer),
                ],
                rows: vec![vec![
                    Value::String("web-1".to_owned()),
                    Value::UInt(u64::MAX),
                ]],
            },
            capped: Vec::new(),
        };
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "columns": [
                    {"name": "host", "type": "string"},
                    {"name": "total", "type": "integer"},
                ],
                "rows": [["web-1", "18446744073709551615"]],
            })
        );
        let back: SnapshotFrame = serde_json::from_str(&wire.to_string()).unwrap();
        assert_eq!(back, frame);

        let capped = SnapshotFrame {
            capped: vec![SnapshotCap {
                cap: SnapshotCapKind::Groups,
                limit: 10_000,
            }],
            ..frame
        };
        let wire = serde_json::to_string(&capped).unwrap();
        assert!(
            wire.ends_with(r#""capped":[{"cap":"groups","limit":10000}]}"#),
            "{wire}"
        );
        assert_eq!(
            serde_json::from_str::<SnapshotFrame>(&wire).unwrap(),
            capped
        );
    }

    #[test]
    fn snapshot_frame_refuses_a_cell_its_column_refuses() {
        let err = serde_json::from_str::<SnapshotFrame>(
            r#"{"columns":[{"name":"total","type":"integer"}],"rows":[[18446744073709551615]]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(r#"column "total""#), "{err}");
    }

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
