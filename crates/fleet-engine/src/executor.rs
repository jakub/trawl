//! Query execution against `DuckDB`.
//!
//! Handles connection management, prepared statements, parameter binding,
//! and result extraction.

use duckdb::Connection;
use duckdb::types::ValueRef;
use fleet_core::emitter::{self, EmittedQuery, SqlValue};
use fleet_core::parser;

use crate::error::EngineError;
use crate::value::{Column, QueryResult, Value};

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

    /// Execute a pre-emitted query (SQL + params) against `DuckDB`.
    pub fn execute_emitted(&self, query: &EmittedQuery) -> Result<QueryResult, EngineError> {
        let mut stmt = self.conn.prepare(&query.sql)?;

        let params = bind_params(&query.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        let columns: Vec<Column> = stmt
            .column_names()
            .into_iter()
            .map(|name| Column { name })
            .collect();
        let col_count = columns.len();

        let row_iter = stmt.query_map(param_refs.as_slice(), |row| {
            let mut cells = Vec::with_capacity(col_count);
            for i in 0..col_count {
                cells.push(extract_value(row, i));
            }
            Ok(cells)
        })?;

        let mut rows = Vec::new();
        for row_result in row_iter {
            rows.push(row_result?);
        }

        Ok(QueryResult { columns, rows })
    }

    /// Full pipeline: parse DSL, emit SQL, execute against `DuckDB`.
    ///
    /// `source` is the parquet glob path passed to `read_parquet()`,
    /// e.g. `"/data/**/*.parquet"`.
    pub fn run_query(&self, dsl: &str, source: &str) -> Result<QueryResult, EngineError> {
        let ast = parser::parse(dsl).map_err(EngineError::Parse)?;
        let emitted = emitter::emit(&ast, source)?;
        self.execute_emitted(&emitted)
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
        // temporal types and everything else: let duckdb format as string
        _ => row
            .get::<_, String>(idx)
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}
