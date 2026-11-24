// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Comments, and the one padding owner every whitespace-skipping site in
//! the grammar goes through (ADR-0014).

use chumsky::input::InputRef;
use chumsky::prelude::*;

use crate::parser::primitives::{ParserExtra, ParserInput};

pub(crate) const OPENER: char = '#';

/// The rule is one rule, but the working spelling differs by position,
/// and a hint whose advice changes what the query means is worse than no
/// hint at all — quoting a whole `color=#ff0000` turns a field filter
/// into a phrase search. So the position is carried in the message, and
/// [`hint_for`] is the one place each position's escape is written down.
pub(crate) const MSG_OPENER_IN_TOKEN: &str = "'#' inside an unquoted token";

/// [`MSG_OPENER_IN_TOKEN`] for the value half of a field filter, whose
/// escape quotes the value alone (`color="#ff0000"`).
pub(crate) const MSG_OPENER_IN_VALUE: &str = "'#' inside an unquoted value";

/// [`MSG_OPENER_IN_TOKEN`] for a negated search term, where quoting is no
/// escape at all — `-"a#b"` is not a negated phrase, it is a bare term
/// spelling literal quote characters. `NOT "a#b"` is the working form.
pub(crate) const MSG_OPENER_IN_NEGATED_TERM: &str = "'#' inside a negated search term";

/// [`MSG_OPENER_IN_TOKEN`] for a pipe stage name (`| co#unt()`), where
/// quoting is no escape either: a stage name is grammar, not data.
pub(crate) const MSG_OPENER_IN_COMMAND: &str = "'#' inside a pipe stage name";

pub(crate) const MSG_SLASHES_NOT_A_COMMENT: &str =
    "'//' does not start a comment — '#' is the comment character";

const HINT_SLASHES: &str =
    "write '#' to start a comment, or quote the term to search for the slashes";

pub(crate) const SLASHES: &str = "//";

/// What counts as layout between tokens: `char::is_whitespace`, Unicode
/// and all, the same set chumsky's `.padded()` consumes. This is
/// consumption only — it says what separates two tokens, never where a
/// comment may open.
pub(crate) const fn is_layout(c: char) -> bool {
    c.is_whitespace()
}

/// Whether a comment may open directly after `prev` — the character
/// immediately before the `#`, or `None` at the start of the input.
pub(crate) fn opens_comment_after(prev: Option<char>) -> bool {
    prev.is_none_or(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

/// The line boundary is `\n` alone — a `\r` is an ordinary character
/// that may sit inside a comment.
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

/// Written as a `custom` walk rather than a combinator tower on purpose:
/// a combinator form inflates the nested parser type enough to overflow
/// the stack on a deep pipeline.
pub(crate) fn ws<'src>() -> impl Parser<'src, ParserInput<'src>, (), ParserExtra<'src>> + Clone {
    custom(|inp| {
        skip_layout(inp);
        Ok(())
    })
}

pub(crate) fn leading_ws<'src>()
-> impl Parser<'src, ParserInput<'src>, (), ParserExtra<'src>> + Clone {
    custom(|inp| {
        skip_comment(inp);
        skip_layout(inp);
        Ok(())
    })
}

pub(crate) fn first_opener(text: &str) -> Option<usize> {
    text.find(OPENER)
}

pub(crate) fn starts_with_slashes(text: &str) -> bool {
    text.starts_with(SLASHES)
}

/// The split takes the first separator: every message in this module is a
/// `const` that carries none, so the head is always exactly the message.
const PAYLOAD_SEP: char = '\u{1f}';

/// The messages a [`payload`] may be minted with — the one list
/// [`split_payload`] recognises.
///
/// The separator is not enough on its own: other productions build their
/// custom message by interpolating user-derived text (an invalid regex's
/// `Display`, a refused reserved name), so a query carrying a literal
/// U+001F could hand the renderer a string that splits into a head this
/// module never wrote. Splitting only on an exact head keeps a foreign
/// message whole, message and all.
const PAYLOAD_MESSAGES: [&str; 5] = [
    MSG_OPENER_IN_TOKEN,
    MSG_OPENER_IN_VALUE,
    MSG_OPENER_IN_NEGATED_TERM,
    crate::parser::search::MSG_ORDERED_GLOB,
    crate::parser::primitives::MSG_UNKNOWN_ESCAPE,
];

/// Passing the slice beats re-deriving it from the raw input. A left
/// boundary re-derived by scanning backwards for `=`/`<`/`>`/`!` cuts
/// `url=https://example.test/p?a=b#frag` at the query string's own `=`
/// and hints `"b#frag"`.
pub(crate) fn payload(msg: &str, exact: &str) -> String {
    // The call sites and [`PAYLOAD_MESSAGES`] must stay one set: a payload
    // minted with a message [`split_payload`] does not recognise comes back
    // whole, separator and slice and all, and would print as the message.
    debug_assert!(
        PAYLOAD_MESSAGES.contains(&msg),
        "payload minted with an unlisted message: {msg:?}"
    );
    format!("{msg}{PAYLOAD_SEP}{exact}")
}

pub(crate) fn split_payload(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once(PAYLOAD_SEP) {
        Some((msg, exact)) if PAYLOAD_MESSAGES.contains(&msg) => (msg, Some(exact)),
        _ => (raw, None),
    }
}

pub(crate) fn message_for(msg: &str, exact: Option<&str>) -> String {
    if msg == crate::parser::primitives::MSG_UNKNOWN_ESCAPE
        && let Some(after) = exact.and_then(escaped_char)
    {
        let shown = if showable_escape(after) {
            format!("\\{after}")
        } else {
            format!("(a backslash before U+{:04X})", u32::from(after))
        };
        let rest = msg
            .strip_prefix("unknown escape")
            .expect("the unknown-escape message opens with its name");
        return format!("unknown escape {shown}{rest}");
    }
    msg.to_string()
}

fn escaped_char(pair: &str) -> Option<char> {
    let mut chars = pair.strip_prefix('\\')?.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

fn showable_escape(c: char) -> bool {
    !c.is_control() && !c.is_whitespace() && !crate::sanitize::is_unsafe_display_char(c)
}

fn unknown_escape_hint(exact: Option<&str>) -> String {
    let keep = match exact.and_then(escaped_char).filter(|c| showable_escape(*c)) {
        Some(c) => format!("write \\\\{c} to keep the backslash"),
        None => "double the backslash to keep it".to_string(),
    };
    format!(
        "{keep}; in a regex position, a /…/ regex takes backslashes as written \
         (matches /^\\s+at /); an extract or rex body decodes nothing, so its \
         backslashes need no doubling"
    )
}

/// A concrete rewrite is offered only where the emitting production held
/// the exact slice ([`payload`]) and the rewrite means what the hint
/// claims. `exact` is `None` for the generic net in
/// [`crate::parser::rich_to_parse_error`], which knows only an offset,
/// so the net names the escapes abstractly and mints no spelling.
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
            // is a glob pattern and quoted wildcards are data. The advice
            // names the one cost true under every operator and every pin: a
            // quoted value is never a pattern.
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
        crate::parser::primitives::MSG_UNKNOWN_ESCAPE => Some(unknown_escape_hint(exact)),
        // A pattern takes `=`/`!=` only (ADR-0060). Quoting is offered for
        // a glob alone, and says what it means: `host<"a*"` compares the
        // text `a*`. The quoted text of a regex would carry its slashes.
        crate::parser::search::MSG_ORDERED_GLOB => {
            Some(match exact.filter(|t| quotable_verbatim(t)) {
                Some(glob) => format!(
                    "{}, or quote the value (\"{glob}\") to compare it as text",
                    pattern_operators()
                ),
                None => pattern_operators(),
            })
        }
        crate::parser::search::MSG_ORDERED_REGEX => Some(pattern_operators()),
        crate::parser::search::MSG_QUOTE_AFTER_REGEX => Some(
            "put whitespace after the regex, or quote the whole value to compare text".to_string(),
        ),
        crate::pattern::MSG_PATTERN_NOT_LITERAL => Some(HINT_PATTERN_NOT_LITERAL.to_string()),
        crate::pattern::MSG_CLASS_SET_SYNTAX => Some(HINT_CLASS_SET_SYNTAX.to_string()),
        crate::pattern::MSG_BATCH_TOO_LARGE => Some(HINT_BATCH_TOO_LARGE.to_string()),
        crate::pattern::MSG_REGEX_TOO_LARGE => Some(
            "a counted repeat compiles its class once per count, and `\\w`, `\\d` and `\\pL` \
             are large Unicode classes: write `\\w+`, or an ASCII class such as \
             `[A-Za-z0-9_]{3,30}`"
                .to_string(),
        ),
        crate::parser::primitives::MSG_REGEX_LINE_BREAK => Some(HINT_REGEX_LINE_BREAK.to_string()),
        crate::parser::primitives::MSG_REGEX_PIPE_STAGE => Some(HINT_REGEX_PIPE_STAGE.to_string()),
        crate::parser::search::MSG_NESTED_TIME_CLAUSE => Some(HINT_NESTED_TIME_CLAUSE.to_string()),
        crate::parser::search::MSG_UNREADABLE_BOUND => Some(HINT_UNREADABLE_BOUND.to_string()),
        crate::parser::search::MSG_LAST_WITH_BOUND => Some(HINT_LAST_WITH_BOUND.to_string()),
        crate::parser::primitives::MSG_QUOTE_OPENS_BARE_VALUE => {
            Some(HINT_QUOTE_OPENS_BARE_VALUE.to_string())
        }
        crate::parser::search::MSG_OR_ONLY_GROUP => Some(HINT_OR_ONLY_GROUP.to_string()),
        crate::parser::search::MSG_NOT_DASH_BEFORE_BACKTICK => {
            Some(HINT_NOT_DASH_BEFORE_BACKTICK.to_string())
        }
        // The budget messages carry their counts after a fixed head.
        m if m.starts_with(crate::pattern::MSG_REPEAT_BUDGET) => {
            Some(HINT_REPEAT_BUDGET.to_string())
        }
        m if m.starts_with(crate::pattern::MSG_TRANSLATION_BUDGET) => {
            Some(HINT_TRANSLATION_BUDGET.to_string())
        }
        _ => None,
    }
}

pub(crate) const HINT_PATTERN_NOT_LITERAL: &str =
    "write the pattern itself as quoted text, such as \"err%\", or as /err.*/ after matches";

pub(crate) const HINT_CLASS_SET_SYNTAX: &str = "the batch engine reads them as plain \
     characters, so write the class flat, as one list: `[a-c]` instead of `[a-z&&[a-c]]`, \
     `[abc]` instead of `[a[bc]]`, `[a-df-z]` instead of `[a-z--e]`. Escape a `[`, `]`, `&`, \
     `-` or `~` meant as itself, as in `[a\\&b]`; a POSIX class such as `[[:alpha:]]` is fine";

pub(crate) const HINT_BATCH_TOO_LARGE: &str = "the batch engine compiles a class once per \
     count and per branch, and `\\pL`, `\\p{…}` and wide non-ASCII ranges are large there: \
     repeat them fewer times, write `+` or `*` instead of a count, or use an ASCII class such as \
     `[A-Za-z]`. `\\w`, `\\d` and `\\s` are ASCII in the batch engine and cost little";

pub(crate) const HINT_REPEAT_BUDGET: &str = "a pattern may hold 64 positions: each character, \
     class, anchor, word boundary, capture group and `|` is one, a counted repeat multiplies its \
     positions by the count, and every repeat counts at least one more than it holds. Write `+` \
     or `*` for a long run instead of a count such as \
     `{1000}`, `(?:…)` for a group you do not need to capture, or match a shorter run. For long \
     plain text, `contains(field, \"text\")` has no position limit";

pub(crate) const HINT_TRANSLATION_BUDGET: &str = "large Unicode classes such as `\\p{…}`, `\\w` \
     and wide ranges cost the most to translate, and `(?i)` multiplies them: use fewer of \
     them, or ASCII classes such as `[A-Za-z]`";

pub(crate) const HINT_REGEX_LINE_BREAK: &str = "write \\n";

pub(crate) const HINT_REGEX_PIPE_STAGE: &str = "drop the spaces or write \\s";

pub(crate) const HINT_NESTED_TIME_CLAUSE: &str =
    "time clauses apply to the whole search; write it outside the group";

pub(crate) const HINT_UNREADABLE_BOUND: &str = "write a date such as \"2026-03-14\", a time such \
     as \"2026-03-14T09:00:00\", or epoch, infinity or -infinity; years run up to 9999";

pub(crate) const HINT_LAST_WITH_BOUND: &str =
    "use last= alone, or earliest= and latest= without it";

pub(crate) const HINT_QUOTE_OPENS_BARE_VALUE: &str = "close the quote";

pub(crate) const HINT_OR_ONLY_GROUP: &str =
    "put a search term or filter inside the group, or remove it";

pub(crate) const HINT_NOT_DASH_BEFORE_BACKTICK: &str =
    "put a space after NOT to negate the field, or quote \"NOT-\" to search for the text";

fn pattern_operators() -> String {
    "use `=` or `!=` to match the pattern".to_string()
}

/// The one advice every hint here ends with, and the only one that is
/// true in every position.
fn comment_half() -> String {
    format!("put whitespace before the '{OPENER}' to start a comment")
}

fn generic_advice() -> String {
    format!(
        "{}, or carry the '{OPENER}' inside a quoted value (\"…\") \
         or a backticked field name (`…`)",
        comment_half()
    )
}

/// Whether `"{text}"` is a rewrite the grammar reads back as exactly
/// `text` — the one gate on the quoting half of every hint here.
///
/// This is the double-quote counterpart of
/// [`crate::parser::suggest::quote_dsl_field`], not a caller of it: that
/// one answers how to spell a field name, whose escape is backticks with
/// doubling.
fn quotable_verbatim(text: &str) -> bool {
    quotable(text, false)
}

/// [`quotable_verbatim`] for a search term, where a `,` is an ordinary
/// word byte rather than a separator.
///
/// `foo,#bar` is one positive bare term — the term charset ends at
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
/// ASCII whitespace because that is where the unquoted token charsets end
/// — a no-break space sits inside a bare word.
pub(crate) fn token_span(input: &str, offset: usize) -> (usize, &str) {
    let ends = |c: char| c.is_ascii_whitespace() || c == '|';
    let start = input[..offset.min(input.len())].rfind(ends).map_or(0, |i| {
        i + input[i..].chars().next().map_or(1, char::len_utf8)
    });
    let rest = &input[start..];
    let end = rest.find(ends).unwrap_or(rest.len());
    (start, &rest[..end])
}

fn quoted_term(token: &str) -> Option<String> {
    let inner = token.strip_prefix('-').unwrap_or(token);
    let body = inner
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(inner);
    quotable_as_term(body).then(|| format!("\"{body}\""))
}

/// The comment opener inside a pipe stage name containing `offset`, if
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

pub(crate) trait Spaced<'src, O>:
    Parser<'src, ParserInput<'src>, O, ParserExtra<'src>> + Clone + Sized
{
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

    #[test]
    fn ws_does_not_open_a_comment_without_whitespace() {
        assert!(
            ws().then_ignore(end())
                .parse("# note")
                .into_result()
                .is_err()
        );
        assert!(
            leading_ws()
                .then_ignore(end())
                .parse("# note")
                .into_result()
                .is_ok()
        );
    }

    #[test]
    fn only_ascii_whitespace_opens_a_comment() {
        for c in [' ', '\t', '\n', '\r'] {
            assert!(opens_comment_after(Some(c)), "{c:?}");
        }
        for c in ['\u{a0}', '\u{2003}', '\u{feff}', 'x'] {
            assert!(!opens_comment_after(Some(c)), "{c:?}");
        }
        assert!(opens_comment_after(None), "start of input");

        assert!(
            ws().then_ignore(end())
                .parse("\u{a0}# note")
                .into_result()
                .is_err()
        );
        assert!(
            ws().then_ignore(end())
                .parse("\u{a0} # note")
                .into_result()
                .is_ok()
        );
        assert!(
            ws().then_ignore(end())
                .parse("\u{a0}\u{2003}")
                .into_result()
                .is_ok()
        );
    }

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

    #[test]
    fn a_value_hint_quotes_the_slice_the_production_held() {
        for exact in [
            "#ff0000",
            "1#",
            "/foo#bar/",
            "#x",
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

    #[test]
    fn a_payload_splits_back_into_its_halves() {
        let raw = payload(MSG_OPENER_IN_VALUE, "a\u{1f}b");
        assert_eq!(split_payload(&raw), (MSG_OPENER_IN_VALUE, Some("a\u{1f}b")));
        assert_eq!(split_payload("regex too long"), ("regex too long", None));
    }

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

        for text in ["foo,#bar", "#a,#b"] {
            assert!(quotable_as_term(text), "{text:?}");
            assert!(!quotable_verbatim(text), "{text:?}");
        }

        let hint = hint_for(MSG_OPENER_IN_VALUE, Some("a\"b#c")).expect("hint");
        assert_eq!(hint, "put whitespace before the '#' to start a comment");
    }

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

    #[test]
    fn a_negated_term_hint_offers_the_not_spelling() {
        for (exact, want) in [("-\"a#b\"", "NOT \"a#b\""), ("-foo#bar", "NOT \"foo#bar\"")] {
            let hint = hint_for(MSG_OPENER_IN_NEGATED_TERM, Some(exact)).expect("negated hint");
            assert!(hint.starts_with(&format!("write it as {want}")), "{hint:?}");
        }

        let hint = hint_for(MSG_OPENER_IN_NEGATED_TERM, Some("-a\"b#c")).expect("hint");
        assert_eq!(hint, "put whitespace before the '#' to start a comment");
    }

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
        assert_eq!(opener_in_command_word("* | staats", 4), None);
    }

    #[test]
    fn unknown_escape_hint_never_prints_unsafe_chars() {
        for c in [
            '\u{7}',    // a control
            '\u{1b}',   // escape, which starts a terminal sequence
            '\u{7f}',   // delete
            '\u{85}',   // a C1 control, also a line break
            '\u{1f}',   // the payload separator
            ' ',        // whitespace
            '\t',       // a raw tab, not the `\t` escape
            '\u{a0}',   // a no-break space
            '\u{3000}', // an ideographic space
            '\u{2028}', // the line separator
            '\u{202e}', // a bidi override
            '\u{2066}', // a bidi isolate
            '\u{200b}', // a zero-width space
            '\u{ad}',   // a soft hyphen
        ] {
            let dsl = format!("\"a\\{c}b\"");
            let errors = crate::parser::parse(&dsl).expect_err("an unknown escape is an error");
            let [one] = &errors[..] else {
                panic!("{c:?}: {errors:?}")
            };
            assert_eq!(one.span, 2..2 + 1 + c.len_utf8(), "{c:?}");
            let hint = one.hint.as_deref().expect("an escape hint");
            assert!(
                hint.starts_with("double the backslash to keep it;"),
                "{c:?}: {hint:?}"
            );
            // the copy holds ordinary spaces of its own, so a space is
            // checked as the pair it would be printed in
            for text in [hint, one.message.as_str()] {
                assert!(!text.contains(&format!("\\{c}")), "{c:?}: {text:?}");
                assert!(c == ' ' || !text.contains(c), "{c:?}: {text:?}");
            }
            assert!(
                one.message.starts_with(&format!(
                    "unknown escape (a backslash before U+{:04X}):",
                    u32::from(c)
                )),
                "{c:?}: {:?}",
                one.message
            );
        }

        let errors = crate::parser::parse("\"a\\éb\"").expect_err("an unknown escape");
        let [one] = &errors[..] else {
            panic!("{errors:?}")
        };
        assert!(one.message.starts_with("unknown escape \\é:"), "{one:?}");
        let hint = one.hint.as_deref().expect("an escape hint");
        assert!(
            hint.starts_with("write \\\\é to keep the backslash;"),
            "{hint:?}"
        );

        let raw = payload(crate::parser::primitives::MSG_UNKNOWN_ESCAPE, "\\\u{1f}");
        let (msg, exact) = split_payload(&raw);
        assert_eq!(msg, crate::parser::primitives::MSG_UNKNOWN_ESCAPE);
        assert_eq!(exact, Some("\\\u{1f}"));
        assert!(!message_for(msg, exact).contains('\u{1f}'));
        assert!(!hint_for(msg, exact).expect("hint").contains('\u{1f}'));
    }
}
