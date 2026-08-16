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

/// Replace `//` and `#` comments with spaces, preserving byte positions.
///
/// Handles `//` (line comment) and `#` (line comment) outside of quoted
/// strings and backtick-quoted field names. This runs BEFORE the grammar, so
/// it is the only layer that can protect a name's bytes: without the
/// backtick state `` | table `a#b` `` and `` | table `http://x` `` are gutted
/// before the field parser ever sees them. A backtick region has no
/// backslash escape — the only exit is a tick, and a doubled tick is data.
///
/// This scanner cannot parse, so it engages that state CONSERVATIVELY —
/// two conditions, both needed, because a tick is also ordinary data inside
/// a regex or a bare filter value ([`primitives::bare_value`] admits it):
///
/// 1. the tick must sit where a field name can syntactically START
///    ([`can_start_field_name`]), which a tick inside a regex body does not;
/// 2. the region must CLOSE on the same line. An unterminated tick that
///    swallowed the rest of the line would leave a real comment unstripped,
///    and a comment made of bare words parses as extra search terms —
///    silently answering a different question. Declining to engage there
///    puts the comment back in the stripper's hands, and the leftover tick
///    then dies at the grammar (text search and the `NOT` lookahead both
///    refuse it) rather than quietly meaning something.
///
/// Residual, stated: a bare value that is itself tick-delimited
/// (`` service=`a#b` ``) satisfies both conditions and keeps its `#`, since
/// at this layer it is indistinguishable from a field name after `==`.
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
    let mut in_backtick = false;

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
        } else if in_backtick {
            if bytes[i] == b'`' {
                if i + 1 < len && bytes[i + 1] == b'`' {
                    // doubled tick — one escaped backtick of name, still inside
                    i += 2;
                } else {
                    in_backtick = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
        } else if bytes[i] == b'"' {
            in_string = true;
            i += 1;
        } else if bytes[i] == b'`'
            && can_start_field_name(bytes, i)
            && backtick_closes_on_line(bytes, i)
        {
            in_backtick = true;
            i += 1;
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

/// Whether a field name could begin at `i` — judged from the byte before it,
/// which is all a pre-parse scanner has.
///
/// The permitted predecessors are the ones the grammar actually puts in
/// front of a name with no whitespace: the start of input or any
/// whitespace, an opening paren or comma (argument and field lists), a
/// pipe, the tail of a comparison operator (`=`, `<`, `>`, `!`) — an
/// expression may compare against a field, so `` a==`b c` `` is a name —
/// and the arithmetic operators, which an expression may equally put in
/// front of one (`` 1+`a b` ``). `-` covers both the arithmetic case and a
/// descending sort key. Anything else (a letter, a digit) means the tick
/// is inside a value or a regex, where it is ordinary data.
///
/// `/` is deliberately ABSENT: it opens a regex far more often than it
/// divides, and a regex body is exactly the context this predicate exists
/// to keep out. A quoted name divided into by a slash pays the
/// unterminated-name error instead.
///
/// Erring narrow is the safe direction: a name this declines is one whose
/// comment markers stay unprotected, so it dies loudly at the grammar as an
/// unterminated name. Erring wide would hide a comment.
fn can_start_field_name(bytes: &[u8], i: usize) -> bool {
    let Some(prev) = i.checked_sub(1).map(|p| bytes[p]) else {
        return true;
    };
    prev.is_ascii_whitespace()
        || matches!(
            prev,
            b'(' | b',' | b'|' | b'-' | b'=' | b'<' | b'>' | b'!' | b'+' | b'*' | b'%'
        )
}

/// Whether the backtick region opening at `i` closes before the next newline,
/// counting a doubled tick as data rather than a delimiter.
fn backtick_closes_on_line(bytes: &[u8], i: usize) -> bool {
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'`' if bytes.get(j + 1) == Some(&b'`') => j += 2,
            b'`' => return true,
            b'\n' => return false,
            _ => j += 1,
        }
    }
    false
}

/// Spellings the grammar reads as something other than a field name, in at
/// least one field position.
///
/// Two groups, both decided by reading the grammar rather than by taste:
/// the words `parser::expr` binds before it ever tries `field_ref`
/// (literals and operator keywords), and the clause keywords a stage
/// consumes before its own field list. The three unconditional search
/// keywords (`last`, `earliest`, `latest`) are here for the same reason —
/// ADR-0013 ruling 7 keeps them unconditional, which is exactly what makes
/// the bare spelling unusable as a name.
///
/// Stage names are deliberately absent: a field position never begins a
/// stage, so `| table stats` names the column `stats`. The round-trip
/// property test is what proves this set is complete, not the list itself.
const AMBIGUOUS_BARE_NAMES: &[&str] = &[
    // literals `parser::primitives::literal` binds first
    "true", "false", "null", // operator keywords in expression position
    "and", "or", "not", "in", "matches", "like", "ilike",
    // clause keywords a stage reads before its field list
    "as", "by", "on", "from", "sep", "kv", "span", "run", "saved", "all",
    // the unconditional search keywords (ADR-0013 ruling 7)
    "last", "earliest", "latest",
    // the search stage's own OR/NOT markers, tried before any leaf token
    "OR", "NOT",
];

/// Whether `name` can be written into the DSL with no backticks and mean
/// itself in every field position.
///
/// Two conditions, and the second is not optional: the name must match the
/// unquoted grammar ([`primitives::plain_name`] — an identifier with
/// optional dots, or an `@`-prefixed one), AND it must not be a spelling
/// the grammar reads as something else first. `true` and `by` are perfectly
/// good identifiers that never reach `field_ref`.
#[must_use]
pub fn is_bare_dsl_name(name: &str) -> bool {
    if AMBIGUOUS_BARE_NAMES.contains(&name) {
        return false;
    }
    let body = name.strip_prefix('@').unwrap_or(name);
    !body.is_empty()
        && body.split('.').all(|segment| {
            let mut chars = segment.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// `name` rendered as DSL text: itself when bare-safe, else backtick-quoted
/// with embedded backticks doubled.
///
/// `None` when the grammar cannot express the name at all — it is empty, or
/// carries a character [`primitives::backtick_name`] refuses. Callers
/// decline to offer such a name, the way the repin hint declines a name it
/// cannot print safely; inventing a lossy spelling would offer a name that
/// is not the field.
///
/// This is the ONE renderer. Every surface that manufactures DSL from a
/// field name goes through it — hand-rolled backtick wrapping is drift.
#[must_use]
pub fn quote_dsl_name(name: &str) -> Option<String> {
    if name.is_empty() || name.chars().any(crate::sanitize::is_unsafe_display_char) {
        return None;
    }
    if is_bare_dsl_name(name) {
        return Some(name.to_string());
    }
    Some(format!("`{}`", name.replace('`', "``")))
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

    // --- the quoting helper, and its drift guards ---

    /// Names chosen to break a naive helper: every grammar keyword and
    /// literal, the three unconditional search spellings, stage names,
    /// and the shapes only backticks can express.
    ///
    /// The keyword half is written out here rather than read from
    /// [`AMBIGUOUS_BARE_NAMES`] ON PURPOSE — drawing the corpus from the
    /// constant under test would make dropping a keyword invisible, since
    /// the name would leave the corpus with it. This list comes from
    /// grepping `keyword(...)` and `just("...=")` out of the grammar.
    fn adversarial_names() -> Vec<&'static str> {
        vec![
            // shapes the bare grammar cannot express at all
            "request id",
            "http-status",
            "a`b",
            "a\"b",
            "a b`c",
            "日本語",
            "2fast",
            "a-b.c",
            "",
            " ",
            "a=b",
            "a,b",
            "a|b",
            "a#b",
            "http://x",
            "(a)",
            "*",
            // shapes it can — including the ones the FOLD makes equal
            "host",
            "host.name",
            "@timestamp",
            "_time",
            "Dur",
            "dur",
            "count",
            "stats",
            "where",
            "table",
            "sort",
            "limit",
            "eventstats",
            "timechart",
            // every keyword spelling the grammar matches
            "true",
            "false",
            "null",
            "and",
            "or",
            "not",
            "OR",
            "NOT",
            "in",
            "matches",
            "like",
            "ilike",
            "as",
            "by",
            "on",
            "from",
            "sep",
            "kv",
            "span",
            "run",
            "saved",
            "all",
            "last",
            "earliest",
            "latest",
            "head",
            "tail",
            "drop",
            "dedup",
            "rare",
            "top",
            "let",
            "eval",
            "rename",
            "pivot",
            "sample",
            "fields",
        ]
    }

    /// A field position for each door the helper's output must survive.
    /// Read positions only: a write position adds the sealed-prefix policy,
    /// which is not a quoting question.
    fn read_positions(rendered: &str) -> Vec<String> {
        vec![
            format!("{rendered}=1"),
            format!("* | where {rendered} == 1"),
            format!("* | stats count() by {rendered}"),
            format!("* | table {rendered}"),
            format!("* | sort -{rendered}"),
            format!("* | top 5 {rendered}"),
        ]
    }

    /// The helper is the grammar's inverse: whatever it renders parses back
    /// to the name it was given, in every field position — that is the
    /// contract every suggestion surface leans on.
    #[test]
    fn quote_dsl_name_round_trips() {
        for original in adversarial_names() {
            let Some(rendered) = quote_dsl_name(original) else {
                // refused names are the ones the grammar cannot express;
                // a caller declines to offer them rather than inventing one.
                assert!(
                    original.is_empty(),
                    "{original:?} was refused but is expressible"
                );
                continue;
            };
            for dsl in read_positions(&rendered) {
                let query = parse(&dsl)
                    .unwrap_or_else(|e| panic!("{original:?} rendered as {dsl:?}: {e:?}"));
                assert!(
                    field_positions(&query).iter().any(|n| n == original),
                    "{original:?} rendered as {dsl:?} parsed as {:?}",
                    field_positions(&query)
                );
            }
        }
    }

    /// The half a forgotten keyword breaks: if the helper calls a name
    /// bare-safe, the BARE spelling must mean that name in every position.
    /// `true` and `by` are good identifiers that never reach `field_ref`.
    #[test]
    fn is_bare_dsl_name_agrees_with_the_grammar() {
        for original in adversarial_names() {
            if !is_bare_dsl_name(original) {
                continue;
            }
            for dsl in read_positions(original) {
                let query = parse(&dsl)
                    .unwrap_or_else(|e| panic!("{original:?} claimed bare-safe: {dsl:?}: {e:?}"));
                assert!(
                    field_positions(&query).iter().any(|n| n == original),
                    "{original:?} claimed bare-safe but {dsl:?} parsed as {:?}",
                    field_positions(&query)
                );
            }
        }
    }

    /// A name the grammar cannot express has no rendering — the helper
    /// says so rather than inventing one, matching the repin hint's
    /// "a name that cannot be printed gets no command" precedent.
    #[test]
    fn quote_dsl_name_refuses_the_inexpressible() {
        assert_eq!(quote_dsl_name(""), None);
        for hostile in ["a\u{202e}b", "a\u{200b}b", "a\u{1}b", "a\u{00ad}b"] {
            assert_eq!(quote_dsl_name(hostile), None, "{hostile:?}");
        }
    }

    // --- backtick-quoted field names (ADR-0013 ruling 7) ---

    /// Every name a query wrote in a FIELD position, in AST order.
    ///
    /// The position table below asserts against this rather than against
    /// "the parse succeeded", because a backtick arm that reached only
    /// SOME positions would not error: the unreached name degrades into a
    /// text search and the query still parses. Where the name landed is
    /// the whole question.
    fn walk_expr(expr: &Expr, out: &mut Vec<String>) {
        match expr {
            Expr::FieldRef(name) => out.push(name.clone()),
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    walk_expr(&a.node, out);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                walk_expr(&lhs.node, out);
                walk_expr(&rhs.node, out);
            }
            Expr::Unary { operand, .. } => walk_expr(&operand.node, out),
            Expr::InList { expr, list } => {
                walk_expr(&expr.node, out);
                for item in list {
                    walk_expr(&item.node, out);
                }
            }
            Expr::Literal(_) => {}
        }
    }

    fn walk_token(token: &SearchToken, out: &mut Vec<String>) {
        match token {
            SearchToken::FieldFilter(f) => out.push(f.field.clone()),
            SearchToken::Not(inner) => walk_token(&inner.node, out),
            SearchToken::Group(groups) => {
                for group in groups {
                    for t in group {
                        walk_token(&t.node, out);
                    }
                }
            }
            _ => {}
        }
    }

    fn walk_aggs(aggs: &[AggExpr], out: &mut Vec<String>) {
        for agg in aggs {
            for a in &agg.args {
                walk_expr(&a.node, out);
            }
            if let Some(alias) = &agg.alias {
                out.push(alias.clone());
            }
        }
    }

    fn field_positions(query: &Query) -> Vec<String> {
        let mut out = Vec::new();
        for group in &query.search.groups {
            for token in group {
                walk_token(&token.node, &mut out);
            }
        }
        for stage in &query.pipeline {
            match &stage.node {
                PipeStage::Stats(s) => {
                    walk_aggs(&s.aggregations, &mut out);
                    out.extend(s.group_by.iter().cloned());
                }
                PipeStage::EventStats(s) => {
                    walk_aggs(&s.aggregations, &mut out);
                    out.extend(s.group_by.iter().cloned());
                }
                PipeStage::Timechart(s) => {
                    walk_aggs(&s.aggregations, &mut out);
                    out.extend(s.group_by.iter().cloned());
                }
                PipeStage::Pivot(s) => {
                    walk_aggs(std::slice::from_ref(&s.aggregation), &mut out);
                    out.push(s.on_field.clone());
                    out.extend(s.by.iter().cloned());
                }
                PipeStage::Where(s) => walk_expr(&s.condition.node, &mut out),
                PipeStage::Let(s) => {
                    for (target, value) in &s.assignments {
                        out.push(target.clone());
                        walk_expr(&value.node, &mut out);
                    }
                }
                PipeStage::Sort(s) => out.extend(s.fields.iter().map(|f| f.field.clone())),
                PipeStage::Table(s) => out.extend(s.fields.iter().cloned()),
                PipeStage::Drop(s) => out.extend(s.fields.iter().cloned()),
                PipeStage::Dedup(s) => out.extend(s.fields.iter().cloned()),
                PipeStage::Top(s) => {
                    out.push(s.field.clone());
                    out.extend(s.by.iter().cloned());
                }
                PipeStage::Rare(s) => {
                    out.push(s.field.clone());
                    out.extend(s.by.iter().cloned());
                }
                PipeStage::Rename(s) => {
                    for (from, to) in &s.renames {
                        out.push(from.clone());
                        out.push(to.clone());
                    }
                }
                PipeStage::Extract(s) => out.extend(s.source_field.iter().cloned()),
                _ => {}
            }
        }
        out
    }

    /// One production feeds every field-name position, so a backticked
    /// name reaches all of them — decoded, verbatim, and as ONE name (a
    /// dot inside the ticks is a literal, exactly as a bare dotted name is
    /// one column reference).
    #[test]
    fn backticks_are_accepted_in_every_field_position() {
        let cases: &[(&str, &[&str])] = &[
            // search stage — the .rewind() lookahead AND the committed parse
            ("`request id`=5", &["request id"]),
            ("NOT `request id`=5", &["request id"]),
            ("(`request id`=5 OR `a b`=6)", &["request id", "a b"]),
            // expressions: where / let RHS / in-list / function arg
            ("* | where `http status` > 400", &["http status"]),
            ("* | where `a b` in (1, 2)", &["a b"]),
            ("* | where lower(`a b`) == \"x\"", &["a b"]),
            // aggregation argument, by-key and explicit alias
            (
                "* | stats avg(`resp ms`) as `mean ms` by `a b`",
                &["resp ms", "mean ms", "a b"],
            ),
            (
                "* | eventstats count() as `n rows` by `a b`",
                &["n rows", "a b"],
            ),
            (
                "* | timechart span=5m count() as `n` by `a b`",
                &["n", "a b"],
            ),
            ("* | pivot count() on `a b` by `c d`", &["a b", "c d"]),
            // projection and ordering stages
            (
                "* | table `request id`, `http-status`",
                &["request id", "http-status"],
            ),
            ("* | fields `request id`", &["request id"]),
            ("* | sort -`request id`", &["request id"]),
            ("* | drop `request id`", &["request id"]),
            ("* | dedup `request id`, `a b`", &["request id", "a b"]),
            ("* | top 5 `a b` by `c d`", &["a b", "c d"]),
            ("* | rare 5 `a b` by `c d`", &["a b", "c d"]),
            // write positions
            ("* | let `a b` = 1", &["a b"]),
            ("* | eval `a b` = `c d` + 1", &["a b", "c d"]),
            ("* | rename `a b` as `c d`", &["a b", "c d"]),
            // extract source
            ("* | extract kv from `a b`", &["a b"]),
            (r#"* | extract "(?P<x>.)" from `a b`"#, &["a b"]),
            // a dot inside the ticks is data, not a path
            ("* | table `a.b`", &["a.b"]),
        ];

        for (dsl, expected) in cases {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            let names = field_positions(&query);
            for want in *expected {
                assert!(
                    names.iter().any(|n| n == want),
                    "{dsl}: expected the name {want:?} in a field position, got {names:?}"
                );
            }
        }
    }

    /// A backtick is a metacharacter, so a malformed one is a parse ERROR —
    /// never a text search that quietly answers a different question.
    #[test]
    fn malformed_backticks_never_degrade_into_text_search() {
        for dsl in [
            "``=x",                    // empty name
            "`unterminated=5",         // no closing tick
            "`a\u{202e}b`=5",          // bidi override in the name
            "`a\u{0007}b`=5",          // control character in the name
            "`request id`",            // a quoted name is not a search term
            "* | table `unterminated", // and not only in the search stage
        ] {
            assert!(
                parse(dsl).is_err(),
                "{dsl} must be a parse error, got {:?}",
                parse(dsl).map(|q| field_positions(&q))
            );
        }
    }

    /// AC1: the two spellings coexist. `last=` is an unconditional keyword
    /// (ADR-0013 ruling 7) and `` `last` `` is the field of that name —
    /// and both `field_filter` sites see the same production, so the
    /// lookahead cannot admit a name the committed parse then refuses.
    #[test]
    fn backticked_last_is_a_field_beside_the_last_keyword() {
        let query = parse("`last`=5 last=2h").unwrap();
        assert_eq!(field_positions(&query), vec!["last".to_string()]);
        let tf = query
            .search
            .time_filter
            .expect("last=2h is the time filter");
        assert_eq!(tf.node.duration.quantity, 2);
        assert_eq!(tf.node.duration.unit, TimeUnit::Hours);
    }

    /// A doubled tick is one literal backtick of NAME; the pair is data,
    /// not a delimiter.
    #[test]
    fn doubled_backticks_escape_one_backtick() {
        let query = parse("* | table `a``b`").unwrap();
        assert_eq!(field_positions(&query), vec!["a`b".to_string()]);
    }

    /// Quoting is not an escape from policy (ADR-0013 ruling 7): the
    /// sealed `_` prefix is refused at the same door, through backticks.
    #[test]
    fn backticks_do_not_unseal_the_reserved_namespace() {
        for dsl in [
            "* | let `_foo` = 1",
            "* | eval `_severity` = 17",
            "* | rename service as `_svc`",
            "* | stats count() as `_severity`",
        ] {
            assert!(parse(dsl).is_err(), "{dsl} must be a parse error");
        }
    }

    /// A function is not a field, so the escape stops at call position
    /// (AC5): `` `lower`(x) `` reads as the field `lower` with an
    /// unconsumed `(x)` after it.
    #[test]
    fn backticks_are_refused_where_a_name_is_not_a_field() {
        for dsl in [
            "* | where `lower`(x) == 1",
            "* | stats `count`()",
            "* | from saved `my report`",
        ] {
            assert!(parse(dsl).is_err(), "{dsl} must be a parse error");
        }
        // …while the double-quoted saved-query name still works.
        assert!(parse(r#"* | from saved "my report""#).is_ok());
    }

    /// `strip_comments` runs before the grammar, so it is the only layer
    /// that can keep a comment marker inside a name from being blanked.
    #[test]
    fn comment_markers_inside_a_backticked_name_survive() {
        for (dsl, want) in [
            ("* | table `a#b`", "a#b"),
            ("* | table `http://x`", "http://x"),
            ("* | table `a``#b`", "a`#b"),
        ] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            assert_eq!(field_positions(&query), vec![want.to_string()], "{dsl}");
        }
        // …and a comment outside one is still a comment.
        let query = parse("# leading\n* | table `a b`").unwrap();
        assert_eq!(field_positions(&query), vec!["a b".to_string()]);
    }

    /// A tick inside a regex or a bare value is DATA, not the opening of a
    /// name, so comment stripping there is exactly what it was before
    /// backticks existed. The scanner decides by position, since it cannot
    /// parse: a regex body puts a letter in front of the tick.
    #[test]
    fn a_backtick_inside_a_value_does_not_shield_a_comment() {
        let query = parse("host=/foo`bar/ # | bad_stage").expect("the comment must be stripped");
        assert_eq!(
            query.pipeline.len(),
            0,
            "the comment must not reach the parser"
        );
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "host".to_string(),
                op: FilterOp::Regex,
                value: FilterValue::Literal("foo`bar".to_string()),
            })
        );
        // …and the same through the formatter, which is what the server's
        // /validate hands back to a client.
        assert_eq!(
            crate::format::reformat("host=/foo`bar/ # | bad_stage").as_deref(),
            Some("host=/foo`bar/")
        );
    }

    /// An unterminated tick must never swallow a comment: a comment made of
    /// bare words would parse as extra search terms and quietly answer a
    /// different question. Declining to engage puts the line back in the
    /// stripper's hands, and the stray tick then dies at the grammar.
    #[test]
    fn an_unterminated_backtick_never_swallows_a_comment() {
        // The tick sits where a name COULD start, but never closes.
        let query = parse("host=`x # comment").expect("the comment must be stripped");
        assert_eq!(
            query.search.groups[0].len(),
            1,
            "no comment word may become a search term: {:?}",
            query.search.groups
        );
        assert_eq!(
            query.search.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "host".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Literal("`x".to_string()),
            })
        );

        // Where the stray tick is a genuine name position, the truncated
        // name is a loud parse error — the other permitted outcome.
        assert!(parse("| table `a b # x").is_err());
        assert!(parse("* | where `a b // x").is_err());
    }

    /// An expression may put an ARITHMETIC operator in front of a name,
    /// so those predecessors open a name too — otherwise `` 1+`a#b` ``
    /// loses its comment markers and dies as an unterminated name.
    #[test]
    fn a_backtick_after_an_operator_opens_a_name() {
        for (dsl, want) in [
            ("* | let x = 1+`a#b`", "a#b"),
            ("* | let x = 2*`http://x`", "http://x"),
            ("* | let x = `a#b`%2", "a#b"),
            ("* | let x = 1-`a#b`", "a#b"),
            ("* | where `a#b`>1", "a#b"),
            ("* | sort -`a#b`", "a#b"),
            ("* | stats count() by `a#b`", "a#b"),
        ] {
            let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl} must parse: {e:?}"));
            assert!(
                field_positions(&query).iter().any(|n| n == want),
                "{dsl}: {:?}",
                field_positions(&query)
            );
        }

        // …and the regex case F1 fixed stays fixed: a tick inside a regex
        // body follows a letter or a `/`, never an operator, so the
        // comment on that line is still stripped.
        let query = parse("host=/a+b`c/ # | bad_stage").expect("the comment must be stripped");
        assert_eq!(
            query.pipeline.len(),
            0,
            "the comment must not reach the parser"
        );
        assert_eq!(query.search.groups[0].len(), 1);
        // a division inside a regex is the same shape
        let query = parse("host=/a`b/ # x").expect("the comment must be stripped");
        assert_eq!(query.search.groups[0].len(), 1);
    }
}
