//! Layer 1: primitive parsers.
//!
//! Numbers, quoted strings, bare words, `@`-prefixed system fields,
//! filter operators, durations, and boolean/null keywords.

use chumsky::prelude::*;

use crate::ast::{FilterOp, FleetDuration, LiteralValue, Spanned, TimeUnit};

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
        .from_str::<u64>()
        .unwrapped()
        .labelled("integer")
}

/// Parse a signed integer.
pub(crate) fn int<'src>() -> impl Parser<'src, ParserInput<'src>, i64, ParserExtra<'src>> + Clone {
    just('-')
        .or_not()
        .then(text::int(10).to_slice())
        .to_slice()
        .from_str::<i64>()
        .unwrapped()
        .labelled("integer")
}

/// Parse a float (must contain a `.` to distinguish from int).
pub(crate) fn float<'src>() -> impl Parser<'src, ParserInput<'src>, f64, ParserExtra<'src>> + Clone
{
    just('-')
        .or_not()
        .then(text::int(10))
        .then(just('.').then(text::digits(10)))
        .to_slice()
        .from_str::<f64>()
        .unwrapped()
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

/// Parse a field name — either a regular identifier or an `@`-prefixed system field.
pub(crate) fn field_name<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    system_field().or(ident())
}

// ---------------------------------------------------------------------------
// filter operators (for field:op value syntax)
// ---------------------------------------------------------------------------

/// Parse a filter operator, ordered longest-first to avoid prefix ambiguity.
pub(crate) fn filter_op<'src>()
-> impl Parser<'src, ParserInput<'src>, FilterOp, ParserExtra<'src>> + Clone {
    choice((
        just(">=").to(FilterOp::Gte),
        just(">").to(FilterOp::Gt),
        just("<=").to(FilterOp::Lte),
        just("<").to(FilterOp::Lt),
        just("!=").to(FilterOp::Ne),
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
pub(crate) fn duration<'src>()
-> impl Parser<'src, ParserInput<'src>, FleetDuration, ParserExtra<'src>> + Clone {
    uint()
        .then(time_unit())
        .map(|(quantity, unit)| FleetDuration { quantity, unit })
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
// bare filter values (for search stage field:value)
// ---------------------------------------------------------------------------

/// Parse a bare (unquoted) value in a field filter — stops at whitespace and `|`.
pub(crate) fn bare_value<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    any()
        .filter(|c: &char| !c.is_ascii_whitespace() && *c != '|' && *c != ',')
        .repeated()
        .at_least(1)
        .to_slice()
        .map(String::from)
        .labelled("value")
}

/// Parse a regex pattern delimited by `/`: `/pattern/`.
pub(crate) fn regex_pattern<'src>()
-> impl Parser<'src, ParserInput<'src>, String, ParserExtra<'src>> + Clone {
    none_of("/")
        .repeated()
        .at_least(1)
        .collect::<String>()
        .delimited_by(just('/'), just('/'))
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
        assert!((v - 3.25).abs() < f64::EPSILON);
        let v = float().parse("-0.5").into_result().unwrap();
        assert!((v - -0.5).abs() < f64::EPSILON);
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
    fn test_filter_op() {
        assert_eq!(
            filter_op().parse(">=").into_result().unwrap(),
            FilterOp::Gte
        );
        assert_eq!(filter_op().parse(">").into_result().unwrap(), FilterOp::Gt);
        assert_eq!(
            filter_op().parse("<=").into_result().unwrap(),
            FilterOp::Lte
        );
        assert_eq!(filter_op().parse("<").into_result().unwrap(), FilterOp::Lt);
        assert_eq!(filter_op().parse("!=").into_result().unwrap(), FilterOp::Ne);
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
}
