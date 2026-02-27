//! Query execution against `DuckDB`.
//!
//! Handles connection management, prepared statements, parameter binding,
//! and result extraction.

use std::path::Path;
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

    /// Lightweight health check: runs `SELECT 1` to verify the connection is alive.
    pub fn ping(&self) -> Result<(), EngineError> {
        self.conn
            .query_row("SELECT 1", [], |_row| Ok(()))
            .map_err(EngineError::from)
    }

    /// Full pipeline: parse DSL, emit SQL, execute.
    ///
    /// `utc_offset_secs` is applied to all timestamp values at format time.
    /// Pass `0` for UTC display.
    pub fn run_query(
        &self,
        dsl: &str,
        source: &str,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit(&ast, source)?;
        let mut result = self.execute_emitted(&emitted, max_rows, utc_offset_secs)?;
        if !emitted.rust_stages.is_empty() {
            result = crate::post_process::apply_rust_stages(result, &emitted.rust_stages)?;
        }
        Ok(result)
    }

    /// Full pipeline with hot buffer: parse DSL, emit composite SQL, execute.
    ///
    /// The emitted SQL unions the primary parquet source with a hot buffer
    /// ndjson file via `UNION ALL BY NAME`.
    ///
    /// Handles cold-start gracefully: when no parquet files exist yet
    /// (empty columns = no data source), falls back to querying just the
    /// hot buffer so events ingested before the first compaction are visible.
    pub fn run_query_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit_with_hot_source(&ast, source, hot_source)?;
        let result = self.execute_emitted(&emitted, max_rows, utc_offset_secs);
        let mut result = match &result {
            // Columns present → real result (possibly empty rows). Return as-is.
            Ok(r) if !r.columns.is_empty() => result?,
            // No columns (no parquet source files), database error (UNION
            // fails on missing source), or binder error remapped to Emit
            // (column not found in empty parquet) → fall back to hot-only.
            // ResultTooLarge is excluded: the query worked, just too many rows.
            Ok(_) | Err(EngineError::Database(_) | EngineError::Emit(_)) => {
                let hot_emitted = emitter::emit(&ast, hot_source)?;
                match self.execute_emitted(&hot_emitted, max_rows, utc_offset_secs) {
                    // Hot-only also hit a binder/emit error (e.g. empty ndjson
                    // between compaction cycles). Treat as empty, not error.
                    Err(EngineError::Emit(_)) => QueryResult::empty(),
                    other => other?,
                }
            }
            Err(_) => result?,
        };
        if !emitted.rust_stages.is_empty() {
            result = crate::post_process::apply_rust_stages(result, &emitted.rust_stages)?;
        }
        Ok(result)
    }

    /// Execute a pre-emitted query (SQL + params) against `DuckDB`.
    ///
    /// `max_rows` caps the number of result rows to prevent unbounded memory
    /// allocation. Returns [`EngineError::ResultTooLarge`] if exceeded.
    ///
    /// `utc_offset_secs` is applied to all timestamp values at format time.
    pub fn execute_emitted(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
        utc_offset_secs: i32,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = match self.conn.prepare(&query.sql) {
            Ok(s) => s,
            Err(e) if is_no_files_error(&e) => return Ok(QueryResult::empty()),
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        let params = bind_params(&query.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        // start query execution — column metadata is only available
        // after DuckDB resolves table-valued functions like read_parquet()
        let mut result_rows = match stmt.query(param_refs.as_slice()) {
            Ok(r) => r,
            Err(e) if is_no_files_error(&e) => return Ok(QueryResult::empty()),
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

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
                cells.push(extract_value(row, i, utc_offset_secs));
            }
            rows.push(cells);
        }

        Ok(QueryResult { columns, rows })
    }

    /// Describe the schema without reading row data.
    pub fn describe_schema(&self, source: &str) -> Result<SchemaResult, EngineError> {
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

    /// Sample distinct values for a field (for autocomplete).
    ///
    /// Returns up to `limit` distinct values for the specified field, ordered
    /// lexicographically. Only string values are returned; numeric/timestamp
    /// fields are skipped.
    ///
    /// # Security
    ///
    /// The field name is validated to prevent SQL injection. Only alphanumeric
    /// characters, underscores, and dots are allowed.
    pub fn sample_field_values(
        &self,
        source: &str,
        field: &str,
        limit: usize,
    ) -> Result<Vec<String>, EngineError> {
        // Validate field name to prevent SQL injection.
        if !field
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return Err(EngineError::Emit(
                fleet_core::emitter::EmitError::UnsupportedOperation {
                    message: format!("invalid field name: {field}"),
                },
            ));
        }

        fleet_core::emitter::validate_source_path(source)?;

        // Use DuckDB identifier quoting for safety.
        let sql = format!(
            r#"SELECT DISTINCT "{field}" FROM read_parquet(?, union_by_name=true)
               WHERE "{field}" IS NOT NULL
               ORDER BY 1
               LIMIT ?"#
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = stmt.query(duckdb::params![source, limit_i64])?;

        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            // Only return string values (skip numeric/timestamp fields).
            if let Ok(val) = row.get::<_, String>(0) {
                values.push(val);
            }
        }

        Ok(values)
    }

    /// Export query results directly to a Parquet file via `DuckDB` `COPY TO`.
    ///
    /// Uses a temp table to stage the query results, then writes them to
    /// the output path as Snappy-compressed Parquet.
    ///
    /// Returns an error if the pipeline contains Rust post-processing stages
    /// (e.g. `extract kv`) since those can't be expressed as pure SQL.
    pub fn export_parquet(
        &self,
        dsl: &str,
        source: &str,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit(&ast, source)?;
        self.export_parquet_from_emitted(&emitted, output_path, max_rows)
    }

    /// Export with hot buffer union, falling back to hot-only on cold start.
    pub fn export_parquet_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit_with_hot_source(&ast, source, hot_source)?;
        match self.export_parquet_from_emitted(&emitted, output_path, max_rows) {
            // No columns / no parquet files → fall back to hot-only.
            Err(EngineError::Database(_) | EngineError::Emit(_)) => {
                let hot_emitted = emitter::emit(&ast, hot_source)?;
                self.export_parquet_from_emitted(&hot_emitted, output_path, max_rows)
            }
            other => other,
        }
    }

    /// Internal: stage an emitted query into a temp table and COPY to parquet.
    fn export_parquet_from_emitted(
        &self,
        emitted: &EmittedQuery,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<(), EngineError> {
        if !emitted.rust_stages.is_empty() {
            return Err(EngineError::Emit(
                fleet_core::emitter::EmitError::UnsupportedOperation {
                    message: "parquet export is not supported for queries with post-processing stages (e.g. extract kv)".into(),
                },
            ));
        }

        let path_str = output_path.to_str().ok_or_else(|| {
            EngineError::Emit(fleet_core::emitter::EmitError::UnsupportedOperation {
                message: "output path is not valid UTF-8".into(),
            })
        })?;

        // Create temp table from query results.
        let create_sql = format!(
            "CREATE TEMP TABLE __fleet_export AS (SELECT * FROM ({}) LIMIT {max_rows})",
            emitted.sql
        );

        let params = bind_params(&emitted.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        let cleanup = |conn: &Connection| {
            let _ = conn.execute_batch("DROP TABLE IF EXISTS __fleet_export");
        };

        match self.conn.prepare(&create_sql) {
            Ok(mut stmt) => {
                if let Err(e) = stmt.execute(param_refs.as_slice()) {
                    cleanup(&self.conn);
                    return Err(e.into());
                }
            }
            Err(e) => {
                cleanup(&self.conn);
                return Err(e.into());
            }
        }

        // COPY to parquet. Path is validated above (UTF-8), and the temp table
        // name is a constant — no injection risk.
        let copy_sql =
            format!("COPY __fleet_export TO '{path_str}' (FORMAT PARQUET, COMPRESSION SNAPPY)");
        if let Err(e) = self.conn.execute_batch(&copy_sql) {
            // Clean up temp table and partial file on error.
            cleanup(&self.conn);
            let _ = std::fs::remove_file(output_path);
            return Err(e.into());
        }

        cleanup(&self.conn);
        Ok(())
    }
}

/// Check if a `DuckDB` error is the "No files found" error from `read_parquet()`
/// when a glob matches zero files. Semantically this means "no data" — not a
/// server error.
fn is_no_files_error(e: &duckdb::Error) -> bool {
    let msg = e.to_string();
    msg.contains("No files found that match the pattern")
}

/// Check if a `DuckDB` error is a binder error about a missing column. This is
/// a user error (querying a nonexistent field), not a server error.
fn is_binder_column_error(e: &duckdb::Error) -> bool {
    let msg = e.to_string();
    msg.contains("Binder Error") && (msg.contains("column") || msg.contains("not found"))
}

/// Remap a `DuckDB` binder error about missing columns to `EngineError::Emit`
/// so it surfaces as HTTP 400 instead of 500.
fn remap_binder_error(e: &duckdb::Error) -> EngineError {
    let msg = e.to_string();
    // try to extract the field name from the error message
    let user_msg = if let Some(start) = msg.find('"') {
        if let Some(end) = msg[start + 1..].find('"') {
            let field = &msg[start + 1..start + 1 + end];
            format!("unknown field: {field}")
        } else {
            format!("unknown field in query: {msg}")
        }
    } else {
        format!("unknown field in query: {msg}")
    };
    EngineError::Emit(fleet_core::emitter::EmitError::UnsupportedOperation { message: user_msg })
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
///
/// `utc_offset_secs` is applied to timestamp values before civil time
/// formatting. Pass `0` for UTC.
fn extract_value(row: &duckdb::Row<'_>, idx: usize, utc_offset_secs: i32) -> Value {
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
        ValueRef::Timestamp(unit, val) => {
            Value::String(format_timestamp(unit, val, utc_offset_secs))
        }
        ValueRef::Date32(days) => Value::String(format_date(days)),
        ValueRef::Time64(unit, val) => Value::String(format_time(unit, val)),
        // list values from aggregations like LIST(DISTINCT col)
        ValueRef::List(..) => row
            .get::<_, duckdb::types::Value>(idx)
            .map(convert_duckdb_value)
            .unwrap_or(Value::Null),
        // everything else: try string extraction, fall back to null
        _ => row
            .get::<_, String>(idx)
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

/// Recursively convert a `duckdb::types::Value` to our `Value`.
fn convert_duckdb_value(v: duckdb::types::Value) -> Value {
    match v {
        duckdb::types::Value::Null => Value::Null,
        duckdb::types::Value::Boolean(b) => Value::Boolean(b),
        duckdb::types::Value::TinyInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::SmallInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::Int(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::BigInt(i) => Value::Integer(i),
        duckdb::types::Value::Float(f) => Value::Float(f64::from(f)),
        duckdb::types::Value::Double(f) => Value::Float(f),
        duckdb::types::Value::Text(s) => Value::String(s),
        duckdb::types::Value::List(elements) => {
            Value::Array(elements.into_iter().map(convert_duckdb_value).collect())
        }
        other => Value::String(format!("{other:?}")),
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
fn format_timestamp(unit: TimeUnit, val: i64, utc_offset_secs: i32) -> String {
    let micros = to_micros(unit, val) + i64::from(utc_offset_secs) * 1_000_000;

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
