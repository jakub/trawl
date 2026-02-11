//! fleet-core: DSL parser, AST definitions, and SQL emitter.
//!
//! This crate is the pure-logic heart of fleet. It has no I/O dependencies
//! and no database coupling — it transforms DSL query strings into SQL
//! strings with parameters. Everything here should be testable in isolation.

/// AST types representing parsed fleet DSL queries.
pub mod ast;

/// DSL parser — transforms query strings into AST.
pub mod parser;

/// SQL emitter — transforms AST into DuckDB-compatible SQL.
pub mod emitter;
