// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one text-level scan over a DSL query that can change an ANSWER
//! (ADR-0014 ruling 5).
//!
//! The web UI's date-range popover rewrites a span this walk picks — where
//! the search stage ends, and whether a `last=` clause is already there —
//! so a span chosen wrongly silently drops or duplicates a time bound.
//! It lives here, beside the grammar it approximates, rather than in the
//! consumer.
//!
//! This is NOT the parser and never will be: it runs on partial,
//! mid-typing input the grammar would reject outright, so it is a
//! deliberate approximation with its rules stated. The grammar remains
//! the authority on what a query MEANS; this only decides where to splice
//! text.

/// Walk `input`'s bytes, calling `hit` only for bytes that sit OUTSIDE a
/// delimited span — a double-quoted string, a `/`-delimited regex
/// literal, a backtick-quoted field name (ADR-0013 ruling 7), or a
/// comment (ADR-0014). Returns the index of the first byte `hit`
/// accepted.
///
/// Backticks matter as much as quotes: a backticked name can spell any
/// character, so it can carry a `|` or the text `last=` these walks would
/// otherwise read as grammar. Backslash escapes are honoured everywhere
/// except inside backticks, whose only escape is a doubled backtick
/// (which this walk sees as a close immediately followed by a re-open, so
/// no inner byte leaks out as bare).
///
/// Three rules are worth stating outright:
///
/// * **`'` is not a delimiter.** The DSL has no single-quoted string, so
///   treating one as an opener made an apostrophe (`message=it's`)
///   swallow the rest of the query.
/// * **A `/` opens a regex only at a token boundary** — start of input,
///   or after whitespace or one of the bytes a value may follow
///   (`= < > ! , ( |`). A slash mid-token is ordinary data, which is what
///   keeps `url=https://a/b last=1h` and `path=/api//v1 last=1h` from
///   hiding their `last=` clause inside a phantom regex span. The cost,
///   accepted: a division written with spaces (`| let x = 1 / 2`) reads
///   as a regex open. That is confined to the pipeline tail, which
///   neither consumer scans.
/// * **A `#` opens a comment only after whitespace or at the start of
///   input** — the same structural boundary the grammar uses
///   (`parser::comment`). A comment runs to `\n`, and nothing inside it
///   is offered to `hit`, so a `|` or `last=` written in one is prose.
///
/// On unterminated input — a quote or regex the user has not closed yet —
/// the open span runs to end of input and nothing after it is offered.
/// That is the existing fail-shape and is deliberate: mid-typing, the
/// safest reading of an unclosed quote is that everything after it is
/// still inside it.
pub fn scan_outside_quotes<F>(input: &str, mut hit: F) -> Option<usize>
where
    F: FnMut(usize, u8) -> bool,
{
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut delim: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match delim {
            None => match b {
                b'\\' if i + 1 < bytes.len() => i += 2,
                b'#' if at_layout_boundary(bytes, i) => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if at_boundary(bytes, i) => {
                    delim = Some(b);
                    i += 1;
                }
                b'"' | b'`' => {
                    delim = Some(b);
                    i += 1;
                }
                _ => {
                    if hit(i, b) {
                        return Some(i);
                    }
                    i += 1;
                }
            },
            Some(d) => {
                if d != b'`' && b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    if b == d {
                        delim = None;
                    }
                    i += 1;
                }
            }
        }
    }
    None
}

/// Whether the byte at `i` sits where the GRAMMAR admits a comment: the
/// beginning of the input, or directly after whitespace
/// (`parser::comment::ws` / `leading_ws`). Deliberately narrower than
/// [`at_boundary`] — admitting `=` would read `color=#ff0000` as a
/// comment, which is exactly the shape the grammar refuses.
fn at_layout_boundary(bytes: &[u8], i: usize) -> bool {
    i.checked_sub(1)
        .map(|p| bytes[p])
        .is_none_or(|p| p.is_ascii_whitespace())
}

/// Whether the byte at `i` sits where a new VALUE could START: the
/// beginning of the input, after whitespace, or after one of the bytes a
/// value or a regex may directly follow.
fn at_boundary(bytes: &[u8], i: usize) -> bool {
    match i.checked_sub(1).map(|p| bytes[p]) {
        None => true,
        Some(p) => {
            p.is_ascii_whitespace() || matches!(p, b'=' | b'<' | b'>' | b'!' | b',' | b'(' | b'|')
        }
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

    /// A `//` in a value is ordinary data now (ADR-0014 ruling 3), so the
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
        // …and a `#` INSIDE a token is not a comment (ADR-0014 ruling 2).
        assert!(has_last("color=#ff0000 last=1h"));
    }

    /// Quoted spans keep their contents out of the walk.
    #[test]
    fn quotes_and_backticks_shield_their_contents() {
        assert!(!has_last(r#"message="last=1h""#));
        assert!(!has_last("`last=1h`=x"));
        assert_eq!(first_pipe(r#"message="a|b" | stats count()"#), Some(14));
        assert_eq!(first_pipe("`a|b`=1 | stats count()"), Some(8));
    }

    /// An unterminated span runs to end of input — the existing
    /// fail-shape for partial, mid-typing text.
    #[test]
    fn an_unterminated_span_runs_to_end_of_input() {
        assert_eq!(first_pipe(r#"message="a | stats"#), None);
        assert_eq!(first_pipe("message=/a | stats"), None);
    }
}
