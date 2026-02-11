//! DSL parser for fleet's query language.
//!
//! Transforms a query string like `service:nginx level:error last:2h | stats count() by host`
//! into a structured AST representation.
//!
//! The parser is built in layers:
//! 1. **Primitives** — numbers, strings, identifiers, operators, durations
//! 2. **Search tokens** — field filters, text search, time filters, quoted phrases
//! 3. **Search stage** — whitespace-separated tokens (implicit AND)
//! 4. **Expressions** — recursive expression parser with operator precedence
//! 5. **Pipe stages** — stats, where, sort, limit, table

mod expr;
mod pipe;
mod primitives;
mod search;

use crate::ast::Query;

/// An error produced by the parser, with source location and context.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: std::ops::Range<usize>,
    pub label: Option<String>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(label) = &self.label {
            write!(f, "{} ({})", self.message, label)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

/// Parse a fleet DSL query string into a structured AST.
///
/// # Errors
///
/// Returns a list of parse errors if the input is not valid fleet DSL.
pub fn parse(_input: &str) -> Result<Query, Vec<ParseError>> {
    todo!("wire up in commit 8")
}
