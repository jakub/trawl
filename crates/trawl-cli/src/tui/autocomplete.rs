// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ghost-text autocomplete for the query editor.

use trawl_core::emitter::is_aggregate_function;
use trawl_core::parser::suggest::{KNOWN_FUNCTIONS, KNOWN_PIPE_STAGES, quote_dsl_field};

#[derive(Debug, Clone)]
pub struct SchemaField {
    pub name: String,
    #[allow(dead_code)]
    pub is_numeric: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    PipeStage { prefix: String },
    AggFunction { prefix: String },
    ScalarFunction { prefix: String },
    FieldName { prefix: String },
    Keyword { prefix: String },
    None,
}

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

#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    is_function: bool,
    /// Whether this is a zero-arg function (cursor goes after `()`).
    is_zero_arg: bool,
}

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

    // Suppress ghost text when cursor is in the middle of an existing word.
    // Without this, moving the cursor into "me|ssage" would suggest completions
    // for the "me" prefix and splice ghost text inside the word.
    let after = &text[cursor_byte..];
    if after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }

    let match_prefix = match prefix.strip_prefix('`') {
        Some(rest) if matches!(context, CompletionContext::FieldName { .. }) => rest,
        Some(_) => return None,
        None => prefix.as_str(),
    };
    let candidates = candidates_for(&context, schema_fields);
    let candidate = prefix_match(match_prefix, &candidates)?;
    build_completion(&candidate, prefix)
}

pub fn detect_context(text: &str, cursor_byte: usize) -> CompletionContext {
    let before = &text[..cursor_byte.min(text.len())];

    // Once a name is open, its spaces and punctuation are part of the
    // completion prefix. Only consult the ordinary string/regex detector
    // when no quoted name is open.
    let quoted_prefix = open_backtick_prefix(before);
    if quoted_prefix.is_none() && inside_string_or_regex(before) {
        return CompletionContext::None;
    }

    let prefix = quoted_prefix.unwrap_or_else(|| extract_prefix(before));
    if prefix.is_empty() {
        return CompletionContext::None;
    }

    let before_prefix = before[..before.len() - prefix.len()].trim_end();

    if before_prefix.ends_with('|') {
        return CompletionContext::PipeStage {
            prefix: prefix.to_owned(),
        };
    }

    let stage = find_current_stage(before_prefix);

    match stage.as_deref() {
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
        Some("where") => {
            if before_prefix.ends_with('(') {
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            } else {
                // Deliberately the same as the `(` arm, unlike `let` above: a
                // filter names a field far more often than it calls a function,
                // so no scalar-function offer here.
                CompletionContext::FieldName {
                    prefix: prefix.to_owned(),
                }
            }
        }
        Some("sort" | "table" | "fields" | "drop" | "dedup" | "rename" | "top" | "rare") => {
            CompletionContext::FieldName {
                prefix: prefix.to_owned(),
            }
        }
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

/// Case-insensitive prefix match, taking the first candidate in list order
/// (every candidate list is built sorted).
///
/// Returns `None` when the prefix exactly matches a candidate — the user
/// has already typed a valid name, so suggesting a longer variant (e.g.
/// "services" when they typed "service") would be confusing.
fn prefix_match(prefix: &str, candidates: &[Candidate]) -> Option<Candidate> {
    let lower = prefix.to_lowercase();

    if candidates.iter().any(|c| c.name.to_lowercase() == lower) {
        return None;
    }

    candidates
        .iter()
        .find(|c| c.name.to_lowercase().starts_with(&lower))
        .cloned()
}

fn build_completion(candidate: &Candidate, prefix: &str) -> Option<Completion> {
    // Candidate slicing is in bytes, editor replacement/cursor offsets are
    // character counts. Mixing them deletes text beside a non-ASCII prefix.
    let prefix_len = prefix.len();
    let prefix_cols = prefix.chars().count();
    if candidate.is_function {
        let suffix = &candidate.name[prefix_len..];
        if candidate.is_zero_arg {
            Some(Completion {
                ghost_text: format!("{suffix}()"),
                insert_text: format!("{}()", candidate.name),
                replace_len: prefix_cols,
                cursor_offset: None,
            })
        } else {
            Some(Completion {
                ghost_text: format!("{suffix}()"),
                insert_text: format!("{}()", candidate.name),
                replace_len: prefix_cols,
                cursor_offset: Some(candidate.name.chars().count() + 1),
            })
        }
    } else if KNOWN_PIPE_STAGES.contains(&candidate.name.as_str()) {
        let suffix = &candidate.name[prefix_len..];
        Some(Completion {
            ghost_text: format!("{suffix} "),
            insert_text: format!("{} ", candidate.name),
            replace_len: prefix_cols,
            cursor_offset: None,
        })
    } else {
        let rendered = quote_dsl_field(&candidate.name)?;
        let ghost_text = rendered
            .strip_prefix(prefix)
            .map_or_else(|| rendered.clone(), ToOwned::to_owned);
        Some(Completion {
            ghost_text,
            insert_text: rendered,
            replace_len: prefix_cols,
            cursor_offset: None,
        })
    }
}

fn is_zero_arg_function(name: &str) -> bool {
    matches!(name, "count" | "now")
}

/// Return the open quoted-name region ending at the cursor, opening tick
/// included. String/regex regions are skipped so a data backtick cannot
/// disable completion for the rest of the line.
///
/// **Completion only, never correctness** (ADR-0014 ruling 5).
fn open_backtick_prefix(before: &str) -> Option<&str> {
    let bytes = before.as_bytes();
    let mut open = None;
    let mut in_string = false;
    let mut in_regex = false;
    let mut i = 0;
    while i < bytes.len() {
        let escaped = i > 0 && bytes[i - 1] == b'\\';
        match bytes[i] {
            b'"' if open.is_none() && !in_regex && !escaped => in_string = !in_string,
            b'/' if open.is_none() && !in_string && !escaped => in_regex = !in_regex,
            b'`' if !in_string && !in_regex => {
                if open.is_some() && bytes.get(i + 1) == Some(&b'`') {
                    i += 2;
                    continue;
                }
                open = if open.is_some() {
                    None
                } else if quoted_name_can_start(bytes, i) {
                    Some(i)
                } else {
                    None
                };
            }
            _ => {}
        }
        i += 1;
    }
    open.map(|start| &before[start..])
}

fn quoted_name_can_start(bytes: &[u8], i: usize) -> bool {
    let Some(prev) = i.checked_sub(1).map(|p| bytes[p]) else {
        return true;
    };
    prev.is_ascii_whitespace()
        || matches!(
            prev,
            b'(' | b',' | b'|' | b'-' | b'=' | b'<' | b'>' | b'!' | b'+' | b'*' | b'%'
        )
}

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
/// **Completion only, never correctness** (ADR-0014 ruling 5): it has no
/// backtick awareness and no comment case, and a `/` in a URL counts as a
/// delimiter here.
fn inside_string_or_regex(before: &str) -> bool {
    let mut in_double_quote = false;
    let mut in_regex = false;
    let mut prev_char = '\0';

    for ch in before.chars() {
        if ch == '"' && prev_char != '\\' {
            in_double_quote = !in_double_quote;
        }
        if ch == '/' && !in_double_quote && prev_char != '\\' {
            in_regex = !in_regex;
        }
        prev_char = ch;
    }

    in_double_quote || in_regex
}

fn find_current_stage(before: &str) -> Option<String> {
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

fn is_past_by_keyword(before: &str, stage: &str) -> bool {
    let lower = before.to_lowercase();
    if let Some(pipe_pos) = lower.rfind(&format!("| {stage}")) {
        let after_stage = &lower[pipe_pos + 2 + stage.len()..];
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

pub fn cursor_to_byte_offset(lines: &[String], row: usize, col: usize) -> usize {
    let mut offset = 0;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            offset += line
                .char_indices()
                .nth(col)
                .map_or(line.len(), |(byte_idx, _)| byte_idx);
            break;
        }
        offset += line.len() + 1;
    }
    offset
}

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
        assert_eq!(c.cursor_offset, None);
    }

    #[test]
    fn complete_agg_function_avg() {
        let c = complete("| stats av", 10, &fields()).unwrap();
        assert_eq!(c.ghost_text, "g()");
        assert_eq!(c.insert_text, "avg()");
        assert_eq!(c.replace_len, 2);
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
        let c = complete("| stats count() by hostn", 24, &fields()).unwrap();
        assert_eq!(c.ghost_text, "ame");
        assert_eq!(c.insert_text, "hostname");
        assert_eq!(c.replace_len, 5);
    }

    #[test]
    fn complete_field_needing_backticks_inserts_quoted() {
        let mut schema = fields();
        schema.push(SchemaField {
            name: "x-request-id".to_owned(),
            is_numeric: false,
        });
        let c = complete("| table x", 9, &schema).unwrap();
        assert_eq!(c.insert_text, "`x-request-id`");
        assert_eq!(c.ghost_text, "`x-request-id`");
        assert_eq!(c.replace_len, 1);
        assert!(
            trawl_core::parser::parse(&format!("| table {}", c.insert_text)).is_ok(),
            "the inserted text must parse"
        );
    }

    #[test]
    fn completing_an_open_quoted_name_replaces_its_opening_tick() {
        let mut schema = fields();
        schema.push(SchemaField {
            name: "request id".to_owned(),
            is_numeric: false,
        });
        let text = "| table `request i";
        let completion = complete(text, text.len(), &schema).expect("a completion");
        assert_eq!(completion.insert_text, "`request id`");
        assert_eq!(completion.replace_len, "`request i".chars().count());
        let kept = text.chars().count() - completion.replace_len;
        let spliced = format!(
            "{}{}",
            text.chars().take(kept).collect::<String>(),
            completion.insert_text
        );
        assert_eq!(spliced, "| table `request id`");
    }

    #[test]
    fn completion_lengths_are_characters_not_utf8_bytes() {
        let mut schema = fields();
        schema.push(SchemaField {
            name: "日本語".to_owned(),
            is_numeric: false,
        });
        let text = "| table `日本";
        let completion = complete(text, text.len(), &schema).expect("a completion");
        assert_eq!(completion.insert_text, "`日本語`");
        assert_eq!(completion.replace_len, "`日本".chars().count());
    }

    #[test]
    fn unrepresentable_catalog_names_are_not_suggested() {
        let schema = [SchemaField {
            name: "a\u{202e}b".to_owned(),
            is_numeric: false,
        }];
        assert!(complete("| table a", 9, &schema).is_none());
    }

    #[test]
    fn a_data_backtick_does_not_capture_the_rest_of_the_line() {
        let text = "host=/foo`bar/ | table hos";
        let completion = complete(text, text.len(), &fields()).expect("a completion");
        assert_eq!(completion.insert_text, "host");
    }

    #[test]
    fn complete_no_match() {
        assert!(complete("| stats zzz", 11, &fields()).is_none());
    }

    #[test]
    fn complete_exact_match_no_ghost() {
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
        assert_eq!(c.cursor_offset, Some(6));
    }

    #[test]
    fn complete_now_zero_arg() {
        let c = complete("| let x = no", 12, &fields()).unwrap();
        assert_eq!(c.ghost_text, "w()");
        assert_eq!(c.insert_text, "now()");
        assert_eq!(c.replace_len, 2);
        assert_eq!(c.cursor_offset, None);
    }

    #[test]
    fn complete_sort_field() {
        let c = complete("| sort se", 9, &fields()).unwrap();
        assert_eq!(c.ghost_text, "rvice");
        assert_eq!(c.insert_text, "service");
        assert_eq!(c.replace_len, 2);
    }

    #[test]
    fn complete_mid_word_suppressed() {
        let result = complete("| table me", 10, &fields());
        assert!(result.is_some(), "sanity: 'me' at end of input completes");

        let result = complete("| table message", 10, &fields());
        assert!(
            result.is_none(),
            "mid-word cursor should suppress ghost text"
        );
    }

    #[test]
    fn complete_where_stage() {
        let c = complete("| wh", 4, &fields()).unwrap();
        assert_eq!(c.ghost_text, "ere ");
        assert_eq!(c.insert_text, "where ");
        assert_eq!(c.replace_len, 2);
    }

    #[test]
    fn byte_offset_single_line() {
        let lines = vec!["| stats count()".to_owned()];
        assert_eq!(cursor_to_byte_offset(&lines, 0, 4), 4);
    }

    #[test]
    fn byte_offset_multiline() {
        let lines = vec!["| stats count()".to_owned(), "  by host".to_owned()];
        assert_eq!(cursor_to_byte_offset(&lines, 1, 5), 21);
    }

    #[test]
    fn byte_offset_at_line_end() {
        let lines = vec!["abc".to_owned()];
        assert_eq!(cursor_to_byte_offset(&lines, 0, 3), 3);
    }

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
        assert_eq!(find_current_stage("service=nginx _severity=error"), None);
    }

    #[test]
    fn past_by_true() {
        assert!(is_past_by_keyword("| stats count() by", "stats"));
    }

    #[test]
    fn past_by_false() {
        assert!(!is_past_by_keyword("| stats count()", "stats"));
    }

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

    #[test]
    fn prefix_match_exact_suppresses_longer_variant() {
        let candidates = vec![
            Candidate {
                name: "service".to_owned(),
                is_function: false,
                is_zero_arg: false,
            },
            Candidate {
                name: "services".to_owned(),
                is_function: false,
                is_zero_arg: false,
            },
        ];
        assert!(prefix_match("service", &candidates).is_none());
    }

    #[test]
    fn complete_exact_field_no_ghost_with_longer_variant() {
        let schema = vec![
            SchemaField {
                name: "service".to_owned(),
                is_numeric: false,
            },
            SchemaField {
                name: "services".to_owned(),
                is_numeric: false,
            },
        ];
        assert!(complete("| stats count() by service", 26, &schema).is_none());
    }
}
