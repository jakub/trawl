//! fleet-engine: `DuckDB` integration and query execution.
//!
//! This crate owns the database connection lifecycle, query execution,
//! and result extraction. It takes SQL + parameters from fleet-core's
//! emitter and executes them against `DuckDB`.

pub mod error;
pub mod executor;
pub mod value;

pub use executor::{QueryEngine, SchemaIntrospector};
