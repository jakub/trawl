// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-core: DSL parser, AST definitions, and SQL emitter.
//!
//! This crate is the pure-logic heart of trawl. It has no I/O dependencies
//! and no database coupling — it transforms DSL query strings into SQL
//! strings with parameters. Everything here should be testable in isolation.

/// AST types representing parsed trawl DSL queries.
pub mod ast;

/// DSL parser — transforms query strings into AST.
pub mod parser;

/// SQL emitter — transforms AST into DuckDB-compatible SQL.
pub mod emitter;

/// DSL query formatter — canonical pretty-printing of parsed queries.
pub mod format;

/// In-memory expression evaluator for streaming pipeline stages.
pub mod eval;

/// In-memory event filter compiled from the search stage.
pub mod filter;

/// Streaming pipeline compiler and executor.
pub mod stream;

/// Build-time version metadata.
pub mod version;
