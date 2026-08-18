// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Comments, and the ONE padding owner every whitespace-skipping site in
//! the grammar goes through (ADR-0014).
//!
//! A comment is a grammar production — `'#' (!'\n')*` — admitted only
//! where the grammar already sits between tokens. There is no pre-parse
//! scanner and nothing re-derives "am I inside a string / regex /
//! backtick name": the parser *is* inside them, so a `#` is only ever
//! tested at a position the grammar already knows is a token boundary.
//!
//! The boundary is encoded structurally, with no lookback:
//! [`ws`] is `(whitespace+ comment?)*`, so a comment is reachable only
//! after consumed whitespace, and [`leading_ws`] adds the one other
//! comment site — the start of the input. That whitespace must be ASCII
//! ([`opens_comment_after`]), because the unquoted token charsets end on
//! ASCII whitespace alone. Consequence, deliberate:
//! `count(),# x` and `(#x` are parse errors, not comments (the "after a
//! delimiter" case is unenumerable — admitting `=` would stop
//! `color=#ff0000` erroring).
//!
//! Every site that used to call chumsky's `.padded()` calls
//! [`Spaced::spaced`] instead, and `chumsky::Parser::padded` is in
//! clippy's `disallowed_methods` so a missed site — or a new one added
//! later — fails the build rather than silently becoming a place
//! comments do not work (ADR-0014 ruling 4).

use chumsky::input::InputRef;
use chumsky::prelude::*;

use crate::parser::primitives::{ParserExtra, ParserInput};

/// The one comment opener. `//` was dropped in ADR-0014 ruling 3: it
/// collides with every URL scheme, and it blanked to end-of-line, so it
/// deleted sibling filters rather than merely truncating its own token.
pub(crate) const OPENER: char = '#';

/// The message a comment opener INSIDE an unquoted token carries
/// (ADR-0014 ruling 2). Never data, never a comment: quoted strings,
/// backticked names and regex bodies carry `#` verbatim, so there is
/// always an escape.
///
/// The rule is one rule, but the WORKING SPELLING differs by position,
/// and a hint whose advice changes what the query means is worse than no
/// hint at all — quoting a whole `color=#ff0000` turns a field filter
/// into a phrase search. So the position is carried in the message, and
/// [`hint_for`] is the one place each position's escape is written down.
pub(crate) const MSG_OPENER_IN_TOKEN: &str = "'#' inside an unquoted token";

/// [`MSG_OPENER_IN_TOKEN`] for the VALUE half of a field filter, whose
/// escape quotes the value alone (`color="#ff0000"`).
pub(crate) const MSG_OPENER_IN_VALUE: &str = "'#' inside an unquoted value";

/// [`MSG_OPENER_IN_TOKEN`] for a NEGATED search term, where quoting is no
/// escape at all — `-"a#b"` is not a negated phrase, it is a bare term
/// spelling literal quote characters. `NOT "a#b"` is the working form.
pub(crate) const MSG_OPENER_IN_NEGATED_TERM: &str = "'#' inside a negated search term";

/// [`MSG_OPENER_IN_TOKEN`] for a pipe stage name (`| co#unt()`), where
/// quoting is no escape either: a stage name is grammar, not data.
pub(crate) const MSG_OPENER_IN_COMMAND: &str = "'#' inside a pipe stage name";

/// The message a search-stage bare term STARTING with `//` carries — the
/// loud half of dropping the second opener (ADR-0014 ruling 3).
pub(crate) const MSG_SLASHES_NOT_A_COMMENT: &str =
    "'//' does not start a comment — '#' is the comment character";

/// The hint for [`MSG_SLASHES_NOT_A_COMMENT`].
const HINT_SLASHES: &str =
    "write '#' to start a comment, or quote the term to search for the slashes";

/// The `//` opener a search-stage bare term may not START with.
pub(crate) const SLASHES: &str = "//";

/// What counts as LAYOUT between tokens: `char::is_whitespace`, Unicode
/// and all, exactly as chumsky's `.padded()` always was. This is
/// consumption only — it says what separates two tokens, never where a
/// comment may open.
pub(crate) const fn is_layout(c: char) -> bool {
    c.is_whitespace()
}

/// Whether a comment may OPEN directly after `prev` — the character
/// immediately before the `#`, or `None` at the start of the input.
///
/// The boundary is ASCII whitespace, deliberately NARROWER than
/// [`is_layout`], because the unquoted token charsets end on ASCII
/// whitespace and nothing else: a no-break space is an ordinary character
/// INSIDE a bare word or value. Reading `message="x"\u{a0}# last=1h` as a
/// comment while the token grammar reads `foo\u{a0}#` as one token would
/// be the parser and the scanner answering the same question two ways.
/// Both consequences are LOUD — `"x"\u{a0}# c` is a parse error at the
/// `#`, `foo\u{a0}# c` is the inside-a-token error — never a silently
/// different query.
///
/// The ONE predicate: [`skip_layout`] and [`crate::parser::scan`] both
/// ask it, of the same character.
pub(crate) fn opens_comment_after(prev: Option<char>) -> bool {
    prev.is_none_or(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

/// Consume one comment if the cursor is sitting on its opener: the
/// opener, then everything up to (not including) the newline that ends
/// it. The line boundary is `\n` alone — a `\r` is an ordinary character
/// that may sit inside a comment, and there is no CRLF machinery
/// anywhere.
fn skip_comment<'src>(inp: &mut InputRef<'src, '_, ParserInput<'src>, ParserExtra<'src>>) {
    if inp.peek() != Some(OPENER) {
        return;
    }
    inp.skip();
    while let Some(c) = inp.peek() {
        if c == '\n' {
            break;
        }
        inp.skip();
    }
}

/// Consume the layout run `(whitespace+ comment?)*`.
///
/// Each iteration consumes at least one whitespace character, so the walk
/// always terminates. A comment is reachable only after that whitespace —
/// which is precisely the token-boundary rule, encoded structurally
/// rather than tested by a lookback — and only when the LAST whitespace
/// character consumed is one [`opens_comment_after`] admits, so the run
/// `foo\u{a0}` leaves the following `#` to the token grammar.
fn skip_layout<'src>(inp: &mut InputRef<'src, '_, ParserInput<'src>, ParserExtra<'src>>) {
    loop {
        let mut last = None;
        while let Some(c) = inp.peek() {
            if !is_layout(c) {
                break;
            }
            inp.skip();
            last = Some(c);
        }
        if !(last.is_some() && opens_comment_after(last)) {
            return;
        }
        skip_comment(inp);
    }
}

/// The layout run, as a parser.
///
/// Written as a `custom` walk rather than a combinator tower on purpose:
/// this parser is embedded at all 84 padding call sites, several of them
/// inside the recursive expression grammar, and a combinator form
/// (`filter+ then comment? repeated`) inflates the nested parser TYPE
/// enough to overflow the stack on a deep pipeline — the failure mode
/// `expr.rs`'s existing `.boxed()` was added for. `custom` is one opaque
/// type and allocates nothing.
pub(crate) fn ws<'src>() -> impl Parser<'src, ParserInput<'src>, (), ParserExtra<'src>> + Clone {
    custom(|inp| {
        skip_layout(inp);
        Ok(())
    })
}

/// [`ws`] plus the one comment site whitespace cannot precede: the start
/// of the input. Used exactly once, at the front of the query parser.
pub(crate) fn leading_ws<'src>()
-> impl Parser<'src, ParserInput<'src>, (), ParserExtra<'src>> + Clone {
    custom(|inp| {
        skip_comment(inp);
        skip_layout(inp);
        Ok(())
    })
}

/// Byte offset of the first comment opener in an unquoted token's text,
/// or `None` when it carries none. The one place the validators on
/// [`crate::parser::primitives::bare_value`] and the search-stage bare
/// word ask the question.
pub(crate) fn first_opener(text: &str) -> Option<usize> {
    text.find(OPENER)
}

/// Whether a search-stage bare term opens with `//` — an old-style
/// comment line, which must fail loudly instead of becoming AND-ed text
/// terms that narrow the result set to nothing.
pub(crate) fn starts_with_slashes(text: &str) -> bool {
    text.starts_with(SLASHES)
}

/// The hint for one of this module's messages, derived from the real
/// input and the error's own span. ONE owner: both emitters — the
/// `validate` calls on the value and term productions, and the generic
/// net in [`crate::parser::rich_to_parse_error`] — come here, so a
/// position's working spelling is written down once.
///
/// Every hint offers something the user can FOLLOW without changing what
/// the query means, which is why the position rides in the message:
/// quoting the whole of `color=#ff0000` is a phrase search, and quoting
/// a `-`-negated term spells literal quote characters.
///
/// The token is recovered from the input rather than carried in the
/// message: `Rich::custom` transports a string and nothing else, and the
/// error conversion in [`crate::parser::rich_to_parse_error`] already
/// holds both the source text and the offset. That recovery is hint
/// RENDERING only — it never decides what anything means, and the
/// grammar has already refused the text by the time it runs.
pub(crate) fn hint_for(msg: &str, input: &str, offset: usize) -> Option<String> {
    let comment_half = format!("put whitespace before the '{OPENER}' to start a comment");
    match msg {
        MSG_SLASHES_NOT_A_COMMENT => Some(HINT_SLASHES.to_string()),
        MSG_OPENER_IN_TOKEN => Some(format!(
            "quote it (\"{}\") to search for it, or {comment_half}",
            token_around(input, offset)
        )),
        MSG_OPENER_IN_VALUE => Some(format!(
            "quote the value (\"{}\"), or {comment_half}",
            value_around(input, offset)
        )),
        MSG_OPENER_IN_NEGATED_TERM => Some(format!(
            "write it as NOT {}, or {comment_half}",
            quoted_term(token_around(input, offset))
        )),
        MSG_OPENER_IN_COMMAND => Some(comment_half),
        _ => None,
    }
}

/// The unquoted token surrounding `offset` and the byte it starts at: the
/// run of non-ASCII-whitespace, non-`|` bytes around it. Used to render a
/// hint, and to decide whether that token is a pipe stage name.
///
/// ASCII whitespace because that is where the unquoted token charsets end
/// — a no-break space sits INSIDE a bare word, so a hint bounded by
/// Unicode whitespace would quote less than the user has to change.
pub(crate) fn token_span(input: &str, offset: usize) -> (usize, &str) {
    let ends = |c: char| c.is_ascii_whitespace() || c == '|';
    let start = input[..offset.min(input.len())].rfind(ends).map_or(0, |i| {
        i + input[i..].chars().next().map_or(1, char::len_utf8)
    });
    let rest = &input[start..];
    let end = rest.find(ends).unwrap_or(rest.len());
    (start, &rest[..end])
}

fn token_around(input: &str, offset: usize) -> &str {
    token_span(input, offset).1
}

/// The bytes a filter VALUE may directly follow. The value half of the
/// token surrounding `offset` starts after the last of them — which is
/// what lets the hint say `color="#ff0000"` rather than quoting the
/// field name into the string with it.
const VALUE_STARTERS: [char; 6] = ['=', '<', '>', '!', ',', '('];

/// The value slice surrounding `offset` inside its token. Hint rendering
/// only: it never decides what anything MEANS, and the grammar has
/// already refused this token by the time it is called.
///
/// Bounded on BOTH sides by the comma-delimited element, not just on the
/// left: an IN list is one whitespace-delimited token, so a hint that ran
/// to the end of it copied every remaining element — and every element
/// carrying a `#` emits its own diagnostic, which made the rendered
/// hints quadratic in the length of the list. The element is also the
/// only slice a user can act on: quoting `"#a"` is the fix, quoting
/// `"#a,#b,#c"` is a different query.
fn value_around(input: &str, offset: usize) -> &str {
    let (start, token) = token_span(input, offset);
    let at = offset.saturating_sub(start).min(token.len());
    let from = token[..at].rfind(VALUE_STARTERS).map_or(0, |i| i + 1);
    let end = token[at..].find(',').map_or(token.len(), |i| at + i);
    &token[from..end]
}

/// A negated term's working spelling under `NOT`: the term without its
/// leading `-`, double-quoted unless it already is.
fn quoted_term(token: &str) -> String {
    let inner = token.strip_prefix('-').unwrap_or(token);
    if inner.len() >= 2 && inner.starts_with('"') && inner.ends_with('"') {
        inner.to_string()
    } else {
        format!("\"{inner}\"")
    }
}

/// The comment opener inside a PIPE STAGE NAME containing `offset`, if
/// there is one — the position chumsky reports as a truncated unknown
/// command (`| co#unt()` → "unknown command 'co'") because the stage word
/// ends at the `#`.
///
/// Narrow on purpose: the token containing the offset must itself be the
/// word directly after the `|`, so `| stats count(),# x` — whose real
/// problem is the `,` — keeps its own diagnostic.
pub(crate) fn opener_in_command_word(input: &str, offset: usize) -> Option<usize> {
    let (start, token) = token_span(input, offset);
    if !input[..start].trim_end().ends_with('|') {
        return None;
    }
    first_opener(token).map(|at| start + at)
}

/// The project-owned padding combinator: whitespace and comments on both
/// sides of a token.
///
/// Every former `.padded()` site adopts this, which is what makes
/// `a=1 # note\nhost=x` and `| stats count()\n# note\n| sort -count`
/// work with no special case — `.padded()` is also what separates search
/// tokens and pipeline stages.
pub(crate) trait Spaced<'src, O>:
    Parser<'src, ParserInput<'src>, O, ParserExtra<'src>> + Clone + Sized
{
    /// Skip whitespace and comments before and after this parser.
    fn spaced(self) -> impl Parser<'src, ParserInput<'src>, O, ParserExtra<'src>> + Clone {
        self.padded_by(ws())
    }
}

impl<'src, O, P> Spaced<'src, O> for P where
    P: Parser<'src, ParserInput<'src>, O, ParserExtra<'src>> + Clone
{
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_consumes_whitespace_and_comments() {
        for input in ["", "   ", " # note", " # note\n  ", "\n\t # a\n# b\n"] {
            assert!(
                ws().then_ignore(end()).parse(input).into_result().is_ok(),
                "{input:?} must be pure layout"
            );
        }
    }

    /// A comment is reachable only AFTER whitespace — the token-boundary
    /// rule, structural and without lookback.
    #[test]
    fn ws_does_not_open_a_comment_without_whitespace() {
        assert!(
            ws().then_ignore(end())
                .parse("# note")
                .into_result()
                .is_err()
        );
        // …and start-of-input is the one other site, spelled separately.
        assert!(
            leading_ws()
                .then_ignore(end())
                .parse("# note")
                .into_result()
                .is_ok()
        );
    }

    /// The boundary is ASCII whitespace: a no-break space is layout the
    /// run consumes, but it does not open a comment, because the token
    /// charsets carry it INSIDE a bare word.
    #[test]
    fn only_ascii_whitespace_opens_a_comment() {
        for c in [' ', '\t', '\n', '\r'] {
            assert!(opens_comment_after(Some(c)), "{c:?}");
        }
        for c in ['\u{a0}', '\u{2003}', '\u{feff}', 'x'] {
            assert!(!opens_comment_after(Some(c)), "{c:?}");
        }
        assert!(opens_comment_after(None), "start of input");

        // …and the layout run agrees: `\u{a0}#` leaves the `#` unconsumed
        assert!(
            ws().then_ignore(end())
                .parse("\u{a0}# note")
                .into_result()
                .is_err()
        );
        // while a following ASCII space re-opens the site
        assert!(
            ws().then_ignore(end())
                .parse("\u{a0} # note")
                .into_result()
                .is_ok()
        );
        // …and Unicode whitespace is still LAYOUT, consumed as ever
        assert!(
            ws().then_ignore(end())
                .parse("\u{a0}\u{2003}")
                .into_result()
                .is_ok()
        );
    }

    /// The line boundary is `\n` alone; `\r` sits inside a comment.
    #[test]
    fn carriage_return_is_comment_content() {
        assert!(
            leading_ws()
                .then_ignore(end())
                .parse("# a\rb\n")
                .into_result()
                .is_ok()
        );
    }

    #[test]
    fn token_around_recovers_the_offending_token() {
        assert_eq!(token_around("foo#bar", 3), "foo#bar");
        assert_eq!(token_around("a=1 color=#ff0000", 10), "color=#ff0000");
        assert_eq!(token_around("x|color=#f", 8), "color=#f");
    }

    /// A hint the user can FOLLOW: quoting the whole `color=#ff0000`
    /// would turn a field filter into a phrase search, so the value
    /// position quotes the value alone.
    #[test]
    fn a_value_hint_quotes_only_the_value() {
        for (input, offset, want) in [
            ("color=#ff0000", 6, "\"#ff0000\""),
            ("a=1# note", 3, "\"1#\""),
            ("1=/foo#bar/", 6, "\"/foo#bar/\""),
            ("status=200,#x", 11, "\"#x\""),
            // an IN list is ONE token: the hint is the element, not the
            // remaining suffix — quoting the rest would be a different
            // query, and copying it made the hints quadratic
            ("f=#a,#b,#c", 2, "\"#a\""),
            ("f=#a,#b,#c", 5, "\"#b\""),
            ("f=#a,#b,#c", 8, "\"#c\""),
        ] {
            let hint = hint_for(MSG_OPENER_IN_VALUE, input, offset).expect("value hint");
            assert!(
                hint.starts_with(&format!("quote the value ({want})")),
                "{input:?}: {hint:?}"
            );
        }
    }

    /// Quoting is no escape under `-`: `-"a#b"` spells literal quotes.
    #[test]
    fn a_negated_term_hint_offers_the_not_spelling() {
        for (input, offset, want) in [
            ("-\"a#b\"", 3, "NOT \"a#b\""),
            ("-foo#bar", 4, "NOT \"foo#bar\""),
        ] {
            let hint = hint_for(MSG_OPENER_IN_NEGATED_TERM, input, offset).expect("negated hint");
            assert!(hint.starts_with(&format!("write it as {want}")), "{hint:?}");
        }
    }

    /// A stage name is grammar, not data — so the only advice is the
    /// whitespace one.
    #[test]
    fn a_command_hint_offers_only_the_comment_spelling() {
        let hint = hint_for(MSG_OPENER_IN_COMMAND, "* | co#unt()", 6).expect("command hint");
        assert_eq!(hint, "put whitespace before the '#' to start a comment");
    }

    #[test]
    fn opener_in_command_word_is_narrow() {
        assert_eq!(opener_in_command_word("* | co#unt()", 4), Some(6));
        // the token holding the offset is `count(),#`, not the stage word
        assert_eq!(opener_in_command_word("* | stats count(),# x", 18), None);
        // …and a stage word with no opener is not this diagnosis
        assert_eq!(opener_in_command_word("* | staats", 4), None);
    }
}
