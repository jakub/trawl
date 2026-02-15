//! Custom DSL syntax highlighting.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use fleet_client::SchemaResponse;

/// Token type for syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenType {
    /// Pipe stages (stats, where, sort, etc.)
    Stage,
    /// Field filter keywords (service:, level:, last:, etc.)
    FilterKey,
    /// Operators (|, :, >, >=, ==, etc.)
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
    /// Comments
    Comment,
    /// Whitespace
    Whitespace,
}

/// Syntax highlighter for DSL queries.
pub struct Highlighter {
    /// Known field names from schema (for validation).
    fields: Vec<String>,
}

impl Highlighter {
    /// Create a new highlighter with schema information.
    pub fn new(schema: Option<&SchemaResponse>) -> Self {
        let fields = schema
            .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();

        Self { fields }
    }

    /// Highlight a single line of DSL query text.
    pub fn highlight_line(&self, text: &str) -> Line<'static> {
        let tokens = Self::tokenize(text);
        let spans: Vec<Span<'static>> = tokens
            .into_iter()
            .map(|(token_type, text)| {
                let style = match token_type {
                    TokenType::Stage | TokenType::Function => Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                    TokenType::FilterKey => Style::default().fg(Color::Cyan),
                    TokenType::Operator => Style::default().fg(Color::Yellow),
                    TokenType::Logical => Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    TokenType::Field => {
                        // Green if in schema, white otherwise
                        if self.fields.contains(&text) {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::White)
                        }
                    }
                    TokenType::String => Style::default().fg(Color::LightYellow),
                    TokenType::Number => Style::default().fg(Color::LightBlue),
                    TokenType::Comment => Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                    TokenType::Whitespace => Style::default(),
                };
                Span::styled(text.clone(), style)
            })
            .collect();

        Line::from(spans)
    }

    /// Tokenize a line into (`TokenType`, text) pairs.
    fn tokenize(text: &str) -> Vec<(TokenType, String)> {
        let mut tokens = Vec::new();
        let mut chars = text.chars().peekable();
        let mut current = String::new();

        while let Some(ch) = chars.next() {
            // Comment: // to end of line
            if ch == '/' && chars.peek() == Some(&'/') {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                // Consume rest of line as comment
                let mut comment = String::from("//");
                chars.next(); // consume second /
                for c in chars.by_ref() {
                    comment.push(c);
                }
                tokens.push((TokenType::Comment, comment));
                break;
            }

            // String literals
            if ch == '"' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                let mut string = String::from('"');
                while let Some(c) = chars.next() {
                    string.push(c);
                    if c == '"' {
                        break;
                    }
                    // Handle escaped quotes
                    if c == '\\' {
                        if let Some(escaped) = chars.next() {
                            string.push(escaped);
                        }
                    }
                }
                tokens.push((TokenType::String, string));
                continue;
            }

            // Multi-character operators
            if ch == ':' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                let mut op = String::from(':');
                if let Some(&next) = chars.peek() {
                    if next == '>' || next == '<' || next == '=' || next == '!' {
                        op.push(chars.next().unwrap());
                        if chars.peek() == Some(&'=') {
                            op.push(chars.next().unwrap());
                        }
                    }
                }
                tokens.push((TokenType::Operator, op));
                continue;
            }

            // Comparison operators
            if ch == '>' || ch == '<' || ch == '=' || ch == '!' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                let mut op = String::from(ch);
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
                tokens.push((TokenType::Operator, String::from('|')));
                continue;
            }

            // Whitespace
            if ch.is_whitespace() {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Whitespace, String::from(ch)));
                continue;
            }

            // Parentheses (for functions)
            if ch == '(' || ch == ')' || ch == ',' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                tokens.push((TokenType::Operator, String::from(ch)));
                continue;
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

        // Filter keywords
        if lower.ends_with(':') {
            return TokenType::FilterKey;
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
