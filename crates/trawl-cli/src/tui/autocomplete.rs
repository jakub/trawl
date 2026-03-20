// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ghost-text autocomplete for the query editor.
//!
//! Pure functions — no I/O, fully unit-testable. The editor calls
//! [`complete`] on every keypress and renders the returned ghost text
//! as a dimmed inline suffix.

use trawl_core::emitter::is_aggregate_function;
use trawl_core::parser::suggest::{KNOWN_FUNCTIONS, KNOWN_PIPE_STAGES};

// ── Public types ────────────────────────────────────────────────────

/// Schema field metadata passed in from the cached schema.
#[derive(Debug, Clone)]
pub struct SchemaField {
    pub name: String,
    /// Whether this field has a numeric type (for filtering agg function arguments).
    #[allow(dead_code)]
    // reserved for v2: suggest only numeric fields inside avg(), sum(), etc.
    pub is_numeric: bool,
}

/// What kind of completion to offer at the cursor position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// After `|` — suggest pipe stage names.
    PipeStage { prefix: String },
    /// Inside `stats`/`timechart`/`eventstats` before `by` — suggest aggregate functions.
    AggFunction { prefix: String },
    /// Inside `let`/`eval` or general expression — suggest scalar (and agg) functions.
    ScalarFunction { prefix: String },
    /// Field position (`by`, `sort`, `table`, `fields`, `drop`, etc.) — suggest field names.
    FieldName { prefix: String },
    /// Keyword position — suggest `by`, `as`, `from`, `on`.
    Keyword { prefix: String },
    /// Inside a string literal, regex, or free text — stay quiet.
    None,
}

/// A resolved ghost-text completion ready for display and insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Dimmed suffix to display after the cursor (the part the user hasn't typed yet).
    pub ghost_text: String,
    /// Full text to insert when accepted (replaces the typed prefix).
    pub insert_text: String,
    /// Number of chars of typed prefix to replace (backwards from cursor).
    pub replace_len: usize,
    /// Cursor position within `insert_text` after acceptance.
    /// `None` means cursor goes to the end.
    pub cursor_offset: Option<usize>,
}

/// Internal candidate from the candidate list.
#[derive(Debug, Clone)]
struct Candidate {
    /// Display/insert name (e.g. "stats", "count", "host").
    name: String,
    /// Whether this is a function (gets `()` appended).
    is_function: bool,
    /// Whether this is a zero-arg function (cursor goes after `()`).
    is_zero_arg: bool,
}

// ── Main entry point ────────────────────────────────────────────────

/// Compute a ghost-text completion for the given editor state.
///
/// Returns `None` when no suggestion is appropriate (free text, inside
/// strings, empty prefix, no match, etc.).
pub fn complete(
    text: &str,
    cursor_byte: usize,
    schema_fields: &[SchemaField],
) -> Option<Completion> {
    let context = detect_context(text, cursor_byte);
    let prefix = match &context {
        CompletionContext::PipeStage { prefix }
        | CompletionContext::AggFunction { prefix }
        | CompletionContext::ScalarFunction { prefix }
        | CompletionContext::FieldName { prefix }
        | CompletionContext::Keyword { prefix } => prefix,
        CompletionContext::None => return None,
    };

    if prefix.is_empty() {
        return None;
    }

    let candidates = candidates_for(&context, schema_fields);
    let candidate = prefix_match(prefix, &candidates)?;
    Some(build_completion(&candidate, prefix.len()))
}

// ── Context detection ───────────────────────────────────────────────

/// Detect what kind of completion context the cursor is in.
pub fn detect_context(text: &str, cursor_byte: usize) -> CompletionContext {
    let before = &text[..cursor_byte.min(text.len())];

    // Check if inside a string literal or regex.
    if inside_string_or_regex(before) {
        return CompletionContext::None;
    }

    // Extract the word prefix at cursor (scan back for word chars).
    let prefix = extract_prefix(before);
    if prefix.is_empty() {
        return CompletionContext::None;
    }

    // Look at what precedes the prefix (skipping whitespace).
    let before_prefix = before[..before.len() - prefix.len()].trim_end();

    // After `|` → pipe stage.
    if before_prefix.ends_with('|') {
        return CompletionContext::PipeStage {
            prefix: prefix.to_owned(),
        };
    }

    // Detect the current pipe stage by scanning backwards for `| <stage>`.
    let stage = find_current_stage(before_prefix);

    match stage.as_deref() {
        // In stats/timechart/eventstats and not past `by` → agg functions.
        Some("stats" | "timechart" | "eventstats") => {
            if is_past_by_keyword(before_prefix, stage.as_deref().unwrap_or_default()) {
                // After `by` → field names.
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            } else if before_prefix.ends_with('(') {
                // Inside function parens → field names (argument position).
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            } else {
                CompletionContext::AggFunction {
                    prefix: prefix.to_owned(),
                }
            }
        }
        // In let/eval → scalar functions or field names.
        Some("let" | "eval") => {
            if looks_like_function_position(before_prefix) {
                CompletionContext::ScalarFunction {
                    prefix: prefix.to_owned(),
                }
            } else if before_prefix.ends_with('(') {
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            } else {
                CompletionContext::ScalarFunction {
                    prefix: prefix.to_owned(),
                }
            }
        }
        // In where → could be fields or scalar functions.
        Some("where") => {
            if before_prefix.ends_with('(') {
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            } else {
                // Offer field names in where clauses (most common use).
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            }
        }
        // In sort/table/fields/drop/dedup/rename/top/rare → field names.
        Some("sort" | "table" | "fields" | "drop" | "dedup" | "rename" | "top" | "rare") => {
            CompletionContext::FieldName {
                prefix: prefix.to_owned(),
            }
        }
        // After `by` keyword (any stage) → field names.
        _ if before_prefix
            .rsplit_once(|c: char| !c.is_alphanumeric() && c != '_')
            .is_some_and(|(_, last_word)| last_word == "by") =>
        {
            CompletionContext::FieldName {
                prefix: prefix.to_owned(),
            }
        }
        // Before any pipe (search stage) → no suggestion for bare text.
        // The search stage is free-form — autocomplete would be noisy.
        _ if !before.contains('|') => CompletionContext::None,
        // Fallback: try keywords if prefix looks like one.
        _ => {
            let keyword_prefixes = ["by", "as", "from", "on"];
            if keyword_prefixes
                .iter()
                .any(|kw| kw.starts_with(&prefix.to_lowercase()) && *kw != prefix.to_lowercase())
            {
                CompletionContext::Keyword {
                    prefix: prefix.to_owned(),
                }
            } else {
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            }
        }
    }
}

// ── Candidate generation ────────────────────────────────────────────

/// Build the candidate list for a given context.
fn candidates_for(context: &CompletionContext, schema_fields: &[SchemaField]) -> Vec<Candidate> {
    match context {
        CompletionContext::PipeStage { .. } => {
            let mut candidates: Vec<Candidate> = KNOWN_PIPE_STAGES
                .iter()
                .map(|&name| Candidate {
                    name: name.to_owned(),
                    is_function: false,
                    is_zero_arg: false,
                })
                .collect();
            candidates.sort_by(|a, b| a.name.cmp(&b.name));
            candidates
        }
        CompletionContext::AggFunction { .. } => {
            let mut candidates: Vec<Candidate> = KNOWN_FUNCTIONS
                .iter()
                .filter(|&&name| is_aggregate_function(name))
                .map(|&name| Candidate {
                    name: name.to_owned(),
                    is_function: true,
                    is_zero_arg: is_zero_arg_function(name),
                })
                .collect();
            candidates.sort_by(|a, b| a.name.cmp(&b.name));
            candidates
        }
        CompletionContext::ScalarFunction { .. } => {
            let mut candidates: Vec<Candidate> = KNOWN_FUNCTIONS
                .iter()
                .filter(|&&name| !is_aggregate_function(name))
                .map(|&name| Candidate {
                    name: name.to_owned(),
                    is_function: true,
                    is_zero_arg: is_zero_arg_function(name),
                })
                .collect();
            candidates.sort_by(|a, b| a.name.cmp(&b.name));
            candidates
        }
        CompletionContext::FieldName { .. } => {
            let mut candidates: Vec<Candidate> = schema_fields
                .iter()
                .map(|f| Candidate {
                    name: f.name.clone(),
                    is_function: false,
                    is_zero_arg: false,
                })
                .collect();
            candidates.sort_by(|a, b| a.name.cmp(&b.name));
            candidates
        }
        CompletionContext::Keyword { .. } => ["as", "by", "from", "on"]
            .iter()
            .map(|&kw| Candidate {
                name: kw.to_owned(),
                is_function: false,
                is_zero_arg: false,
            })
            .collect(),
        CompletionContext::None => Vec::new(),
    }
}

/// Case-insensitive prefix match. Returns the first alphabetical match.
fn prefix_match(prefix: &str, candidates: &[Candidate]) -> Option<Candidate> {
    let lower = prefix.to_lowercase();
    candidates
        .iter()
        .find(|c| {
            let name_lower = c.name.to_lowercase();
            name_lower.starts_with(&lower) && name_lower != lower
        })
        .cloned()
}

// ── Completion building ─────────────────────────────────────────────

/// Build the final `Completion` from a matched candidate.
fn build_completion(candidate: &Candidate, prefix_len: usize) -> Completion {
    if candidate.is_function {
        let suffix = &candidate.name[prefix_len..];
        if candidate.is_zero_arg {
            // count() / now() → cursor after closing paren.
            Completion {
                ghost_text: format!("{suffix}()"),
                insert_text: format!("{}()", candidate.name),
                replace_len: prefix_len,
                cursor_offset: None, // end
            }
        } else {
            // avg( → cursor between parens.
            Completion {
                ghost_text: format!("{suffix}()"),
                insert_text: format!("{}()", candidate.name),
                replace_len: prefix_len,
                cursor_offset: Some(candidate.name.len() + 1), // between ( and )
            }
        }
    } else {
        // Stage or field — trailing space for stages, bare name for fields.
        let suffix = &candidate.name[prefix_len..];
        let is_stage = KNOWN_PIPE_STAGES.contains(&candidate.name.as_str());
        if is_stage {
            Completion {
                ghost_text: format!("{suffix} "),
                insert_text: format!("{} ", candidate.name),
                replace_len: prefix_len,
                cursor_offset: None,
            }
        } else {
            Completion {
                ghost_text: suffix.to_owned(),
                insert_text: candidate.name.clone(),
                replace_len: prefix_len,
                cursor_offset: None,
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Whether a function takes zero arguments (cursor goes after `()`).
fn is_zero_arg_function(name: &str) -> bool {
    matches!(name, "count" | "now")
}

/// Extract the word prefix immediately before the cursor.
fn extract_prefix(before_cursor: &str) -> &str {
    let end = before_cursor.len();
    let start = before_cursor
        .char_indices()
        .rev()
        .take_while(|&(_, ch)| ch.is_alphanumeric() || ch == '_')
        .last()
        .map_or(end, |(i, _)| i);
    &before_cursor[start..end]
}

/// Rough check for whether the cursor is inside a string literal or regex.
///
/// Counts unescaped `"` and `/` delimiters before the cursor. An odd
/// count means we're inside one.
fn inside_string_or_regex(before: &str) -> bool {
    let mut in_double_quote = false;
    let mut in_regex = false;
    let mut prev_char = '\0';

    for ch in before.chars() {
        if ch == '"' && prev_char != '\\' {
            in_double_quote = !in_double_quote;
        }
        // Regex delimiters: `/pattern/` — only toggle when not in a string.
        if ch == '/' && !in_double_quote && prev_char != '\\' {
            in_regex = !in_regex;
        }
        prev_char = ch;
    }

    in_double_quote || in_regex
}

/// Find the current pipe stage name by scanning backwards from the cursor.
///
/// Looks for the pattern `| <stage_name>` and returns the stage name
/// if it's a known stage.
fn find_current_stage(before: &str) -> Option<String> {
    // Find the last `|` and extract the first word after it.
    let after_pipe = before.rsplit('|').next()?;
    let trimmed = after_pipe.trim_start();
    let first_word: String = trimmed
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();

    if first_word.is_empty() {
        return None;
    }

    let lower = first_word.to_lowercase();
    if KNOWN_PIPE_STAGES.contains(&lower.as_str()) {
        Some(lower)
    } else {
        None
    }
}

/// Check if we're past the `by` keyword in the current stage.
fn is_past_by_keyword(before: &str, stage: &str) -> bool {
    // Find the last occurrence of `| <stage>` and check for `by` after it.
    let lower = before.to_lowercase();
    if let Some(pipe_pos) = lower.rfind(&format!("| {stage}")) {
        let after_stage = &lower[pipe_pos + 2 + stage.len()..];
        // Look for standalone `by` keyword.
        after_stage.split_whitespace().any(|w| w == "by")
    } else if let Some(pipe_pos) = lower.rfind('|') {
        let after_pipe = lower[pipe_pos + 1..].trim_start();
        if let Some(after_stage) = after_pipe.strip_prefix(stage) {
            after_stage.split_whitespace().any(|w| w == "by")
        } else {
            false
        }
    } else {
        false
    }
}

/// Check if the position looks like it expects a function name.
///
/// Heuristic: after `=`, `,`, `(`, or at the start of an expression.
fn looks_like_function_position(before: &str) -> bool {
    let trimmed = before.trim_end();
    trimmed.ends_with('=')
        || trimmed.ends_with(',')
        || trimmed.ends_with('(')
        || trimmed.ends_with('+')
        || trimmed.ends_with('-')
        || trimmed.ends_with('*')
        || trimmed.ends_with('/')
}

/// Convert editor `(row, col)` cursor position to a byte offset in the full text.
pub fn cursor_to_byte_offset(lines: &[String], row: usize, col: usize) -> usize {
    let mut offset = 0;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            // Convert char col to byte offset within this line.
            offset += line
                .char_indices()
                .nth(col)
                .map_or(line.len(), |(byte_idx, _)| byte_idx);
            break;
        }
        offset += line.len() + 1; // +1 for newline
    }
    offset
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Vec<SchemaField> {
        vec![
            SchemaField {
                name: "host".to_owned(),
                is_numeric: false,
            },
            SchemaField {
                name: "hostname".to_owned(),
                is_numeric: false,
            },
            SchemaField {
                name: "service".to_owned(),
                is_numeric: false,
            },
            SchemaField {
                name: "level".to_owned(),
                is_numeric: false,
            },
            SchemaField {
                name: "message".to_owned(),
                is_numeric: false,
            },
            SchemaField {
                name: "duration".to_owned(),
                is_numeric: true,
            },
            SchemaField {
                name: "status".to_owned(),
                is_numeric: true,
            },
        ]
    }

    // ── extract_prefix ──────────────────────────────────────────────

    #[test]
    fn prefix_simple_word() {
        assert_eq!(extract_prefix("| st"), "st");
    }

    #[test]
    fn prefix_after_space() {
        assert_eq!(extract_prefix("| stats count() by ho"), "ho");
    }

    #[test]
    fn prefix_empty_at_space() {
        assert_eq!(extract_prefix("| stats "), "");
    }

    #[test]
    fn prefix_underscore() {
        assert_eq!(extract_prefix("| let my_var"), "my_var");
    }

    // ── inside_string_or_regex ──────────────────────────────────────

    #[test]
    fn not_inside_string() {
        assert!(!inside_string_or_regex("| stats count() by "));
    }

    #[test]
    fn inside_double_quote() {
        assert!(inside_string_or_regex(r#"| extract "(?P<ip>"#));
    }

    #[test]
    fn closed_string_not_inside() {
        assert!(!inside_string_or_regex(r#""hello" | st"#));
    }

    #[test]
    fn inside_regex() {
        assert!(inside_string_or_regex("message=/error"));
    }

    // ── detect_context ──────────────────────────────────────────────

    #[test]
    fn context_after_pipe() {
        assert_eq!(
            detect_context("| st", 4),
            CompletionContext::PipeStage {
                prefix: "st".to_owned()
            }
        );
    }

    #[test]
    fn context_after_pipe_with_space() {
        assert_eq!(
            detect_context("|  wh", 5),
            CompletionContext::PipeStage {
                prefix: "wh".to_owned()
            }
        );
    }

    #[test]
    fn context_stats_function() {
        assert_eq!(
            detect_context("| stats cou", 11),
            CompletionContext::AggFunction {
                prefix: "cou".to_owned()
            }
        );
    }

    #[test]
    fn context_stats_by_field() {
        assert_eq!(
            detect_context("| stats count() by ho", 21),
            CompletionContext::FieldName {
                prefix: "ho".to_owned()
            }
        );
    }

    #[test]
    fn context_sort_field() {
        assert_eq!(
            detect_context("| sort ho", 9),
            CompletionContext::FieldName {
                prefix: "ho".to_owned()
            }
        );
    }

    #[test]
    fn context_table_field() {
        assert_eq!(
            detect_context("| table ho", 10),
            CompletionContext::FieldName {
                prefix: "ho".to_owned()
            }
        );
    }

    #[test]
    fn context_let_function() {
        assert_eq!(
            detect_context("| let x = low", 13),
            CompletionContext::ScalarFunction {
                prefix: "low".to_owned()
            }
        );
    }

    #[test]
    fn context_function_arg_field() {
        assert_eq!(
            detect_context("| stats avg(du", 14),
            CompletionContext::FieldName {
                prefix: "du".to_owned()
            }
        );
    }

    #[test]
    fn context_inside_string_is_none() {
        assert_eq!(
            detect_context(r#"| extract "st"#, 13),
            CompletionContext::None
        );
    }

    #[test]
    fn context_bare_search_text_is_none() {
        assert_eq!(detect_context("err", 3), CompletionContext::None);
    }

    #[test]
    fn context_empty_prefix_is_none() {
        assert_eq!(detect_context("| stats ", 8), CompletionContext::None);
    }

    #[test]
    fn context_where_field() {
        assert_eq!(
            detect_context("| where ho", 10),
            CompletionContext::FieldName {
                prefix: "ho".to_owned()
            }
        );
    }

    #[test]
    fn context_timechart_agg() {
        assert_eq!(
            detect_context("| timechart span=5m cou", 23),
            CompletionContext::AggFunction {
                prefix: "cou".to_owned()
            }
        );
    }

    #[test]
    fn context_eventstats_agg() {
        assert_eq!(
            detect_context("| eventstats av", 15),
            CompletionContext::AggFunction {
                prefix: "av".to_owned()
            }
        );
    }

    // ── complete (integration) ──────────────────────────────────────

    #[test]
    fn complete_pipe_stage() {
        let c = complete("| st", 4, &fields()).unwrap();
        assert_eq!(c.ghost_text, "ats ");
        assert_eq!(c.insert_text, "stats ");
        assert_eq!(c.replace_len, 2);
    }

    #[test]
    fn complete_agg_function() {
        let c = complete("| stats cou", 11, &fields()).unwrap();
        assert_eq!(c.ghost_text, "nt()");
        assert_eq!(c.insert_text, "count()");
        assert_eq!(c.replace_len, 3);
        // count is zero-arg → cursor at end
        assert_eq!(c.cursor_offset, None);
    }

    #[test]
    fn complete_agg_function_avg() {
        let c = complete("| stats av", 10, &fields()).unwrap();
        assert_eq!(c.ghost_text, "g()");
        assert_eq!(c.insert_text, "avg()");
        assert_eq!(c.replace_len, 2);
        // avg takes args → cursor between parens
        assert_eq!(c.cursor_offset, Some(4));
    }

    #[test]
    fn complete_field_after_by() {
        let c = complete("| stats count() by ho", 21, &fields()).unwrap();
        assert_eq!(c.ghost_text, "st");
        assert_eq!(c.insert_text, "host");
        assert_eq!(c.replace_len, 2);
    }

    #[test]
    fn complete_field_hostname() {
        // "hostn" should match "hostname", not "host"
        let c = complete("| stats count() by hostn", 24, &fields()).unwrap();
        assert_eq!(c.ghost_text, "ame");
        assert_eq!(c.insert_text, "hostname");
        assert_eq!(c.replace_len, 5);
    }

    #[test]
    fn complete_no_match() {
        assert!(complete("| stats zzz", 11, &fields()).is_none());
    }

    #[test]
    fn complete_exact_match_no_ghost() {
        // Typing "stats" exactly should not suggest "stats" again.
        assert!(complete("| stats", 7, &fields()).is_none());
    }

    #[test]
    fn complete_free_text_before_pipe() {
        assert!(complete("error", 5, &fields()).is_none());
    }

    #[test]
    fn complete_inside_string() {
        assert!(complete(r#"| extract "st"#, 13, &fields()).is_none());
    }

    #[test]
    fn complete_scalar_function_in_let() {
        let c = complete("| let x = low", 13, &fields()).unwrap();
        assert_eq!(c.ghost_text, "er()");
        assert_eq!(c.insert_text, "lower()");
        assert_eq!(c.replace_len, 3);
        assert_eq!(c.cursor_offset, Some(6)); // between parens
    }

    #[test]
    fn complete_now_zero_arg() {
        let c = complete("| let x = no", 12, &fields()).unwrap();
        assert_eq!(c.ghost_text, "w()");
        assert_eq!(c.insert_text, "now()");
        assert_eq!(c.replace_len, 2);
        assert_eq!(c.cursor_offset, None); // zero-arg, cursor at end
    }

    #[test]
    fn complete_sort_field() {
        let c = complete("| sort se", 9, &fields()).unwrap();
        assert_eq!(c.ghost_text, "rvice");
        assert_eq!(c.insert_text, "service");
        assert_eq!(c.replace_len, 2);
    }

    #[test]
    fn complete_where_stage() {
        let c = complete("| wh", 4, &fields()).unwrap();
        assert_eq!(c.ghost_text, "ere ");
        assert_eq!(c.insert_text, "where ");
        assert_eq!(c.replace_len, 2);
    }

    // ── cursor_to_byte_offset ───────────────────────────────────────

    #[test]
    fn byte_offset_single_line() {
        let lines = vec!["| stats count()".to_owned()];
        assert_eq!(cursor_to_byte_offset(&lines, 0, 4), 4);
    }

    #[test]
    fn byte_offset_multiline() {
        let lines = vec!["| stats count()".to_owned(), "  by host".to_owned()];
        // Row 1, col 5 = "host" starts at byte 16 (line0 len=15, +1 newline, +5)
        assert_eq!(cursor_to_byte_offset(&lines, 1, 5), 21);
    }

    #[test]
    fn byte_offset_at_line_end() {
        let lines = vec!["abc".to_owned()];
        assert_eq!(cursor_to_byte_offset(&lines, 0, 3), 3);
    }

    // ── find_current_stage ──────────────────────────────────────────

    #[test]
    fn stage_stats() {
        assert_eq!(
            find_current_stage("| stats count() by"),
            Some("stats".to_owned())
        );
    }

    #[test]
    fn stage_sort_after_stats() {
        assert_eq!(
            find_current_stage("| stats count() | sort"),
            Some("sort".to_owned())
        );
    }

    #[test]
    fn stage_none_before_pipe() {
        assert_eq!(find_current_stage("service=nginx level=error"), None);
    }

    // ── is_past_by_keyword ──────────────────────────────────────────

    #[test]
    fn past_by_true() {
        assert!(is_past_by_keyword("| stats count() by", "stats"));
    }

    #[test]
    fn past_by_false() {
        assert!(!is_past_by_keyword("| stats count()", "stats"));
    }

    // ── prefix_match ────────────────────────────────────────────────

    #[test]
    fn prefix_match_finds_first() {
        let candidates = vec![
            Candidate {
                name: "avg".to_owned(),
                is_function: true,
                is_zero_arg: false,
            },
            Candidate {
                name: "abs".to_owned(),
                is_function: true,
                is_zero_arg: false,
            },
        ];
        // "ab" should match "abs" (first alphabetically that starts with "ab")
        // candidates are sorted: abs, avg — but input is pre-sorted, so first is avg then abs.
        // Actually the vec is [avg, abs], so "a" matches "avg" first.
        let result = prefix_match("av", &candidates);
        assert_eq!(result.unwrap().name, "avg");
    }

    #[test]
    fn prefix_match_case_insensitive() {
        let candidates = vec![Candidate {
            name: "stats".to_owned(),
            is_function: false,
            is_zero_arg: false,
        }];
        assert!(prefix_match("ST", &candidates).is_some());
    }

    #[test]
    fn prefix_match_exact_returns_none() {
        let candidates = vec![Candidate {
            name: "stats".to_owned(),
            is_function: false,
            is_zero_arg: false,
        }];
        assert!(prefix_match("stats", &candidates).is_none());
    }
}
