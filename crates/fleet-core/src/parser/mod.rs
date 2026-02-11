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

pub(crate) mod expr;
pub(crate) mod pipe;
pub(crate) mod primitives;
pub(crate) mod search;

use chumsky::prelude::*;

use crate::ast::Query;
use primitives::{ParserExtra, ParserInput};

/// An error produced by the parser, with source location and context.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: std::ops::Range<usize>,
    pub label: Option<String>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}..{}] {}",
            self.span.start, self.span.end, self.message
        )?;
        if let Some(label) = &self.label {
            write!(f, " (while parsing {label})")?;
        }
        Ok(())
    }
}

/// Parse a fleet DSL query string into a structured AST.
///
/// # Errors
///
/// Returns a list of parse errors if the input is not valid fleet DSL.
pub fn parse(input: &str) -> Result<Query, Vec<ParseError>> {
    let parser = query_parser();
    let result = parser.parse(input);

    match result.into_result() {
        Ok(query) => Ok(query),
        Err(errors) => Err(errors
            .into_iter()
            .map(|e| rich_to_parse_error(&e, input))
            .collect()),
    }
}

/// Convert a chumsky `Rich` error into our `ParseError` with a human-friendly message.
fn rich_to_parse_error(e: &Rich<'_, char>, input: &str) -> ParseError {
    use chumsky::error::RichReason;

    let span = e.span();
    let label = e.contexts().next().map(|(l, _)| l.to_string());

    // custom errors (e.g. overflow, invalid regex) carry their own message
    let message = if let RichReason::Custom(msg) = e.reason() {
        msg.clone()
    } else {
        let offset = span.start;
        let found = if offset >= input.len() {
            "end of input".to_string()
        } else {
            let ch = &input[offset..];
            let end = ch.char_indices().nth(1).map_or(ch.len(), |(idx, _)| idx);
            format!("'{}'", &ch[..end])
        };

        let expected: Vec<String> = e
            .expected()
            .map(|exp| match exp {
                chumsky::error::RichPattern::Token(c) => format!("'{}'", &**c),
                chumsky::error::RichPattern::Label(l) => l.to_string(),
                chumsky::error::RichPattern::Identifier(id) => format!("`{id}`"),
                chumsky::error::RichPattern::Any => "any token".to_string(),
                chumsky::error::RichPattern::SomethingElse => "something else".to_string(),
                chumsky::error::RichPattern::EndOfInput => "end of input".to_string(),
                _ => "unknown".to_string(),
            })
            .collect();

        if expected.is_empty() {
            format!("unexpected {found}")
        } else {
            format!("found {found}, expected {}", expected.join(" or "))
        }
    };

    ParseError {
        message,
        span: span.start..span.end,
        label,
    }
}

/// Build the top-level query parser: search stage, then pipeline, then EOF.
fn query_parser<'src>() -> impl Parser<'src, ParserInput<'src>, Query, ParserExtra<'src>> {
    search::search_stage()
        .then(pipe::pipeline())
        .then_ignore(end())
        .map(|(search, pipeline)| Query { search, pipeline })
        .labelled("query")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;

    #[test]
    fn test_simple_field_filter() {
        let query = parse("service:nginx").unwrap();
        assert_eq!(query.search.tokens.len(), 1);
        assert_eq!(query.pipeline.len(), 0);
    }

    #[test]
    fn test_multi_token_search() {
        let query = parse("service:nginx level:error last:2h").unwrap();
        assert_eq!(query.search.tokens.len(), 3);
    }

    #[test]
    fn test_quoted_search() {
        let query = parse(r#""connection refused""#).unwrap();
        assert_eq!(query.search.tokens.len(), 1);
        assert_eq!(
            query.search.tokens[0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "connection refused".to_string(),
            })
        );
    }

    #[test]
    fn test_negated_text_search() {
        let query = parse("-debug service:nginx").unwrap();
        assert_eq!(query.search.tokens.len(), 2);
        assert_eq!(
            query.search.tokens[0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

    #[test]
    fn test_search_with_stats() {
        let query = parse("service:nginx last:1h | stats count() by host").unwrap();
        assert_eq!(query.search.tokens.len(), 2);
        assert_eq!(query.pipeline.len(), 1);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
    }

    #[test]
    fn test_full_pipeline() {
        let query = parse("service:nginx | stats count() by host | where count > 10 | sort -count")
            .unwrap();
        assert_eq!(query.search.tokens.len(), 1);
        assert_eq!(query.pipeline.len(), 3);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
        assert!(matches!(query.pipeline[1].node, PipeStage::Where(_)));
        assert!(matches!(query.pipeline[2].node, PipeStage::Sort(_)));
    }

    #[test]
    fn test_stats_avg_table() {
        let query =
            parse("service:nginx | stats avg(duration) by status | table status, avg_duration")
                .unwrap();
        assert_eq!(query.pipeline.len(), 2);
        match &query.pipeline[0].node {
            PipeStage::Stats(stats) => {
                assert_eq!(stats.aggregations[0].function, "avg");
                assert_eq!(stats.group_by, vec!["status".to_string()]);
            }
            other => panic!("expected Stats, got {other:?}"),
        }
        match &query.pipeline[1].node {
            PipeStage::Table(table) => {
                assert_eq!(
                    table.fields,
                    vec!["status".to_string(), "avg_duration".to_string()]
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn test_complex_query() {
        let query =
            parse("status:>=400 last:24h | stats count() by host, uri | sort -count | limit 20")
                .unwrap();
        assert_eq!(query.search.tokens.len(), 2);
        assert_eq!(query.pipeline.len(), 3);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
        assert!(matches!(query.pipeline[1].node, PipeStage::Sort(_)));
        assert!(matches!(query.pipeline[2].node, PipeStage::Limit(_)));
    }

    #[test]
    fn test_parse_error() {
        let result = parse("| | invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_query() {
        let query = parse("").unwrap();
        assert_eq!(query.search.tokens.len(), 0);
        assert_eq!(query.pipeline.len(), 0);
    }

    // --- error case tests ---

    #[test]
    fn test_error_invalid_pipe_stage() {
        let result = parse("service:nginx | bogus");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_error_missing_stats_agg() {
        let result = parse("service:nginx | stats");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_unclosed_paren() {
        let result = parse("service:nginx | where (count > 10");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_unclosed_quote() {
        let result = parse(r#""unterminated string"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_error_message_has_span() {
        let result = parse("| badstage");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        // errors should have valid span information
        for error in &errors {
            assert!(error.span.start <= error.span.end);
        }
    }

    #[test]
    fn test_error_integer_overflow_no_panic() {
        // overflowing integers must produce a parse error, not a panic
        let result = parse("x | where count > 99999999999999999999999");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_display() {
        let result = parse("| badstage");
        let errors = result.unwrap_err();
        let msg = errors[0].to_string();
        // should contain span info and the error description
        assert!(msg.contains('['));
        assert!(msg.contains(']'));
    }
}
