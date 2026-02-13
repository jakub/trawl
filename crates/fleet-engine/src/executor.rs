//! Query execution against `DuckDB`.
//!
//! Handles connection management, prepared statements, parameter binding,
//! and result extraction.

use std::sync::Arc;

use duckdb::Connection;
use duckdb::types::{TimeUnit, ValueRef};
use fleet_core::emitter::{self, EmittedQuery, SqlValue};
use fleet_core::parser;

use crate::error::EngineError;
use crate::value::{Column, QueryResult, SchemaColumn, SchemaResult, Value};

/// Query executor backed by an in-memory `DuckDB` connection.
#[derive(Debug)]
pub struct Executor {
    conn: Connection,
}

impl Executor {
    /// Create a new executor with an in-memory `DuckDB` connection.
    pub fn new() -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn })
    }

    /// Create a new executor sharing the same underlying database.
    ///
    /// The cloned connection benefits from `DuckDB`'s internal metadata
    /// caching (parquet file stats, column statistics) accumulated by
    /// other connections to the same database.
    pub fn try_clone(&self) -> Result<Self, EngineError> {
        let conn = self.conn.try_clone()?;
        Ok(Self { conn })
    }

    /// Get an interrupt handle for cancelling in-flight queries from another thread.
    pub fn interrupt_handle(&self) -> Arc<duckdb::InterruptHandle> {
        self.conn.interrupt_handle()
    }

    /// Execute a pre-emitted query (SQL + params) against `DuckDB`.
    ///
    /// `max_rows` caps the number of result rows to prevent unbounded memory
    /// allocation. Returns [`EngineError::ResultTooLarge`] if exceeded.
    fn execute_emitted_inner(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = self.conn.prepare(&query.sql)?;

        let params = bind_params(&query.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        // start query execution — column metadata is only available
        // after DuckDB resolves table-valued functions like read_parquet()
        let mut result_rows = stmt.query(param_refs.as_slice())?;

        let stmt_ref =
            result_rows
                .as_ref()
                .ok_or(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "statement unavailable after query execution".into(),
                )))?;
        let col_count = stmt_ref.column_count();
        let columns: Vec<Column> = stmt_ref
            .column_names()
            .into_iter()
            .map(|name| Column { name })
            .collect();

        let mut rows = Vec::new();
        while let Some(row) = result_rows.next()? {
            if rows.len() >= max_rows {
                return Err(EngineError::ResultTooLarge(max_rows));
            }
            let mut cells = Vec::with_capacity(col_count);
            for i in 0..col_count {
                cells.push(extract_value(row, i));
            }
            rows.push(cells);
        }

        Ok(QueryResult { columns, rows })
    }
}

/// Trait for executing fleet DSL queries against a data backend.
pub trait QueryEngine {
    /// Full pipeline: parse DSL, emit SQL, execute.
    fn run_query(
        &self,
        dsl: &str,
        source: &str,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError>;

    /// Execute a pre-emitted query (SQL + params).
    fn execute_emitted(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError>;
}

/// Trait for schema introspection of the data source.
pub trait SchemaIntrospector {
    /// Describe the schema without reading row data.
    fn describe_schema(&self, source: &str) -> Result<SchemaResult, EngineError>;
}

impl QueryEngine for Executor {
    fn run_query(
        &self,
        dsl: &str,
        source: &str,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit(&ast, source)?;
        self.execute_emitted_inner(&emitted, max_rows)
    }

    fn execute_emitted(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        self.execute_emitted_inner(query, max_rows)
    }
}

impl SchemaIntrospector for Executor {
    fn describe_schema(&self, source: &str) -> Result<SchemaResult, EngineError> {
        // Validate source path before interpolation — DuckDB doesn't truly
        // parameterize table-valued function arguments.
        emitter::validate_source_path(source)?;

        let mut stmt = self
            .conn
            .prepare("DESCRIBE SELECT * FROM read_parquet(?, union_by_name=true)")?;
        let mut rows = stmt.query([source])?;

        let mut columns = Vec::new();
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let data_type: String = row.get(1)?;
            columns.push(SchemaColumn { name, data_type });
        }

        // Count matching files.
        let file_count: i64 =
            self.conn
                .query_row("SELECT count(*)::BIGINT FROM glob(?)", [source], |row| {
                    row.get(0)
                })?;

        Ok(SchemaResult {
            columns,
            file_count: u64::try_from(file_count).unwrap_or(0),
        })
    }
}

/// Convert fleet-core `SqlValue` params into duckdb `ToSql` trait objects.
fn bind_params(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
        .iter()
        .map(|val| -> Box<dyn duckdb::ToSql> {
            match val {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
            }
        })
        .collect()
}

/// Extract a `Value` from a duckdb row at the given column index.
///
/// Uses `ValueRef` for type-safe dispatch. Temporal types fall back to
/// string extraction so duckdb handles its own formatting.
fn extract_value(row: &duckdb::Row<'_>, idx: usize) -> Value {
    match row.get_ref_unwrap(idx) {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Boolean(b),
        ValueRef::TinyInt(i) => Value::Integer(i64::from(i)),
        ValueRef::SmallInt(i) => Value::Integer(i64::from(i)),
        ValueRef::Int(i) => Value::Integer(i64::from(i)),
        ValueRef::BigInt(i) => Value::Integer(i),
        // log data shouldn't exceed i64 range; stringify if it does
        ValueRef::HugeInt(i) => {
            i64::try_from(i).map_or_else(|_| Value::String(i.to_string()), Value::Integer)
        }
        ValueRef::UTinyInt(i) => Value::Integer(i64::from(i)),
        ValueRef::USmallInt(i) => Value::Integer(i64::from(i)),
        ValueRef::UInt(i) => Value::Integer(i64::from(i)),
        ValueRef::UBigInt(i) => {
            i64::try_from(i).map_or_else(|_| Value::String(i.to_string()), Value::Integer)
        }
        ValueRef::Float(f) => Value::Float(f64::from(f)),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::String(format!("<blob {} bytes>", bytes.len())),
        ValueRef::Timestamp(unit, val) => Value::String(format_timestamp(unit, val)),
        ValueRef::Date32(days) => Value::String(format_date(days)),
        ValueRef::Time64(unit, val) => Value::String(format_time(unit, val)),
        // everything else: try string extraction, fall back to null
        _ => row
            .get::<_, String>(idx)
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

/// Convert a timestamp value to an ISO 8601 string.
///
/// `DuckDB` stores timestamps as integer offsets from the Unix epoch.
/// The `TimeUnit` indicates the resolution.
/// Convert a temporal value to microseconds based on its `TimeUnit`.
const fn to_micros(unit: TimeUnit, val: i64) -> i64 {
    match unit {
        TimeUnit::Second => val * 1_000_000,
        TimeUnit::Millisecond => val * 1_000,
        TimeUnit::Microsecond => val,
        TimeUnit::Nanosecond => val / 1_000,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn format_timestamp(unit: TimeUnit, val: i64) -> String {
    let micros = to_micros(unit, val);

    let (total_secs, sub_secs) = if micros >= 0 {
        (micros / 1_000_000, (micros % 1_000_000) as u32)
    } else {
        // handle pre-epoch timestamps
        let s = (micros - 999_999) / 1_000_000;
        let us = (micros - s * 1_000_000) as u32;
        (s, us)
    };

    // days since epoch and time-of-day
    let (days, day_secs) = if total_secs >= 0 {
        ((total_secs / 86400) as i32, (total_secs % 86400) as u32)
    } else {
        let d = (total_secs - 86399) / 86400;
        let s = (total_secs - d * 86400) as u32;
        (d as i32, s)
    };

    let (y, m, d) = days_to_ymd(days);
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    if sub_secs == 0 {
        format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02}")
    } else {
        // trim trailing zeros from fractional seconds
        let frac = format!("{sub_secs:06}");
        let trimmed = frac.trim_end_matches('0');
        format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02}.{trimmed}")
    }
}

/// Convert a date (days since Unix epoch) to YYYY-MM-DD.
fn format_date(days: i32) -> String {
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert a time value to HH:MM:SS.
fn format_time(unit: TimeUnit, val: i64) -> String {
    let micros = to_micros(unit, val);
    let total_secs = micros / 1_000_000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
///
/// Uses the civil calendar algorithm from Howard Hinnant's `date` library.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn days_to_ymd(days: i32) -> (i32, u32, u32) {
    // shift epoch from 0000-03-01 to 1970-01-01
    let z = i64::from(days) + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d)
}
