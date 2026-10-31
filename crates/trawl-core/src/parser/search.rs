// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layers 2-3: search token and search stage parsers.
//!
//! Parses the implicit search stage before the first `|`:
//! time filters, quoted searches, field filters, and bare text search.

use chumsky::prelude::*;

use crate::ast::{
    FieldFilter, FilterOp, FilterValue, QuotedSearch, Scalar, SearchStage, SearchToken, Spanned,
    TextSearch, TimeClause, TimeFilter,
};
use crate::parser::comment::{self, Spaced};
use crate::parser::primitives::{
    ParserExtra, ParserInput, bare_value, duration, ends_bare_value, field_name, filter_op,
    keyword, quoted_string, regex_body_error, regex_shape, spanned,
};

/// Parse a `last=2h` time filter.
fn time_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    just("last=")
        .ignore_then(duration())
        .map(|d| SearchToken::TimeFilter(TimeFilter { duration: d }))
        .labelled("time filter")
}

/// Parse a `"quoted phrase"` search.
fn quoted_search<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    quoted_string()
        .map(|phrase| SearchToken::QuotedSearch(QuotedSearch { phrase }))
        .labelled("quoted search")
}

/// Detect whether a value contains glob characters (`*` or `?`).
///
/// The single glob predicate: it decides the value kind here, and
/// [`crate::parser::comment::hint_for`] asks it before promising a quoted
/// rewrite matches "exactly" — a second spelling of the rule could make a
/// hint contradict the parser that answers it.
pub(crate) fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// The message an ordered operator with a glob value carries (ADR-0060).
/// A payload: the hint quotes the glob, which only its production holds.
pub(crate) const MSG_ORDERED_GLOB: &str = "glob patterns only support `=` and `!=`";

/// The message an ordered operator with a regex value carries (ADR-0060).
pub(crate) const MSG_ORDERED_REGEX: &str = "regex patterns only support `=` and `!=`";

/// The message a recognised regex followed by `"` carries (ADR-0060).
pub(crate) const MSG_QUOTE_AFTER_REGEX: &str =
    "a regex ends at its closing '/'; '\"' cannot follow it";

/// A parsed filter value, with what the field filter's diagnostics need
/// to know about it. Parser-local: none of it reaches the AST.
#[derive(Clone)]
struct ParsedValue {
    value: FilterValue,
    /// Whether an element's own production already emitted an error: the
    /// ADR-0014 opener, a regex body that does not compile, or a `"`
    /// after a regex. Such a value gets no operator error on top: one
    /// error per value, and the first one names what the user has to
    /// change.
    reported: bool,
}

/// Parse the value side of a field filter: one element, or a comma list
/// of them.
///
/// Each element carries its own kind (ADR-0060): a recognised `/body/` is
/// a regex, a quoted value is exact, and a bare value is a glob when it
/// holds `*` or `?` and exact otherwise. So `f=a,b*,/c/` is an exact, a
/// glob and a regex. The comparison operator is parsed separately by
/// [`field_filter`]; nothing here reads it.
fn filter_value<'src>()
-> impl Parser<'src, ParserInput<'src>, ParsedValue, ParserExtra<'src>> + Clone {
    // A regex is a `/body/` followed by a byte that ends a bare value, or
    // by the end (ADR-0060). The shape is read first, then the next byte,
    // then the body, so a candidate the next byte makes a bare value is
    // never validated (`/tmp/s`, `/[a/]/`). Any other next byte fails
    // this arm without consuming, and the bare arm reads the whole run:
    // `/api/*` is a glob, and `/x/#` reaches the opener error. Tried
    // before the list splits, so a body may hold a `,`.
    let ends_regex = any()
        .filter(|c: &char| ends_bare_value(*c))
        .rewind()
        .ignored()
        .or(end());
    // A `"` ends nothing, and a complete `/x/` never silently becomes
    // text: it is an error, and the rest of the run goes with it so the
    // value reports nothing else.
    let quote_after = just('"')
        .then(any().filter(|c: &char| !ends_bare_value(*c)).repeated())
        .ignored();
    let regex = regex_shape()
        .map_with(|body, e| (body, e.span()))
        .then(choice((ends_regex.to(false), quote_after.to(true))))
        .validate(|((body, span), quote_after), _, emitter| {
            let span: SimpleSpan = span;
            // An error is emitted, never returned: a returned one would
            // let the bare arm read the same text as a literal.
            let reported = if quote_after {
                emitter.emit(Rich::custom(
                    (span.start..span.end + '"'.len_utf8()).into(),
                    MSG_QUOTE_AFTER_REGEX,
                ));
                true
            } else if let Some(message) = regex_body_error(&body) {
                emitter.emit(Rich::custom(span, message));
                true
            } else {
                false
            };
            (Scalar::Regex(body), reported)
        });

    // A quoted value (`service="Activity Monitor"`, quotes stripped) is
    // never a pattern: the wildcards in `host="a*b"` are data, while the
    // ones in `host=a*b` are the pattern.
    //
    // Both spellings are admitted in every position of a comma list, not
    // just alone, so `format` can quote a list element that would
    // otherwise re-lex as something else — a `#` in one is a parse error
    // bare (ADR-0014 ruling 2), and the round trip has to survive it.
    let quoted = quoted_string().map(|s| (Scalar::Exact(s), false));
    let bare = bare_value().map(|s| {
        // `bare_value` emitted the opener error exactly when this holds.
        let reported = comment::first_opener(&s).is_some();
        let scalar = if has_glob_chars(&s) {
            Scalar::Glob(s)
        } else {
            Scalar::Exact(s)
        };
        (scalar, reported)
    });

    choice((regex, quoted, bare))
        .separated_by(just(','))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|elements| {
            let reported = elements.iter().any(|(_, reported)| *reported);
            let mut scalars: Vec<Scalar> = elements.into_iter().map(|(s, _)| s).collect();
            let value = if scalars.len() == 1 {
                FilterValue::Single(scalars.pop().expect("one element"))
            } else {
                FilterValue::List(scalars)
            };
            ParsedValue { value, reported }
        })
        .labelled("filter value")
}

/// Parse a `field=value` filter, including `field>=100`, `field!=200`,
/// `field=200,301,404`, `field=/pattern/`, and glob auto-detection.
///
/// The operator (`=`, `>=`, `<=`, `!=`, `>`, `<`) is parsed as part of the
/// field filter — no `:` separator. Uses `.rewind()` lookahead on
/// `field_name + operator` so that bare words like `NOT` or `error` don't
/// commit the parser.
///
/// The operator is kept as typed (ADR-0060). An ordered operator takes
/// neither a list nor a pattern, and either is an emitted error.
fn field_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    field_name()
        .then(filter_op())
        .rewind()
        .ignore_then(field_name())
        .then(filter_op().map_with(|op, e| (op, e.span())))
        .then(filter_value().map_with(|value, e| (value, e.span())))
        .validate(
            |((field, (op, op_span)), (parsed, value_span)), extra, emitter| {
                let op_span: SimpleSpan = op_span;
                let value_span: SimpleSpan = value_span;
                let ordered = !matches!(op, FilterOp::Eq | FilterOp::Ne);
                // Emit while still returning the parsed token. A returned
                // error would let the leaf choice recover as a bare text
                // search for the whole `f>=a,b` spelling, or for `f>a*`.
                match &parsed.value {
                    _ if !ordered || parsed.reported => {}
                    FilterValue::List(_) => emitter.emit(Rich::custom(
                        extra.span(),
                        "comma-separated lists only support `=` and `!=`",
                    )),
                    FilterValue::Single(Scalar::Glob(glob)) => emitter.emit(Rich::custom(
                        (op_span.start..value_span.end).into(),
                        comment::payload(MSG_ORDERED_GLOB, glob),
                    )),
                    FilterValue::Single(Scalar::Regex(_)) => emitter.emit(Rich::custom(
                        (op_span.start..value_span.end).into(),
                        MSG_ORDERED_REGEX,
                    )),
                    FilterValue::Single(Scalar::Exact(_)) => {}
                }
                SearchToken::FieldFilter(FieldFilter {
                    field,
                    op,
                    value: parsed.value,
                })
            },
        )
        .labelled("field filter")
}

/// Emit the comment-opener diagnostics for a search-stage bare term
/// (ADR-0014).
///
/// A `#` anywhere in the term is an error: the opener is never data and
/// never a comment inside a token. A term starting with `//` is an error
/// too — a comment line in that spelling must fail rather than silently
/// become AND-ed text terms that narrow the match set to nothing. Under
/// `-` neither rule holds as written: nothing negates a comment, so
/// `-//cdn.example.com` stays an ordinary negated term, and the `#`
/// message is the negated one because quoting is no escape there
/// (`-"a#b"` is a bare term spelling literal quote characters, while
/// `NOT "a#b"` is the working form the hint offers).
///
/// The term is still produced and the diagnostics are emitted, so the
/// branch succeeds structurally: chumsky ranks alternatives by how far
/// they got, and a returned error here would lose to a worse one from a
/// later arm. `into_result()` is still `Err`.
fn check_term<'src>(
    p: impl Parser<'src, ParserInput<'src>, &'src str, ParserExtra<'src>> + Clone,
    negated: bool,
) -> impl Parser<'src, ParserInput<'src>, &'src str, ParserExtra<'src>> + Clone {
    p.validate(move |s: &str, extra, emitter| {
        let span: SimpleSpan = extra.span();
        let start = span.start;
        let opener_msg = if negated {
            comment::MSG_OPENER_IN_NEGATED_TERM
        } else {
            comment::MSG_OPENER_IN_TOKEN
        };
        if !negated && comment::starts_with_slashes(s) {
            emitter.emit(Rich::custom(
                (start..start + comment::SLASHES.len()).into(),
                comment::MSG_SLASHES_NOT_A_COMMENT,
            ));
        } else if let Some(at) = comment::first_opener(s) {
            emitter.emit(Rich::custom(
                (start + at..start + at + comment::OPENER.len_utf8()).into(),
                // The term rides along with the message: this production
                // holds the exact slice the grammar refused, so the hint
                // never has to re-derive one from the raw text.
                comment::payload(opener_msg, s),
            ));
        }
        s
    })
}

/// Parse a bare text search term, optionally negated with `-`.
///
/// A backtick is a metacharacter here, not a word byte: it opens a quoted
/// field name (ADR-0013 ruling 7), and text search is the last alternative
/// in the leaf choice. If a term could absorb one, a quoted name the
/// grammar refuses — or one whose filter never reaches an operator — would
/// not error at all: no alternative would match, and the query would
/// silently become a substring search for the literal ticks
/// (`` service=`my service` `` searching the whole line). So it ends the
/// term in every position, leading or mid-word, negated as much as bare —
/// the same exclusion [`crate::parser::primitives::bare_value`] makes on the
/// value side. The deliberate cost: a bare word carrying a tick must be
/// double-quoted (`` "a`b" ``).
///
/// A comment opener inside the term, and a term opening with `//`, are
/// parse errors ([`check_term`], ADR-0014).
fn text_search<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    let negated = just('-')
        .ignore_then(check_term(
            any()
                .filter(|c: &char| {
                    !c.is_ascii_whitespace() && *c != '|' && *c != ')' && *c != '(' && *c != '`'
                })
                .repeated()
                .at_least(1)
                .to_slice(),
            true,
        ))
        .map(|term: &str| {
            SearchToken::TextSearch(TextSearch {
                term: term.to_string(),
                negated: true,
            })
        });

    let positive = check_term(
        any()
            .filter(|c: &char| {
                !c.is_ascii_whitespace()
                    && *c != '|'
                    && *c != '"'
                    && *c != ')'
                    && *c != '('
                    && *c != '`'
            })
            .repeated()
            .at_least(1)
            .to_slice(),
        false,
    )
    .map(|s: &str| {
        SearchToken::TextSearch(TextSearch {
            term: s.to_string(),
            negated: false,
        })
    });

    // Excluding the byte from both terms is not enough for the `-` case:
    // it would leave `-` matching as a positive term of its own, so
    // `` -`http-status`=500 `` would parse as a text search for `-` beside
    // an un-negated field filter — the negation silently gone, which is
    // worse than the term that matched nothing. Refusing the pair ahead of
    // both arms is what makes a negated quoted name loud.
    negated
        .or(positive)
        .and_is(just('-').then(just('`')).not())
        .labelled("text search")
}

/// Parse `earliest="2026-03-14T03:00:00Z"` absolute time bound.
fn earliest_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    just("earliest=")
        .ignore_then(quoted_string())
        .map(SearchToken::EarliestFilter)
        .labelled("earliest filter")
}

/// Parse `latest="2026-03-14T03:15:00Z"` absolute time bound.
fn latest_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    just("latest=")
        .ignore_then(quoted_string())
        .map(SearchToken::LatestFilter)
        .labelled("latest filter")
}

/// Parse a single search token, including NOT prefix and parenthesized groups.
fn search_token<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    recursive(|token| {
        // NOT followed by another token (or group) = negation.
        // Use rewind-based lookahead: NOT must be followed by something
        // that's a valid token start, otherwise treat "NOT" as text search.
        let not_token = keyword("NOT")
            .then(
                // Keeps "NOT |" and a trailing "NOT" out of the negation arm.
                any()
                    .filter(|c: &char| {
                        c.is_alphanumeric()
                            || *c == '_'
                            || *c == '"'
                            || *c == '-'
                            || *c == '('
                            // a backtick opens a quoted field name
                            || *c == '`'
                    })
                    .rewind()
                    .spaced(),
            )
            .ignore_then(spanned(token.clone()))
            .map(|inner| SearchToken::Not(Box::new(inner)));

        // Parenthesized group: (tokens OR tokens)
        let or_marker = keyword("OR")
            .or(keyword("or"))
            .to(TokenOrSep::<Spanned<SearchToken>>::Or);
        let paren_token = spanned(token).map(TokenOrSep::Token);
        let paren_group = choice((or_marker, paren_token))
            .spaced()
            .repeated()
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just('(').spaced(), just(')').spaced())
            .map(|items| {
                let mut groups: Vec<Vec<_>> = vec![vec![]];
                for item in items {
                    match item {
                        TokenOrSep::Or => groups.push(vec![]),
                        TokenOrSep::Token(t) => {
                            groups.last_mut().expect("groups always non-empty").push(t);
                        }
                    }
                }
                groups.retain(|g| !g.is_empty());
                SearchToken::Group(groups)
            });

        // Leaf tokens (non-recursive)
        let leaf = choice((
            time_filter(),
            earliest_filter(),
            latest_filter(),
            quoted_search(),
            field_filter(),
            text_search(),
        ));

        choice((not_token, paren_group, leaf)).labelled("search token")
    })
}

/// Intermediate enum for parsing OR-separated groups.
#[derive(Clone)]
enum TokenOrSep<T> {
    Token(T),
    Or,
}

/// Recursively extract time filters from a token list, hoisting them globally.
///
/// Every clause hoisted is also recorded in `hoisted` with its span, since
/// the three slots keep only the last of each kind.
fn hoist_time_filters(
    tokens: &mut Vec<Spanned<SearchToken>>,
    time_filter: &mut Option<Spanned<TimeFilter>>,
    earliest: &mut Option<Spanned<String>>,
    latest: &mut Option<Spanned<String>>,
    hoisted: &mut Vec<Spanned<TimeClause>>,
) {
    // First, recurse into Group and Not tokens to hoist from nested structures.
    for token in tokens.iter_mut() {
        match &mut token.node {
            SearchToken::Group(groups) => {
                for group in groups.iter_mut() {
                    hoist_time_filters(group, time_filter, earliest, latest, hoisted);
                }
                groups.retain(|g| !g.is_empty());
            }
            SearchToken::Not(inner) => {
                // If NOT wraps a time filter, hoist it (time is always global).
                match &inner.node {
                    SearchToken::TimeFilter(tf) => {
                        *time_filter = Some(Spanned::new(tf.clone(), inner.span.clone()));
                        hoisted.push(Spanned::new(TimeClause::Last, inner.span.clone()));
                    }
                    SearchToken::EarliestFilter(ts) => {
                        *earliest = Some(Spanned::new(ts.clone(), inner.span.clone()));
                        hoisted.push(Spanned::new(TimeClause::Earliest, inner.span.clone()));
                    }
                    SearchToken::LatestFilter(ts) => {
                        *latest = Some(Spanned::new(ts.clone(), inner.span.clone()));
                        hoisted.push(Spanned::new(TimeClause::Latest, inner.span.clone()));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Now remove hoistable tokens from the flat list.
    tokens.retain(|t| match &t.node {
        SearchToken::TimeFilter(tf) => {
            *time_filter = Some(Spanned::new(tf.clone(), t.span.clone()));
            hoisted.push(Spanned::new(TimeClause::Last, t.span.clone()));
            false
        }
        SearchToken::EarliestFilter(ts) => {
            *earliest = Some(Spanned::new(ts.clone(), t.span.clone()));
            hoisted.push(Spanned::new(TimeClause::Earliest, t.span.clone()));
            false
        }
        SearchToken::LatestFilter(ts) => {
            *latest = Some(Spanned::new(ts.clone(), t.span.clone()));
            hoisted.push(Spanned::new(TimeClause::Latest, t.span.clone()));
            false
        }
        _ => true,
    });
}

/// Parse the full search stage: OR-separated groups of AND-joined tokens.
///
/// `a b OR c d` → groups: `[[a, b], [c, d]]`
/// Implicit AND (whitespace) binds tighter than explicit OR.
pub(crate) fn search_stage<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchStage, ParserExtra<'src>> + Clone {
    // OR keyword must be tried BEFORE text_search() to prevent "OR"
    // being consumed as a bare text search term.
    let or_marker = keyword("OR").or(keyword("or")).to(TokenOrSep::Or);

    let token = spanned(search_token()).map(TokenOrSep::Token);

    choice((or_marker, token))
        .spaced()
        .repeated()
        .collect::<Vec<_>>()
        .map(|items| {
            let mut groups: Vec<Vec<_>> = vec![vec![]];
            for item in items {
                match item {
                    TokenOrSep::Or => groups.push(vec![]),
                    TokenOrSep::Token(t) => {
                        groups.last_mut().expect("groups always non-empty").push(t);
                    }
                }
            }
            // Remove empty groups (trailing OR, leading OR, double OR).
            groups.retain(|g| !g.is_empty());

            // Hoist time filters out of groups — they apply globally.
            // If multiple tokens of the same kind appear, last one wins.
            let mut time_filter = None;
            let mut earliest = None;
            let mut latest = None;
            let mut hoisted = Vec::new();
            for group in &mut groups {
                hoist_time_filters(
                    group,
                    &mut time_filter,
                    &mut earliest,
                    &mut latest,
                    &mut hoisted,
                );
            }
            // Remove groups that became empty after hoisting.
            groups.retain(|g| !g.is_empty());

            SearchStage {
                groups,
                time_filter,
                earliest,
                latest,
                hoisted,
            }
        })
        .labelled("search stage")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{TimeUnit, TrawlDuration};

    #[test]
    fn test_time_filter() {
        let result = search_stage().parse("last=2h").into_result().unwrap();
        // Time filter is hoisted out of groups.
        assert!(result.groups.is_empty());
        let tf = result.time_filter.expect("time_filter should be hoisted");
        assert_eq!(
            tf.node,
            TimeFilter {
                duration: TrawlDuration {
                    quantity: 2,
                    unit: TimeUnit::Hours,
                },
            }
        );
    }

    #[test]
    fn test_quoted_search() {
        let result = search_stage()
            .parse(r#""connection refused""#)
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "connection refused".to_string(),
            })
        );
    }

    #[test]
    fn test_field_filter_simple() {
        let result = search_stage().parse("service=nginx").into_result().unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "service".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("nginx".to_string())),
            })
        );
    }

    #[test]
    fn test_field_filter_with_op() {
        let result = search_stage().parse("status>=400").into_result().unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "status".to_string(),
                op: FilterOp::Gte,
                value: FilterValue::Single(Scalar::Exact("400".to_string())),
            })
        );
    }

    #[test]
    fn test_field_filter_ne() {
        let result = search_stage()
            .parse("service!=kernel")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "service".to_string(),
                op: FilterOp::Ne,
                value: FilterValue::Single(Scalar::Exact("kernel".to_string())),
            })
        );
    }

    #[test]
    fn test_field_filter_list() {
        let result = search_stage()
            .parse("status=200,301,404")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "status".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::List(vec![
                    Scalar::Exact("200".to_string()),
                    Scalar::Exact("301".to_string()),
                    Scalar::Exact("404".to_string()),
                ]),
            })
        );
    }

    #[test]
    fn ordered_field_filter_lists_name_the_supported_operators() {
        for dsl in ["status>a,b", "status>=a,b", "status<a,b", "status<=a,b"] {
            let errors = crate::parser::parse(dsl).expect_err("ordered list must not parse");
            assert!(
                errors.iter().any(|error| {
                    error
                        .message
                        .contains("comma-separated lists only support `=` and `!=`")
                }),
                "{dsl}: {errors:?}"
            );
        }
    }

    fn only_filter(dsl: &str) -> FieldFilter {
        let query = crate::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
        match &query.search.groups[..] {
            [group] => match &group[..] {
                [token] => match &token.node {
                    SearchToken::FieldFilter(ff) => ff.clone(),
                    other => panic!("{dsl}: expected a field filter, got {other:?}"),
                },
                other => panic!("{dsl}: expected one token, got {other:?}"),
            },
            other => panic!("{dsl}: expected one group, got {other:?}"),
        }
    }

    fn filter(field: &str, op: FilterOp, value: FilterValue) -> FieldFilter {
        FieldFilter {
            field: field.to_string(),
            op,
            value,
        }
    }

    fn exact(s: &str) -> Scalar {
        Scalar::Exact(s.to_string())
    }

    fn glob(s: &str) -> Scalar {
        Scalar::Glob(s.to_string())
    }

    fn regex(s: &str) -> Scalar {
        Scalar::Regex(s.to_string())
    }

    /// The operator is what the author typed, whatever the value's kind
    /// (ADR-0060): `!=` with a pattern excludes it.
    #[test]
    fn ne_keeps_its_operator_for_a_pattern() {
        assert_eq!(
            only_filter("host!=web*"),
            filter("host", FilterOp::Ne, FilterValue::Single(glob("web*")))
        );
        assert_eq!(
            only_filter("host!=/x/"),
            filter("host", FilterOp::Ne, FilterValue::Single(regex("x")))
        );
        assert_eq!(
            only_filter("host!=*"),
            filter("host", FilterOp::Ne, FilterValue::Single(glob("*")))
        );
    }

    /// Each list element carries its own kind: a bare `*`/`?` makes a
    /// glob, a quoted element is exact, and a regex element is a regex.
    #[test]
    fn list_elements_carry_their_own_kind() {
        assert_eq!(
            only_filter("f=a,b*"),
            filter(
                "f",
                FilterOp::Eq,
                FilterValue::List(vec![exact("a"), glob("b*")])
            )
        );
        assert_eq!(
            only_filter("f!=a,b*,/c/"),
            filter(
                "f",
                FilterOp::Ne,
                FilterValue::List(vec![exact("a"), glob("b*"), regex("c")])
            )
        );
        assert_eq!(
            only_filter(r#"f="a*",b?"#),
            filter(
                "f",
                FilterOp::Eq,
                FilterValue::List(vec![exact("a*"), glob("b?")])
            )
        );
        // A regex body may hold a `,`: the regex is read before the list
        // splits.
        assert_eq!(
            only_filter("f=x,/a,b/"),
            filter(
                "f",
                FilterOp::Eq,
                FilterValue::List(vec![exact("x"), regex("a,b")])
            )
        );
    }

    /// The one error `dsl` produces, with the source text it marks.
    fn one_error(dsl: &str) -> (crate::parser::ParseError, &str) {
        let errors = crate::parser::parse(dsl).expect_err(dsl);
        assert_eq!(errors.len(), 1, "{dsl}: {errors:?}");
        let error = errors.into_iter().next().expect("one error");
        let marked = &dsl[error.span.clone()];
        (error, marked)
    }

    /// An ordered operator takes no pattern (ADR-0060). The error marks
    /// the operator through the end of the value, and its hint names
    /// `=`/`!=` — and, for a glob alone, quoting to compare text.
    #[test]
    fn an_ordered_operator_with_a_pattern_is_an_error() {
        for (dsl, marked, quoted) in [
            ("host<a*", "<a*", "a*"),
            ("host>=web-?", ">=web-?", "web-?"),
            ("host<=*", "<=*", "*"),
            // after a multibyte prefix, the span is still the bytes
            ("\"é\" host>a*b", ">a*b", "a*b"),
        ] {
            let (error, got) = one_error(dsl);
            assert_eq!(error.message, MSG_ORDERED_GLOB, "{dsl}");
            assert_eq!(got, marked, "{dsl}");
            assert_eq!(
                error.hint.as_deref(),
                Some(
                    format!(
                        "use `=` or `!=` to match the pattern, or quote the value \
                         (\"{quoted}\") to compare it as text"
                    )
                    .as_str()
                ),
                "{dsl}"
            );
        }

        for (dsl, marked) in [("host>/x/", ">/x/"), ("host<=/a.b/ c", "<=/a.b/")] {
            let (error, got) = one_error(dsl);
            assert_eq!(error.message, MSG_ORDERED_REGEX, "{dsl}");
            assert_eq!(got, marked, "{dsl}");
            assert_eq!(
                error.hint.as_deref(),
                Some("use `=` or `!=` to match the pattern"),
                "{dsl}"
            );
        }

        // A glob whose quoted spelling would not read back as the same
        // text earns no quoting half.
        let (error, _) = one_error(r#"host<a"b*"#);
        assert_eq!(error.message, MSG_ORDERED_GLOB);
        assert_eq!(
            error.hint.as_deref(),
            Some("use `=` or `!=` to match the pattern")
        );
    }

    /// A quoted value is never a pattern, so an ordered comparison of its
    /// text stays one.
    #[test]
    fn an_ordered_operator_with_a_quoted_pattern_shape_compares_text() {
        assert_eq!(
            only_filter(r#"host<"a*""#),
            filter("host", FilterOp::Lt, FilterValue::Single(exact("a*")))
        );
        assert_eq!(
            only_filter(r#"host>="/x/""#),
            filter("host", FilterOp::Gte, FilterValue::Single(exact("/x/")))
        );
    }

    /// One error per value: an ordered list keeps the list error alone,
    /// pattern elements or not, and a value whose own production already
    /// reported a `#` gets no operator error on top.
    #[test]
    fn one_error_per_value() {
        for dsl in ["f>a,b*", "f<=a*,/c/"] {
            let (error, _) = one_error(dsl);
            assert_eq!(
                error.message, "comma-separated lists only support `=` and `!=`",
                "{dsl}"
            );
        }
        for dsl in ["f>#a*", "f>#a,b", "f<=a,#b*"] {
            let (error, marked) = one_error(dsl);
            assert_eq!(error.message, comment::MSG_OPENER_IN_VALUE, "{dsl}");
            assert_eq!(marked, "#", "{dsl}");
        }
    }

    #[test]
    fn test_field_filter_glob() {
        let result = search_stage().parse("path=/api/*").into_result().unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "path".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Glob("/api/*".to_string())),
            })
        );
    }

    #[test]
    fn test_field_filter_regex() {
        let result = search_stage()
            .parse("message=/error.*/")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "message".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Regex("error.*".to_string())),
            })
        );
    }

    #[test]
    fn test_field_filter_quoted_value() {
        // service="kernel" should strip quotes.
        let result = search_stage()
            .parse(r#"service="kernel""#)
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "service".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("kernel".to_string())),
            })
        );
    }

    #[test]
    fn test_field_filter_quoted_value_with_spaces() {
        // service="Activity Monitor" — quotes allow spaces in field values.
        let result = search_stage()
            .parse(r#"service="Activity Monitor""#)
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "service".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("Activity Monitor".to_string())),
            })
        );
    }

    #[test]
    fn test_negated_text_search() {
        let result = search_stage().parse("-debug").into_result().unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

    #[test]
    fn test_multiple_tokens() {
        let result = search_stage()
            .parse("service=nginx _severity=error last=2h")
            .into_result()
            .unwrap();
        // Time filter hoisted, 2 tokens remain in group.
        assert_eq!(result.groups[0].len(), 2);
        assert!(result.time_filter.is_some());
    }

    // ── backtick escape (ADR-0013 ruling 7) ────────────────────────────

    /// `last=` stays an unconditional keyword and `` `last` ``=5 is the
    /// field — in the SAME query, so BOTH `field_filter` sites (the
    /// `.rewind()` lookahead and the committed parse) take the backtick
    /// production rather than backtracking into text search.
    #[test]
    fn backticked_keyword_is_a_field_filter_beside_the_time_filter() {
        let result = search_stage()
            .parse("`last`=5 last=2h")
            .into_result()
            .unwrap();
        let tf = result
            .time_filter
            .expect("last=2h is still the time filter");
        assert_eq!(tf.node.duration.quantity, 2);
        assert_eq!(tf.node.duration.unit, TimeUnit::Hours);
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "last".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("5".to_string())),
            })
        );
    }

    /// The other two members of the closed keyword set behave the same.
    #[test]
    fn backticked_earliest_and_latest_are_field_filters() {
        let result = search_stage()
            .parse(r#"`earliest`=x `latest`=y earliest="2026-01-01T00:00:00Z""#)
            .into_result()
            .unwrap();
        assert!(result.earliest.is_some());
        assert_eq!(result.groups[0].len(), 2);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "earliest".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("x".to_string())),
            })
        );
    }

    /// `NOT` must see a backtick as a token start, else it degrades to a
    /// bare text search for the word "NOT".
    #[test]
    fn not_negates_a_backticked_field_filter() {
        let result = search_stage()
            .parse("NOT `http-status`=500")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        match &result.groups[0][0].node {
            SearchToken::Not(inner) => assert_eq!(
                inner.node,
                SearchToken::FieldFilter(FieldFilter {
                    field: "http-status".to_string(),
                    op: FilterOp::Eq,
                    value: FilterValue::Single(Scalar::Exact("500".to_string())),
                })
            ),
            other => panic!("expected Not, got {other:?}"),
        }
    }

    /// A backtick ends a bare word and a bare value alike, in every
    /// position: no unquoted position may absorb the one character whose
    /// meaning is settled before the grammar runs. Without the refusal
    /// each shape below answers something quietly different — a substring
    /// search for the literal ticks, or a filter narrowed to half the
    /// value the user wrote beside a stray text term.
    #[test]
    fn no_unquoted_position_absorbs_a_backtick() {
        for dsl in [
            "er`ror",                // a bare word carrying a tick
            "service=`nginx`",       // the typo: ticks are not value quotes
            "service=`my service`",  // …which read as a filter plus a term
            "status=200,`a b` more", // …and as an IN-list element
            "host=a+`b` more",       // a tick after an operator in a value
            "-`http-status`=500",    // a negated quoted NAME, unspellable
            "-`a b`",                // …and one the whitespace split in two
        ] {
            let result = search_stage().then_ignore(end()).parse(dsl).into_result();
            assert!(
                result.is_err(),
                "{dsl} must be a loud parse error: {result:?}"
            );
        }
    }

    /// …and the double-quoted form is how a tick reaches a value or a
    /// term, carrying it verbatim.
    #[test]
    fn a_double_quoted_value_still_carries_a_backtick() {
        let result = search_stage()
            .then_ignore(end())
            .parse(r#"host="a`b""#)
            .into_result()
            .unwrap();
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "host".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("a`b".to_string())),
            })
        );

        let result = search_stage()
            .then_ignore(end())
            .parse(r#""er`ror""#)
            .into_result()
            .unwrap();
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::QuotedSearch(QuotedSearch {
                phrase: "er`ror".to_string(),
            })
        );
    }

    #[test]
    fn test_not_at_end_is_text_search() {
        // "NOT" at end of input (no following token) → text search.
        let result = search_stage().parse("NOT").into_result().unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "NOT".to_string(),
                negated: false,
            })
        );
    }

    #[test]
    fn test_not_negates_field_filter() {
        // "NOT _severity=error" → single NOT(FieldFilter) token.
        let result = search_stage()
            .parse("NOT _severity=error")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        match &result.groups[0][0].node {
            SearchToken::Not(inner) => {
                assert_eq!(
                    inner.node,
                    SearchToken::FieldFilter(FieldFilter {
                        field: "_severity".to_string(),
                        op: FilterOp::Eq,
                        value: FilterValue::Single(Scalar::Exact("error".to_string())),
                    })
                );
            }
            other => panic!("expected Not, got {other:?}"),
        }
    }

    #[test]
    fn test_not_parenthesized_group() {
        // "NOT (service=nginx OR service=apache) _severity=error"
        let result = search_stage()
            .parse("NOT (service=nginx OR service=apache) _severity=error")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 2);
        assert!(matches!(result.groups[0][0].node, SearchToken::Not(_)));
        assert!(matches!(
            result.groups[0][1].node,
            SearchToken::FieldFilter(_)
        ));
    }

    #[test]
    fn test_parenthesized_group() {
        // "(service=nginx OR service=apache)"
        let result = search_stage()
            .parse("(service=nginx OR service=apache)")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 1);
        match &result.groups[0][0].node {
            SearchToken::Group(groups) => {
                assert_eq!(groups.len(), 2);
            }
            other => panic!("expected Group, got {other:?}"),
        }
    }

    #[test]
    fn test_negated_with_field_filter() {
        let result = search_stage()
            .parse("-debug service=nginx")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 2);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "debug".to_string(),
                negated: true,
            })
        );
    }

    #[test]
    fn test_or_two_groups() {
        let result = search_stage()
            .parse("service=kernel OR service=trawld")
            .into_result()
            .unwrap();
        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(result.groups[1].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "service".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("kernel".to_string())),
            })
        );
        assert_eq!(
            result.groups[1][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "service".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Single(Scalar::Exact("trawld".to_string())),
            })
        );
    }

    #[test]
    fn test_or_implicit_and_binds_tighter() {
        // "a b OR c d" → [[a, b], [c, d]]
        let result = search_stage()
            .parse("service=nginx _severity=error OR service=postgres _severity=warn")
            .into_result()
            .unwrap();
        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.groups[0].len(), 2);
        assert_eq!(result.groups[1].len(), 2);
    }

    #[test]
    fn test_or_lowercase() {
        let result = search_stage()
            .parse("service=a or service=b")
            .into_result()
            .unwrap();
        assert_eq!(result.groups.len(), 2);
    }

    #[test]
    fn test_empty_query_groups() {
        let result = search_stage().parse("").into_result().unwrap();
        assert!(result.groups.is_empty());
        assert!(result.time_filter.is_none());
    }

    #[test]
    fn test_time_filter_hoisted_from_or_groups() {
        // `service=nginx last=2h OR service=postgres` — time filter applies globally.
        let result = search_stage()
            .parse("service=nginx last=2h OR service=postgres")
            .into_result()
            .unwrap();
        assert_eq!(result.groups.len(), 2);
        // Neither group should contain the time filter.
        for group in &result.groups {
            for token in group {
                assert!(
                    !matches!(token.node, SearchToken::TimeFilter(_)),
                    "time filter should be hoisted out of groups"
                );
            }
        }
        // Hoisted time filter should be present.
        let tf = result.time_filter.expect("time_filter should be hoisted");
        assert_eq!(tf.node.duration.quantity, 2);
        assert_eq!(tf.node.duration.unit, TimeUnit::Hours);
    }

    #[test]
    fn test_last_time_filter_wins() {
        // Multiple time filters — last one wins.
        let result = search_stage()
            .parse("last=1h service=nginx last=2h")
            .into_result()
            .unwrap();
        let tf = result.time_filter.expect("time_filter should be hoisted");
        assert_eq!(tf.node.duration.quantity, 2);
        assert_eq!(tf.node.duration.unit, TimeUnit::Hours);
        // Only the service token remains in the group.
        assert_eq!(result.groups[0].len(), 1);
    }

    /// Every hoisted clause is recorded with its own span, the ones the
    /// last of each kind displaced included, in hoisting order: a list's
    /// nested clauses before the ones beside them.
    #[test]
    fn every_hoisted_clause_is_recorded_with_its_span() {
        let input = r#"last=1h (a OR last=2h) NOT earliest="x" b latest="y" last=3h"#;
        let result = search_stage().parse(input).into_result().unwrap();
        let hoisted: Vec<(TimeClause, &str)> = result
            .hoisted
            .iter()
            .map(|c| (c.node, &input[c.span.clone()]))
            .collect();
        assert_eq!(
            hoisted,
            [
                (TimeClause::Last, "last=2h"),
                (TimeClause::Earliest, r#"earliest="x""#),
                (TimeClause::Last, "last=1h"),
                (TimeClause::Latest, r#"latest="y""#),
                (TimeClause::Last, "last=3h"),
            ]
        );
        let tf = result.time_filter.expect("time_filter should be hoisted");
        assert_eq!(&input[tf.span], "last=3h", "the last `last=` is the slot's");

        let plain = search_stage().parse("a b").into_result().unwrap();
        assert!(plain.hoisted.is_empty());
    }

    #[test]
    fn test_only_time_filter_produces_empty_groups() {
        // A query with only a time filter — groups become empty after hoisting.
        let result = search_stage().parse("last=5m").into_result().unwrap();
        assert!(result.groups.is_empty());
        assert!(result.time_filter.is_some());
    }
}
