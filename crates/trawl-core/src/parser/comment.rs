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
//! comment site — the start of the input. Consequence, deliberate:
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
pub(crate) const MSG_OPENER_IN_TOKEN: &str = "'#' inside an unquoted token";

/// The message a search-stage bare term STARTING with `//` carries — the
/// loud half of dropping the second opener (ADR-0014 ruling 3).
pub(crate) const MSG_SLASHES_NOT_A_COMMENT: &str =
    "'//' does not start a comment — '#' is the comment character";

/// The hint for [`MSG_SLASHES_NOT_A_COMMENT`].
const HINT_SLASHES: &str =
    "write '#' to start a comment, or quote the term to search for the slashes";

/// The `//` opener a search-stage bare term may not START with.
pub(crate) const SLASHES: &str = "//";

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
/// rather than tested by a lookback.
fn skip_layout<'src>(inp: &mut InputRef<'src, '_, ParserInput<'src>, ParserExtra<'src>>) {
    loop {
        let mut saw_whitespace = false;
        while inp.peek().is_some_and(char::is_whitespace) {
            inp.skip();
            saw_whitespace = true;
        }
        if !saw_whitespace {
            return;
        }
        skip_comment(inp);
    }
}

/// The layout run, as a parser.
///
/// Written as a `custom` walk rather than a combinator tower on purpose:
/// this parser is embedded at all 84 padding sites, several of them
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

/// The hint for one of this module's two messages, derived from the real
/// input and the error's own span.
///
/// The token is recovered from the input rather than carried in the
/// message: `Rich::custom` transports a string and nothing else, and the
/// error conversion in [`crate::parser::rich_to_parse_error`] already
/// holds both the source text and the offset.
pub(crate) fn hint_for(msg: &str, input: &str, offset: usize) -> Option<String> {
    if msg == MSG_SLASHES_NOT_A_COMMENT {
        return Some(HINT_SLASHES.to_string());
    }
    if msg != MSG_OPENER_IN_TOKEN {
        return None;
    }
    let token = token_around(input, offset);
    Some(format!(
        "quote it (\"{token}\") to search for it, or put whitespace before the '{OPENER}' to start a comment"
    ))
}

/// The unquoted token surrounding `offset`: the run of non-whitespace,
/// non-`|` bytes around it. Used only to render a hint.
fn token_around(input: &str, offset: usize) -> &str {
    let ends = |c: char| c.is_whitespace() || c == '|';
    let start = input[..offset.min(input.len())].rfind(ends).map_or(0, |i| {
        i + input[i..].chars().next().map_or(1, char::len_utf8)
    });
    let rest = &input[start..];
    let end = rest.find(ends).unwrap_or(rest.len());
    &rest[..end]
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
}
