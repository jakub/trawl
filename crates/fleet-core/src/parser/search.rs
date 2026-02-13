//! Layers 2-3: search token and search stage parsers.
//!
//! Parses the implicit search stage before the first `|`:
//! time filters, quoted searches, field filters, and bare text search.

use chumsky::prelude::*;

use crate::ast::{
    FieldFilter, FilterOp, FilterValue, QuotedSearch, SearchStage, SearchToken, TextSearch,
    TimeFilter,
};
use crate::parser::primitives::{
    ParserExtra, ParserInput, bare_value, duration, field_name, filter_op, keyword, quoted_string,
    regex_pattern, spanned,
};

/// Parse a `last:2h` time filter.
fn time_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    just("last:")
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
/// regex patterns, and glob auto-detection.
fn filter_value<'src>()
-> impl Parser<'src, ParserInput<'src>, (FilterOp, FilterValue), ParserExtra<'src>> + Clone {
    // try explicit operator first (>=, >, <=, <, !=)
    let with_op = filter_op()
        .then(bare_value())
        .map(|(op, val)| (op, FilterValue::Literal(val)));

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

    // quoted value: service:"Activity Monitor" → strips quotes
    let quoted_val = quoted_string().map(|s| (FilterOp::Eq, FilterValue::Literal(s)));

    // bare value(s), possibly comma-separated
    let bare_vals = bare_value()
        .separated_by(just(','))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|vals| {
            if vals.len() == 1 {
                let val = vals.into_iter().next().unwrap();
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

    choice((with_op, regex_val, quoted_val, bare_vals)).labelled("filter value")
}

/// Parse a `field:value` filter, including `field:>100`, `field:200,301,404`,
/// `field:/pattern/`, and glob auto-detection.
///
/// Uses `.rewind()` lookahead on `field_name:` so that bare words like `NOT`
/// don't commit the parser — if the colon is missing, `choice()` backtracks
/// to `text_search()` instead.
fn field_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    // lookahead: check ident+colon without consuming, so choice() can backtrack
    field_name()
        .then(just(':'))
        .rewind()
        .ignore_then(field_name())
        .then_ignore(just(':'))
        .then(filter_value())
        .map(|(field, (op, value))| SearchToken::FieldFilter(FieldFilter { field, op, value }))
        .labelled("field filter")
}

/// Parse a bare text search term, optionally negated with `-`.
fn text_search<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    let negated = just('-')
        .ignore_then(
            any()
                .filter(|c: &char| !c.is_ascii_whitespace() && *c != '|')
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
        .filter(|c: &char| !c.is_ascii_whitespace() && *c != '|' && *c != '"')
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

/// Parse a single search token.
fn search_token<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    choice((
        time_filter(),
        quoted_search(),
        field_filter(),
        text_search(),
    ))
    .labelled("search token")
}

/// Intermediate enum for parsing OR-separated groups.
#[derive(Clone)]
enum TokenOrSep<T> {
    Token(T),
    Or,
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
            SearchStage { groups }
        })
        .labelled("search stage")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{FleetDuration, TimeUnit};

    #[test]
    fn test_time_filter() {
        let result = search_stage().parse("last:2h").into_result().unwrap();
        assert_eq!(result.groups[0].len(), 1);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::TimeFilter(TimeFilter {
                duration: FleetDuration {
                    quantity: 2,
                    unit: TimeUnit::Hours,
                },
            })
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
        let result = search_stage().parse("service:nginx").into_result().unwrap();
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
        let result = search_stage().parse("status:>=400").into_result().unwrap();
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
    fn test_field_filter_list() {
        let result = search_stage()
            .parse("status:200,301,404")
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
        let result = search_stage().parse("path:/api/*").into_result().unwrap();
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
            .parse("message:/error.*/")
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
        // service:"kernel" should strip quotes.
        let result = search_stage()
            .parse(r#"service:"kernel""#)
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
        // service:"Activity Monitor" — quotes allow spaces in field values.
        let result = search_stage()
            .parse(r#"service:"Activity Monitor""#)
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
            .parse("service:nginx level:error last:2h")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 3);
    }

    #[test]
    fn test_bare_word_not_mistaken_for_field() {
        // "NOT" should parse as text search, not fail as field_filter.
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
    fn test_not_before_field_filter() {
        // "NOT level:error" — NOT is text search, level:error is field filter.
        let result = search_stage()
            .parse("NOT level:error")
            .into_result()
            .unwrap();
        assert_eq!(result.groups[0].len(), 2);
        assert_eq!(
            result.groups[0][0].node,
            SearchToken::TextSearch(TextSearch {
                term: "NOT".to_string(),
                negated: false,
            })
        );
        assert_eq!(
            result.groups[0][1].node,
            SearchToken::FieldFilter(FieldFilter {
                field: "level".to_string(),
                op: FilterOp::Eq,
                value: FilterValue::Literal("error".to_string()),
            })
        );
    }

    #[test]
    fn test_negated_with_field_filter() {
        let result = search_stage()
            .parse("-debug service:nginx")
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
            .parse("service:kernel OR service:fleetd")
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
                value: FilterValue::Literal("fleetd".to_string()),
            })
        );
    }

    #[test]
    fn test_or_implicit_and_binds_tighter() {
        // "a b OR c d" → [[a, b], [c, d]]
        let result = search_stage()
            .parse("service:nginx level:error OR service:postgres level:warn")
            .into_result()
            .unwrap();
        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.groups[0].len(), 2);
        assert_eq!(result.groups[1].len(), 2);
    }

    #[test]
    fn test_or_lowercase() {
        let result = search_stage()
            .parse("service:a or service:b")
            .into_result()
            .unwrap();
        assert_eq!(result.groups.len(), 2);
    }

    #[test]
    fn test_empty_query_groups() {
        let result = search_stage().parse("").into_result().unwrap();
        assert!(result.groups.is_empty());
    }
}
