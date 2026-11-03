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
use crate::pattern::{MAX_REGEX_LEN, PatternError, PatternKind, price};
use crate::sanitize::is_unsafe_display_char;

/// Shorthand for our parser type — `&str` input, `Rich` errors, and the
/// regexes the parse recognised as its state ([`RegexSites`]).
pub(crate) type ParserInput<'src> = &'src str;
pub(crate) type ParserExtra<'src> = extra::Full<Rich<'src, char>, RegexSites, ()>;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Wrap a value with its source span via `map_with`.
pub(crate) fn spanned<'src, T: 'src>(
    p: impl Parser<'src, ParserInput<'src>, T, ParserExtra<'src>> + Clone,
) -> impl Parser<'src, ParserInput<'src>, Spanned<T>, ParserExtra<'src>> + Clone {
    p.map_with(|node, extra| {
        let span = extra.span();
        Spanned::new(node, span.start..span.end)
    })
}

// ---------------------------------------------------------------------------
// numbers
// ---------------------------------------------------------------------------

/// Parse an unsigned integer (sequence of digits).
pub(crate) fn uint<'src>() -> impl Parser<'src, ParserInput<'src>, u64, ParserExtra<'src>> + Clone {
    text::int(10)
        .to_slice()
        .try_map(|s: &str, span| {
            s.parse::<u64>()
                .map_err(|e| Rich::custom(span, format!("invalid integer: {e}")))
        })
        .labelled("integer")
}

/// Parse a signed integer.
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

/// Parse a float (must contain a `.` to distinguish from int).
///
/// The token is kept beside the parsed double: `f64` is lossy past 53 bits
/// and pin-aware pipeline comparison binds the literal's text, never the
/// re-rendered double (ADR-0011 ruling #6, [`FloatLiteral`]).
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

/// Parse a numeric literal — tries float first, then int.
pub(crate) fn number_literal<'src>()
-> impl Parser<'src, ParserInput<'src>, LiteralValue, ParserExtra<'src>> + Clone {
    float()
        .map(LiteralValue::Float)
        .or(int().map(LiteralValue::Int))
}

// ---------------------------------------------------------------------------
// strings
// ---------------------------------------------------------------------------

/// Parse a double-quoted string with basic escape support.
pub(crate) fn quoted_string<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    let escape = just('\\').ignore_then(choice((
        just('\\'),
        just('"'),
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
    )));

    none_of("\\\"")
        .or(escape)
        .repeated()
        .collect::<String>()
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

// ---------------------------------------------------------------------------
// identifiers and field names
// ---------------------------------------------------------------------------

/// Parse a bare identifier: `[a-zA-Z_][a-zA-Z0-9_]*`
/// Also handles dot-separated field names like `host.name`.
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

/// Parse an `@`-prefixed name like `@timestamp`. The `@` is spelling
/// only: the name is ordinary sender vocabulary, not a trawl slot.
pub(crate) fn system_field<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    just('@')
        .then(ident())
        .to_slice()
        .map(String::from)
        .labelled("system field")
}

/// Parse a name in a position that is not a field: a function name, a
/// stage name, a saved-query name. These take no backticks (ADR-0013
/// ruling 7), since `` `lower`(x) `` must not be a call, so this is the
/// unquoted production: `field_name()` minus the backtick arm.
pub(crate) fn plain_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    system_field().or(ident())
}

/// Parse a backtick-quoted name: `` `request id` ``.
///
/// Backticks change how a name is lexed, never what a name may be
/// (ADR-0013 ruling 7). Content is any character except a backtick; a
/// doubled backtick escapes one. Empty names and names carrying a
/// character that cannot be rendered as itself ([`is_unsafe_display_char`]:
/// controls, the line and paragraph separators, bidi and zero-width format
/// characters, the soft hyphen) are
/// parse errors, and they are the only two refusals, because this is
/// lexing, not policy: the ASCII fold ([`crate::schema::catalog_key`]) and
/// the sealed `_` namespace ([`crate::schema::is_reserved_name`]) apply to
/// the result exactly as they do to a bare name. A dot inside the quotes
/// is a literal character, not a nested-name segment.
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

/// Parse a field name: a backtick-quoted name, an `@`-prefixed name, or
/// a regular identifier.
pub(crate) fn field_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    choice((quoted_name(), system_field(), ident()))
}

// ---------------------------------------------------------------------------
// filter operators (for field:op value syntax)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// durations
// ---------------------------------------------------------------------------

/// Parse a time unit suffix.
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

/// Parse a duration like `2h`, `5m`, `30s`.
///
/// Rejects zero-duration values and durations large enough to overflow
/// `u64` when converted to seconds.
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

// ---------------------------------------------------------------------------
// keywords with word boundary checks
// ---------------------------------------------------------------------------

/// Parse a keyword that must not be followed by an alphanumeric char or `_`.
/// This prevents `android` from matching as `and`.
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

/// Parse `true` or `false` as a boolean literal.
pub(crate) fn bool_literal<'src>()
-> impl Parser<'src, ParserInput<'src>, LiteralValue, ParserExtra<'src>> + Clone {
    keyword("true")
        .to(LiteralValue::Bool(true))
        .or(keyword("false").to(LiteralValue::Bool(false)))
}

/// Parse `null`.
pub(crate) fn null_literal<'src>()
-> impl Parser<'src, ParserInput<'src>, LiteralValue, ParserExtra<'src>> + Clone {
    keyword("null").to(LiteralValue::Null)
}

/// Parse any literal value: bool, null, float, int, or quoted string.
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

// ---------------------------------------------------------------------------
// bare filter values (for search stage field=value)
// ---------------------------------------------------------------------------

/// Whether `c` ends an unquoted field-filter value: ASCII whitespace, `|`,
/// the list separator `,`, a parenthesis or a backtick. A regex ends at
/// the same bytes (ADR-0060), so the two cannot drift apart.
pub(crate) fn ends_bare_value(c: char) -> bool {
    c.is_ascii_whitespace() || matches!(c, '|' | ',' | ')' | '(' | '`')
}

/// Parse a bare (unquoted) value in a field filter — stops at a byte
/// [`ends_bare_value`] names.
///
/// A backtick ends a bare value, the same exclusion the search stage's bare
/// word makes and for the same reason (ADR-0013 ruling 7): a backticked
/// name is a field reference in every field position, so no unquoted
/// position may absorb one. A value that genuinely contains a backtick is
/// written double-quoted (`` host="a`b" ``).
///
/// A comment opener inside the value is a parse error, not data and not a
/// comment (ADR-0014 ruling 2): `color=#ff0000` names its own `#` instead
/// of quietly becoming a text search for `color=`. It carries the
/// value-position message, whose hint quotes the value alone
/// (`color="#ff0000"`), because quoting the whole token would turn a field
/// filter into a phrase search, and a hint that changes what the query
/// means is worse than none. The value is still produced, so the
/// diagnostic is emitted rather than returned: a structurally-successful
/// branch keeps chumsky's alternative selection from ranking a worse error
/// from a later arm ahead of this one, while `into_result()` is still
/// `Err`.
///
/// A value that opens with `"` and reaches this production is an error
/// too ([`MSG_QUOTE_OPENS_BARE_VALUE`], ADR-0061 ruling 3): the quoted
/// arm tried it first and found no complete string, so the author left a
/// quote open. Read bare it would be a value carrying a literal quote, and
/// a glob such as `host="*` has no spelling at all, since quoting makes a
/// glob exact. It takes precedence over the opener error, one error per
/// value.
///
/// This is the single bare-RHS production — single values, comma-separated
/// IN lists and glob detection all route through it — so every exclusion
/// covers every unquoted value shape at once.
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
                    // The exact value slice rides along: this production
                    // is the one place its bounds are known, and an IN
                    // list's element is its own `bare_value`, so the hint
                    // names the element the user can act on without
                    // re-deriving a boundary from the raw text.
                    crate::parser::comment::payload(crate::parser::comment::MSG_OPENER_IN_VALUE, s),
                ));
            }
            s
        })
        .map(String::from)
        .labelled("value")
}

/// The message a bare value opening with `"` carries (ADR-0061 ruling 3).
pub(crate) const MSG_QUOTE_OPENS_BARE_VALUE: &str =
    "a value that opens with '\"' must be a complete quoted string";

/// What cuts a `/…/` run so it cannot be a regex (ADR-0061 ruling 1). A
/// regex body holds no raw line break and no `|` after whitespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexCut {
    /// A raw `\n` or `\r` that no `|` follows. A regex spells it `\n`.
    LineBreak,
    /// A run of ASCII whitespace, line breaks included, and then `|`. A
    /// new pipe stage always ends what came before it, so this is a stage
    /// boundary even inside what would otherwise be a regex body.
    PipeStage,
}

impl RegexCut {
    /// The message a regex position that holds this cut carries.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::LineBreak => MSG_REGEX_LINE_BREAK,
            Self::PipeStage => MSG_REGEX_PIPE_STAGE,
        }
    }
}

/// The message a regex cut by a raw line break carries.
pub(crate) const MSG_REGEX_LINE_BREAK: &str = "a regex cannot hold a raw line break";

/// The message a regex cut by `|` after whitespace carries.
pub(crate) const MSG_REGEX_PIPE_STAGE: &str =
    "a regex cannot hold '|' after whitespace, which starts a new pipe stage";

/// Every cut in `body`, in text order, each with the byte offset it
/// starts at: a pipe-stage cut at the first byte of the whitespace before
/// its `|`, a line-break cut at its `\n` or `\r`. One pass, so a long
/// whitespace run costs its length once.
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

/// The first cut in a regex body, with its byte offset, or `None` when
/// the body rule admits it (ADR-0061 ruling 1).
///
/// The one owner of the rule. The grammar asks it in every regex
/// position, and so does every text-level reader that has to agree with
/// the grammar about where a regex ends: the splice scan
/// ([`crate::parser::scan`]), the search-stage split in `trawl-search-url`
/// and the formatter's `/body/` spelling. `body` is the text between the
/// two slashes. Whitespace is ASCII whitespace, the set that ends an
/// unquoted value ([`ends_bare_value`]), and a line break is `\n` or
/// `\r`. Every byte the rule tests is ASCII, so each offset is a char
/// boundary.
#[must_use]
pub fn first_cut(body: &str) -> Option<(usize, RegexCut)> {
    cuts(body).next()
}

/// How a search value reads a `/…/` run that the rule before ruling 1
/// would have closed: a pipe-stage cut anywhere in the body wins over a
/// line break, since the stage ends the value whatever came before it.
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

/// Parse a `/`, a body of one or more bytes none of them `/`, and a `/`.
/// Produces the body and judges nothing: this is the run each regex
/// position reads before the body rule ([`first_cut`]) decides what it is.
pub(crate) fn slash_run<'src>()
-> impl Parser<'src, ParserInput<'src>, &'src str, ParserExtra<'src>> + Clone {
    none_of('/')
        .repeated()
        .at_least(1)
        .to_slice()
        .delimited_by(just('/'), just('/'))
}

/// Parse the shape of a regex delimited by `/`: `/body/`, where the body
/// is one or more bytes, none of them `/`, and [`first_cut`] finds no cut
/// in it. Produces the body and validates nothing else.
///
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

/// A pattern the parse recognised and checked, still to be built.
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

/// The parse state: the patterns the parse recognised, in text order, and
/// the `/…/` runs a pipe stage cut out of a search value.
///
/// The parser checks each pattern as it reads it ([`check_pattern`]) and
/// builds nothing. Building waits until the query has parsed and holds no
/// more than [`crate::parser::MAX_QUERY_PATTERNS`] patterns, because the
/// count alone does not bound the work once building starts.
///
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

/// Check a pattern the grammar recognised, inside a `validate` step
/// ([`pattern_refusal`]): a refusal is emitted over `span`, and a pattern
/// that passes is recorded for building once the parse is done. Returns
/// whether it emitted.
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
        Ok(Checked::Unbuilt) => false,
    }
}

/// Check a recognised regex body: [`check_pattern`] for a regex.
pub(crate) fn check_regex_body(
    body: &str,
    span: SimpleSpan,
    sites: &mut RegexSites,
    emitter: &mut chumsky::input::Emitter<Rich<'_, char>>,
) -> bool {
    check_pattern(PatternKind::Regex, body, span, sites, emitter)
}

/// What [`pattern_refusal`] admits a pattern as.
enum Checked {
    /// Recorded, to be built once the query has parsed, with what
    /// translating it costs.
    Recorded { units: u64 },
    /// A glob whose regex does not parse: neither refused nor built.
    Unbuilt,
}

// The hint table (`comment::hint_for`) still names the compiled-size
// message by this path.
pub(crate) use crate::pattern::MSG_REGEX_TOO_LARGE;

/// Why a pattern is refused before anything builds it, in the order every
/// pattern site shares (ADR-0063): a regex body's syntax, then the length
/// of the text as the query decodes it, delimiters excluded, then the
/// repeat budget ([`price::MAX_REPEAT_POSITIONS`]). One syntax parse
/// serves the syntax check and the walk that prices the pattern
/// ([`price::shape`]). Each step is linear in the text and builds
/// nothing, so the parser asks it of every pattern it reads, however
/// many. Invalid syntax is refused before it is priced, so it charges
/// nothing.
///
/// A glob or `like` operand has no regex syntax of its own, and its length
/// is the operand's: the regex it stands for is longer, and is not capped
/// again, but it is priced like any regex. A glob whose regex does not
/// parse (`[z-a]*`) is left as it was before these checks existed: the
/// parser neither refuses, prices nor builds it, and the live lane refuses
/// it when it compiles the filter.
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
        Some(ast) => ast,
        None => match regex_syntax(&kind.regex_source(text)) {
            Ok(ast) => ast,
            Err(_) => return Ok(Checked::Unbuilt),
        },
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
/// parse is linear in the body and free of Unicode tables, and the `regex`
/// crate parses with the same defaults, so the message is the one
/// compiling would give.
pub(crate) fn regex_syntax(body: &str) -> Result<Ast, String> {
    regex_syntax::ast::parse::Parser::new()
        .parse(body)
        .map_err(|e| format!("invalid regex: {e}"))
}

/// The parse message for a regex body a ledger refused. A syntax error
/// reads `invalid regex: …`, as the syntax check words it.
pub(crate) fn regex_error_message(error: &PatternError) -> String {
    match error {
        PatternError::Syntax(_) => format!("invalid regex: {error}"),
        other => other.to_string(),
    }
}

/// Why a regex body cannot be used, or `None` when the parser accepts it:
/// the checks at its site, then a build of its own.
#[cfg(test)]
pub(crate) fn regex_body_error(body: &str) -> Option<String> {
    pattern_refusal(PatternKind::Regex, body)
        .err()
        .or_else(|| regex_build_error(body))
}

/// Why `body` does not build, from a ledger of its own.
#[cfg(test)]
fn regex_build_error(body: &str) -> Option<String> {
    Preparer::new()
        .build(PatternKind::Regex, body)
        .err()
        .map(|e| regex_error_message(&e))
}

/// Parse a regex `/body/` and check its body, for a position where the
/// shape alone makes a regex (`matches /re/`).
///
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

    /// The token survives the parse verbatim, trailing zeros and all:
    /// `f64` is lossy past 53 bits and pin-aware comparison binds the text
    /// (ADR-0011 ruling #6), so re-rendering the double would silently
    /// answer for a different number.
    #[test]
    fn test_float_keeps_source_token() {
        let v = float().parse("9007199254740993.0").into_result().unwrap();
        assert_eq!(v.text(), "9007199254740993.0");
        // …which the parsed double cannot express: it rounds to the
        // adjacent even.
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

    // ── backtick-quoted names (ADR-0013 ruling 7) ──────────────────────

    /// Backticks change how a name is lexed, never what a name may be:
    /// the content is any character except a backtick, and the parse
    /// yields the bare name with the quotes gone.
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

    /// A doubled backtick escapes one.
    #[test]
    fn backtick_name_doubled_backtick_escapes() {
        assert_eq!(field_name().parse("`a``b`").into_result().unwrap(), "a`b");
        // `` `` `` — open, one escaped backtick, close.
        assert_eq!(field_name().parse("````").into_result().unwrap(), "`");
    }

    /// A dot inside backticks is literal, not a nested-name segment —
    /// a leading dot is unreachable for the bare production.
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

    /// Empty and control-char names are parse errors (ruling 7): an empty
    /// column name is unnameable and a control char would rewrite the
    /// terminal line that renders it.
    #[test]
    fn backtick_name_empty_and_control_chars_rejected() {
        assert!(field_name().parse("``").into_result().is_err());
        assert!(field_name().parse("`a\nb`").into_result().is_err());
        assert!(field_name().parse("`a\tb`").into_result().is_err());
        assert!(field_name().parse("`a\u{7f}b`").into_result().is_err());
        assert!(field_name().parse("`\u{1b}[2J`").into_result().is_err());
    }

    /// Function names, stage names and saved-query names are not fields,
    /// so they take the unquoted production and no backticks.
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

        // "android" should not match as keyword "and"
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

    /// A raw string decodes nothing, so an `extract` body's length is its
    /// written length: a backslash before the closing quote is a byte of
    /// the body, not an escape.
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

    /// The body rule (ADR-0061 ruling 1): a raw line break or `|` after
    /// whitespace cuts a body, and nothing else does.
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
            // the first cut, in text order
            ("a\nb | c", (1, RegexCut::LineBreak)),
            ("é |", (2, RegexCut::PipeStage)),
        ] {
            assert_eq!(first_cut(body), Some(cut), "{body:?}");
        }
        // …while a search value lets a pipe stage anywhere win
        assert_eq!(value_cut("a\nb | c"), Some(RegexCut::PipeStage));
        assert_eq!(value_cut("a\nb"), Some(RegexCut::LineBreak));
        assert_eq!(value_cut("a b"), None);
    }

    /// The shape refuses a cut body without consuming, and `matches`
    /// reports the cut over the whole run.
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

    /// A body at the limit is 1024 bytes in 256 classes: each character
    /// is a repeat position, so 1024 `a`s would be refused by the repeat
    /// budget, not the length cap.
    #[test]
    fn test_regex_pattern_at_length_limit() {
        let pattern = format!("/{}/", "[ab]".repeat(256));
        assert_eq!(pattern.len(), 1024 + 2);
        assert!(regex_pattern().parse(&pattern).into_result().is_ok());
    }

    /// The limit counts bytes, and the message says so: 513 two-byte
    /// characters are 1026 bytes. The admitted body is 768 characters and
    /// 1024 bytes, written as 256 classes to stay inside the repeat budget.
    #[test]
    fn test_regex_pattern_length_limit_counts_bytes() {
        assert_eq!(
            regex_body_error(&"é".repeat(513)).as_deref(),
            Some("regex pattern too long (1026 bytes, max 1024)")
        );
        let at_limit = "[é]".repeat(256);
        assert_eq!((at_limit.chars().count(), at_limit.len()), (768, 1024));
        assert_eq!(regex_body_error(&at_limit), None);
    }

    /// The syntax check the parser runs gives the message compiling would:
    /// both parse a body with the same defaults.
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
        for body in [r"\w{100}", r"\b\w{3,30}\b\s+\b\w{3,30}\b"] {
            assert_eq!(
                regex_body_error(body).as_deref(),
                Some(MSG_REGEX_TOO_LARGE),
                "{body}"
            );
        }
    }

    /// Log regexes an author writes compile under the limit, counted
    /// Unicode repeats included.
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
            r".{256}",
        ] {
            assert_eq!(regex_body_error(body), None, "{body}");
        }
    }

    /// A count past [`price::MAX_REPEAT_POSITIONS`] is refused before it
    /// builds, whether or not it would compile under the size limit: `.`
    /// and `\d` would, `\w` would not.
    #[test]
    fn a_long_counted_repeat_is_refused_before_it_builds() {
        let limit = price::MAX_REPEAT_POSITIONS;
        for body in [r".{1000}", r"\d{1000}", r"\w{257}", r"(?:ab){129}"] {
            let refusal = regex_body_error(body).unwrap_or_default();
            assert!(
                refusal.starts_with(crate::pattern::MSG_REPEAT_BUDGET),
                "{body}: {refusal}"
            );
            assert!(refusal.ends_with(&format!("max {limit})")), "{refusal}");
        }
        assert_eq!(
            regex_body_error(r"\d{1000}").as_deref(),
            Some("pattern repeats too much (1000 positions, max 256)")
        );
        assert_eq!(regex_body_error(r"(?:ab){128}"), None, "256 positions");
    }
}
