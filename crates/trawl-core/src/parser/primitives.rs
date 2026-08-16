// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layer 1: primitive parsers.
//!
//! Numbers, quoted strings, bare words, `@`-prefixed system fields,
//! filter operators, durations, and boolean/null keywords.

use chumsky::prelude::*;

use crate::ast::{FilterOp, FloatLiteral, LiteralValue, Spanned, TimeUnit, TrawlDuration};

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

/// Parse an unquoted name — either a regular identifier or an `@`-prefixed
/// system field.
///
/// The name shapes that are NOT fields go through this directly: a saved
/// query is not a column, so it takes no backtick escape (ADR-0013 ruling 7 —
/// quoting changes how a FIELD name is lexed, and nothing else).
pub(crate) fn plain_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    system_field().or(ident())
}

/// Parse a function name: `[A-Za-z_][A-Za-z0-9_]*`, and nothing more.
///
/// Split off [`field_name`] because a function is not a field. The backtick
/// escape exists so every COLUMN is reachable; sharing one production with
/// call position would make `` `lower`(x) `` a call, i.e. a second spelling
/// for a function name (ADR-0013 ruling 7: function names take no
/// backticks). Dots and `@` leave with it — no function has ever had either;
/// they were reachable only because the two productions were one.
pub(crate) fn function_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    any()
        .filter(|c: &char| c.is_ascii_alphabetic() || *c == '_')
        .then(
            any()
                .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_')
                .repeated(),
        )
        .to_slice()
        .map(String::from)
        .labelled("function name")
}

/// Parse a backtick-quoted field name — the ticks delimit, and a doubled
/// tick is one literal tick of name.
///
/// Content is any character but a backtick, with two refusals that are
/// errors rather than non-matches — past the opening tick there is no other
/// reading, so falling through would let the token degrade into a text
/// search (see [`crate::parser::search`]):
///
/// - the empty name, which names no column;
/// - any [`crate::sanitize::is_unsafe_display_char`]. Not merely
///   [`char::is_control`]: the characters that rewrite a rendering are the
///   bidi and zero-width FORMAT characters, and `sanitize`'s module doc
///   rests on the grammar being unable to express one (ADR-0013 ruling 7
///   widened here at prep — quoting changes how a name is LEXED, and a name
///   no terminal can print honestly is not a capability anyone loses).
pub(crate) fn backtick_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    // greedy and unambiguous: at a lone tick `just("``")` fails, `none_of`
    // fails, the repetition stops and the closing delimiter matches.
    choice((just("``").to('`'), none_of("`")))
        .repeated()
        .collect::<String>()
        .delimited_by(just('`'), just('`'))
        .try_map(|name: String, span| {
            if name.is_empty() {
                return Err(Rich::custom(
                    span,
                    "empty field name: `` names no column — put the field's \
                     name between the backticks",
                ));
            }
            if let Some(c) = name
                .chars()
                .find(|c| crate::sanitize::is_unsafe_display_char(*c))
            {
                return Err(Rich::custom(
                    span,
                    format!(
                        "field name contains control or invisible format \
                         characters (U+{:04X}) — a name that cannot be \
                         rendered honestly cannot be quoted either",
                        c as u32
                    ),
                ));
            }
            Ok(name)
        })
        .labelled("quoted field name")
}

/// Parse a field name — every position in the grammar that names a column.
///
/// The backtick arm is what makes the whole column vocabulary reachable
/// (ADR-0013 ruling 7): one production, so a name spells the same in every
/// position by construction. What a name may BE is unchanged — the fold
/// still happens downstream at [`crate::schema::catalog_key`], and
/// `is_reserved_name` still refuses write positions.
pub(crate) fn field_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    choice((backtick_name(), plain_name()))
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
/// A backtick ENDS a bare value, the same exclusion [`crate::parser::search`]
/// makes for text search and for the same reason: it is the one character
/// whose meaning is decided before the grammar runs. The pre-parse comment
/// scanner engages its backtick state wherever a NAME could start, and an
/// operator inside a value looks exactly like that from outside
/// (`host=a+` is a value, `1+` is arithmetic), so a value that could
/// absorb a tick could absorb a shielded `#` with it — turning a comment
/// into part of the value SILENTLY. Ending the value here makes the stray
/// tick a parse error instead: loud, or correct, never quietly different.
///
/// A value that genuinely contains a backtick is written double-quoted
/// (`` host="a+`b" ``), which the quoted arm has always accepted.
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

    /// The function production is deliberately narrower than the field one:
    /// no dots, no `@`, and (once backticks land) no quoting.
    #[test]
    fn test_function_name_is_narrower_than_field_name() {
        assert_eq!(
            function_name().parse("lower").into_result().unwrap(),
            "lower"
        );
        assert_eq!(function_name().parse("p95").into_result().unwrap(), "p95");
        assert_eq!(
            function_name().parse("_private").into_result().unwrap(),
            "_private"
        );

        // a dotted name is a FIELD shape — the function production stops at
        // the dot, so `end()` refuses the remainder.
        assert!(
            function_name()
                .then_ignore(end())
                .parse("host.name")
                .into_result()
                .is_err()
        );
        assert!(function_name().parse("@timestamp").into_result().is_err());
    }

    #[test]
    fn test_backtick_name() {
        assert_eq!(
            backtick_name().parse("`request id`").into_result().unwrap(),
            "request id"
        );
        // a doubled tick is one literal backtick of name; the greedy escape
        // leaves the closing delimiter unambiguous.
        assert_eq!(
            backtick_name().parse("`a``b`").into_result().unwrap(),
            "a`b"
        );
        // every byte but a backtick is name content — including the ones
        // that mean something everywhere else in the grammar.
        for (input, want) in [
            ("`a b`", "a b"),
            ("`a.b`", "a.b"),
            ("`last`", "last"),
            ("`WHERE`", "WHERE"),
            ("`a\"b`", "a\"b"),
            ("`#`", "#"),
            ("`日本語`", "日本語"),
        ] {
            assert_eq!(backtick_name().parse(input).into_result().unwrap(), want);
        }
        // the empty name names no column
        assert!(backtick_name().parse("``").into_result().is_err());
        // …and an unterminated one is an error, never a shorter name
        assert!(backtick_name().parse("`abc").into_result().is_err());
    }

    /// The hostile-character set has ONE owner
    /// ([`crate::sanitize::is_unsafe_display_char`]) and the quoted-name
    /// production is its query-side door. `sanitize`'s module doc asserts
    /// the grammar cannot express such a name; this is what keeps that
    /// true now that any column is nameable.
    #[test]
    fn the_grammar_admits_no_unrenderable_name() {
        for (name, group) in [
            ("a\u{1}b", "C0 control"),
            ("a\u{7f}b", "DEL"),
            ("a\u{9c}b", "C1 control"),
            ("a\u{202e}b", "RLO — the Trojan-Source lever"),
            ("a\u{2066}b", "first strong isolate"),
            ("a\u{061c}b", "arabic letter mark"),
            ("a\u{200b}b", "zero-width space"),
            ("a\u{feff}b", "zero-width no-break space"),
            ("a\u{00ad}b", "soft hyphen"),
        ] {
            let input = format!("`{name}`");
            assert!(
                backtick_name().parse(&input).into_result().is_err(),
                "{group} must not be nameable"
            );
        }
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
