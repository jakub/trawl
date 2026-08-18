// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Custom DSL syntax highlighting.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use trawl_client::SchemaResponse;

use crate::tui::theme::SyntaxColors;

/// Token type for syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenType {
    /// Pipe stages (stats, where, sort, etc.)
    Stage,
    /// Field filter keywords (service=, level=, status>=, etc.)
    FilterKey,
    /// Operators (|, =, >, >=, ==, etc.)
    Operator,
    /// Logical keywords (and, or, not, AND, OR, NOT)
    Logical,
    /// Function names (count, avg, sum, etc.)
    Function,
    /// Field names
    Field,
    /// String literals
    String,
    /// Numeric literals
    Number,
    /// Regex literal (/pattern/)
    Regex,
    /// Negated term (-word)
    Negated,
    /// Comments
    Comment,
    /// Whitespace
    Whitespace,
}

/// Syntax highlighter for DSL queries.
pub struct Highlighter<'a> {
    /// Known field names from schema (for validation).
    fields: Vec<std::string::String>,
    /// Syntax colors from the active theme.
    colors: &'a SyntaxColors,
}

impl<'a> Highlighter<'a> {
    /// Create a new highlighter with schema information and theme colors.
    pub fn new(schema: Option<&SchemaResponse>, colors: &'a SyntaxColors) -> Self {
        let fields = schema
            .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();

        Self { fields, colors }
    }

    /// Highlight a single line of DSL query text.
    pub fn highlight_line(&self, text: &str) -> Line<'static> {
        let mut tokens = Self::tokenize(text);

        // Post-tokenization: fix FilterKey detection.
        // If token[i] is Field and token[i+1] is ":" operator, reclassify as FilterKey.
        Self::fix_filter_keys(&mut tokens);

        let colors = self.colors;
        let spans: Vec<Span<'static>> = tokens
            .into_iter()
            .map(|(token_type, text)| {
                let style = match token_type {
                    TokenType::Stage | TokenType::Function => Style::default()
                        .fg(colors.stage)
                        .add_modifier(Modifier::BOLD),
                    TokenType::FilterKey => Style::default().fg(colors.filter_key),
                    TokenType::Operator => Style::default().fg(colors.operator),
                    TokenType::Logical => Style::default()
                        .fg(colors.logical)
                        .add_modifier(Modifier::BOLD),
                    TokenType::Field => {
                        if self.fields.contains(&text) {
                            Style::default().fg(colors.field_known)
                        } else {
                            Style::default().fg(colors.field_unknown)
                        }
                    }
                    TokenType::String => Style::default().fg(colors.string),
                    TokenType::Number => Style::default().fg(colors.number),
                    TokenType::Regex => Style::default().fg(colors.regex),
                    TokenType::Negated => Style::default()
                        .fg(colors.negated)
                        .add_modifier(Modifier::DIM),
                    TokenType::Comment => Style::default()
                        .fg(colors.comment)
                        .add_modifier(Modifier::ITALIC),
                    TokenType::Whitespace => Style::default(),
                };
                Span::styled(text.clone(), style)
            })
            .collect();

        Line::from(spans)
    }

    /// Post-tokenization pass: if a Field token is immediately followed by a
    /// filter operator (`=`, `>=`, `<=`, `!=`, `>`, `<`), reclassify the Field
    /// as `FilterKey`.
    fn fix_filter_keys(tokens: &mut [(TokenType, std::string::String)]) {
        for i in 0..tokens.len().saturating_sub(1) {
            if tokens[i].0 == TokenType::Field
                && tokens[i + 1].0 == TokenType::Operator
                && matches!(
                    tokens[i + 1].1.as_str(),
                    "=" | ">=" | "<=" | "!=" | ">" | "<"
                )
            {
                tokens[i].0 = TokenType::FilterKey;
            }
        }
    }

    /// Tokenize a line into (`TokenType`, text) pairs.
    ///
    /// **Display only, never correctness.** This is a colouring walk over
    /// text the user is still typing, not a reading of the query: it has
    /// no backtick awareness and no comment case, and it must never be
    /// consulted for what a query MEANS. `trawl_core::parser` is the
    /// authority on that, and `trawl_core::parser::scan` is the one
    /// text-level walk allowed to change an answer (ADR-0014 ruling 5).
    /// Unifying this walk with either needs a partial-input policy for an
    /// unterminated token at the cursor, which is its own design problem.
    #[allow(clippy::too_many_lines)]
    fn tokenize(text: &str) -> Vec<(TokenType, std::string::String)> {
        let mut tokens = Vec::new();
        let mut chars = text.chars().peekable();
        let mut current = std::string::String::new();
        // Whether the cursor sits where the GRAMMAR admits a comment:
        // start of line, or directly after whitespace. `current.is_empty()`
        // is NOT that test — after `color=` it is empty too, and colouring
        // `#ff0000` as a comment told the user prose where the parser
        // reports an error (ADR-0014 ruling 2).
        let mut at_layout_boundary = true;

        while let Some(ch) = chars.next() {
            let boundary = at_layout_boundary;
            at_layout_boundary = false;

            // Comment: `#` to end of line (ADR-0014 — `//` is not an
            // opener any more, and colouring it as one falsely dimmed
            // every URL in a query).
            if ch == '#' && boundary {
                let mut comment = std::string::String::from('#');
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                    comment.push(c);
                }
                tokens.push((TokenType::Comment, comment));
                at_layout_boundary = true;
                continue;
            }

            // Regex literal: /pattern/
            if ch == '/' && current.is_empty() {
                let mut regex = std::string::String::from('/');
                let mut closed = false;
                while let Some(c) = chars.next() {
                    regex.push(c);
                    if c == '/' {
                        closed = true;
                        break;
                    }
                    // Handle escaped chars in regex
                    if c == '\\'
                        && let Some(escaped) = chars.next()
                    {
                        regex.push(escaped);
                    }
                }
                if closed {
                    tokens.push((TokenType::Regex, regex));
                } else {
                    // Unclosed — treat as operator
                    tokens.push((TokenType::Operator, regex));
                }
                continue;
            }

            // String literals
            if ch == '"' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                let mut string = std::string::String::from('"');
                while let Some(c) = chars.next() {
                    string.push(c);
                    if c == '"' {
                        break;
                    }
                    // Handle escaped quotes
                    if c == '\\'
                        && let Some(escaped) = chars.next()
                    {
                        string.push(escaped);
                    }
                }
                tokens.push((TokenType::String, string));
                continue;
            }

            // Comparison operators
            if ch == '>' || ch == '<' || ch == '=' || ch == '!' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                let mut op = std::string::String::from(ch);
                if chars.peek() == Some(&'=') {
                    op.push(chars.next().unwrap());
                }
                tokens.push((TokenType::Operator, op));
                continue;
            }

            // Pipe operator
            if ch == '|' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Operator, std::string::String::from('|')));
                continue;
            }

            // Whitespace
            if ch.is_whitespace() {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Whitespace, std::string::String::from(ch)));
                at_layout_boundary = true;
                continue;
            }

            // Parentheses (for functions)
            if ch == '(' || ch == ')' || ch == ',' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Operator, std::string::String::from(ch)));
                continue;
            }

            // Negated term: - at start of word
            if ch == '-' && current.is_empty() {
                // Check if next char starts a word (not another operator)
                if chars
                    .peek()
                    .is_some_and(|c| c.is_alphanumeric() || *c == '_')
                {
                    let mut negated = std::string::String::from('-');
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() || c == '|' || c == '(' || c == ')' || c == ',' {
                            break;
                        }
                        negated.push(chars.next().unwrap());
                    }
                    tokens.push((TokenType::Negated, negated));
                    continue;
                }
            }

            // Build up current word
            current.push(ch);
        }

        // Push final token if any
        if !current.is_empty() {
            tokens.push((Self::classify_word(&current), current));
        }

        tokens
    }

    /// Classify a word into a token type.
    fn classify_word(word: &str) -> TokenType {
        let lower = word.to_lowercase();

        // Pipe stages
        match lower.as_str() {
            "stats" | "where" | "sort" | "limit" | "head" | "tail" | "table" | "fields" | "top"
            | "rare" | "drop" | "let" | "eval" | "extract" | "rex" | "rename" | "dedup"
            | "timechart" | "pivot" => return TokenType::Stage,
            _ => {}
        }

        // Logical operators
        if lower == "and" || lower == "or" || lower == "not" {
            return TokenType::Logical;
        }

        // Functions
        if word.contains('(') || Self::is_function_name(&lower) {
            return TokenType::Function;
        }

        // Numbers
        if word.parse::<f64>().is_ok() {
            return TokenType::Number;
        }

        // Default to field name
        TokenType::Field
    }

    /// Check if a word is a known function name.
    fn is_function_name(word: &str) -> bool {
        matches!(
            word,
            "count"
                | "avg"
                | "sum"
                | "min"
                | "max"
                | "dc"
                | "distinct_count"
                | "p50"
                | "p90"
                | "p95"
                | "p99"
                | "first"
                | "last"
                | "values"
                | "list"
                | "median"
                | "stddev"
                | "lower"
                | "upper"
                | "length"
                | "len"
                | "trim"
                | "ltrim"
                | "rtrim"
                | "replace"
                | "substr"
                | "abs"
                | "ceil"
                | "ceiling"
                | "floor"
                | "round"
                | "if"
                | "isnull"
                | "isnotnull"
                | "coalesce"
                | "typeof"
                | "now"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Highlighter, TokenType};

    fn kinds(text: &str) -> Vec<(TokenType, String)> {
        Highlighter::tokenize(text)
    }

    fn comment_text(text: &str) -> Option<String> {
        kinds(text)
            .into_iter()
            .find(|(t, _)| *t == TokenType::Comment)
            .map(|(_, s)| s)
    }

    /// A `#` colours as a comment only where the grammar opens one: line
    /// start or after whitespace. Anywhere else the parser reports an
    /// error, and dimming the rest of the line says the opposite.
    #[test]
    fn a_hash_colours_as_a_comment_only_at_a_layout_boundary() {
        assert_eq!(comment_text("# note"), Some("# note".to_string()));
        assert_eq!(
            comment_text("a=1 # note"),
            Some("# note".to_string()),
            "after whitespace"
        );
        assert_eq!(comment_text("color=#ff0000"), None, "inside a token");
        assert_eq!(comment_text("a=1# note"), None, "inside a value");
        assert_eq!(comment_text("count(),# x"), None, "after a delimiter");
    }

    /// `//` is not an opener since ADR-0014 ruling 3, so a URL keeps its
    /// ordinary colouring.
    #[test]
    fn slashes_are_not_a_comment() {
        assert_eq!(comment_text("url=https://example.com/x"), None);
    }
}
