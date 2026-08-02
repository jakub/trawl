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

    /// The hot+cold union read matched no files even though cold parquet
    /// exists on disk — the substituted empty result would silently drop the
    /// cold data, which ADR-0008 forbids. Transient by nature: the read raced
    /// retention/compaction moving a file, or the hot snapshot vanished.
    #[error("query matched no files while cold data exists on disk; retry the query")]
    ColdDataUnread,

    /// Filesystem I/O error (e.g. writing temp files for parquet export).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

fn format_parse_errors(errors: &[ParseError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}
