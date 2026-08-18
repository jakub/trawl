// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DSL parser for trawl.s query language.
//!
//! Transforms a query string like `service=nginx _severity=error last=2h | stats count() by host`
//! into a structured AST representation.
//!
//! The parser is built in layers:
//! 1. **Primitives** — numbers, strings, identifiers, operators, durations
//! 2. **Search tokens** — field filters, text search, time filters, quoted phrases
//! 3. **Search stage** — whitespace-separated tokens (implicit AND)
//! 4. **Expressions** — recursive expression parser with operator precedence
//! 5. **Pipe stages** — stats, where, sort, limit, table

pub(crate) mod comment;
pub(crate) mod expr;
pub(crate) mod pipe;
pub(crate) mod primitives;
pub mod scan;
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

/// The most diagnostics ONE parse reports, however many the grammar
/// emitted.
///
/// A single query can carry unboundedly many independent violations —
/// `f=#a,#b,#c,…` emits one per element — and every one of them renders a
/// hint quoting text from the input, so the reported bytes grow as the
/// product of the two. The cap is applied HERE, before rendering, so the
/// quadratic work is never done rather than merely never returned; the
/// server serializes every detail it is handed, so this is the bound the
/// wire sees too.
///
/// Eight, not one: a parse error list is a work list, and chumsky orders
/// it by position, so the first few are the ones a user reads.
///
/// The cap is not a plain truncation, because the list is not purely
/// positional: the emitted diagnostics come first, and the failure that
/// actually ENDED the parse is appended LAST. Truncating dropped it — an
/// eight-element `f=#a,…` list hid `unknown command 'bogus_stage'`, the
/// one error the query cannot succeed without fixing. So the report is
/// the first `MAX_REPORTED_ERRORS - 1` PLUS that final one, whose message
/// is annotated with how many were left out ([`omitted_suffix`]).
const MAX_REPORTED_ERRORS: usize = 8;

/// How the annotation on the last reported error reads, so the count the
/// cap swallowed is never silent. `{n}` diagnostics between the reported
/// prefix and the terminal error were not rendered.
fn omitted_suffix(n: usize) -> String {
    let plural = if n == 1 { "error" } else { "errors" };
    format!(" ({n} earlier {plural} omitted)")
}

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

    // Comments are a grammar production (ADR-0014), so the raw input goes
    // straight to the parser — no pre-pass, and error spans index the real
    // text rather than a byte-length-preserving copy of it.
    let parser = query_parser();
    let result = parser.parse(input);

    match result.into_result() {
        Ok(query) => Ok(query),
        Err(errors) => Err(report_errors(&errors, input)),
    }
}

/// Render at most [`MAX_REPORTED_ERRORS`] diagnostics from one parse,
/// keeping the TERMINAL failure whatever the emitted list does to the
/// budget. Rendering is done only for the errors reported, so a
/// pathological query costs a constant amount of work, not a product.
fn report_errors(errors: &[Rich<'_, char>], input: &str) -> Vec<ParseError> {
    if errors.len() <= MAX_REPORTED_ERRORS {
        return errors
            .iter()
            .map(|e| rich_to_parse_error(e, input))
            .collect();
    }

    let omitted = errors.len() - MAX_REPORTED_ERRORS;
    let mut reported: Vec<ParseError> = errors
        .iter()
        .take(MAX_REPORTED_ERRORS - 1)
        .map(|e| rich_to_parse_error(e, input))
        .collect();
    let mut last = rich_to_parse_error(
        errors.last().expect("the list is longer than the cap"),
        input,
    );
    last.message.push_str(&omitted_suffix(omitted));
    reported.push(last);
    reported
}

/// Convert a chumsky `Rich` error into our `ParseError` with a human-friendly message.
fn rich_to_parse_error(e: &Rich<'_, char>, input: &str) -> ParseError {
    use chumsky::error::RichReason;

    let span = e.span();
    let label = e.contexts().next().map(|(l, _)| l.to_string());

    // custom errors (e.g. overflow, invalid regex) carry their own message
    if let RichReason::Custom(raw) = e.reason() {
        // A comment diagnostic carries the exact token its production
        // held, past the message it renders (`comment::payload`); every
        // other custom error is its message and nothing else.
        let (msg, exact) = comment::split_payload(raw);
        return ParseError {
            message: msg.to_string(),
            hint: comment::hint_for(msg, exact),
            span: span.start..span.end,
            label,
        };
    }

    let offset = span.start;

    // A comment opener the grammar could not admit (ADR-0014 rulings 2
    // and 3). This runs BEFORE the generic enrichment because it owns its
    // own span: chumsky reports where the production died, which for
    // `| co#unt()` is the start of the stage word and for `// note` is one
    // slash of two — neither of which is the byte the user has to change.
    if let Some(d) = comment_diagnostic(input, offset) {
        return ParseError {
            message: d.message,
            span: d.span,
            label,
            hint: d.hint,
        };
    }

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
            chumsky::error::RichPattern::Token(c) => format!("'{}'", **c),
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

/// Whether `offset` sits at the start of the input or directly after an
/// ASCII whitespace character — the two positions the grammar admits a
/// comment at, asked of the one predicate that owns the question
/// ([`comment::opens_comment_after`], which [`comment::ws`] and
/// [`scan`] also go through).
fn preceded_by_whitespace(input: &str, offset: usize) -> bool {
    comment::opens_comment_after(input[..offset].chars().next_back())
}

/// A comment diagnostic, with the span of the byte the user must change.
struct CommentDiagnostic {
    message: String,
    hint: Option<String>,
    span: std::ops::Range<usize>,
}

impl CommentDiagnostic {
    /// No `exact`: this is the position-BLIND net, and a hint it minted a
    /// spelling for would be guessing at both the token's bounds and the
    /// position's escape (`comment::hint_for`).
    fn new(msg: &str, at: usize, len: usize) -> Self {
        Self {
            message: msg.to_string(),
            hint: comment::hint_for(msg, None),
            span: at..at + len,
        }
    }
}

/// The generic net for a comment opener the grammar could not admit, at
/// every position the two validators (the bare value and the search-stage
/// bare word) do not cover — a stage name, an expression, a sort key. It
/// exists so a `#` never surfaces as "found '#', expected …".
fn comment_diagnostic(input: &str, offset: usize) -> Option<CommentDiagnostic> {
    if offset >= input.len() {
        return None;
    }
    let rest = &input[offset..];

    // A `#` right where the parser died, and not at a boundary the
    // grammar would have admitted a comment at (ADR-0014 ruling 2).
    if rest.starts_with(comment::OPENER) && !preceded_by_whitespace(input, offset) {
        return Some(CommentDiagnostic::new(
            comment::MSG_OPENER_IN_TOKEN,
            offset,
            comment::OPENER.len_utf8(),
        ));
    }

    // …and the same net for the retired second opener, at the one place
    // it could have been a comment: a token boundary (ADR-0014 ruling 3).
    // The span is BOTH slashes — one of two underlines nothing the user
    // can act on.
    if comment::starts_with_slashes(rest) && preceded_by_whitespace(input, offset) {
        return Some(CommentDiagnostic::new(
            comment::MSG_SLASHES_NOT_A_COMMENT,
            offset,
            comment::SLASHES.len(),
        ));
    }

    // A `#` inside a PIPE STAGE NAME. chumsky reports the start of the
    // word (the stage word ends at the `#`), which the unknown-command
    // rule below then renders as `unknown command 'co'` plus a suggestion
    // for a typo the user did not make.
    let at = comment::opener_in_command_word(input, offset)?;
    Some(CommentDiagnostic::new(
        comment::MSG_OPENER_IN_COMMAND,
        at,
        comment::OPENER.len_utf8(),
    ))
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
    comment::leading_ws()
        .ignore_then(search::search_stage())
        .then(pipe::pipeline())
        .then_ignore(comment::ws())
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
        let query = parse("service=nginx _severity=error last=2h").unwrap();
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

    // --- comment tests ---

    #[test]
    fn test_hash_comment_at_start_of_input() {
        let query = parse("# this is a comment\nservice=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
    }

    /// `//` stopped being a comment opener (ADR-0014 ruling 3), and the
    /// removal is LOUD at the one place it could have been one: a bare
    /// term at a token boundary. Silently reading it as two AND-ed text
    /// terms would narrow the match set to nothing.
    #[test]
    fn test_double_slash_is_no_longer_a_comment() {
        let errors = parse("service=nginx // filter by service\n| stats count()").unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].message, comment::MSG_SLASHES_NOT_A_COMMENT);
        assert_eq!(errors[0].span, 14..16);
    }

    #[test]
    fn test_comment_inside_string_preserved() {
        let query = parse(r#""hello # world""#).unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "hello # world".to_string(),
            })
        );
    }

    /// A `//` in a VALUE is ordinary data and needs no quoting — that is
    /// the everyday half of ADR-0014 ruling 3.
    #[test]
    fn test_double_slash_in_a_value_is_data() {
        for (dsl, field, value) in [
            ("url=https://example.com/x", "url", "https://example.com/x"),
            ("path=/api//v1", "path", "/api//v1"),
            ("url=//cdn.example.com/x", "url", "//cdn.example.com/x"),
        ] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            assert_eq!(query.search.groups[0].len(), 1, "{dsl}");
            assert_eq!(
                query.search.groups[0][0].node,
                SearchToken::FieldFilter(FieldFilter {
                    field: field.to_string(),
                    op: FilterOp::Eq,
                    value: FilterValue::Literal(value.to_string()),
                }),
                "{dsl}"
            );
        }
        // …and a sibling filter on the same line survives, which the
        // blanking scanner deleted.
        let query = parse("referrer=https://a.b/c status=200").expect("both filters");
        assert_eq!(query.search.groups[0].len(), 2);
    }

    #[test]
    fn test_double_slash_inside_string_preserved() {
        let query = parse(r#""http://example.com""#).unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "http://example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_multiline_comments() {
        let query =
            parse("# find errors\nservice=nginx\n# only recent\nlast=1h | stats count() by host")
                .unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(query.search.time_filter.is_some());
        assert_eq!(query.pipeline.len(), 1);
    }

    #[test]
    fn test_comment_at_end_of_input() {
        let query = parse("service=nginx # trailing").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
    }

    #[test]
    fn test_only_comments() {
        let query = parse("# just a comment").unwrap();
        assert!(query.search.groups.is_empty());
        assert_eq!(query.pipeline.len(), 0);
    }

    #[test]
    fn test_only_comments_multiline() {
        let query = parse("# line 1\n# line 2\n# line 3").unwrap();
        assert!(query.search.groups.is_empty());
        assert_eq!(query.pipeline.len(), 0);
    }

    #[test]
    fn test_comment_error_spans_match_original() {
        // error should point to the original input position
        let result = parse("# comment\n| badstage");
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors[0].span.start >= 10, // after "# comment\n"
            "span should point into original input, got: {:?}",
            errors[0].span
        );
    }

    // ── backtick escape (ADR-0013 ruling 7) ─────────────────────────────

    /// A backticked NAME is a quoted context, so a `#` or `//` inside it
    /// is part of the name. Nothing re-derives that fact any more — the
    /// comment production is simply unreachable from inside
    /// [`primitives::quoted_name`].
    #[test]
    fn backticked_names_carry_comment_openers_verbatim() {
        for (dsl, want) in [("| table `a#b`", "a#b"), ("| table `a//b`", "a//b")] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            match &query.pipeline[0].node {
                PipeStage::Table(t) => assert_eq!(t.fields, vec![want.to_string()], "{dsl}"),
                other => panic!("expected Table, got {other:?}"),
            }
        }
        let query = parse("| table `a//b`, host").expect("parses");
        match &query.pipeline[0].node {
            PipeStage::Table(t) => {
                assert_eq!(t.fields, vec!["a//b".to_string(), "host".to_string()]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        // …and the name reaches the AST intact.
        let query = parse("| table `a#b`").expect("parses");
        match &query.pipeline[0].node {
            PipeStage::Table(t) => assert_eq!(t.fields, vec!["a#b".to_string()]),
            other => panic!("expected Table, got {other:?}"),
        }
        // A comment AFTER a closed backtick is still a comment.
        let query = parse("`a b`=x # trailing").expect("parses");
        assert_eq!(query.search.groups[0].len(), 1);
        match &query.search.groups[0][0].node {
            SearchToken::FieldFilter(ff) => assert_eq!(ff.field, "a b"),
            other => panic!("expected FieldFilter, got {other:?}"),
        }
    }

    /// A stray backtick inside a REGEX literal must not protect a later
    /// comment: the comment's own words would parse as extra AND-ed
    /// text-search terms and silently narrow the match set. The grammar
    /// answers this by construction now — a regex body is a production,
    /// not a scanner state — so a partner backtick later on cannot open a
    /// pseudo-name across the comment either.
    #[test]
    fn stray_backtick_does_not_shield_later_comments() {
        let query = parse("message=/back`tick/ # comment").expect("parses");
        assert_eq!(query.search.groups[0].len(), 1);

        // …and with a legitimate backticked name AFTER the comment, whose
        // presence used to open a pseudo-name across it.
        let input = "message=/a`b/ # secret note\n`req id`=1";
        let query = parse(input).unwrap_or_else(|e| panic!("{input:?} parses: {e:?}"));
        assert_eq!(
            query.search.groups[0].len(),
            2,
            "{input:?}: only the two filters, never the comment's words"
        );

        // The glob shape reaches the same invariant from the other side:
        // a bare value cannot absorb a tick, so the query is a loud parse
        // error rather than a comment quietly read as part of a value.
        assert!(parse("cmd=*`* # secret note\n`req id`=1").is_err());
    }

    /// No unquoted position absorbs a backtick (ADR-0013 ruling 7), so a
    /// value that runs into one is a loud parse error rather than a
    /// quietly different query.
    #[test]
    fn a_value_never_absorbs_a_backtick_shielded_comment() {
        for dsl in [
            "host=a+`b#c` # outside",
            "host=a*`b#c` # outside",
            "host=a%`b#c` # outside",
            "host=a-`b#c` # outside",
            // the plain typo, and the one that used to turn a comment's
            // own words into AND-ed search terms
            "service=`my service`",
            "host=`a # b` more",
            "status=200,`a # b` more",
        ] {
            assert!(parse(dsl).is_err(), "{dsl} must be a loud parse error");
        }

        // A value that genuinely contains a tick is double-quoted, and the
        // comment beside it is still a comment.
        let query = parse(r#"host="a+`b" # outside"#).expect("the quoted form parses");
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "host".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Literal("a+`b".to_string()),
            })
        );
    }

    /// A backticked name is a field reference in every field position,
    /// including directly after an operator — a descending sort key or
    /// arithmetic. It carries comment openers verbatim, because a quoted
    /// name has no whitespace-skip site inside it for the comment
    /// production to reach.
    #[test]
    fn a_backtick_after_an_operator_opens_a_name() {
        // the descending sort key, precisely: the name arrives whole
        for (dsl, want) in [
            ("* | sort -`a#b`", "a#b"),
            ("* | sort -`http://x`", "http://x"),
        ] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            match &query.pipeline[0].node {
                PipeStage::Sort(s) => {
                    assert_eq!(s.fields[0].field, want);
                    assert_eq!(s.fields[0].direction, SortDirection::Desc);
                }
                other => panic!("expected Sort, got {other:?}"),
            }
        }

        // …and every other operator an expression can put in front of one
        for dsl in [
            "* | let x = 1+`a#b`",
            "* | let x = 2*`http://x`",
            "* | let x = `a#b`%2",
            "* | let x = 1-`a#b`",
            "* | where `a#b`>1",
            "* | stats count() by `a#b`",
        ] {
            assert!(parse(dsl).is_ok(), "{dsl} must parse");
        }

        // …and a tick inside a REGEX is ordinary body content, with the
        // trailing comment still a comment.
        let query = parse("host=/a+b`c/ # | bad_stage").expect("the comment is a comment");
        assert_eq!(
            query.pipeline.len(),
            0,
            "the comment must not reach the parser"
        );
        assert_eq!(query.search.groups[0].len(), 1);
    }

    /// A negated quoted NAME has no production in the search grammar, so
    /// it is a loud parse error. Reading it as text would search for the
    /// literal ticks (matching nothing), and dropping the `-` to a term of
    /// its own would silently discard the negation.
    #[test]
    fn a_negated_backtick_is_a_parse_error() {
        for dsl in ["-`http-status`=500", "-`a b`", "error -`x", "-`"] {
            assert!(parse(dsl).is_err(), "{dsl} must be a loud parse error");
        }
        // `NOT` is the spelling that negates a field filter, and it works.
        let query = parse("NOT `http-status`=500").expect("NOT negates a quoted name");
        assert_eq!(query.search.groups[0].len(), 1);
        assert!(matches!(
            &query.search.groups[0][0].node,
            SearchToken::Not(_)
        ));
        // An ordinary negated term is untouched.
        let query = parse("-debug").expect("parses");
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

    /// An adversarial backtick run stays cheap. The retired scanner had a
    /// quadratic shape here (a re-scan to end-of-input from every backtick
    /// byte); the grammar has no such pass at all, so this is a plain
    /// smoke test that the shapes still terminate with an answer.
    #[test]
    fn a_backtick_run_parses_without_blowing_up() {
        let n = 65_536;
        for body in [
            "a``".repeat(n / 3),
            format!("{} x", "`".repeat(n - 2)),
            " ``a".repeat(n / 4),
        ] {
            let _ = parse(&body);
        }
    }

    /// The pathological corpus parses **inside a 2 MiB stack** — the size
    /// of a tokio worker thread, which is where trawld actually runs the
    /// parser.
    ///
    /// The padding rewrite (ADR-0014 ruling 4) put a project-owned
    /// combinator at every whitespace site, which deepens the parser's
    /// nested TYPE and so its per-frame stack cost. `cargo test`'s own
    /// threads get 8 MiB by default, so a suite that merely calls `parse`
    /// would keep passing while the server overflowed. This runs the
    /// corpus in a thread sized like the one that matters, and asserts it
    /// finishes rather than dies.
    #[test]
    fn the_pathological_corpus_parses_within_a_tokio_workers_stack() {
        /// A tokio worker thread's default stack.
        const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;

        let corpus = vec![
            format!("*{}", "| head 1 ".repeat(2000)),
            format!("* | where a =={}1", " ".repeat(100_000)),
            "a=1 ".repeat(10_000),
            format!("a=1{}b=2", " ".repeat(60_000)),
            format!("* | where {}1{}", "( ".repeat(300), ") ".repeat(300)),
        ];

        std::thread::Builder::new()
            .stack_size(TOKIO_WORKER_STACK)
            .spawn(move || {
                for body in &corpus {
                    // the ANSWER is not the point — reaching one is
                    let _ = parse(body);
                }
            })
            .expect("spawn")
            .join()
            .expect("the parser must not overflow a 2 MiB stack");
    }

    /// A LEADING backtick that fails the quoted production is a loud
    /// parse error, never a quiet slide into text search: unterminated,
    /// empty, or a character that cannot render as itself inside — a
    /// control character, but also the bidi and zero-width format
    /// characters `sanitize` exists to neutralise, since a name that
    /// parses is echoed verbatim into notices and error messages.
    #[test]
    fn leading_backtick_that_is_not_a_name_is_a_parse_error() {
        for input in [
            "`unterminated",
            "``",
            "`a\nb`",
            "`foo` bar",
            "`a\u{202e}b`",
            "`a\u{200b}b`",
            "`a\u{00ad}b`",
        ] {
            assert!(
                parse(input).is_err(),
                "{input:?} must be a loud parse error, not text search"
            );
        }
    }
}
