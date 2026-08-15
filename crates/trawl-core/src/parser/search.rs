// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layers 2-3: search token and search stage parsers.
//!
//! Parses the implicit search stage before the first `|`:
//! time filters, quoted searches, field filters, and bare text search.

use chumsky::prelude::*;

use crate::ast::{
    FieldFilter, FilterOp, FilterValue, QuotedSearch, SearchStage, SearchToken, Spanned,
    TextSearch, TimeFilter,
};
use crate::parser::primitives::{
    ParserExtra, ParserInput, bare_value, duration, field_name, filter_op, keyword, quoted_string,
    regex_pattern, spanned,
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
fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Parse the value side of a field filter, handling comma-separated lists,
/// regex patterns, quoted strings, and glob auto-detection.
///
/// The comparison operator is parsed separately by [`field_filter`]; this
/// parser only handles the value portion. Returns an auto-detected
/// [`FilterOp`] that the caller uses to override when appropriate (e.g.
/// glob `*` or regex `/pattern/` detection).
fn filter_value<'src>()
-> impl Parser<'src, ParserInput<'src>, (FilterOp, FilterValue), ParserExtra<'src>> + Clone {
    // regex: /pattern/ — must be followed by whitespace, pipe, or end
    let regex_val = regex_pattern()
        .then_ignore(
            any()
                .filter(|c: &char| !c.is_ascii_whitespace() && *c != '|')
                .not()
                .rewind()
                .or(end().to(())),
        )
        .map(|pat| (FilterOp::Regex, FilterValue::Literal(pat)));

    // quoted value: service="Activity Monitor" → strips quotes
    let quoted_val = quoted_string().map(|s| (FilterOp::Eq, FilterValue::Literal(s)));

    // bare value(s), possibly comma-separated
    let bare_vals = bare_value()
        .separated_by(just(','))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|vals| {
            if vals.len() == 1 {
                let val = vals
                    .into_iter()
                    .next()
                    .expect("at_least(1) guarantees a value");
                let op = if has_glob_chars(&val) {
                    FilterOp::Glob
                } else {
                    FilterOp::Eq
                };
                (op, FilterValue::Literal(val))
            } else {
                (FilterOp::Eq, FilterValue::List(vals))
            }
        });

    choice((regex_val, quoted_val, bare_vals)).labelled("filter value")
}

/// Parse a `field=value` filter, including `field>=100`, `field!=200`,
/// `field=200,301,404`, `field=/pattern/`, and glob auto-detection.
///
/// The operator (`=`, `>=`, `<=`, `!=`, `>`, `<`) is parsed as part of the
/// field filter — no `:` separator. Uses `.rewind()` lookahead on
/// `field_name + operator` so that bare words like `NOT` or `error` don't
/// commit the parser.
fn field_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    // lookahead: check ident + operator without consuming
    field_name()
        .then(filter_op())
        .rewind()
        .ignore_then(field_name())
        .then(filter_op())
        .then(filter_value())
        .map(|((field, op), (auto_op, value))| {
            // auto_op overrides for glob/regex detection
            let final_op = if auto_op == FilterOp::Eq { op } else { auto_op };
            SearchToken::FieldFilter(FieldFilter {
                field,
                op: final_op,
                value,
            })
        })
        .labelled("field filter")
}

/// Parse a bare text search term, optionally negated with `-`.
fn text_search<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    let negated = just('-')
        .ignore_then(
            any()
                .filter(|c: &char| !c.is_ascii_whitespace() && *c != '|' && *c != ')' && *c != '(')
                .repeated()
                .at_least(1)
                .to_slice()
                .map(String::from),
        )
        .map(|term| {
            SearchToken::TextSearch(TextSearch {
                term,
                negated: true,
            })
        });

    let positive = any()
        .filter(|c: &char| {
            !c.is_ascii_whitespace() && *c != '|' && *c != '"' && *c != ')' && *c != '('
        })
        .repeated()
        .at_least(1)
        .to_slice()
        .map(|s: &str| {
            SearchToken::TextSearch(TextSearch {
                term: s.to_string(),
                negated: false,
            })
        });

    negated.or(positive).labelled("text search")
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
                // Peek ahead: next char after whitespace must be a valid token start.
                // This prevents "NOT |" or "NOT" at end from being parsed as negation.
                any()
                    .filter(|c: &char| {
                        c.is_alphanumeric() || *c == '_' || *c == '"' || *c == '-' || *c == '('
                    })
                    .rewind()
                    .padded(),
            )
            .ignore_then(spanned(token.clone()))
            .map(|inner| SearchToken::Not(Box::new(inner)));

        // Parenthesized group: (tokens OR tokens)
        let or_marker = keyword("OR")
            .or(keyword("or"))
            .to(TokenOrSep::<Spanned<SearchToken>>::Or);
        let paren_token = spanned(token).map(TokenOrSep::Token);
        let paren_group = choice((or_marker, paren_token))
            .padded()
            .repeated()
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just('(').padded(), just(')').padded())
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
fn hoist_time_filters(
    tokens: &mut Vec<Spanned<SearchToken>>,
    time_filter: &mut Option<Spanned<TimeFilter>>,
    earliest: &mut Option<Spanned<String>>,
    latest: &mut Option<Spanned<String>>,
) {
    // First, recurse into Group and Not tokens to hoist from nested structures.
    for token in tokens.iter_mut() {
        match &mut token.node {
            SearchToken::Group(groups) => {
                for group in groups.iter_mut() {
                    hoist_time_filters(group, time_filter, earliest, latest);
                }
                groups.retain(|g| !g.is_empty());
            }
            SearchToken::Not(inner) => {
                // If NOT wraps a time filter, hoist it (time is always global).
                match &inner.node {
                    SearchToken::TimeFilter(tf) => {
                        *time_filter = Some(Spanned::new(tf.clone(), inner.span.clone()));
                    }
                    SearchToken::EarliestFilter(ts) => {
                        *earliest = Some(Spanned::new(ts.clone(), inner.span.clone()));
                    }
                    SearchToken::LatestFilter(ts) => {
                        *latest = Some(Spanned::new(ts.clone(), inner.span.clone()));
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
            false
        }
        SearchToken::EarliestFilter(ts) => {
            *earliest = Some(Spanned::new(ts.clone(), t.span.clone()));
            false
        }
        SearchToken::LatestFilter(ts) => {
            *latest = Some(Spanned::new(ts.clone(), t.span.clone()));
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
        .padded()
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
            for group in &mut groups {
                hoist_time_filters(group, &mut time_filter, &mut earliest, &mut latest);
            }
            // Remove groups that became empty after hoisting.
            groups.retain(|g| !g.is_empty());

            SearchStage {
                groups,
                time_filter,
                earliest,
                latest,
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
                value: FilterValue::Literal("nginx".to_string()),
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
                value: FilterValue::Literal("400".to_string()),
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
                value: FilterValue::Literal("kernel".to_string()),
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
                    "200".to_string(),
                    "301".to_string(),
                    "404".to_string(),
                ]),
            })
        );
    }

    #[test]
    fn test_field_filter_glob() {
        let result = search_stage().parse("path=/api/*").into_result().unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "path".to_string(),
                op: FilterOp::Glob,
                value: FilterValue::Literal("/api/*".to_string()),
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
                op: FilterOp::Regex,
                value: FilterValue::Literal("error.*".to_string()),
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
                value: FilterValue::Literal("kernel".to_string()),
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
                value: FilterValue::Literal("Activity Monitor".to_string()),
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
                        value: FilterValue::Literal("error".to_string()),
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
                value: FilterValue::Literal("kernel".to_string()),
            })
        );
        assert_eq!(
            result.groups[1][0].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "service".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Literal("trawld".to_string()),
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

    #[test]
    fn test_only_time_filter_produces_empty_groups() {
        // A query with only a time filter — groups become empty after hoisting.
        let result = search_stage().parse("last=5m").into_result().unwrap();
        assert!(result.groups.is_empty());
        assert!(result.time_filter.is_some());
    }
}
