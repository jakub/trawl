//! DSL parser for fleet's query language.
//!
//! Transforms a query string like `service:nginx level:error last:2h | stats count() by host`
//! into a structured AST representation.
