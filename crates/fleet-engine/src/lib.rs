//! fleet-engine: `DuckDB` integration and query execution.
//!
//! This crate owns the database connection lifecycle, query execution,
//! result streaming, and query cancellation. It takes SQL + parameters
//! from fleet-core's emitter and executes them against `DuckDB`.

/// Query executor — manages DuckDB connections and query lifecycle.
pub mod executor;
