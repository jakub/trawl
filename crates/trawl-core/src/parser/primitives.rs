// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layer 1: primitive parsers.
//!
//! Numbers, quoted strings, bare words, `@`-prefixed names, filter
//! operators, durations, and boolean/null keywords.

use chumsky::prelude::*;

use crate::ast::{FilterOp, FloatLiteral, LiteralValue, Spanned, TimeUnit, TrawlDuration};
use regex_syntax::ast::Ast;

#[cfg(test)]
use crate::pattern::Preparer;
use crate::pattern::{MAX_REGEX_LEN, PatternError, PatternKind, price, re2};
use crate::sanitize::is_unsafe_display_char;

pub(crate) type ParserInput<'src> = &'src str;
pub(crate) type ParserExtra<'src> = extra::Full<Rich<'src, char>, RegexSites, ()>;

pub(crate) fn spanned<'src, T: 'src>(
    p: impl Parser<'src, ParserInput<'src>, T, ParserExtra<'src>> + Clone,
) -> impl Parser<'src, ParserInput<'src>, Spanned<T>, ParserExtra<'src>> + Clone {
    p.map_with(|node, extra| {
        let span = extra.span();
        Spanned::new(node, span.start..span.end)
    })
}

pub(crate) fn uint<'src>() -> impl Parser<'src, ParserInput<'src>, u64, ParserExtra<'src>> + Clone {
    text::int(10)
        .to_slice()
        .try_map(|s: &str, span| {
            s.parse::<u64>()
                .map_err(|e| Rich::custom(span, format!("invalid integer: {e}")))
        })
        .labelled("integer")
}

pub(crate) fn int<'src>() -> impl Parser<'src, ParserInput<'src>, i64, ParserExtra<'src>> + Clone {
    just('-')
        .or_not()
        .then(text::int(10).to_slice())
        .to_slice()
        .try_map(|s: &str, span| {
            s.parse::<i64>()
                .map_err(|e| Rich::custom(span, format!("invalid integer: {e}")))
        })
        .labelled("integer")
}

pub(crate) fn float<'src>()
-> impl Parser<'src, ParserInput<'src>, FloatLiteral, ParserExtra<'src>> + Clone {
    just('-')
        .or_not()
        .then(text::int(10))
        .then(just('.').then(text::digits(10)))
        .to_slice()
        .try_map(|s: &str, span| {
            s.parse::<f64>()
                .map(|value| FloatLiteral::new(value, s))
                .map_err(|e| Rich::custom(span, format!("invalid float: {e}")))
        })
        .labelled("float")
}

pub(crate) fn number_literal<'src>()
-> impl Parser<'src, ParserInput<'src>, LiteralValue, ParserExtra<'src>> + Clone {
    float()
        .map(LiteralValue::Float)
        .or(int().map(LiteralValue::Int))
}

/// Boxed: the expression grammar holds this lexer inside every recursive
/// level, and chumsky inlines a combinator tower into the frame of each
/// level that holds it (the stack premise `tests/tree_depth_stack.rs`
/// checks).
pub(crate) fn quoted_string<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    quoted_string_reported().map(|(s, _)| s).boxed()
}

/// A payload: the backslash pair rides along, and
/// [`crate::parser::comment::message_for`] puts a display-safe rendering
/// of it into the message.
pub(crate) const MSG_UNKNOWN_ESCAPE: &str =
    "unknown escape: a quoted string decodes only \\\\, \\\", \\n, \\t and \\r";

#[derive(Clone)]
enum Chunk<'src> {
    Char(char),
    Unknown(&'src str),
}

/// The five escapes decode. A backslash before any other character is an
/// emitted error over the backslash and that whole character, and both
/// are kept in the value as written, so the string still closes and no
/// later arm reads the same text again: a returned failure would let a
/// filter value fall back to the bare scanner and report `close the
/// quote` for a string that is closed. A filter value asks for the flag
/// so it adds no operator error on top (`ParsedValue::reported`).
///
/// The unknown arm also expects a `"` after the backslash, which `any()`
/// already covers: the redundant alternative keeps `'"'` in the expected
/// set at the end of input, so `"a\` still reports an unterminated string
/// (`crate::parser::enrich_error`).
pub(crate) fn quoted_string_reported<'src>()
-> impl Parser<'src, ParserInput<'src>, (String, bool), ParserExtra<'src>> + Clone {
    let escape = just('\\').ignore_then(choice((
        just('\\'),
        just('"'),
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
    )));
    let unknown = just('\\').then(any().or(just('"'))).to_slice().validate(
        |pair: &'src str, extra, emitter| {
            emitter.emit(Rich::custom(
                extra.span(),
                crate::parser::comment::payload(MSG_UNKNOWN_ESCAPE, pair),
            ));
            Chunk::Unknown(pair)
        },
    );

    choice((none_of("\\\"").or(escape).map(Chunk::Char), unknown))
        .repeated()
        .collect::<Vec<_>>()
        .map(|chunks| {
            let mut reported = false;
            let mut s = String::new();
            for chunk in chunks {
                match chunk {
                    Chunk::Char(c) => s.push(c),
                    Chunk::Unknown(pair) => {
                        reported = true;
                        s.push_str(pair);
                    }
                }
            }
            (s, reported)
        })
        .delimited_by(just('"'), just('"'))
        .labelled("quoted string")
}

/// Parse a double-quoted string without escape processing: every byte
/// between the quotes is the string, a backslash included, so `\d` and
/// `\w` in an `extract` regex need no doubling. The first `"` closes it,
/// even after a backslash, so a raw string cannot hold a `"`.
pub(crate) fn raw_quoted_string<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    let escaped_quote = just('\\').then(just('"')).to('"');

    none_of("\"")
        .or(escaped_quote)
        .repeated()
        .collect::<String>()
        .delimited_by(just('"'), just('"'))
        .labelled("quoted string")
}

pub(crate) fn ident<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    let segment = any()
        .filter(|c: &char| c.is_ascii_alphabetic() || *c == '_')
        .then(
            any()
                .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_')
                .repeated(),
        )
        .to_slice();

    segment
        .then(just('.').then(segment).repeated().collect::<Vec<_>>())
        .to_slice()
        .map(String::from)
        .labelled("identifier")
}

pub(crate) fn system_field<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    just('@')
        .then(ident())
        .to_slice()
        .map(String::from)
        .labelled("system field")
}

pub(crate) fn plain_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    system_field().or(ident())
}

pub(crate) fn quoted_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    choice((just("``").to('`'), none_of('`')))
        .repeated()
        .collect::<String>()
        .delimited_by(just('`'), just('`'))
        .try_map(|name: String, span| {
            if name.is_empty() {
                return Err(Rich::custom(span, "empty field name: `` names no column"));
            }
            if let Some(bad) = name.chars().find(|c| is_unsafe_display_char(*c)) {
                return Err(Rich::custom(
                    span,
                    format!(
                        "field name contains a control, separator or invisible format character \
                         (U+{:04X}); such characters are not part of any column name",
                        bad as u32
                    ),
                ));
            }
            Ok(name)
        })
        .labelled("backtick-quoted field name")
}

pub(crate) fn field_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    choice((quoted_name(), system_field(), ident()))
}

/// Parse a search-stage filter operator, ordered longest-first to avoid
/// prefix ambiguity (`>=` before `>`, etc.).
pub(crate) fn filter_op<'src>()
-> impl Parser<'src, ParserInput<'src>, FilterOp, ParserExtra<'src>> + Clone {
    choice((
        just(">=").to(FilterOp::Gte),
        just("<=").to(FilterOp::Lte),
        just("!=").to(FilterOp::Ne),
        just(">").to(FilterOp::Gt),
        just("<").to(FilterOp::Lt),
        just("=").to(FilterOp::Eq),
    ))
    .labelled("filter operator")
}

pub(crate) fn time_unit<'src>()
-> impl Parser<'src, ParserInput<'src>, TimeUnit, ParserExtra<'src>> + Clone {
    choice((
        just('s').to(TimeUnit::Seconds),
        just('m').to(TimeUnit::Minutes),
        just('h').to(TimeUnit::Hours),
        just('d').to(TimeUnit::Days),
        just('w').to(TimeUnit::Weeks),
    ))
    .labelled("time unit")
}

pub(crate) fn duration<'src>()
-> impl Parser<'src, ParserInput<'src>, TrawlDuration, ParserExtra<'src>> + Clone {
    uint()
        .then(time_unit())
        .try_map(|(quantity, unit), span| {
            if quantity == 0 {
                return Err(Rich::custom(span, "duration must be greater than zero"));
            }
            let multiplier = match unit {
                TimeUnit::Seconds => 1,
                TimeUnit::Minutes => 60,
                TimeUnit::Hours => 3600,
                TimeUnit::Days => 86_400,
                TimeUnit::Weeks => 604_800,
            };
            if quantity.checked_mul(multiplier).is_none() {
                return Err(Rich::custom(span, "duration too large"));
            }
            Ok(TrawlDuration { quantity, unit })
        })
        .labelled("duration")
}

pub(crate) fn keyword<'src>(
    kw: &'static str,
) -> impl Parser<'src, ParserInput<'src>, &'src str, ParserExtra<'src>> + Clone {
    just(kw)
        .then_ignore(
            any()
                .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_')
                .not()
                .rewind(),
        )
        .labelled(kw)
}

pub(crate) fn bool_literal<'src>()
-> impl Parser<'src, ParserInput<'src>, LiteralValue, ParserExtra<'src>> + Clone {
    keyword("true")
        .to(LiteralValue::Bool(true))
        .or(keyword("false").to(LiteralValue::Bool(false)))
}

pub(crate) fn null_literal<'src>()
-> impl Parser<'src, ParserInput<'src>, LiteralValue, ParserExtra<'src>> + Clone {
    keyword("null").to(LiteralValue::Null)
}

pub(crate) fn literal<'src>()
-> impl Parser<'src, ParserInput<'src>, LiteralValue, ParserExtra<'src>> + Clone {
    choice((
        bool_literal(),
        null_literal(),
        number_literal(),
        quoted_string().map(LiteralValue::String),
    ))
    .labelled("literal")
}

pub(crate) fn ends_bare_value(c: char) -> bool {
    c.is_ascii_whitespace() || matches!(c, '|' | ',' | ')' | '(' | '`')
}

/// A comment opener (ADR-0014 ruling 2) or an opening `"` (ADR-0061
/// ruling 3) is emitted rather than returned: a structurally-successful
/// branch keeps chumsky's alternative selection from ranking a worse error
/// from a later arm ahead of this one, while `into_result()` is still
/// `Err`.
pub(crate) fn bare_value<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    any()
        .filter(|c: &char| !ends_bare_value(*c))
        .repeated()
        .at_least(1)
        .to_slice()
        .validate(|s: &str, extra, emitter| {
            if s.starts_with('"') {
                emitter.emit(Rich::custom(extra.span(), MSG_QUOTE_OPENS_BARE_VALUE));
            } else if let Some(at) = crate::parser::comment::first_opener(s) {
                let span: SimpleSpan = extra.span();
                let start = span.start + at;
                emitter.emit(Rich::custom(
                    (start..start + crate::parser::comment::OPENER.len_utf8()).into(),
                    // The exact value slice rides along: an IN list's element is its own
                    // `bare_value`, so the hint names the element the user can act on.
                    crate::parser::comment::payload(crate::parser::comment::MSG_OPENER_IN_VALUE, s),
                ));
            }
            s
        })
        .map(String::from)
        .labelled("value")
}

pub(crate) const MSG_QUOTE_OPENS_BARE_VALUE: &str =
    "a value that opens with '\"' must be a complete quoted string";

/// What cuts a `/…/` run so it cannot be a regex (ADR-0061 ruling 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexCut {
    LineBreak,
    PipeStage,
}

impl RegexCut {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::LineBreak => MSG_REGEX_LINE_BREAK,
            Self::PipeStage => MSG_REGEX_PIPE_STAGE,
        }
    }
}

pub(crate) const MSG_REGEX_LINE_BREAK: &str = "a regex cannot hold a raw line break";

pub(crate) const MSG_REGEX_PIPE_STAGE: &str =
    "a regex cannot hold '|' after whitespace, which starts a new pipe stage";

fn cuts(body: &str) -> impl Iterator<Item = (usize, RegexCut)> + '_ {
    let bytes = body.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            if !bytes[i].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            let start = i;
            let mut line_break = None;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                if line_break.is_none() && matches!(bytes[i], b'\n' | b'\r') {
                    line_break = Some(i);
                }
                i += 1;
            }
            if bytes.get(i) == Some(&b'|') {
                return Some((start, RegexCut::PipeStage));
            }
            if let Some(at) = line_break {
                return Some((at, RegexCut::LineBreak));
            }
        }
        None
    })
}

/// The one owner of the rule. The grammar asks it in every regex
/// position, and so does every text-level reader that has to agree with
/// the grammar about where a regex ends: the splice scan
/// ([`crate::parser::scan`]), the search-stage split in `trawl-search-url`
/// and the formatter's `/body/` spelling. Every byte the rule tests is
/// ASCII, so each offset is a char boundary.
#[must_use]
pub fn first_cut(body: &str) -> Option<(usize, RegexCut)> {
    cuts(body).next()
}

pub(crate) fn value_cut(body: &str) -> Option<RegexCut> {
    let mut found = None;
    for (_, cut) in cuts(body) {
        if cut == RegexCut::PipeStage {
            return Some(cut);
        }
        found.get_or_insert(cut);
    }
    found
}

pub(crate) fn slash_run<'src>()
-> impl Parser<'src, ParserInput<'src>, &'src str, ParserExtra<'src>> + Clone {
    none_of('/')
        .repeated()
        .at_least(1)
        .to_slice()
        .delimited_by(just('/'), just('/'))
}

/// The shape alone does not make a regex. Each caller decides what may
/// follow the closing `/` before it asks [`check_regex_body`], so a
/// candidate its caller reads as something else is never validated
/// (ADR-0060). A cut run fails the shape without consuming, so a search
/// value reads it as a bare value, which the cut ends.
pub(crate) fn regex_shape<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    slash_run()
        .filter(|body: &&str| first_cut(body).is_none())
        .map(String::from)
        .labelled("regex pattern")
}

#[derive(Debug, Clone)]
pub(crate) struct RegexSite {
    /// What a build error covers: the whole `/…/`, quoted string or bare
    /// glob, as written.
    pub(crate) span: std::ops::Range<usize>,
    pub(crate) kind: PatternKind,
    /// The pattern as the query decodes it: a regex body, or a glob or
    /// `like` operand.
    pub(crate) text: String,
    /// What translating its regex costs, in units: the query's sum of
    /// these is held to the translation budget before anything builds.
    pub(crate) units: u64,
}

/// A site is dropped when the parse backtracks past it, as chumsky drops
/// a diagnostic emitted there, so the sites left belong to the reading
/// that parsed.
#[derive(Debug, Default)]
pub(crate) struct RegexSites {
    pub(crate) sites: Vec<RegexSite>,
    /// The span of each `/…/` run that a search value read as a bare
    /// value because a pipe-stage cut sits in its body, though the rule
    /// before ADR-0061 ruling 1 read the run as a regex. A backtrack keeps
    /// them: the bare reading succeeds, so the run is a cause only once the
    /// query has failed, and `crate::parser::parse` then names it beside
    /// any error that falls inside it.
    pub(crate) pipe_cut_runs: Vec<std::ops::Range<usize>>,
}

impl RegexSites {
    fn record(&mut self, span: SimpleSpan, kind: PatternKind, text: &str, units: u64) {
        self.sites.push(RegexSite {
            span: span.start..span.end,
            kind,
            text: text.to_string(),
            units,
        });
    }
}

impl<'src, I: chumsky::input::Input<'src>> chumsky::inspector::Inspector<'src, I> for RegexSites {
    type Checkpoint = usize;

    fn on_token(&mut self, _: &I::Token) {}

    fn on_save<'parse>(&self, _: &chumsky::input::Cursor<'src, 'parse, I>) -> usize {
        self.sites.len()
    }

    fn on_rewind<'parse>(&mut self, marker: &chumsky::input::Checkpoint<'src, 'parse, I, usize>) {
        self.sites.truncate(*marker.inspector());
    }
}

pub(crate) fn check_pattern(
    kind: PatternKind,
    text: &str,
    span: SimpleSpan,
    sites: &mut RegexSites,
    emitter: &mut chumsky::input::Emitter<Rich<'_, char>>,
) -> bool {
    match pattern_refusal(kind, text) {
        Err(message) => {
            emitter.emit(Rich::custom(span, message));
            true
        }
        Ok(Checked::Recorded { units }) => {
            sites.record(span, kind, text, units);
            false
        }
    }
}

pub(crate) fn check_regex_body(
    body: &str,
    span: SimpleSpan,
    sites: &mut RegexSites,
    emitter: &mut chumsky::input::Emitter<Rich<'_, char>>,
) -> bool {
    check_pattern(PatternKind::Regex, body, span, sites, emitter)
}

enum Checked {
    Recorded { units: u64 },
}

/// Why a pattern is refused before anything builds it, in the order every
/// pattern site shares (ADR-0063). The class check is a regex body's alone:
/// the batch lane runs a glob, `like` or `ilike` operand as `DuckDB`'s GLOB,
/// LIKE or ILIKE, never RE2.
fn pattern_refusal(kind: PatternKind, text: &str) -> Result<Checked, String> {
    let body = match kind {
        PatternKind::Regex => Some(regex_syntax(text)?),
        _ => None,
    };
    if text.len() > MAX_REGEX_LEN {
        return Err(PatternError::Length {
            kind,
            bytes: text.len(),
        }
        .to_string());
    }
    let ast = match body {
        Some(ast) => {
            re2::class_set_refusal(&ast).map_err(|e| e.to_string())?;
            ast
        }
        None => regex_syntax(&kind.regex_source(text))?,
    };
    let shape = price::shape(&ast);
    if shape.positions > price::MAX_REPEAT_POSITIONS {
        return Err(PatternError::Repeat {
            positions: shape.positions,
            limit: price::MAX_REPEAT_POSITIONS,
        }
        .to_string());
    }
    Ok(Checked::Recorded { units: shape.units })
}

/// Parse `body` as regex syntax, or say why it is not regex syntax. The
/// parse is linear in the body and free of Unicode tables, and the
/// pattern owner's build parses with the same defaults, so the message is
/// the one compiling would give.
pub(crate) fn regex_syntax(body: &str) -> Result<Ast, String> {
    regex_syntax::ast::parse::Parser::new()
        .parse(body)
        .map_err(|e| format!("invalid regex: {e}"))
}

pub(crate) fn regex_error_message(error: &PatternError) -> String {
    match error {
        PatternError::Syntax(_) => format!("invalid regex: {error}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
pub(crate) fn regex_body_error(body: &str) -> Option<String> {
    pattern_refusal(PatternKind::Regex, body)
        .err()
        .or_else(|| regex_build_error(body))
}

#[cfg(test)]
fn regex_build_error(body: &str) -> Option<String> {
    Preparer::check()
        .check(PatternKind::Regex, body)
        .err()
        .map(|e| regex_error_message(&e))
}

/// A body that a cut refuses ([`first_cut`]) or that [`check_pattern`]
/// refuses is emitted over the whole `/…/`, not returned:
/// the branch still succeeds, so an enclosing choice cannot recover by
/// reading the same text another way, and `into_result()` is still
/// `Err`. Nothing but a regex can follow `matches` in a `/`, so a cut
/// here has no other reading to fall back to.
pub(crate) fn regex_pattern<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    slash_run()
        .validate(|body: &str, extra, emitter| {
            let span = extra.span();
            if let Some((_, cut)) = first_cut(body) {
                emitter.emit(Rich::custom(span, cut.message()));
            } else {
                check_regex_body(body, span, extra.state(), emitter);
            }
            body.to_string()
        })
        .labelled("regex pattern")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::MSG_REGEX_TOO_LARGE;

    // chumsky parsers have opaque types tied to a specific lifetime, so we
    // can't use a generic helper with HRTB. instead, inline the parse calls.

    #[test]
    fn test_uint() {
        assert_eq!(uint().parse("42").into_result().unwrap(), 42);
        assert_eq!(uint().parse("0").into_result().unwrap(), 0);
        assert_eq!(uint().parse("999999").into_result().unwrap(), 999_999);
    }

    #[test]
    fn test_int() {
        assert_eq!(int().parse("42").into_result().unwrap(), 42);
        assert_eq!(int().parse("-7").into_result().unwrap(), -7);
        assert_eq!(int().parse("0").into_result().unwrap(), 0);
    }

    #[test]
    fn test_float() {
        let v = float().parse("3.25").into_result().unwrap();
        assert!((v.value() - 3.25).abs() < f64::EPSILON);
        assert_eq!(v.text(), "3.25");
        let v = float().parse("-0.5").into_result().unwrap();
        assert!((v.value() - -0.5).abs() < f64::EPSILON);
        assert_eq!(v.text(), "-0.5");
    }

    #[test]
    fn test_float_keeps_source_token() {
        let v = float().parse("9007199254740993.0").into_result().unwrap();
        assert_eq!(v.text(), "9007199254740993.0");
        assert_eq!(v.value().to_string(), "9007199254740992");

        let v = float().parse("1.50").into_result().unwrap();
        assert_eq!(v.text(), "1.50");
    }

    #[test]
    fn test_quoted_string() {
        assert_eq!(
            quoted_string().parse(r#""hello""#).into_result().unwrap(),
            "hello"
        );
        assert_eq!(
            quoted_string()
                .parse(r#""with \"escape\"""#)
                .into_result()
                .unwrap(),
            "with \"escape\""
        );
        assert_eq!(
            quoted_string()
                .parse(r#""line\nbreak""#)
                .into_result()
                .unwrap(),
            "line\nbreak"
        );
    }

    #[test]
    fn test_ident() {
        assert_eq!(ident().parse("host").into_result().unwrap(), "host");
        assert_eq!(
            ident().parse("host_name").into_result().unwrap(),
            "host_name"
        );
        assert_eq!(
            ident().parse("host.name").into_result().unwrap(),
            "host.name"
        );
        assert_eq!(ident().parse("_private").into_result().unwrap(), "_private");
    }

    #[test]
    fn test_system_field() {
        assert_eq!(
            system_field().parse("@timestamp").into_result().unwrap(),
            "@timestamp"
        );
        assert_eq!(
            system_field().parse("@source").into_result().unwrap(),
            "@source"
        );
    }

    #[test]
    fn backtick_name_lexes_any_content() {
        assert_eq!(
            field_name().parse("`request id`").into_result().unwrap(),
            "request id"
        );
        assert_eq!(
            field_name().parse("`http-status`").into_result().unwrap(),
            "http-status"
        );
        assert_eq!(
            field_name().parse("`where`").into_result().unwrap(),
            "where"
        );
        assert_eq!(field_name().parse("`last`").into_result().unwrap(), "last");
    }

    #[test]
    fn backtick_name_doubled_backtick_escapes() {
        assert_eq!(field_name().parse("`a``b`").into_result().unwrap(), "a`b");
        // `` `` `` — open, one escaped backtick, close.
        assert_eq!(field_name().parse("````").into_result().unwrap(), "`");
    }

    #[test]
    fn backtick_name_dot_is_literal() {
        assert_eq!(
            field_name().parse("`.leading`").into_result().unwrap(),
            ".leading"
        );
        assert_eq!(
            field_name().parse("`host.name`").into_result().unwrap(),
            "host.name"
        );
    }

    #[test]
    fn backtick_name_empty_and_control_chars_rejected() {
        assert!(field_name().parse("``").into_result().is_err());
        assert!(field_name().parse("`a\nb`").into_result().is_err());
        assert!(field_name().parse("`a\tb`").into_result().is_err());
        assert!(field_name().parse("`a\u{7f}b`").into_result().is_err());
        assert!(field_name().parse("`\u{1b}[2J`").into_result().is_err());
    }

    #[test]
    fn plain_name_takes_no_backticks() {
        assert!(plain_name().parse("`lower`").into_result().is_err());
        assert_eq!(plain_name().parse("lower").into_result().unwrap(), "lower");
        assert_eq!(
            plain_name().parse("@timestamp").into_result().unwrap(),
            "@timestamp"
        );
    }

    #[test]
    fn test_filter_op() {
        assert_eq!(
            filter_op().parse(">=").into_result().unwrap(),
            FilterOp::Gte
        );
        assert_eq!(
            filter_op().parse("<=").into_result().unwrap(),
            FilterOp::Lte
        );
        assert_eq!(filter_op().parse("!=").into_result().unwrap(), FilterOp::Ne);
        assert_eq!(filter_op().parse(">").into_result().unwrap(), FilterOp::Gt);
        assert_eq!(filter_op().parse("<").into_result().unwrap(), FilterOp::Lt);
        assert_eq!(filter_op().parse("=").into_result().unwrap(), FilterOp::Eq);
    }

    #[test]
    fn test_duration() {
        let d = duration().parse("2h").into_result().unwrap();
        assert_eq!(d.quantity, 2);
        assert_eq!(d.unit, TimeUnit::Hours);

        let d = duration().parse("30s").into_result().unwrap();
        assert_eq!(d.quantity, 30);
        assert_eq!(d.unit, TimeUnit::Seconds);

        let d = duration().parse("7d").into_result().unwrap();
        assert_eq!(d.quantity, 7);
        assert_eq!(d.unit, TimeUnit::Days);
    }

    #[test]
    fn test_keyword_boundary() {
        assert_eq!(keyword("and").parse("and").into_result().unwrap(), "and");

        assert!(keyword("and").parse("android").into_result().is_err());
    }

    #[test]
    fn test_bool_literal() {
        assert_eq!(
            bool_literal().parse("true").into_result().unwrap(),
            LiteralValue::Bool(true)
        );
        assert_eq!(
            bool_literal().parse("false").into_result().unwrap(),
            LiteralValue::Bool(false)
        );
    }

    #[test]
    fn test_null_literal() {
        assert_eq!(
            null_literal().parse("null").into_result().unwrap(),
            LiteralValue::Null
        );
    }

    #[test]
    fn a_raw_string_decodes_nothing() {
        let raw = |input: &str| raw_quoted_string().parse(input).into_result().ok();
        assert_eq!(raw(r#""\d+\\""#).as_deref(), Some(r"\d+\\"));
        assert_eq!(raw(r#""a\""#).as_deref(), Some(r"a\"));
        assert_eq!(raw(r#""a\"b""#), None);
    }

    #[test]
    fn test_regex_pattern() {
        assert_eq!(
            regex_pattern().parse("/error.*/").into_result().unwrap(),
            "error.*"
        );
        assert_eq!(
            regex_pattern().parse("/^foo$/").into_result().unwrap(),
            "^foo$"
        );
    }

    #[test]
    fn test_bare_value() {
        assert_eq!(bare_value().parse("nginx").into_result().unwrap(), "nginx");
        assert_eq!(bare_value().parse("200").into_result().unwrap(), "200");
        assert_eq!(
            bare_value().parse("error*").into_result().unwrap(),
            "error*"
        );
    }

    #[test]
    fn test_uint_overflow() {
        assert!(
            uint()
                .parse("99999999999999999999999")
                .into_result()
                .is_err()
        );
    }

    #[test]
    fn test_int_overflow() {
        assert!(
            int()
                .parse("-99999999999999999999999")
                .into_result()
                .is_err()
        );
    }

    #[test]
    fn test_duration_zero_rejected() {
        assert!(duration().parse("0h").into_result().is_err());
        assert!(duration().parse("0s").into_result().is_err());
        assert!(duration().parse("0d").into_result().is_err());
    }

    #[test]
    fn test_duration_overflow_rejected() {
        // This quantity * 604800 (weeks) would overflow u64.
        assert!(
            duration()
                .parse("99999999999999999w")
                .into_result()
                .is_err()
        );
    }

    #[test]
    fn test_duration_large_but_safe() {
        // ~2739 years in days — fits in u64 (1_000_000 * 86_400 = 86_400_000_000).
        let d = duration().parse("1000000d").into_result().unwrap();
        assert_eq!(d.quantity, 1_000_000);
        assert_eq!(d.to_seconds(), 86_400_000_000);
    }

    #[test]
    fn first_cut_finds_a_line_break_or_a_pipe_stage() {
        for body in [
            "err|warn",
            "a| b",
            "a b",
            "a\tb",
            "x ,|",
            "a\u{a0}|b",
            "é ,|",
        ] {
            assert_eq!(first_cut(body), None, "{body:?}");
        }
        for (body, cut) in [
            ("a\nb", (1, RegexCut::LineBreak)),
            ("a\rb", (1, RegexCut::LineBreak)),
            ("a \r\nb", (2, RegexCut::LineBreak)),
            ("err | warn", (3, RegexCut::PipeStage)),
            ("a\t|b", (1, RegexCut::PipeStage)),
            ("x |", (1, RegexCut::PipeStage)),
            ("a  \n  | b", (1, RegexCut::PipeStage)),
            ("tmp*\n| where x ", (4, RegexCut::PipeStage)),
            ("a\nb | c", (1, RegexCut::LineBreak)),
            ("é |", (2, RegexCut::PipeStage)),
        ] {
            assert_eq!(first_cut(body), Some(cut), "{body:?}");
        }
        assert_eq!(value_cut("a\nb | c"), Some(RegexCut::PipeStage));
        assert_eq!(value_cut("a\nb"), Some(RegexCut::LineBreak));
        assert_eq!(value_cut("a b"), None);
    }

    #[test]
    fn a_cut_body_is_not_a_regex_shape() {
        assert_eq!(
            regex_shape().parse("/err|warn/").into_result().unwrap(),
            "err|warn"
        );
        for input in ["/a\nb/", "/a | b/"] {
            assert!(
                regex_shape().parse(input).into_result().is_err(),
                "{input:?}"
            );
            let errors = regex_pattern().parse(input).into_errors();
            assert_eq!(errors.len(), 1, "{input:?}: {errors:?}");
            let span = errors[0].span();
            assert_eq!(span.start..span.end, 0..input.len(), "{input:?}");
        }
    }

    #[test]
    fn test_regex_pattern_invalid() {
        // unclosed group
        assert!(
            regex_pattern()
                .parse("/(?P<unclosed/")
                .into_result()
                .is_err()
        );
    }

    #[test]
    fn test_regex_pattern_length_limit_exceeded() {
        let long_pattern = format!("/{}/", "a".repeat(1025));
        assert!(regex_pattern().parse(&long_pattern).into_result().is_err());
    }

    /// A body at the limit is 1024 bytes in 64 classes and 192 empty
    /// groups: each character or class is a repeat position and an empty
    /// group is none, so 1024 `a`s, or 256 classes, would be refused by
    /// the repeat budget, not the length cap.
    #[test]
    fn test_regex_pattern_at_length_limit() {
        let pattern = format!("/{}{}/", "[ab]".repeat(64), "(?:)".repeat(192));
        assert_eq!(pattern.len(), 1024 + 2);
        assert!(regex_pattern().parse(&pattern).into_result().is_ok());
    }

    /// The limit counts bytes, and the message says so: 513 two-byte
    /// characters are 1026 bytes. The admitted body is 513 characters and
    /// 1024 bytes, written as one class to stay inside the repeat budget.
    #[test]
    fn test_regex_pattern_length_limit_counts_bytes() {
        assert_eq!(
            regex_body_error(&"é".repeat(513)).as_deref(),
            Some("regex pattern too long (1026 bytes, max 1024)")
        );
        let at_limit = format!("[{}]", "é".repeat(511));
        assert_eq!((at_limit.chars().count(), at_limit.len()), (513, 1024));
        assert_eq!(regex_body_error(&at_limit), None);
    }

    #[test]
    fn the_syntax_check_reports_what_compiling_would() {
        let built = regex_build_error;
        for body in ["[", "*", "(?P<x", "a{2,1}", r"\q", "(a", "a)", "(?z)", "x{"] {
            assert!(regex_syntax(body).is_err(), "{body}");
            assert_eq!(regex_syntax(body).err(), built(body), "{body}");
        }
        // Naming an unknown Unicode class is well-formed syntax: only the
        // build refuses it.
        assert!(regex_syntax(r"\p{Nope}").is_ok());
        assert!(built(r"\p{Nope}").is_some_and(|m| m.starts_with("invalid regex:")));
    }

    /// A short body can compile large: each count of a counted repeat is
    /// another copy of its class, and a Unicode class is a big one.
    #[test]
    fn a_regex_that_compiles_too_large_is_refused() {
        assert_eq!(
            MSG_REGEX_TOO_LARGE,
            format!(
                "regex pattern too large once compiled (max {} MiB)",
                crate::pattern::MAX_REGEX_COMPILED_BYTES >> 20
            )
        );
        for body in [r"\w{64}", r"\b\w{3,28}\b\s+\b\w{3,28}\b"] {
            assert_eq!(
                regex_body_error(body).as_deref(),
                Some(MSG_REGEX_TOO_LARGE),
                "{body}"
            );
        }
    }

    #[test]
    fn log_regexes_compile_under_the_size_limit() {
        for body in [
            r"\w{10}",
            r"\w+@\w+\.\w{2,10}",
            r"[\w.+-]+@[\w-]+\.[\w.]{2,10}",
            r"[A-Za-z0-9_]{3,30}",
            r"\d{1,3}(\.\d{1,3}){3}",
            r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})",
            r"([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}",
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            r"(?i)(error|fail(ed|ure)?|exception|timeout|refused|denied)",
            r#"(?P<key>\w+)=(?P<val>"[^"]*"|\S+)"#,
            r"^(?P<ip>\d{1,3}(\.\d{1,3}){3}) - (?P<user>\S+) \[(?P<ts>[^\]]+)\] .(?P<method>[A-Z]+) (?P<path>\S+) HTTP\d\.\d. (?P<status>\d{3}) (?P<bytes>\d+|-)",
            r"(\w+\s+){5}\w+",
            r".{64}",
        ] {
            assert_eq!(regex_body_error(body), None, "{body}");
        }
    }

    #[test]
    fn a_long_counted_repeat_is_refused_before_it_builds() {
        let limit = price::MAX_REPEAT_POSITIONS;
        for body in [r".{1000}", r".{256}", r"\d{1000}", r"\w{65}", r"(?:ab){33}"] {
            let refusal = regex_body_error(body).unwrap_or_default();
            assert!(
                refusal.starts_with(crate::pattern::MSG_REPEAT_BUDGET),
                "{body}: {refusal}"
            );
            assert!(refusal.ends_with(&format!("max {limit})")), "{refusal}");
        }
        assert_eq!(
            regex_body_error(r"\d{1000}").as_deref(),
            Some("pattern repeats too much (1000 positions, max 64)")
        );
        assert_eq!(regex_body_error(r"(?:ab){32}"), None, "64 positions");
    }
}
