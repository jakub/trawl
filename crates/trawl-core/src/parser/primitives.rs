// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layer 1: primitive parsers.
//!
//! Numbers, quoted strings, bare words, `@`-prefixed system fields,
//! filter operators, durations, and boolean/null keywords.

use chumsky::prelude::*;

use crate::ast::{FilterOp, FloatLiteral, LiteralValue, Spanned, TimeUnit, TrawlDuration};
use crate::sanitize::is_unsafe_display_char;

/// Shorthand for our parser type — `&str` input, `Rich` errors.
pub(crate) type ParserInput<'src> = &'src str;
pub(crate) type ParserExtra<'src> = extra::Err<Rich<'src, char>>;

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
/// and pin-aware pipeline comparison binds the literal's TEXT, never the
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

/// Parse a double-quoted string without escape processing.
///
/// Only `\"` is recognized (to allow literal quotes inside the string);
/// all other backslash sequences are passed through verbatim. This is
/// used for regex patterns in `extract` where `\d`, `\w` etc. should
/// not require double-escaping.
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

/// Parse an `@`-prefixed system field like `@timestamp`.
pub(crate) fn system_field<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    just('@')
        .then(ident())
        .to_slice()
        .map(String::from)
        .labelled("system field")
}

/// Parse a name in a position that is NOT a field: a function name, a
/// stage name, a saved-query name. These take no backticks (ADR-0013
/// ruling 7) — `` `lower`(x) `` must not be a call — so this is the
/// unquoted production, `field_name()` minus the backtick arm.
pub(crate) fn plain_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    system_field().or(ident())
}

/// Parse a backtick-quoted name: `` `request id` ``.
///
/// Backticks change how a name is LEXED, never what a name may be
/// (ADR-0013 ruling 7). Content is any character except a backtick; a
/// doubled backtick escapes one. Empty names and names carrying a
/// character that cannot be rendered as itself
/// ([`is_unsafe_display_char`] — controls, bidi and zero-width format
/// characters, the soft hyphen) are parse errors — the only two
/// refusals, because this is lexing, not policy:
/// the ASCII fold ([`crate::schema::catalog_key`]) and the sealed `_`
/// namespace ([`crate::schema::is_reserved_name`]) apply to the result
/// exactly as they do to a bare name. A dot inside the quotes is a
/// literal character, not a nested-name segment.
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
                        "field name contains a control or invisible format character \
                         (U+{:04X}); such characters are not part of any column name",
                        bad as u32
                    ),
                ));
            }
            Ok(name)
        })
        .labelled("backtick-quoted field name")
}

/// Parse a field name — a backtick-quoted name, an `@`-prefixed system
/// field, or a regular identifier.
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

/// Parse a bare (unquoted) value in a field filter — stops at whitespace and `|`.
///
/// A backtick ENDS a bare value, the same exclusion the search stage's bare
/// word makes and for the same reason: it is the one character whose
/// meaning is decided before the grammar runs. The pre-parse comment
/// scanner engages its backtick state wherever a NAME could start, and an
/// operator inside a value looks exactly like that from outside
/// (`host=a+` is a value, `1+` is arithmetic), so a value that could
/// absorb a tick could absorb a shielded `#` with it — turning a comment
/// into part of the value SILENTLY. Ending the value here makes the stray
/// tick a parse error instead: loud, or correct, never quietly different.
///
/// This is the single bare-RHS production — single values, comma-separated
/// IN lists and glob detection all route through it — so the exclusion
/// covers every unquoted value shape at once. A value that genuinely
/// contains a backtick is written double-quoted (`` host="a`b" ``), which
/// the quoted arm has always accepted.
pub(crate) fn bare_value<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    any()
        .filter(|c: &char| {
            !c.is_ascii_whitespace()
                && *c != '|'
                && *c != ','
                && *c != ')'
                && *c != '('
                && *c != '`'
        })
        .repeated()
        .at_least(1)
        .to_slice()
        .map(String::from)
        .labelled("value")
}

/// Maximum regex pattern length to prevent compilation-based denial-of-service.
const MAX_REGEX_LEN: usize = 1024;

/// Parse a regex pattern delimited by `/`: `/pattern/`. Validates syntax.
pub(crate) fn regex_pattern<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    none_of("/")
        .repeated()
        .at_least(1)
        .collect::<String>()
        .delimited_by(just('/'), just('/'))
        .try_map(|pattern, span| {
            if pattern.len() > MAX_REGEX_LEN {
                return Err(Rich::custom(
                    span,
                    format!(
                        "regex pattern too long ({} chars, max {MAX_REGEX_LEN})",
                        pattern.len()
                    ),
                ));
            }
            regex::Regex::new(&pattern)
                .map_err(|e| Rich::custom(span, format!("invalid regex: {e}")))?;
            Ok(pattern)
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

    /// The token survives the parse verbatim, trailing zeros and all —
    /// `f64` is lossy past 53 bits and pin-aware comparison binds the TEXT
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

    /// Backticks change how a name is LEXED, never what a name may be:
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

    /// Empty and control-char names are parse errors (ruling 7) — an
    /// empty column name is unnameable and a control char would rewrite
    /// the terminal line that renders it.
    #[test]
    fn backtick_name_empty_and_control_chars_rejected() {
        assert!(field_name().parse("``").into_result().is_err());
        assert!(field_name().parse("`a\nb`").into_result().is_err());
        assert!(field_name().parse("`a\tb`").into_result().is_err());
        assert!(field_name().parse("`a\u{7f}b`").into_result().is_err());
        assert!(field_name().parse("`\u{1b}[2J`").into_result().is_err());
    }

    /// Function names, stage names and saved-query names are NOT fields,
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

        // "android" should NOT match as keyword "and"
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

    #[test]
    fn test_regex_pattern_at_length_limit() {
        let pattern = format!("/{}/", "a".repeat(1024));
        assert!(regex_pattern().parse(&pattern).into_result().is_ok());
    }
}
