//! SQL emitter for `DuckDB`.
//!
//! Walks the AST and produces parameterized `DuckDB` SQL.
//! Uses CTEs to handle multi-stage pipeline queries.

mod expr;
mod fields;
mod functions;
mod pipeline;
mod search;
mod state;

use crate::ast::Query;
use state::EmitterState;

use std::fmt;

/// The result of emitting SQL from a parsed query.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedQuery {
    /// The parameterized SQL string (placeholders are `?`).
    pub sql: String,
    /// Ordered parameter values corresponding to each `?` placeholder.
    pub params: Vec<SqlValue>,
}

/// A parameter value for a SQL query placeholder.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl fmt::Display for SqlValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "'{s}'"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Bool(b) => write!(f, "{b}"),
        }
    }
}

/// Errors that can occur during SQL emission.
#[derive(Debug, Clone, PartialEq)]
pub enum EmitError {
    UnknownFunction { name: String },
    InvalidAggregation { message: String },
    UnsupportedOperation { message: String },
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFunction { name } => write!(f, "unknown function: {name}"),
            Self::InvalidAggregation { message } => write!(f, "invalid aggregation: {message}"),
            Self::UnsupportedOperation { message } => {
                write!(f, "unsupported operation: {message}")
            }
        }
    }
}

impl std::error::Error for EmitError {}

/// Emit parameterized `DuckDB` SQL from a parsed query.
///
/// `source` is the parquet glob path, e.g. `"/data/**/*.parquet"`.
pub fn emit(query: &Query, source: &str) -> Result<EmittedQuery, EmitError> {
    let mut state = EmitterState::new(source);

    // translate search stage into WHERE clauses
    search::emit_search(&query.search, &mut state);

    // walk pipe stages
    for stage in &query.pipeline {
        pipeline::process_stage(&stage.node, &mut state)?;
    }

    // finalize into SQL
    let sql = state.finalize();
    let params = state.into_params();

    Ok(EmittedQuery { sql, params })
}
