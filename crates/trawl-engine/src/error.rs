// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the query engine.

use trawl_core::parser::ParseError;

/// Errors that can occur during query execution.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// One or more parse errors in the DSL query.
    #[error("parse error: {}", format_parse_errors(.0))]
    Parse(Vec<ParseError>),

    /// The emitter could not produce valid SQL from the AST.
    #[error("emit error: {0}")]
    Emit(#[from] trawl_core::emitter::EmitError),

    /// `DuckDB` returned an error during execution.
    #[error("database error: {0}")]
    Database(#[from] duckdb::Error),

    /// Query produced more rows than the configured limit.
    #[error("result too large: query returned more than {0} rows")]
    ResultTooLarge(usize),

    /// A read matched no files even though its source still reaches files on
    /// disk — the empty answer would silently drop the cold data, which
    /// ADR-0008 forbids. Raised by every lane (query and export, with a hot
    /// buffer and without), because the source is resolved before the read
    /// on all of them and the gate is the same. Transient by nature: the read
    /// raced retention/compaction/a repin moving a file, or the hot snapshot
    /// vanished.
    #[error("query matched no files while cold data exists on disk; retry the query")]
    ColdDataUnread,

    /// The query is wrong in a way the engine proved by asking `DuckDB`
    /// a question of its own, and no fallback can make it right.
    ///
    /// Distinct from [`Self::Emit`], which the hot lanes treat as a
    /// missing column: a column absent from cold parquet but present in
    /// a hot snapshot is a real query the cold read could not answer,
    /// so those lanes retry hot-only and answer an unanswerable one with
    /// an empty result (ADR-0009). A refusal is not that. Retrying it
    /// against a narrower source produces the same refusal, and turning
    /// it into an empty result would answer 200 with zero rows to a
    /// query that was never going to mean anything — which is how a
    /// `timechart` over a non-timestamp column escaped its own check.
    ///
    /// The message is trawl-authored and names the reader's own tokens,
    /// so it crosses the trust boundary intact, like a parse or emit
    /// refusal.
    #[error("{message}")]
    Refused { message: String },

    /// Filesystem I/O error (e.g. writing temp files for parquet export).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The caller's cancellation latch was set at the bind-to-execute
    /// boundary, so this lane stopped before running the statement
    /// (ADR-0024). Content-free: which query, and who cancelled it, is
    /// the caller's own record.
    #[error("query cancelled")]
    Cancelled,
}

fn format_parse_errors(errors: &[ParseError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}
