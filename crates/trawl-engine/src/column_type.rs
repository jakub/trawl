// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Result column types, read off `DuckDB`'s logical types.
//!
//! A `DuckDB` result column is typed before its first row is read
//! (`column_logical_type`), so a zero-row or all-null column is typed from
//! execution like any other (ADR-0035, amended 2026-10-22). Types never
//! come from the catalog: this is what the statement returned.
//!
//! The map has to agree with what the executor's `extract_value` lands for
//! the same column, or the wire encoder refuses the cell. The executor's
//! `every_duckdb_type_agrees_with_its_cells` drives `test_all_types()`
//! through both. A type `extract_value` renders as text, or lands only as
//! null (`DECIMAL`, `INTERVAL`, `STRUCT`, `MAP`, `UNION`), is `string`
//! here, and so is `SQLNULL`, which has no evidence of its own (`DuckDB`
//! resolves a bare `NULL` result column to `INTEGER` before it gets here).
//!
//! [`StoredType`] is the way back: the `DuckDB` type a run's parquet result
//! stores a typed column as, so reading the file maps it to the same
//! `ColumnType` again.

use duckdb::core::{LogicalTypeHandle, LogicalTypeId};
use trawl_api::value::{ColumnType, Value};

/// The wire type of a column `DuckDB` typed as `ty`.
pub fn column_type(ty: &LogicalTypeHandle) -> ColumnType {
    match ty.id() {
        LogicalTypeId::Boolean => ColumnType::Boolean,
        LogicalTypeId::Tinyint
        | LogicalTypeId::Smallint
        | LogicalTypeId::Integer
        | LogicalTypeId::Bigint
        | LogicalTypeId::UTinyint
        | LogicalTypeId::USmallint
        | LogicalTypeId::UInteger
        | LogicalTypeId::UBigint
        | LogicalTypeId::Hugeint
        | LogicalTypeId::UHugeint => ColumnType::Integer,
        LogicalTypeId::Float | LogicalTypeId::Double => ColumnType::Float,
        LogicalTypeId::Timestamp
        | LogicalTypeId::TimestampS
        | LogicalTypeId::TimestampMs
        | LogicalTypeId::TimestampNs
        | LogicalTypeId::TimestampTZ => ColumnType::Timestamp,
        LogicalTypeId::Date => ColumnType::Date,
        LogicalTypeId::Time | LogicalTypeId::TimeTZ | LogicalTypeId::TimeNs => ColumnType::Time,
        LogicalTypeId::List | LogicalTypeId::Array => ColumnType::list(column_type(&ty.child(0))),
        _ => ColumnType::String,
    }
}

/// The `DuckDB` type a run's parquet result stores a column as: the reverse
/// of [`column_type`], for `Executor::write_query_result_to_parquet`.
///
/// The wire has one `integer` type, so the width comes from the cells
/// (ADR-0035, amended 2026-10-22): `BIGINT` when every cell fits `i64`,
/// `UBIGINT` when some cell is above `i64::MAX` and none is negative, and
/// `BIGINT` when there is no cell to ask. Reading the file back maps both
/// to `integer` again, and a `UBIGINT` cell above `i64::MAX` lands as
/// `Value::UInt`.
///
/// Cells that need both a negative and a magnitude above `i64::MAX` have
/// no exact stored type. `DuckDB`'s parquet writer stores a `HUGEINT`
/// column as `DOUBLE`, which rounds past 2^53, so such a column is refused
/// like a `mixed` one, which has no stored type because parquet has no
/// union.
///
/// A timestamp is stored as `TIMESTAMP` in UTC, the way the corpus stores
/// `_time`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredType {
    Boolean,
    Bigint,
    UBigint,
    Double,
    Varchar,
    Timestamp,
    Date,
    Time,
    List(Box<StoredType>),
}

impl StoredType {
    /// How `ty` is stored, given every cell the column holds, or why it
    /// cannot be: a `mixed` column (or list items), or integers that need
    /// both signs and the unsigned range.
    pub fn of<'a>(
        ty: &ColumnType,
        cells: impl IntoIterator<Item = &'a Value>,
    ) -> Result<Self, &'static str> {
        let mut range = IntegerRange::default();
        for cell in cells {
            range.observe(ty, cell);
        }
        Self::with_range(ty, range)
    }

    fn with_range(ty: &ColumnType, range: IntegerRange) -> Result<Self, &'static str> {
        Ok(match ty {
            ColumnType::Boolean => Self::Boolean,
            ColumnType::Integer => match (range.negative, range.above_i64) {
                (_, false) => Self::Bigint,
                (false, true) => Self::UBigint,
                (true, true) => {
                    return Err("holds integers below zero and above the signed 64-bit range");
                }
            },
            ColumnType::Float => Self::Double,
            ColumnType::String => Self::Varchar,
            ColumnType::Timestamp => Self::Timestamp,
            ColumnType::Date => Self::Date,
            ColumnType::Time => Self::Time,
            ColumnType::List { items } => Self::List(Box::new(Self::with_range(items, range)?)),
            ColumnType::Mixed => return Err("is mixed, and parquet has no union type"),
        })
    }

    /// The `DuckDB` type name, `UBIGINT` or `BIGINT[]`.
    pub fn sql(&self) -> String {
        match self {
            Self::Boolean => "BOOLEAN".to_owned(),
            Self::Bigint => "BIGINT".to_owned(),
            Self::UBigint => "UBIGINT".to_owned(),
            Self::Double => "DOUBLE".to_owned(),
            Self::Varchar => "VARCHAR".to_owned(),
            Self::Timestamp => "TIMESTAMP".to_owned(),
            Self::Date => "DATE".to_owned(),
            Self::Time => "TIME".to_owned(),
            Self::List(items) => format!("{}[]", items.sql()),
        }
    }

    /// The type the writer reads the column's wire text as: `VARCHAR`,
    /// nested in as many lists as the column is. Declared, never inferred,
    /// so a string of digits stays a string.
    pub fn text_sql(&self) -> String {
        match self {
            Self::List(items) => format!("{}[]", items.text_sql()),
            _ => "VARCHAR".to_owned(),
        }
    }

    /// The `DuckDB` expression that turns `expr`, the column's wire text
    /// read as [`text_sql`](Self::text_sql), into this type exactly.
    ///
    /// A cast for every type but a timestamp. The wire spells a timestamp
    /// in RFC 3339 with its offset, so it casts to `TIMESTAMPTZ` and is
    /// read back `AT TIME ZONE 'UTC'`, a `TIMESTAMP` holding the same
    /// instant whatever offset it was written in. A list maps its elements
    /// through the same rule; `depth` names each level's lambda parameter.
    pub fn cast_text(&self, expr: &str) -> String {
        self.cast_text_at(expr, 0)
    }

    fn cast_text_at(&self, expr: &str, depth: usize) -> String {
        match self {
            Self::Varchar => expr.to_owned(),
            Self::Timestamp => format!("(CAST({expr} AS TIMESTAMPTZ) AT TIME ZONE 'UTC')"),
            Self::List(items) => {
                let element = format!("_e{depth}");
                format!(
                    "list_transform({expr}, {element} -> {})",
                    items.cast_text_at(&element, depth + 1)
                )
            }
            other => format!("CAST({expr} AS {})", other.sql()),
        }
    }
}

/// What a column's integer cells need: a negative, a magnitude above
/// `i64::MAX`, or both. A list's elements count toward its items' type.
#[derive(Debug, Clone, Copy, Default)]
struct IntegerRange {
    negative: bool,
    above_i64: bool,
}

impl IntegerRange {
    fn observe(&mut self, ty: &ColumnType, cell: &Value) {
        match (ty, cell) {
            (ColumnType::Integer, Value::Integer(i)) => self.negative |= *i < 0,
            (ColumnType::Integer, Value::UInt(_)) => self.above_i64 = true,
            (ColumnType::List { items }, Value::Array(elements)) => {
                for element in elements {
                    self.observe(items, element);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_integer_column_is_stored_in_the_narrowest_type_its_cells_fit() {
        let of = |cells: &[Value]| StoredType::of(&ColumnType::Integer, cells);
        assert_eq!(of(&[]), Ok(StoredType::Bigint), "no cell: BIGINT");
        assert_eq!(of(&[Value::Null]), Ok(StoredType::Bigint));
        assert_eq!(
            of(&[Value::Integer(i64::MIN), Value::Integer(i64::MAX)]),
            Ok(StoredType::Bigint)
        );
        assert_eq!(
            of(&[Value::Integer(0), Value::UInt(u64::MAX)]),
            Ok(StoredType::UBigint)
        );
        assert!(
            of(&[Value::Integer(-1), Value::UInt(u64::MAX)]).is_err(),
            "HUGEINT would be stored as DOUBLE"
        );
    }

    #[test]
    fn a_list_column_takes_its_width_from_every_element() {
        let ty = ColumnType::list(ColumnType::list(ColumnType::Integer));
        let cells = [
            Value::Array(vec![Value::Array(vec![Value::Integer(1)])]),
            Value::Array(vec![Value::Array(vec![Value::UInt(u64::MAX)])]),
        ];
        let stored = StoredType::of(&ty, &cells).unwrap();
        assert_eq!(stored.sql(), "UBIGINT[][]");
        assert_eq!(stored.text_sql(), "VARCHAR[][]");
        assert_eq!(
            stored.cast_text("c0"),
            "list_transform(c0, _e0 -> list_transform(_e0, _e1 -> CAST(_e1 AS UBIGINT)))"
        );
    }

    #[test]
    fn a_mixed_column_has_no_stored_type() {
        assert!(StoredType::of(&ColumnType::Mixed, &[]).is_err());
        assert!(StoredType::of(&ColumnType::list(ColumnType::Mixed), &[]).is_err());
    }

    #[test]
    fn a_timestamp_is_stored_as_utc() {
        assert_eq!(
            StoredType::Timestamp.cast_text("c1"),
            "(CAST(c1 AS TIMESTAMPTZ) AT TIME ZONE 'UTC')"
        );
        assert_eq!(StoredType::Varchar.cast_text("c2"), "c2");
    }
}
