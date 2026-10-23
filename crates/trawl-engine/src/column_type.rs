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

use duckdb::core::{LogicalTypeHandle, LogicalTypeId};
use trawl_api::value::ColumnType;

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
