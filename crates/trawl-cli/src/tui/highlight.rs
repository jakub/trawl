//! Custom DSL syntax highlighting.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use trawl_client::SchemaResponse;

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
pub struct Highlighter {
    /// Known field names from schema (for validation).
    fields: Vec<std::string::String>,
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
        let mut tokens = Self::tokenize(text);

        // Post-tokenization: fix FilterKey detection.
        // If token[i] is Field and token[i+1] is ":" operator, reclassify as FilterKey.
        Self::fix_filter_keys(&mut tokens);

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
                    TokenType::Regex => Style::default().fg(Color::LightRed),
                    TokenType::Negated => {
                        Style::default().fg(Color::Red).add_modifier(Modifier::DIM)
                    }
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

    /// Post-tokenization pass: if a Field token is immediately followed by a ":"
    /// operator token, reclassify the Field as `FilterKey`.
    fn fix_filter_keys(tokens: &mut [(TokenType, std::string::String)]) {
        for i in 0..tokens.len().saturating_sub(1) {
            if tokens[i].0 == TokenType::Field && tokens[i + 1].1.starts_with(':') {
                tokens[i].0 = TokenType::FilterKey;
            }
        }
    }

    /// Tokenize a line into (`TokenType`, text) pairs.
    #[allow(clippy::too_many_lines)]
    fn tokenize(text: &str) -> Vec<(TokenType, std::string::String)> {
        let mut tokens = Vec::new();
        let mut chars = text.chars().peekable();
        let mut current = std::string::String::new();

        while let Some(ch) = chars.next() {
            // Comment: // to end of line
            if ch == '/' && chars.peek() == Some(&'/') {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                // Consume rest of line as comment
                let mut comment = std::string::String::from("//");
                chars.next(); // consume second /
                for c in chars.by_ref() {
                    comment.push(c);
                }
                tokens.push((TokenType::Comment, comment));
                break;
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

            // Multi-character operators
            if ch == ':' {
                if !current.is_empty() {
                    tokens.push((Self::classify_word(&current), current.clone()));
                    current.clear();
                }
                let mut op = std::string::String::from(':');
                if let Some(&next) = chars.peek()
                    && (next == '>' || next == '<' || next == '=' || next == '!')
                {
                    op.push(chars.next().unwrap());
                    if chars.peek() == Some(&'=') {
                        op.push(chars.next().unwrap());
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
                        if c.is_whitespace()
                            || c == '|'
                            || c == ':'
                            || c == '('
                            || c == ')'
                            || c == ','
                        {
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

        // Filter keywords (legacy: word ends with colon)
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
