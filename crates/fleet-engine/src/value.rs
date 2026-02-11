//! Result types for query execution.
//!
//! These types are deliberately decoupled from `DuckDB`'s internal types
//! to keep the public API stable across duckdb crate version changes.

use std::fmt;

/// A cell value from a query result row.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => Ok(()),
            Self::Boolean(b) => write!(f, "{b}"),
            Self::Integer(i) => write!(f, "{i}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::String(s) => write!(f, "{s}"),
        }
    }
}

/// Column metadata from a query result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Column {
    pub name: String,
}

/// The complete result of a query execution.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    /// Number of result rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Whether the result set is empty.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
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
