// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one text-level scan over a DSL query that can change an answer
//! (ADR-0014 ruling 5).
//!
//! This is not the parser: it runs on partial, mid-typing input the
//! grammar would reject outright, so it is a deliberate approximation.
//! The grammar is the authority on what a query means; this only decides
//! where to splice text.

use crate::parser::comment::{is_layout, opens_comment_after};
use crate::parser::primitives::first_cut;

/// Walk `input`'s bytes, calling `hit` only for bytes that sit outside a
/// delimited span — a double-quoted string, a `/`-delimited regex
/// literal, a backtick-quoted field name, or a comment. Returns the index
/// of the first byte `hit` accepted.
///
/// * **A backslash escapes only inside a quoted string.** The grammar has
///   no escape in an unquoted position, so honouring one outside would
///   read the `last=` in `foo\" last=1h"` as grammar while the parser
///   reads it as the contents of a quoted phrase. A backtick's only escape
///   is a doubled backtick, which this walk sees as a close and a re-open.
/// * **`'` is not a delimiter.** The DSL has no single-quoted string, so
///   an apostrophe (`message=it's`) would swallow the rest of the query.
/// * **A `/` opens a regex only at a token boundary, and only when the
///   text up to the next `/` is a body the grammar admits**
///   (`primitives::first_cut`). A slash mid-token is ordinary data, which
///   keeps `url=https://a/b last=1h` from hiding its `last=` clause.
/// * **A `#` opens a comment only after ASCII whitespace or at the start
///   of input**, tested through the grammar's own predicate.
///
/// An unclosed quote or backticked name runs to end of input: mid-typing,
/// everything after it is still inside it. An unclosed `/` opens nothing.
pub fn scan_outside_quotes<F>(input: &str, mut hit: F) -> Option<usize>
where
    F: FnMut(usize, u8) -> bool,
{
    walk(input, &mut hit).0
}

/// Whether `input` ends inside an open comment — the walk's terminal
/// state, which decides whether text appended to it would land in prose.
#[must_use]
pub fn ends_inside_comment(input: &str) -> bool {
    matches!(walk(input, &mut |_, _| false).1, Span::Comment)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Span {
    Bare,
    Delim(u8),
    Comment,
}

fn walk<F>(input: &str, hit: &mut F) -> (Option<usize>, Span)
where
    F: FnMut(usize, u8) -> bool,
{
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut span = Span::Bare;
    while i < bytes.len() {
        let b = bytes[i];
        match span {
            Span::Bare => match b {
                b'#' if at_layout_boundary(input, i) => {
                    span = Span::Comment;
                    i += 1;
                }
                b'/' if regex_at(input, i).is_some() => {
                    i += regex_at(input, i).unwrap_or(1);
                }
                b'"' | b'`' => {
                    span = Span::Delim(b);
                    i += 1;
                }
                _ => {
                    if hit(i, b) {
                        return (Some(i), span);
                    }
                    i += 1;
                }
            },
            Span::Comment => {
                if b == b'\n' {
                    span = Span::Bare;
                }
                i += 1;
            }
            Span::Delim(d) => {
                if d == b'"' && b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    if b == d {
                        span = Span::Bare;
                    }
                    i += 1;
                }
            }
        }
    }
    (None, span)
}

/// Deliberately narrower than [`at_boundary`]: admitting `=` would read
/// `color=#ff0000` as a comment, the shape the grammar refuses.
fn at_layout_boundary(input: &str, i: usize) -> bool {
    opens_comment_after(input[..i].chars().next_back())
}

/// The length of the regex that opens at byte `i`, its slashes included:
/// a `/` at a token boundary, a body the grammar admits, and the `/` that
/// closes it. `None` when no regex opens there.
fn regex_at(input: &str, i: usize) -> Option<usize> {
    if !at_boundary(input, i) {
        return None;
    }
    let body = input[i..].strip_prefix('/')?;
    let close = body.find('/')?;
    let body = &body[..close];
    (!body.is_empty() && first_cut(body).is_none()).then_some(close + 2)
}

fn at_boundary(input: &str, i: usize) -> bool {
    match input[..i].chars().next_back() {
        None => true,
        Some(p) => is_layout(p) || matches!(p, '=' | '<' | '>' | '!' | ',' | '(' | '|'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_pipe(input: &str) -> Option<usize> {
        scan_outside_quotes(input, |_, b| b == b'|')
    }

    fn has_last(input: &str) -> bool {
        let lower = input.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        scan_outside_quotes(&lower, |i, _| bytes[i..].starts_with(b"last=")).is_some()
    }

    #[test]
    fn slashes_in_a_value_do_not_hide_a_later_clause() {
        for input in [
            "url=https://a/b last=1h",
            "path=/api//v1 last=1h",
            "url=//cdn.example.com/x last=1h",
            "referrer=https://a.b/c status=200 last=1h",
        ] {
            assert!(has_last(input), "{input:?} carries a last= clause");
            assert_eq!(first_pipe(input), None, "{input:?} has no pipeline");
        }
    }

    #[test]
    fn a_regex_body_is_skipped() {
        assert!(!has_last("message=/a last=1h b/"));
        assert_eq!(first_pipe("message=/a|b/ | stats count()"), Some(14));
    }

    #[test]
    fn an_apostrophe_is_not_a_delimiter() {
        assert!(has_last("message=it's last=1h"));
        assert_eq!(first_pipe("message=it's | stats count()"), Some(13));
    }

    #[test]
    fn a_comment_hides_its_content() {
        assert!(!has_last("service=x # last=1h"));
        assert!(has_last("service=x # note\nlast=1h"));
        assert_eq!(first_pipe("service=x # | stats count()"), None);
        assert!(has_last("color=#ff0000 last=1h"));
    }

    #[test]
    fn a_backslash_escapes_only_inside_a_delimited_span() {
        assert!(!has_last(r#"foo\" last=1h""#));
        assert_eq!(first_pipe(r#"foo\" | stats"#), None);
        assert!(has_last(r#"message="a\"b" last=1h"#));
        assert_eq!(first_pipe(r#"message="a\"|b" | stats"#), Some(16));
    }

    #[test]
    fn only_ascii_whitespace_opens_a_comment() {
        assert!(has_last("message=\"x\"\u{a0}# last=1h"));
        assert!(has_last("a=1\u{2003}# last=1h"));
        // `a=1` + a two-byte no-break space + `# ` puts the pipe at 7
        assert_eq!(first_pipe("a=1\u{a0}# | stats count()"), Some(7));
        for ws in [" ", "\t", "\n", "\r"] {
            assert!(!has_last(&format!("a=1{ws}# last=1h")), "{ws:?}");
        }
    }

    #[test]
    fn ends_inside_comment_reports_the_open_comment() {
        assert!(ends_inside_comment("service=x # note"));
        assert!(ends_inside_comment("# note"));
        assert!(!ends_inside_comment("service=x # note\n"));
        assert!(!ends_inside_comment("service=x"));
        assert!(!ends_inside_comment("service=\"# not a comment\""));
        assert!(!ends_inside_comment("color=#ff0000"));
    }

    #[test]
    fn quotes_and_backticks_shield_their_contents() {
        assert!(!has_last(r#"message="last=1h""#));
        assert!(!has_last("`last=1h`=x"));
        assert_eq!(first_pipe(r#"message="a|b" | stats count()"#), Some(14));
        assert_eq!(first_pipe("`a|b`=1 | stats count()"), Some(8));
    }

    #[test]
    fn an_unterminated_span_runs_to_end_of_input() {
        assert_eq!(first_pipe(r#"message="a | stats"#), None);
        assert_eq!(first_pipe("message=/a | stats"), Some(11));
        assert!(has_last("message=/a last=1h"));
    }

    #[test]
    fn scan_ends_a_slash_glob_at_a_pipe_stage() {
        assert_eq!(first_pipe("path=/tmp* | where x / 2 == 1"), Some(11));
        assert_eq!(first_pipe("path=/tmp*\n| where x / 2 == 1"), Some(11));
        assert!(has_last("msg=/a\nlast=1h b/"));
        assert_eq!(first_pipe("msg=/err|warn/ | stats count()"), Some(15));
        assert_eq!(first_pipe(r"msg=/a\/ | stats"), Some(9));
    }
}
