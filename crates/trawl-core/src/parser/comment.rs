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

/// The byte that separates a position message from the EXACT token slice
/// the emitting production held, inside one `Rich::custom` payload.
///
/// `Rich::custom` transports a string and nothing else, so a production
/// that wants to hand the hint renderer more than its message has to
/// spell both into that one string. A unit separator is the divider
/// because the split takes the FIRST one: every message in this module is
/// a `const` that carries none, so the head is always exactly the
/// message, whatever control characters the user's own token carries
/// after it.
const PAYLOAD_SEP: char = '\u{1f}';

/// The messages a [`payload`] may be minted with — the ONE list
/// [`split_payload`] recognises.
///
/// The separator is not enough on its own: other productions build their
/// custom message by INTERPOLATING user-derived text (an invalid regex's
/// `Display`, a refused reserved name), so a query carrying a literal
/// U+001F could hand the renderer a string that splits into a head this
/// module never wrote. Splitting only on an exact head keeps a foreign
/// message whole, message and all.
const PAYLOAD_MESSAGES: [&str; 3] = [
    MSG_OPENER_IN_TOKEN,
    MSG_OPENER_IN_VALUE,
    MSG_OPENER_IN_NEGATED_TERM,
];

/// The payload an emitting production sends when it HOLDS the offending
/// token: the position message, plus the exact slice the grammar refused.
///
/// Passing the slice beats re-deriving it from the raw input. A left
/// boundary re-derived by scanning backwards for `=`/`<`/`>`/`!` cuts
/// `url=https://example.test/p?a=b#frag` at the query string's own `=`
/// and hints `"b#frag"` — advice that, followed, means something else
/// entirely. The production that emitted the error already knows where
/// the value starts and ends, and an IN list's element is its own
/// [`crate::parser::primitives::bare_value`], so the comma bounding the
/// hint needs comes free with the slice.
pub(crate) fn payload(msg: &str, exact: &str) -> String {
    // The call sites and [`PAYLOAD_MESSAGES`] must stay one set: a payload
    // minted with a message [`split_payload`] does not recognise comes back
    // WHOLE, separator and slice and all, and would print as the message.
    debug_assert!(
        PAYLOAD_MESSAGES.contains(&msg),
        "payload minted with an unlisted message: {msg:?}"
    );
    format!("{msg}{PAYLOAD_SEP}{exact}")
}

/// Split a [`payload`] back into its message and the exact token, if it
/// carries one. A custom error from anywhere else (an overflowing
/// literal, an invalid regex, a refused reserved name) comes back whole —
/// including one whose own interpolated text carries a [`PAYLOAD_SEP`],
/// because the head must be byte-equal to a message this module mints
/// ([`PAYLOAD_MESSAGES`]).
pub(crate) fn split_payload(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once(PAYLOAD_SEP) {
        Some((msg, exact)) if PAYLOAD_MESSAGES.contains(&msg) => (msg, Some(exact)),
        _ => (raw, None),
    }
}

/// The hint for one of this module's messages. ONE owner: both emitters
/// — the `validate` calls on the value and term productions, and the
/// generic net in [`crate::parser::rich_to_parse_error`] — come here, so
/// a position's working spelling is written down once.
///
/// Every hint offers something the user can FOLLOW, and says what
/// following it costs, which is why the position rides in the message:
/// quoting the whole of `color=#ff0000` is a phrase search, quoting a
/// `-`-negated term spells literal quote characters, and quoting a value
/// carrying `*`/`?` stops it being a pattern.
///
/// A CONCRETE rewrite is offered only where the emitting production held
/// the exact slice ([`payload`]) AND the rewrite means what the hint
/// claims. `exact` is `None` for the generic net in
/// [`crate::parser::rich_to_parse_error`], which knows only an offset:
/// the token run around it can span several grammar tokens, and the
/// POSITION is unknown — `| sort "a#b"` does not parse and
/// `| where a#b > 1` quietly becomes a string literal, whose escape is a
/// backticked name, not quotes. So the net names the escapes ABSTRACTLY
/// and mints no spelling.
pub(crate) fn hint_for(msg: &str, exact: Option<&str>) -> Option<String> {
    match msg {
        MSG_SLASHES_NOT_A_COMMENT => Some(HINT_SLASHES.to_string()),
        MSG_OPENER_IN_TOKEN => Some(match exact.filter(|t| quotable_as_term(t)) {
            Some(term) => format!(
                "quote it (\"{term}\") to search for it, or {}",
                comment_half()
            ),
            None => generic_advice(),
        }),
        MSG_OPENER_IN_VALUE => Some(match exact.filter(|t| quotable_verbatim(t)) {
            // Quoting a value is not operator-neutral: an unquoted `*`/`?`
            // is the glob PATTERN (`crate::parser::search::has_glob_chars`
            // decides it), and quoted wildcards are data. The advice is
            // still the right one — a value carrying a `#` cannot stay
            // unquoted — so it states what it costs instead of hiding it.
            //
            // What it costs is stated as the ONE thing true under every
            // operator and every pin: a quoted value is never a pattern.
            // The glob auto-detect overrides the operator the user typed
            // (`f>a*` is a Glob, not a Gt), so quoting `f>#a*` yields a
            // Gt comparison — "match it exactly" was true for `=` and
            // false for every ordered operator. And the hint promises the
            // `#` reaches the value, never that the value is one the
            // field's pin admits: `_severity="#warn*"` carries the `#` and
            // is then refused, loudly, by the severity vocabulary.
            Some(value) if crate::parser::search::has_glob_chars(value) => format!(
                "quote the value (\"{value}\") to carry the '{OPENER}' \
                 (a quoted value is never a pattern), or {}",
                comment_half()
            ),
            Some(value) => format!("quote the value (\"{value}\"), or {}", comment_half()),
            None => comment_half(),
        }),
        MSG_OPENER_IN_NEGATED_TERM => Some(match exact.and_then(quoted_term) {
            Some(spelling) => format!("write it as NOT {spelling}, or {}", comment_half()),
            None => comment_half(),
        }),
        MSG_OPENER_IN_COMMAND => Some(comment_half()),
        _ => None,
    }
}

/// The one advice every hint here ends with, and the only one that is
/// true in every position.
fn comment_half() -> String {
    format!("put whitespace before the '{OPENER}' to start a comment")
}

/// The position-BLIND advice: both escapes named, no spelling minted.
///
/// The generic net fires wherever the two validators do not — a sort key,
/// an expression, a stage argument — and each of those positions escapes
/// a `#` differently. Naming the two quoted contexts lets the user pick
/// the one their position takes; printing `"a#b"` would pick for them,
/// and pick wrong half the time.
fn generic_advice() -> String {
    format!(
        "{}, or carry the '{OPENER}' inside a quoted value (\"…\") \
         or a backticked field name (`…`)",
        comment_half()
    )
}

/// Whether `"{text}"` is a rewrite the grammar reads back as exactly
/// `text` — the ONE gate on the quoting half of every hint here.
///
/// Two ways it is not, and both are reachable. A `"` or a `\` inside the
/// text needs an escape (`quoted_string` reads `\"` and `\\`) that the
/// hint does not spell, so `message="x"\u{a0}# note` would advise pasting
/// a string that ends at its own second quote. And a run carrying a
/// character no unquoted token may contain — `( ) , |`, a backtick,
/// whitespace — is not one token in the position that emitted it: a value
/// stops at a `,` (it is the IN-list separator), so advising `"a,b"` there
/// would name a value the unquoted spelling never held.
///
/// This is the double-quote counterpart of
/// [`crate::parser::suggest::quote_dsl_field`], not a caller of it: that
/// one answers how to spell a FIELD NAME, whose escape is backticks with
/// doubling, and a backticked rendering here would name a field where the
/// user meant to search for text. The invisible-character half IS shared
/// — both go through [`crate::sanitize::is_unsafe_display_char`].
fn quotable_verbatim(text: &str) -> bool {
    quotable(text, false)
}

/// [`quotable_verbatim`] for a SEARCH TERM, where a `,` is an ordinary
/// word byte rather than a separator.
///
/// `foo,#bar` is ONE positive bare term — the term charset ends at
/// whitespace, `|`, parens and a backtick, and nothing else — and
/// `"foo,#bar"` is a `QuotedSearch` whose emitted SQL is the same
/// substring match the bare term compiles to (`emitter/search.rs`). So
/// the comma exclusion, which the value position needs, over-rejects
/// here.
fn quotable_as_term(text: &str) -> bool {
    quotable(text, true)
}

fn quotable(text: &str, comma_is_data: bool) -> bool {
    !text.is_empty()
        && !text.chars().any(|c| {
            matches!(c, '"' | '\\' | '(' | ')' | '|' | '`')
                || (c == ',' && !comma_is_data)
                || c.is_whitespace()
                || crate::sanitize::is_unsafe_display_char(c)
        })
}

/// The unquoted token surrounding `offset` and the byte it starts at: the
/// run of non-ASCII-whitespace, non-`|` bytes around it. Used to decide
/// whether that token is a pipe stage name.
///
/// It is deliberately NOT a hint source: a run recovered from an offset is
/// a guess at both the token's bounds and its grammatical position, and a
/// hint that quotes a guess is how `url=…?a=b#frag` came to be advised
/// `"b#frag"`. Every concrete rewrite comes from the production that HELD
/// the slice ([`payload`]).
///
/// ASCII whitespace because that is where the unquoted token charsets end
/// — a no-break space sits INSIDE a bare word.
pub(crate) fn token_span(input: &str, offset: usize) -> (usize, &str) {
    let ends = |c: char| c.is_ascii_whitespace() || c == '|';
    let start = input[..offset.min(input.len())].rfind(ends).map_or(0, |i| {
        i + input[i..].chars().next().map_or(1, char::len_utf8)
    });
    let rest = &input[start..];
    let end = rest.find(ends).unwrap_or(rest.len());
    (start, &rest[..end])
}

/// A negated term's working spelling under `NOT`: the term without its
/// leading `-`, double-quoted — or `None` when quoting it is not a
/// rewrite the user can paste ([`quotable_verbatim`]).
///
/// A term that is ALREADY double-quoted is unwrapped first, so
/// `-"a#b"` is answered `NOT "a#b"` and not `NOT "\"a#b\""`.
fn quoted_term(token: &str) -> Option<String> {
    let inner = token.strip_prefix('-').unwrap_or(token);
    let body = inner
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(inner);
    quotable_as_term(body).then(|| format!("\"{body}\""))
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
    fn token_span_recovers_the_offending_token() {
        assert_eq!(token_span("foo#bar", 3).1, "foo#bar");
        assert_eq!(token_span("a=1 color=#ff0000", 10).1, "color=#ff0000");
        assert_eq!(token_span("x|color=#f", 8).1, "color=#f");
    }

    /// A hint the user can FOLLOW: the value position quotes the EXACT
    /// slice its production held, whatever the surrounding token looks
    /// like. Re-deriving that slice from the raw text is what put a URL's
    /// own query string inside the advice.
    #[test]
    fn a_value_hint_quotes_the_slice_the_production_held() {
        for exact in [
            "#ff0000",
            "1#",
            "/foo#bar/",
            "#x",
            // an IN list's element IS its own value production, so the
            // comma bounding comes free
            "#a",
            "https://example.test/p?a=b#frag",
        ] {
            let hint = hint_for(MSG_OPENER_IN_VALUE, Some(exact)).expect("value hint");
            assert!(
                hint.starts_with(&format!("quote the value (\"{exact}\")")),
                "{exact:?}: {hint:?}"
            );
        }
    }

    /// The payload round-trips: the head is always the message, whatever
    /// control characters the user's own token carries.
    #[test]
    fn a_payload_splits_back_into_its_halves() {
        let raw = payload(MSG_OPENER_IN_VALUE, "a\u{1f}b");
        assert_eq!(split_payload(&raw), (MSG_OPENER_IN_VALUE, Some("a\u{1f}b")));
        // …and a custom error from anywhere else comes back whole
        assert_eq!(split_payload("regex too long"), ("regex too long", None));
    }

    /// A FOREIGN custom message carrying a separator round-trips whole:
    /// other productions interpolate user-derived text into theirs (an
    /// invalid regex's `Display`, a refused reserved name), so the
    /// separator alone cannot decide.
    #[test]
    fn a_foreign_message_with_a_separator_is_not_split() {
        for raw in [
            "invalid regex: a\u{1f}b",
            "\u{1f}",
            "'#' inside an unquoted toke\u{1f}n",
            "prefix '#' inside an unquoted token\u{1f}x",
        ] {
            assert_eq!(split_payload(raw), (raw, None), "{raw:?}");
        }
    }

    /// The quote half is offered only where `"…"` reads back unchanged.
    #[test]
    fn only_a_quotable_token_earns_the_quoting_half() {
        for text in ["#ff0000", "foo#bar", "https://a.b/c?d=e#f", "-x#y"] {
            assert!(quotable_verbatim(text), "{text:?}");
        }
        for text in [
            "",
            "a\"b#c",
            "a\\b#c",
            "count(),#",
            "a b#c",
            "a`b#c",
            "a|b#c",
        ] {
            assert!(!quotable_verbatim(text), "{text:?}");
        }

        // …and a `,` is a word byte in a TERM, where nothing splits on it
        for text in ["foo,#bar", "#a,#b"] {
            assert!(quotable_as_term(text), "{text:?}");
            assert!(!quotable_verbatim(text), "{text:?}");
        }

        // …and the hint drops that half rather than print it
        let hint = hint_for(MSG_OPENER_IN_VALUE, Some("a\"b#c")).expect("hint");
        assert_eq!(hint, "put whitespace before the '#' to start a comment");
    }

    /// The position-BLIND net mints no spelling: it does not know the
    /// token's bounds, and it does not know whether the position takes a
    /// quoted value or a backticked name.
    #[test]
    fn the_generic_net_names_the_escapes_without_spelling_one() {
        let hint = hint_for(MSG_OPENER_IN_TOKEN, None).expect("hint");
        assert_eq!(
            hint,
            "put whitespace before the '#' to start a comment, or carry the '#' \
             inside a quoted value (\"…\") or a backticked field name (`…`)"
        );
        assert!(!hint.contains("quote it"), "{hint:?}");
    }

    /// Quoting is no escape under `-`: `-"a#b"` spells literal quotes.
    #[test]
    fn a_negated_term_hint_offers_the_not_spelling() {
        for (exact, want) in [("-\"a#b\"", "NOT \"a#b\""), ("-foo#bar", "NOT \"foo#bar\"")] {
            let hint = hint_for(MSG_OPENER_IN_NEGATED_TERM, Some(exact)).expect("negated hint");
            assert!(hint.starts_with(&format!("write it as {want}")), "{hint:?}");
        }

        // …and a term no quoting can spell keeps only the comment half
        let hint = hint_for(MSG_OPENER_IN_NEGATED_TERM, Some("-a\"b#c")).expect("hint");
        assert_eq!(hint, "put whitespace before the '#' to start a comment");
    }

    /// A stage name is grammar, not data — so the only advice is the
    /// whitespace one.
    #[test]
    fn a_command_hint_offers_only_the_comment_spelling() {
        let hint = hint_for(MSG_OPENER_IN_COMMAND, None).expect("command hint");
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
