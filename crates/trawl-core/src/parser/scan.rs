// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one text-level scan over a DSL query that can change an answer
//! (ADR-0014 ruling 5).
//!
//! The web UI's date-range popover rewrites a span this walk picks — where
//! the search stage ends, and whether a `last=` clause is already there —
//! so a span chosen wrongly silently drops or duplicates a time bound.
//! It lives here, beside the grammar it approximates, rather than in the
//! consumer.
//!
//! This is not the parser and never will be: it runs on partial,
//! mid-typing input the grammar would reject outright, so it is a
//! deliberate approximation with its rules stated. The grammar is the
//! authority on what a query means; this only decides where to splice
//! text.

use crate::parser::comment::{is_layout, opens_comment_after};

/// Walk `input`'s bytes, calling `hit` only for bytes that sit outside a
/// delimited span — a double-quoted string, a `/`-delimited regex
/// literal, a backtick-quoted field name (ADR-0013 ruling 7), or a
/// comment (ADR-0014). Returns the index of the first byte `hit`
/// accepted.
///
/// Backticks matter as much as quotes: a backticked name can spell any
/// character, so it can carry a `|` or the text `last=` these walks would
/// otherwise read as grammar.
///
/// Four rules are worth stating outright:
///
/// * **A backslash escapes only inside a quoted string or a regex body.**
///   The grammar has no escape in an unquoted position — a bare `\` is
///   ordinary text — so honouring one outside a delimited span would read
///   the `last=` in `foo\" last=1h"` as grammar while the parser reads it
///   as the contents of a quoted phrase. Backticks have no backslash
///   escape either: their only escape is a doubled backtick, which this
///   walk sees as a close immediately followed by a re-open, so no inner
///   byte leaks out as bare.
/// * **`'` is not a delimiter.** The DSL has no single-quoted string, so
///   treating one as an opener would let an apostrophe (`message=it's`)
///   swallow the rest of the query.
/// * **A `/` opens a regex only at a token boundary** — start of input,
///   or after whitespace or one of the bytes a value may follow
///   (`= < > ! , ( |`). A slash mid-token is ordinary data, which is what
///   keeps `url=https://a/b last=1h` and `path=/api//v1 last=1h` from
///   hiding their `last=` clause inside a phantom regex span. The cost,
///   accepted: a division written with spaces (`| let x = 1 / 2`) reads
///   as a regex open. That is confined to the pipeline tail, which
///   neither consumer scans.
/// * **A `#` opens a comment only after ASCII whitespace or at the start
///   of input** — the same structural boundary the grammar uses, tested
///   through the grammar's own predicate
///   (`parser::comment::opens_comment_after`) rather than a copy that
///   could drift. ASCII and not Unicode because the unquoted token
///   charsets end on ASCII whitespace: `message="x"\u{a0}# last=1h` is a
///   parse error, so a walk that read it as a comment would be answering
///   a question the grammar answers differently. A comment runs to `\n`,
///   and nothing inside it is offered to `hit`, so a `|` or `last=`
///   written in one is prose.
///
/// On unterminated input — a quote or regex the user has not closed yet —
/// the open span runs to end of input and nothing after it is offered.
/// That fail-shape is deliberate: mid-typing, the safest reading of an
/// unclosed quote is that everything after it is still inside it.
pub fn scan_outside_quotes<F>(input: &str, mut hit: F) -> Option<usize>
where
    F: FnMut(usize, u8) -> bool,
{
    walk(input, &mut hit).0
}

/// Whether `input` ends inside an open comment — the walk's terminal
/// state, which decides whether text appended to it would land in prose.
///
/// The web UI's date-range merge asks this before it joins a rewritten
/// search stage back to its pipeline: `service=x # note` ends inside a
/// comment, so a ` | stats …` tail spliced on the same line would be
/// swallowed whole and the query would parse as something else entirely.
#[must_use]
pub fn ends_inside_comment(input: &str) -> bool {
    matches!(walk(input, &mut |_, _| false).1, Span::Comment)
}

/// The state the walk can be in when it reaches the end of the input.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Span {
    /// Between tokens — bytes here are offered to `hit`.
    Bare,
    /// Inside a `"` string, a `/` regex body or a `` ` `` name.
    Delim(u8),
    /// Inside a comment, which ends at the next `\n`.
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
                b'/' if at_boundary(input, i) => {
                    span = Span::Delim(b);
                    i += 1;
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
                if d != b'`' && b == b'\\' && i + 1 < bytes.len() {
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

/// Whether the byte at `i` sits where the grammar admits a comment: the
/// beginning of the input, or directly after an ASCII whitespace
/// character — asked of `parser::comment`'s own predicate, the one both
/// `ws`/`leading_ws` and this walk go through. Deliberately narrower than
/// [`at_boundary`] — admitting `=` would read `color=#ff0000` as a
/// comment, which is exactly the shape the grammar refuses.
fn at_layout_boundary(input: &str, i: usize) -> bool {
    opens_comment_after(input[..i].chars().next_back())
}

/// Whether the byte at `i` sits where a new value could start: the
/// beginning of the input, after whitespace, or after one of the bytes a
/// value or a regex may directly follow.
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

    /// A `//` in a value is ordinary data (ADR-0014 ruling 3), so the
    /// slashes must not open a phantom regex that hides the clause the
    /// date-range popover is looking for.
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

    /// A real regex literal still shields its body.
    #[test]
    fn a_regex_body_is_skipped() {
        assert!(!has_last("message=/a last=1h b/"));
        assert_eq!(first_pipe("message=/a|b/ | stats count()"), Some(14));
    }

    /// The DSL has no single-quoted string, so an apostrophe is data.
    #[test]
    fn an_apostrophe_is_not_a_delimiter() {
        assert!(has_last("message=it's last=1h"));
        assert_eq!(first_pipe("message=it's | stats count()"), Some(13));
    }

    /// A comment's content is prose, not grammar.
    #[test]
    fn a_comment_hides_its_content() {
        assert!(!has_last("service=x # last=1h"));
        assert!(has_last("service=x # note\nlast=1h"));
        assert_eq!(first_pipe("service=x # | stats count()"), None);
        // …and a `#` inside a token is not a comment (ADR-0014 ruling 2).
        assert!(has_last("color=#ff0000 last=1h"));
    }

    /// The grammar has no escape outside a delimited span, so a bare
    /// backslash must not hide the quote that follows it: reading `foo\"`
    /// as an escaped quote would offer a `last=` that the parser sees
    /// inside a quoted phrase.
    #[test]
    fn a_backslash_escapes_only_inside_a_delimited_span() {
        assert!(!has_last(r#"foo\" last=1h""#));
        assert_eq!(first_pipe(r#"foo\" | stats"#), None);
        // …while inside one it still does
        assert!(has_last(r#"message="a\"b" last=1h"#));
        assert_eq!(first_pipe(r#"message="a\"|b" | stats"#), Some(16));
    }

    /// The comment boundary is ASCII whitespace, because the unquoted
    /// token charsets end on ASCII whitespace: a `#` after a no-break
    /// space is inside a token the grammar refuses, so the walk must not
    /// read it as a comment and hide what follows.
    #[test]
    fn only_ascii_whitespace_opens_a_comment() {
        assert!(has_last("message=\"x\"\u{a0}# last=1h"));
        assert!(has_last("a=1\u{2003}# last=1h"));
        // `a=1` + a two-byte no-break space + `# ` puts the pipe at 7
        assert_eq!(first_pipe("a=1\u{a0}# | stats count()"), Some(7));
        // …every ASCII whitespace character does open one
        for ws in [" ", "\t", "\n", "\r"] {
            assert!(!has_last(&format!("a=1{ws}# last=1h")), "{ws:?}");
        }
    }

    /// The walk's terminal state, which the date-range merge asks before
    /// it splices a pipeline back on.
    #[test]
    fn ends_inside_comment_reports_the_open_comment() {
        assert!(ends_inside_comment("service=x # note"));
        assert!(ends_inside_comment("# note"));
        assert!(!ends_inside_comment("service=x # note\n"));
        assert!(!ends_inside_comment("service=x"));
        assert!(!ends_inside_comment("service=\"# not a comment\""));
        assert!(!ends_inside_comment("color=#ff0000"));
    }

    /// Quoted spans keep their contents out of the walk.
    #[test]
    fn quotes_and_backticks_shield_their_contents() {
        assert!(!has_last(r#"message="last=1h""#));
        assert!(!has_last("`last=1h`=x"));
        assert_eq!(first_pipe(r#"message="a|b" | stats count()"#), Some(14));
        assert_eq!(first_pipe("`a|b`=1 | stats count()"), Some(8));
    }

    /// An unterminated span runs to end of input, the fail-shape for
    /// partial, mid-typing text.
    #[test]
    fn an_unterminated_span_runs_to_end_of_input() {
        assert_eq!(first_pipe(r#"message="a | stats"#), None);
        assert_eq!(first_pipe("message=/a | stats"), None);
    }
}
