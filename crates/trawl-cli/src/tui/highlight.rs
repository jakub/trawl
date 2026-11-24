// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use trawl_client::SchemaResponse;
use trawl_core::sanitize::sanitize_display_text;

use crate::tui::theme::SyntaxColors;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenType {
    Stage,
    FilterKey,
    Operator,
    Logical,
    Function,
    Field,
    String,
    Number,
    Regex,
    Negated,
    Comment,
    Whitespace,
}

pub struct Highlighter<'a> {
    fields: Vec<std::string::String>,
    colors: &'a SyntaxColors,
}

impl<'a> Highlighter<'a> {
    pub fn new(schema: Option<&SchemaResponse>, colors: &'a SyntaxColors) -> Self {
        let fields = schema
            .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();

        Self { fields, colors }
    }

    /// The sanitizing replacement is one character for one, so character
    /// indexes into `text` still index the spans.
    pub fn highlight_line(&self, text: &str) -> Line<'static> {
        let mut tokens = Self::tokenize(text);

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
                Span::styled(sanitize_display_text(&text), style)
            })
            .collect();

        Line::from(spans)
    }

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

    /// Display only, never correctness: nothing may consult it for what a
    /// query means (ADR-0014 ruling 5).
    #[allow(clippy::too_many_lines)]
    fn tokenize(text: &str) -> Vec<(TokenType, std::string::String)> {
        let mut tokens = Vec::new();
        let mut chars = text.chars().peekable();
        let mut current = std::string::String::new();
        // Start of line or directly after whitespace. `current.is_empty()` is
        // not that test: after `color=` it is empty too.
        let mut at_layout_boundary = true;

        while let Some(ch) = chars.next() {
            let boundary = at_layout_boundary;
            at_layout_boundary = false;

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

            if ch == '/' && current.is_empty() {
                let mut regex = std::string::String::from('/');
                let mut closed = false;
                while let Some(c) = chars.next() {
                    regex.push(c);
                    if c == '/' {
                        closed = true;
                        break;
                    }
                    if c == '\\'
                        && let Some(escaped) = chars.next()
                    {
                        regex.push(escaped);
                    }
                }
                if closed {
                    tokens.push((TokenType::Regex, regex));
                } else {
                    tokens.push((TokenType::Operator, regex));
                }
                continue;
            }

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
                    if c == '\\'
                        && let Some(escaped) = chars.next()
                    {
                        string.push(escaped);
                    }
                }
                tokens.push((TokenType::String, string));
                continue;
            }

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

            if ch == '|' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Operator, std::string::String::from('|')));
                continue;
            }

            if ch.is_whitespace() {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Whitespace, std::string::String::from(ch)));
                at_layout_boundary = true;
                continue;
            }

            if ch == '(' || ch == ')' || ch == ',' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Operator, std::string::String::from(ch)));
                continue;
            }

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

            current.push(ch);
        }

        if !current.is_empty() {
            tokens.push((Self::classify_word(&current), current));
        }

        tokens
    }

    fn classify_word(word: &str) -> TokenType {
        let lower = word.to_lowercase();

        match lower.as_str() {
            "stats" | "where" | "sort" | "limit" | "head" | "tail" | "table" | "fields" | "top"
            | "rare" | "drop" | "let" | "eval" | "extract" | "rex" | "rename" | "dedup"
            | "timechart" | "pivot" => return TokenType::Stage,
            _ => {}
        }

        if lower == "and" || lower == "or" || lower == "not" {
            return TokenType::Logical;
        }

        if word.contains('(') || Self::is_function_name(&lower) {
            return TokenType::Function;
        }

        if word.parse::<f64>().is_ok() {
            return TokenType::Number;
        }

        TokenType::Field
    }

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

    #[test]
    fn slashes_are_not_a_comment() {
        assert_eq!(comment_text("url=https://example.com/x"), None);
    }

    #[test]
    fn highlighted_tokens_preserve_raw_classification() {
        use trawl_client::{SchemaColumnResponse, SchemaResponse};

        let schema = SchemaResponse {
            columns: vec![SchemaColumnResponse {
                name: "fk\u{200b}na".into(),
                data_type: "VARCHAR".into(),
            }],
            file_count: 0,
            cached: false,
            earliest_date: None,
            latest_date: None,
            total_bytes: None,
            services: None,
            hot_buffer_events: None,
            hot_buffer_bytes: None,
        };
        let colors = crate::tui::theme::dark().syntax;
        let highlighter = Highlighter::new(Some(&schema), &colors);
        let line =
            highlighter.highlight_line("fk\u{200b}na \"sv\u{202e}x\u{00ad}y\" unk\u{202e}uz");

        let span = |text: &str| {
            line.spans
                .iter()
                .find(|span| span.content == text)
                .unwrap_or_else(|| panic!("no span {text:?} in {:?}", line.spans))
        };
        assert_eq!(span("fk\u{fffd}na").style.fg, Some(colors.field_known));
        assert_eq!(
            span("\"sv\u{fffd}x\u{fffd}y\"").style.fg,
            Some(colors.string)
        );
        assert_eq!(span("unk\u{fffd}uz").style.fg, Some(colors.field_unknown));
        for span in &line.spans {
            for forbidden in ['\u{202e}', '\u{200b}', '\u{00ad}'] {
                assert!(
                    !span.content.contains(forbidden),
                    "U+{:04X} in span {:?}",
                    u32::from(forbidden),
                    span.content
                );
            }
        }
    }
}
