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
pub mod compare;

/// Write-time conformance: the guarded cast binding a pinned column to
/// its catalog type, shared by compaction and the hot branch.
pub mod conform;

pub mod eval;

/// Which catalog keys a query binds — the incomplete-results notice's
/// input (ADR-0011 slice C1).
pub mod field_refs;

/// In-memory event filter compiled from the search stage.
pub mod filter;

/// The shared in-memory pin-aware comparison core (ADR-0011 slice A′):
/// one evaluator behind both the search-stage matcher and the pipeline
/// expression evaluator.
mod pin_match;

/// The compile-time pin scope walk: which catalog pin applies at each
/// pipeline stage, shared by the SQL emitter and the stream compiler.
pub mod pin_scope;

/// The names a projecting stage puts on its output row: the ONE aggregate
/// output-name rule, shared by the SQL emitter, the stream lane and the
/// pin-scope walk.
pub mod projection;

/// Client-chosen text made safe to render: control and format characters
/// that would rewrite or hide a terminal line.
pub mod sanitize;

/// The declared event envelope: field names, reserved keys, wire aliases.
pub mod schema;

/// OTel severity ladder: token tables, bands, syslog inversion.
pub mod severity;

/// Streaming pipeline compiler and executor.
pub mod stream;

/// Build-time version metadata.
pub mod version;
