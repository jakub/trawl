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

    // Strip comments before parsing, replacing with spaces to preserve
    // byte offsets for error spans.
    let stripped = strip_comments(input);

    // If the stripped input is all whitespace (e.g. comment-only input),
    // parse empty string since the parser accepts "" but not "   ".
    let parse_input = if stripped.trim().is_empty() {
        ""
    } else {
        stripped.as_str()
    };

    let parser = query_parser();
    let result = parser.parse(parse_input);

    match result.into_result() {
        Ok(query) => Ok(query),
        Err(errors) => Err(errors
            .into_iter()
            .map(|e| rich_to_parse_error(&e, input))
            .collect()),
    }
}

/// Whether a backtick preceded by `prev` sits where a quoted field name
/// could START: the beginning of the input, after whitespace, or after one
/// of the bytes a name may follow directly — `(` and `,` (argument and
/// field lists), `|` (a stage boundary), `!` and the comparison bytes (a
/// filter or expression operator), and the arithmetic operators, which an
/// expression may equally put in front of a name (`` 1+`a b` ``). `-`
/// covers both the arithmetic case and a descending sort key
/// (`` | sort -`a#b` ``). A backtick anywhere else is inside some other
/// token, exactly as the search grammar reads a mid-word one as ordinary
/// text.
///
/// `/` is deliberately ABSENT: it opens a regex far more often than it
/// divides, and a regex body is exactly the context this predicate exists
/// to keep out. A quoted name divided into by a slash pays the
/// unterminated-name error instead.
///
/// The set may be widened safely only because no UNQUOTED position can
/// absorb a backtick: [`primitives::bare_value`] and the search grammar's
/// bare word both END at one, so a tick this predicate mis-reads as an
/// opener can reach a parse error but never a quietly different query.
fn opens_quoted_name(prev: Option<u8>) -> bool {
    match prev {
        None => true,
        Some(b) => {
            b.is_ascii_whitespace()
                || matches!(
                    b,
                    b'(' | b',' | b'|' | b'!' | b'=' | b'<' | b'>' | b'-' | b'+' | b'*' | b'%'
                )
        }
    }
}

/// Byte index of the backtick CLOSING a quoted name opened at `open`, or
/// `None` when the region would not lex as one.
///
/// Mirrors [`primitives::quoted_name`]'s two refusals — an empty name and
/// a character that cannot render as itself
/// ([`crate::sanitize::is_unsafe_display_char`], which covers the newline
/// like any other control) — plus the doubled backtick that escapes one.
fn quoted_name_end(input: &str, open: usize) -> Option<usize> {
    let rest = &input[open + 1..];
    let mut chars = rest.char_indices();
    let mut has_content = false;
    while let Some((off, c)) = chars.next() {
        if c == '`' {
            if rest[off + 1..].starts_with('`') {
                chars.next();
                has_content = true;
                continue;
            }
            return has_content.then_some(open + 1 + off);
        }
        if crate::sanitize::is_unsafe_display_char(c) {
            return None;
        }
        has_content = true;
    }
    None
}

/// Replace `//` and `#` comments with spaces, preserving byte positions.
///
/// Handles `//` (line comment) and `#` (line comment) outside of quoted
/// strings. Comment content is replaced with spaces so that error spans
/// remain accurate.
///
/// Backtick-quoted field names are tracked beside double-quoted strings,
/// so `` `a#b` `` is a name and not a comment (ADR-0013 ruling 7).
///
/// A backtick opens a name only where one could actually START
/// ([`opens_quoted_name`]) and only when what follows would really lex as
/// [`primitives::quoted_name`]: non-empty, closed, and free of the
/// characters that production refuses (controls, a newline included, plus
/// the bidi and zero-width formats). A stray backtick inside a regex
/// literal (`` message=/a`b/ ``) follows a body character, so it fails the
/// first test outright and cannot run a pseudo-name across the following
/// comment to smuggle its words in as extra AND-ed search terms.
///
/// This is a scan and not the grammar, so it can only ever be a
/// heuristic — an operator inside a bare VALUE looks exactly like
/// arithmetic from outside (`host=a+` reads like `1+`), and the tick after
/// it therefore engages the name state and shields a `#` behind it. The
/// GRAMMAR is the backstop that keeps a mis-engaged scan from changing an
/// answer quietly: no unquoted position may absorb a backtick
/// ([`primitives::bare_value`], and the search stage's bare word), so the
/// shielded region reaches a parse error instead of becoming part of a
/// value. Loud, or correct — never something silently different.
///
/// Known limitation: `#` inside regex literals (`/pattern#here/`) will be
/// treated as a comment start. Use `//` comments on lines containing regex
/// literals, or move the regex to a different line.
fn strip_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = bytes.to_vec();
    let len = bytes.len();
    let mut i = 0;
    let mut in_string = false;

    while i < len {
        if in_string {
            if bytes[i] == b'\\' && i + 1 < len {
                // skip escaped character inside string
                i += 2;
            } else if bytes[i] == b'"' {
                in_string = false;
                i += 1;
            } else {
                i += 1;
            }
        } else if bytes[i] == b'"' {
            in_string = true;
            i += 1;
        } else if bytes[i] == b'`' {
            // Only a region that would really lex as a name quotes anything;
            // anything else is an ordinary character. The name scan is
            // attempted ONLY from a position where a name could open — check
            // that cheap guard BEFORE the forward scan, never after. A stray
            // backtick (in a value, a regex, or an adversarial run) has some
            // non-opening byte before it, so it costs O(1) here instead of a
            // full forward scan; a scan therefore runs at most once per
            // genuine token boundary, and the whole pass stays linear. (The
            // guard-after-scan form re-scanned from every backtick byte, an
            // O(n^2) blowup a 64 KB backtick run could turn into ~1 s of CPU.)
            let opens = opens_quoted_name(i.checked_sub(1).map(|p| bytes[p]));
            match opens.then(|| quoted_name_end(input, i)).flatten() {
                Some(end) => i = end + 1,
                None => i += 1,
            }
        } else if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            // // comment — blank to end of line
            while i < len && bytes[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
        } else if bytes[i] == b'#' {
            // # comment — blank to end of line
            while i < len && bytes[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    // We only replaced ASCII bytes with ASCII spaces — multi-byte UTF-8
    // sequences are untouched (continuation bytes are >= 0x80, never
    // matching `"`, `#`, `/`, `\`, or `\n`). So from_utf8 always succeeds.
    String::from_utf8(out).expect("comment stripping only replaces ASCII bytes with spaces")
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
    fn test_hash_comment_stripped() {
        let query = parse("# this is a comment\nservice=nginx").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
    }

    #[test]
    fn test_double_slash_comment_stripped() {
        let query = parse("service=nginx // filter by service\n| stats count()").unwrap();
        assert_eq!(query.search.groups[0].len(), 1);
        assert_eq!(query.pipeline.len(), 1);
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
            parse("# find errors\nservice=nginx\n// only recent\nlast=1h | stats count() by host")
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
        let query = parse("# line 1\n// line 2\n# line 3").unwrap();
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

    #[test]
    fn strip_comments_preserves_length() {
        let input = "abc # comment\ndef // another\nghi";
        let stripped = strip_comments(input);
        assert_eq!(stripped.len(), input.len());
    }

    // ── backtick escape (ADR-0013 ruling 7) ─────────────────────────────

    /// Comment stripping tracks backticks beside double quotes, so a `#`
    /// or `//` INSIDE a quoted name is part of the name — length still
    /// preserved, since error spans point into the original input.
    #[test]
    fn strip_comments_leaves_backticked_names_alone() {
        for input in ["| table `a#b`", "| table `a//b`, host"] {
            let stripped = strip_comments(input);
            assert_eq!(stripped, input, "backtick content must survive");
            assert_eq!(stripped.len(), input.len());
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
    /// comment: the comment would parse as extra AND-ed text-search terms
    /// and silently narrow the match set. That holds whether or not a
    /// partner backtick appears later on: the stray one is not at a name's
    /// start, and the region it would open carries characters
    /// `quoted_name` refuses.
    #[test]
    fn stray_backtick_does_not_shield_later_comments() {
        let input = "message=/back`tick/ # comment";
        assert_eq!(strip_comments(input), "message=/back`tick/          ");
        let query = parse(input).expect("parses");
        assert_eq!(query.search.groups[0].len(), 1);

        // …and with a legitimate backticked name AFTER the comment, whose
        // presence used to open a pseudo-name across it.
        for input in [
            "message=/a`b/ # secret note\n`req id`=1",
            "message=/a`b/ // secret note\n`req id`=1",
        ] {
            let query = parse(input).unwrap_or_else(|e| panic!("{input:?} parses: {e:?}"));
            assert_eq!(
                query.search.groups[0].len(),
                2,
                "{input:?}: only the two filters, never the comment's words"
            );
        }

        // The glob shape reaches the same invariant from the other side:
        // `*` now opens a name for the scanner, so the tick is no longer
        // ordinary text — but a bare value cannot absorb it either, so the
        // query is a loud parse error rather than a comment quietly read
        // as part of a value.
        assert!(parse("cmd=*`* # secret note\n`req id`=1").is_err());
    }

    /// The scanner's engage rule is a heuristic — an operator inside a
    /// bare VALUE looks exactly like arithmetic from outside (`host=a+`
    /// vs `1+`), so the tick after it engages the name state and shields
    /// the `#` behind it. The grammar is the backstop: no unquoted
    /// position absorbs a tick, so the shielded region is a parse error
    /// rather than a comment silently folded into the query.
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

    /// An expression may put an ARITHMETIC operator in front of a name,
    /// and a descending sort key puts a `-` there, so those predecessors
    /// open a name too — otherwise `` -`a#b` `` loses its comment markers
    /// to the stripper and dies as an unterminated name, and a field
    /// `quote_dsl_field` happily renders could not be sorted descending.
    #[test]
    fn a_backtick_after_an_operator_opens_a_name() {
        // the descending sort key, precisely: the name arrives whole, with
        // the markers the stripper would have blanked
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

        // …and a tick inside a REGEX still follows a body character, never
        // an operator, so that line's comment is still stripped.
        let query = parse("host=/a+b`c/ # | bad_stage").expect("the comment must be stripped");
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

    /// `strip_comments` stays linear on an adversarial backtick run. The
    /// guard-after-scan form re-scanned to end-of-input from every backtick
    /// byte, so a 64 KB body of the shapes below burned 0.3–1.1 s of CPU
    /// (O(n^2)); the fixed pass is well under 5 ms. The bound is generous
    /// (200 ms) so it flags a return of the quadratic blowup — a ~40× jump
    /// on this input — without flaking on a loaded machine.
    #[test]
    fn strip_comments_is_linear_on_a_backtick_run() {
        let n = 65_536;
        for body in [
            "a``".repeat(n / 3),                // clean O(n^2): doubled ticks after content
            format!("{} x", "`".repeat(n - 2)), // worst case found: a solid tick run
            " ``a".repeat(n / 4),               // opens, fails, restarts
        ] {
            let start = std::time::Instant::now();
            let _ = strip_comments(&body);
            let elapsed = start.elapsed();
            assert!(
                elapsed < std::time::Duration::from_millis(200),
                "strip_comments took {elapsed:?} on a {}-byte backtick run — quadratic blowup is back",
                body.len()
            );
        }
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
