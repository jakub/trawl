//! DSL parser for trawl.s query language.
//!
//! Transforms a query string like `service=nginx level=error last=2h | stats count() by host`
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
pub mod suggest;

use chumsky::prelude::*;

use crate::ast::Query;
use primitives::{ParserExtra, ParserInput};

/// An error produced by the parser, with source location and context.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: std::ops::Range<usize>,
    pub label: Option<String>,
    /// Optional contextual suggestion (e.g. "did you mean 'stats'?").
    pub hint: Option<String>,
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
        if let Some(hint) = &self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

/// Maximum query length in bytes. Prevents denial-of-service via pathological parser input.
const MAX_QUERY_LEN: usize = 65_536;

/// Parse a trawl DSL query string into a structured AST.
///
/// # Errors
///
/// Returns a list of parse errors if the input is not valid trawl DSL.
pub fn parse(input: &str) -> Result<Query, Vec<ParseError>> {
    if input.len() > MAX_QUERY_LEN {
        return Err(vec![ParseError {
            message: format!(
                "query too long ({} bytes, max {MAX_QUERY_LEN})",
                input.len()
            ),
            span: 0..input.len(),
            label: None,
            hint: None,
        }]);
    }

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
    if let RichReason::Custom(msg) = e.reason() {
        return ParseError {
            message: msg.clone(),
            span: span.start..span.end,
            label,
            hint: None,
        };
    }

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

    // Detect specific error patterns and produce contextual messages + hints.
    let (message, hint) = enrich_error(input, offset, &found, &expected, label.as_deref());

    ParseError {
        message,
        span: span.start..span.end,
        label,
        hint,
    }
}

/// Find the pipe-command word around the error offset, if the error is in
/// a position that looks like a pipe stage command (i.e. the word follows a `|`).
///
/// Returns `Some(word)` if the word is NOT a known pipe stage (i.e. it's a typo),
/// or `None` if the error isn't in a pipe-command position or the word is valid.
fn find_pipe_command_word(input: &str, offset: usize) -> Option<String> {
    // Guard against byte offsets that land inside a multi-byte character.
    if !input.is_char_boundary(offset) {
        return None;
    }

    // Find the start of the word containing `offset` by scanning backward.
    let before = &input[..offset];
    let word_start = before
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map_or(0, |pos| {
            // Advance past the matched character (which may be multi-byte).
            let c = input[pos..].chars().next().unwrap_or(' ');
            pos + c.len_utf8()
        });

    // Extract the full word from word_start forward.
    let candidate: String = input[word_start..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();

    if candidate.is_empty() {
        return None;
    }

    // Check that there's a `|` before this word (with optional whitespace).
    let prefix = input[..word_start].trim_end();
    if !prefix.ends_with('|') {
        return None;
    }

    // Only return the word if it's NOT a known pipe stage (i.e. it's a typo).
    if suggest::KNOWN_PIPE_STAGES.contains(&candidate.as_str()) {
        return None;
    }

    Some(candidate)
}

/// Produce an enriched (message, hint) pair based on error context.
fn enrich_error(
    input: &str,
    offset: usize,
    found: &str,
    expected: &[String],
    label: Option<&str>,
) -> (String, Option<String>) {
    let expects_end_quote = expected.iter().any(|e| e == "'\"'");
    let expects_close_paren = expected.iter().any(|e| e == "')'");
    let at_end = offset >= input.len();

    // Unknown pipe stage: detected by scanning the input for a `|` before
    // the error position and extracting the word that follows it. chumsky's
    // choice() combinator may report character-level errors from the branch
    // that consumed the most input (e.g. "staats" partially matches "stats"),
    // resulting in no context labels — just a single-char expected token.
    // We recover the full word by scanning backward from the error offset.
    if let Some(ref word) = find_pipe_command_word(input, offset) {
        let hint = suggest::suggest_pipe_stage(word).map(|s| format!("did you mean '{s}'?"));
        return (format!("unknown command '{word}'"), hint);
    }

    // Unterminated string literal
    if at_end && expects_end_quote {
        return (
            "unterminated string literal".to_string(),
            Some("add a closing '\"' to complete the string".to_string()),
        );
    }

    // Unmatched opening parenthesis
    if at_end && expects_close_paren {
        return (
            "unmatched opening parenthesis".to_string(),
            Some("add a closing ')' to match the opening '('".to_string()),
        );
    }

    // Missing aggregation after stats
    if label == Some("stats stage") && expected.iter().any(|e| e == "aggregation expression") {
        return (
            format!("found {found}, expected aggregation expression"),
            Some("stats requires at least one aggregation, e.g. stats count()".to_string()),
        );
    }

    // Default: produce the standard message with no hint
    let message = if expected.is_empty() {
        format!("unexpected {found}")
    } else {
        format!("found {found}, expected {}", expected.join(" or "))
    };
    (message, None)
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
        let query = parse("service=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(query.pipeline.len(), 0);
    }

    #[test]
    fn test_multi_token_search() {
        let query = parse("service=nginx level=error last=2h").unwrap();
        // Time filter hoisted, 2 tokens remain in group.
        assert_eq!(query.search.groups[0].len(), 2);
        assert!(query.search.time_filter.is_some());
    }

    #[test]
    fn test_quoted_search() {
        let query = parse(r#""connection refused""#).unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "connection refused".to_string(),
            })
        );
    }

    #[test]
    fn test_negated_text_search() {
        let query = parse("-debug service=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 2);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

    #[test]
    fn test_search_with_stats() {
        let query = parse("service=nginx last=1h | stats count() by host").unwrap();
        // Time filter hoisted, 1 token remains in group.
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(query.search.time_filter.is_some());
        assert_eq!(query.pipeline.len(), 1);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
    }

    #[test]
    fn test_full_pipeline() {
        let query = parse("service=nginx | stats count() by host | where count > 10 | sort -count")
            .unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(query.pipeline.len(), 3);
        assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
        assert!(matches!(query.pipeline[1].node, PipeStage::Where(_)));
        assert!(matches!(query.pipeline[2].node, PipeStage::Sort(_)));
    }

    #[test]
    fn test_stats_avg_table() {
        let query =
            parse("service=nginx | stats avg(duration) by status | table status, avg_duration")
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
            parse("status>=400 last=24h | stats count() by host, uri | sort -count | limit 20")
                .unwrap();
        // Time filter hoisted, 1 token remains in group.
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(query.search.time_filter.is_some());
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
        assert!(query.search.groups.is_empty());
        assert_eq!(query.pipeline.len(), 0);
    }

    // --- error case tests ---

    #[test]
    fn test_error_invalid_pipe_stage() {
        let result = parse("service=nginx | bogus");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_error_missing_stats_agg() {
        let result = parse("service=nginx | stats");
        assert!(result.is_err());
    }

    #[test]
    fn test_error_unclosed_paren() {
        let result = parse("service=nginx | where (count > 10");
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
    fn test_query_length_limit_exceeded() {
        let long_query = "a".repeat(65_537);
        let result = parse(&long_query);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors[0].message.contains("query too long"));
    }

    #[test]
    fn test_query_at_length_limit_not_rejected_for_length() {
        // exactly at limit — should NOT fail with "query too long"
        // (may fail for syntax reasons, which is fine)
        let query = "a".repeat(65_536);
        let result = parse(&query);
        if let Err(errors) = &result {
            assert!(
                !errors.iter().any(|e| e.message.contains("query too long")),
                "should not be rejected for length at exact limit"
            );
        }
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

    // --- enriched error message tests ---

    #[test]
    fn test_error_unknown_command_with_suggestion() {
        let result = parse("| staats count() by host");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unknown command"),
            "message should say unknown command, got: {}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("staats"),
            "message should contain the typo, got: {}",
            errors[0].message
        );
        assert_eq!(
            errors[0].hint.as_deref(),
            Some("did you mean 'stats'?"),
            "hint should suggest 'stats'"
        );
    }

    #[test]
    fn test_error_unknown_command_no_suggestion() {
        let result = parse("| zzzzzzz");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unknown command"),
            "message should say unknown command, got: {}",
            errors[0].message
        );
        assert!(
            errors[0].hint.is_none(),
            "hint should be None for unrecognizable command"
        );
    }

    #[test]
    fn test_error_unterminated_string() {
        let result = parse(r#""unterminated string"#);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unterminated string"),
            "should detect unterminated string, got: {}",
            errors[0].message
        );
        assert!(errors[0].hint.is_some(), "should provide a hint");
    }

    #[test]
    fn test_error_unmatched_paren() {
        let result = parse("| where (count > 10");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].message.contains("unmatched opening parenthesis"),
            "should detect unmatched paren, got: {}",
            errors[0].message
        );
        assert!(errors[0].hint.is_some(), "should provide a hint");
    }

    #[test]
    fn test_error_display_with_hint() {
        let result = parse("| staats count()");
        let errors = result.unwrap_err();
        let msg = errors[0].to_string();
        assert!(
            msg.contains("did you mean"),
            "display should include hint, got: {msg}"
        );
    }

    #[test]
    fn test_error_whre_suggests_where() {
        let result = parse("| whre count > 10");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].hint.as_deref(), Some("did you mean 'where'?"));
    }
}
