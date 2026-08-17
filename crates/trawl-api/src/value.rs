// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Result types for query execution.
//!
//! These types are deliberately decoupled from `DuckDB`'s internal types
//! to keep the public API stable across duckdb crate version changes.
//!
//! [`Value`] uses custom serde impls to serialize as JSON primitives
//! (not tagged enums), so these types double as the HTTP wire format.

use std::cmp::Ordering;
use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A cell value from a query result row.
///
/// Serializes as a JSON primitive: `null`, `true`, `42`, `3.14`, `"hello"`.
///
/// Integers are `i64`; unsigned values exceeding `i64::MAX` promote to `f64`
/// with potential precision loss beyond 2^53. JSON arrays/objects encountered
/// during deserialization are stringified (these don't occur in normal
/// `DuckDB` result sets).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Boolean(b) => serializer.serialize_bool(*b),
            Self::Integer(i) => serializer.serialize_i64(*i),
            Self::Float(f) => serializer.serialize_f64(*f),
            Self::String(s) => serializer.serialize_str(s),
            Self::Array(arr) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(arr.len()))?;
                for val in arr {
                    seq.serialize_element(val)?;
                }
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON primitive (null, bool, number, or string)")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Boolean(v))
    }

    fn visit_i8<E: de::Error>(self, v: i8) -> Result<Value, E> {
        Ok(Value::Integer(i64::from(v)))
    }

    fn visit_i16<E: de::Error>(self, v: i16) -> Result<Value, E> {
        Ok(Value::Integer(i64::from(v)))
    }

    fn visit_i32<E: de::Error>(self, v: i32) -> Result<Value, E> {
        Ok(Value::Integer(i64::from(v)))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Integer(v))
    }

    fn visit_u8<E: de::Error>(self, v: u8) -> Result<Value, E> {
        Ok(Value::Integer(i64::from(v)))
    }

    fn visit_u16<E: de::Error>(self, v: u16) -> Result<Value, E> {
        Ok(Value::Integer(i64::from(v)))
    }

    fn visit_u32<E: de::Error>(self, v: u32) -> Result<Value, E> {
        Ok(Value::Integer(i64::from(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        // u64 values that fit in i64 stay as integers; others promote to float.
        if let Ok(i) = i64::try_from(v) {
            Ok(Value::Integer(i))
        } else {
            #[allow(clippy::cast_precision_loss)]
            Ok(Value::Float(v as f64))
        }
    }

    fn visit_f32<E: de::Error>(self, v: f32) -> Result<Value, E> {
        Ok(Value::Float(f64::from(v)))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Value::Float(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(val) = seq.next_element::<Value>()? {
            values.push(val);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Value, A::Error> {
        let json: serde_json::Value =
            Deserialize::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(Value::String(json.to_string()))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => Ok(()),
            Self::Boolean(b) => write!(f, "{b}"),
            Self::Integer(i) => write!(f, "{i}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::String(s) => write!(f, "{s}"),
            Self::Array(arr) => {
                let items: Vec<std::string::String> = arr.iter().map(ToString::to_string).collect();
                write!(f, "[{}]", items.join(", "))
            }
        }
    }
}

/// Fields that appear first in reordered query results (ADR-0009 envelope).
///
/// When a query has no explicit column selection (`table`/`fields`) and no
/// aggregation (`stats`/`top`/etc.), columns are reordered so these appear
/// first in this order, followed by remaining columns in their original
/// order, with [`TRAILING_LOG_FIELDS`] demoted to the very end.
///
/// Kept in sync with `trawl_core::schema::LEADING_LOG_FIELDS` (duplicated
/// because trawl-api does not depend on trawl-core; a trawl-engine test
/// asserts parity).
pub const WELL_KNOWN_LOG_FIELDS: &[&str] =
    &["_time", "env", "service", "host", "_severity", "message"];

/// Envelope metadata columns demoted to the end of reordered results.
///
/// Kept in sync with `trawl_core::schema::TRAILING_LOG_FIELDS`.
pub const TRAILING_LOG_FIELDS: &[&str] = &["_raw", "_ingested", "_repairs", "_producer"];

/// Display rank for a field name, mirroring query-result column order:
/// `0` = leading envelope field ([`WELL_KNOWN_LOG_FIELDS`], in declared
/// order), `1` = custom field, `2` = trailing metadata
/// ([`TRAILING_LOG_FIELDS`], in declared order).
///
/// Callers do not sort on this directly — [`sort_by_display_rank`] owns the
/// `(rank, name)` ordering every schema listing shares.
#[must_use]
pub fn field_display_rank(name: &str) -> (u8, usize) {
    if let Some(i) = WELL_KNOWN_LOG_FIELDS.iter().position(|f| *f == name) {
        (0, i)
    } else if let Some(i) = TRAILING_LOG_FIELDS.iter().position(|f| *f == name) {
        (2, i)
    } else {
        (1, 0)
    }
}

/// Compare two field names in schema display order: [`field_display_rank`]
/// first, name as the tie-break.
#[must_use]
pub fn cmp_by_display_rank(a: &str, b: &str) -> Ordering {
    field_display_rank(a)
        .cmp(&field_display_rank(b))
        .then_with(|| a.cmp(b))
}

/// Sort a schema listing into display order, keying each item by its field
/// name via `name`.
///
/// The single home of this ordering, so the `/api/v1/schema` columns,
/// `/api/v1/schema/fields` rows, `/api/v1/schema/services` columns, and the
/// CLI field tables can never drift apart: they present columns the way query
/// results order them, instead of a raw alphabetical sort that would put
/// `_ingested` first.
pub fn sort_by_display_rank<T>(items: &mut [T], name: impl Fn(&T) -> &str) {
    items.sort_by(|a, b| cmp_by_display_rank(name(a), name(b)));
}

/// Column metadata from a query result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
}

/// The complete result of a query execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    /// An empty result set with no columns or rows.
    ///
    /// Used when source narrowing yields zero matching files — semantically
    /// equivalent to "no matching data".
    #[must_use]
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// Number of result rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Whether the result set is empty.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Reorder columns for log display: [`WELL_KNOWN_LOG_FIELDS`] first,
    /// [`TRAILING_LOG_FIELDS`] last, everything else in between in its
    /// original order.
    pub fn reorder_log_columns(&mut self) {
        self.reorder_columns_full(WELL_KNOWN_LOG_FIELDS, TRAILING_LOG_FIELDS);
    }

    /// Reorder columns so `preferred` field names appear first.
    ///
    /// Fields in `preferred` that exist in the result are moved to the front
    /// (in the order they appear in `preferred`), followed by all remaining
    /// columns in their original order. Missing fields are silently skipped.
    ///
    /// Both `columns` and every row in `rows` are permuted together.
    pub fn reorder_columns(&mut self, preferred: &[&str]) {
        self.reorder_columns_full(preferred, &[]);
    }

    /// Reorder columns: `preferred` first, `trailing` demoted to the end,
    /// remaining columns in their original order between them.
    pub fn reorder_columns_full(&mut self, preferred: &[&str], trailing: &[&str]) {
        if self.columns.is_empty() {
            return;
        }

        let mut order: Vec<usize> = Vec::with_capacity(self.columns.len());
        let mut used = vec![false; self.columns.len()];

        for &pref in preferred {
            if let Some(idx) = self.columns.iter().position(|c| c.name == pref)
                && !used[idx]
            {
                order.push(idx);
                used[idx] = true;
            }
        }
        for (i, &u) in used.iter().enumerate() {
            if !u && !trailing.contains(&self.columns[i].name.as_str()) {
                order.push(i);
            }
        }
        for &tr in trailing {
            if let Some(idx) = self.columns.iter().position(|c| c.name == tr)
                && !used[idx]
            {
                order.push(idx);
                used[idx] = true;
            }
        }

        // Skip allocation if already in order.
        if order.iter().enumerate().all(|(new, &old)| new == old) {
            return;
        }

        self.columns = order.iter().map(|&i| self.columns[i].clone()).collect();
        for row in &mut self.rows {
            let orig = std::mem::take(row);
            *row = order.iter().map(|&i| orig[i].clone()).collect();
        }
    }

    /// Apply offset/limit pagination to result rows (post-executor).
    ///
    /// This is a convenience method for client-side pagination. If `offset`
    /// exceeds the number of rows, the result will be empty. The `limit` is
    /// capped at the remaining rows after the offset.
    ///
    /// Uses in-place operations (no new Vec allocation): `drain(..offset)`
    /// removes leading elements, then `truncate(limit)` caps the remainder.
    #[must_use]
    pub fn paginate(mut self, offset: usize, limit: usize) -> Self {
        if offset >= self.rows.len() {
            self.rows.clear();
        } else {
            drop(self.rows.drain(..offset));
            self.rows.truncate(limit);
        }
        self
    }
}

// -- schema introspection types -----------------------------------------------

/// A column descriptor from schema introspection (name + `DuckDB` type).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaColumn {
    /// Column name as declared in the parquet file(s).
    pub name: String,
    /// `DuckDB` logical type (e.g. "VARCHAR", "TIMESTAMP", "BIGINT").
    pub data_type: String,
}

/// The result of a schema introspection query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaResult {
    /// Columns discovered in the data source.
    pub columns: Vec<SchemaColumn>,
    /// Number of parquet files matching the configured glob.
    pub file_count: u64,
}

/// Per-column statistics aggregated from parquet file footers.
///
/// Produced by the footer-stats reader in `trawl-engine`'s `parquet_stats`
/// module, summed across all row groups in the matched parquet files.
#[derive(Debug, Clone)]
pub struct ParquetColumnStats {
    /// Column name (`path_in_schema`).
    pub column_name: String,
    /// Total values across all row groups.
    pub total_count: u64,
    /// Total null values across all row groups.
    pub null_count: u64,
    /// Minimum value (stringified).
    pub min_value: Option<String>,
    /// Maximum value (stringified).
    pub max_value: Option<String>,
    /// Total compressed size in bytes.
    pub compressed_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> Column {
        Column {
            name: name.to_owned(),
        }
    }

    #[test]
    fn field_display_rank_orders_envelope_custom_trailing() {
        // Envelope fields rank first, in declared order.
        assert_eq!(field_display_rank("_time"), (0, 0));
        assert_eq!(field_display_rank("_severity"), (0, 4));
        assert_eq!(field_display_rank("message"), (0, 5));
        // Custom fields sit between envelope and trailing metadata; callers
        // break ties by name.
        assert_eq!(field_display_rank("duration").0, 1);
        // Trailing metadata is demoted to the very end, in declared order.
        assert_eq!(field_display_rank("_raw"), (2, 0));
        assert_eq!(field_display_rank("_repairs"), (2, 2));

        // Sorting by (rank, name) reproduces query-result column order:
        // envelope first, custom alphabetical, metadata last — never
        // `_ingested` alphabetically first.
        let mut names = vec!["duration", "_ingested", "service", "_time", "alpha"];
        sort_by_display_rank(&mut names, |n| n);
        assert_eq!(
            names,
            vec!["_time", "service", "alpha", "duration", "_ingested"]
        );
    }

    #[test]
    fn reorder_columns_moves_well_known_first() {
        let mut result = QueryResult {
            columns: vec![
                col("pid"),
                col("host"),
                col("message"),
                col("_time"),
                col("status"),
            ],
            rows: vec![vec![
                Value::Integer(1),
                Value::String("web-1".into()),
                Value::String("ok".into()),
                Value::String("2026-01-01".into()),
                Value::Integer(200),
            ]],
        };

        result.reorder_columns(WELL_KNOWN_LOG_FIELDS);

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_time", "host", "message", "pid", "status"]);
        // Row data must follow the same permutation.
        assert_eq!(result.rows[0][0], Value::String("2026-01-01".into()));
        assert_eq!(result.rows[0][1], Value::String("web-1".into()));
        assert_eq!(result.rows[0][2], Value::String("ok".into()));
        assert_eq!(result.rows[0][3], Value::Integer(1));
        assert_eq!(result.rows[0][4], Value::Integer(200));
    }

    /// The full log ordering: leading envelope first, `_raw`/`_ingested`/
    /// `_repairs` demoted to the very end, user fields in between.
    #[test]
    fn reorder_log_columns_demotes_trailing() {
        let mut result = QueryResult {
            columns: vec![
                col("_raw"),
                col("pid"),
                col("_ingested"),
                col("host"),
                col("_repairs"),
                col("message"),
                col("_time"),
            ],
            rows: vec![vec![
                Value::String("raw".into()),
                Value::Integer(1),
                Value::String("ing".into()),
                Value::String("web-1".into()),
                Value::Null,
                Value::String("ok".into()),
                Value::String("t".into()),
            ]],
        };

        result.reorder_log_columns();

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "_time",
                "host",
                "message",
                "pid",
                "_raw",
                "_ingested",
                "_repairs"
            ]
        );
        // Rows permute with the columns.
        assert_eq!(result.rows[0][0], Value::String("t".into()));
        assert_eq!(result.rows[0][4], Value::String("raw".into()));
        assert_eq!(result.rows[0][6], Value::Null);
    }

    #[test]
    fn reorder_columns_skips_missing_fields() {
        let mut result = QueryResult {
            columns: vec![col("status"), col("uri")],
            rows: vec![vec![Value::Integer(200), Value::String("/api".into())]],
        };

        result.reorder_columns(WELL_KNOWN_LOG_FIELDS);

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["status", "uri"]); // unchanged
    }

    #[test]
    fn reorder_columns_noop_when_already_ordered() {
        let mut result = QueryResult {
            columns: vec![col("_time"), col("host"), col("extra")],
            rows: vec![vec![
                Value::String("t".into()),
                Value::String("h".into()),
                Value::Integer(1),
            ]],
        };

        result.reorder_columns(WELL_KNOWN_LOG_FIELDS);

        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_time", "host", "extra"]);
    }

    #[test]
    fn reorder_columns_empty_result() {
        let mut result = QueryResult::empty();
        result.reorder_columns(WELL_KNOWN_LOG_FIELDS); // should not panic
        assert!(result.columns.is_empty());
    }
}
