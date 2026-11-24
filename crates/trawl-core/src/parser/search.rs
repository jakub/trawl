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
    TextSearch, TimeBound, TimeClause, TimeFilter,
};
use crate::parser::comment::{self, Spaced};
use crate::parser::primitives::{
    MSG_REGEX_LINE_BREAK, ParserExtra, ParserInput, RegexCut, bare_value, check_pattern,
    check_regex_body, duration, ends_bare_value, field_name, filter_op, keyword, quoted_string,
    quoted_string_reported, regex_shape, slash_run, spanned, value_cut,
};
use crate::pattern::PatternKind;

fn time_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    just("last=")
        .ignore_then(duration())
        .map(|d| SearchToken::TimeFilter(TimeFilter { duration: d }))
        .labelled("time filter")
}

fn quoted_search<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    quoted_string()
        .map(|phrase| SearchToken::QuotedSearch(QuotedSearch { phrase }))
        .labelled("quoted search")
}

/// The single glob predicate: it decides the value kind here, and
/// [`crate::parser::comment::hint_for`] asks it before promising a quoted
/// rewrite matches "exactly" — a second spelling of the rule could make a
/// hint contradict the parser that answers it.
pub(crate) fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

pub(crate) const MSG_ORDERED_GLOB: &str = "glob patterns only support `=` and `!=`";

pub(crate) const MSG_ORDERED_REGEX: &str = "regex patterns only support `=` and `!=`";

pub(crate) const MSG_QUOTE_AFTER_REGEX: &str =
    "a regex ends at its closing '/'; '\"' cannot follow it";

pub(crate) const MSG_NESTED_TIME_CLAUSE: &str =
    "a time clause cannot sit inside parentheses or under NOT";

pub(crate) const MSG_UNREADABLE_BOUND: &str = "cannot read this time bound as a timestamp";

pub(crate) const MSG_LAST_WITH_BOUND: &str = "cannot combine 'last=' with 'earliest='/'latest='";

pub(crate) const MSG_OR_ONLY_GROUP: &str = "a group needs a search term or filter, not only OR";

pub(crate) const MSG_NOT_DASH_BEFORE_BACKTICK: &str =
    "'NOT-' cannot sit directly before a backticked name";

#[derive(Clone)]
struct ParsedValue {
    value: FilterValue,
    /// Whether an element's own production already emitted an error. A
    /// pattern that fails to build is refused after the parse
    /// (`crate::parser::parse`), so it is not counted here. Such a value
    /// gets no operator error on top: one error per value.
    reported: bool,
}

/// Parse the value side of a field filter: one element, or a comma list
/// of them. Each element carries its own kind (ADR-0060).
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
    // The bytes after which the rule before ADR-0061 ruling 1 recognised
    // a `/…/` run: the ones that end a regex, and the `"` it refused.
    let recognised_before = any()
        .filter(|c: &char| ends_bare_value(*c) || *c == '"')
        .rewind()
        .ignored()
        .or(end());
    let line_break = slash_run()
        .then_ignore(recognised_before)
        .filter(|body: &&str| value_cut(body) == Some(RegexCut::LineBreak))
        .validate(|body: &str, extra, emitter| {
            emitter.emit(Rich::custom(extra.span(), MSG_REGEX_LINE_BREAK));
            (Scalar::Regex(body.to_string()), true)
        });
    // Zero-width: it consumes nothing and never fails. `validate`, not
    // `map_with`, because chumsky skips a map in a position whose output
    // is unused, and this one's is.
    let pipe_cut_run = slash_run()
        .then_ignore(recognised_before)
        .filter(|body: &&str| value_cut(body) == Some(RegexCut::PipeStage))
        .validate(|_, extra, _| {
            let span: SimpleSpan = extra.span();
            extra.state().pipe_cut_runs.push(span.start..span.end);
        })
        .rewind()
        .or_not()
        .ignored();
    // A `"` ends nothing, and a complete `/x/` never silently becomes
    // text: it is an error, and the rest of the run goes with it so the
    // value reports nothing else.
    let quote_after = just('"')
        .then(any().filter(|c: &char| !ends_bare_value(*c)).repeated())
        .ignored();
    let regex = regex_shape()
        .map_with(|body, e| (body, e.span()))
        .then(choice((ends_regex.to(false), quote_after.to(true))))
        .validate(|((body, span), quote_after), extra, emitter| {
            let span: SimpleSpan = span;
            // An error is emitted, never returned: a returned one would
            // let the bare arm read the same text as a literal.
            let reported = if quote_after {
                emitter.emit(Rich::custom(
                    (span.start..span.end + '"'.len_utf8()).into(),
                    MSG_QUOTE_AFTER_REGEX,
                ));
                true
            } else {
                check_regex_body(&body, span, extra.state(), emitter)
            };
            (Scalar::Regex(body), reported)
        });

    // Both spellings are admitted in every position of a comma list, not
    // just alone, so `format` can quote a list element that would
    // otherwise re-lex as something else — a `#` in one is a parse error
    // bare (ADR-0014 ruling 2), and the round trip has to survive it.
    let quoted = quoted_string_reported().map(|(s, reported)| (Scalar::Exact(s), reported));
    let bare = pipe_cut_run
        .ignore_then(bare_value())
        .validate(|s, extra, emitter| {
            // `bare_value` emitted its open-quote or opener error exactly
            // when this holds.
            let reported = s.starts_with('"') || comment::first_opener(&s).is_some();
            if !has_glob_chars(&s) {
                return (Scalar::Exact(s), reported);
            }
            let reported = reported
                || check_pattern(PatternKind::Glob, &s, extra.span(), extra.state(), emitter);
            (Scalar::Glob(s), reported)
        });

    choice((regex, line_break, quoted, bare))
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

/// Uses `.rewind()` lookahead on `field_name + operator` so that bare
/// words like `NOT` or `error` don't commit the parser.
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

/// A backtick ends the term in every position, leading or mid-word,
/// negated as much as bare: it opens a quoted field name, and if a term
/// could absorb one, a quoted name the grammar refuses would not error
/// at all — the query would silently become a substring search for the
/// literal ticks. The deliberate cost: a bare word carrying a tick must
/// be double-quoted (`` "a`b" ``).
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
    .then(just('`').rewind().or_not())
    .validate(|(s, tick): (&str, Option<char>), extra, emitter| {
        if is_not_dash(s) && tick.is_some() {
            emitter.emit(Rich::custom(extra.span(), MSG_NOT_DASH_BEFORE_BACKTICK));
        }
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

/// Whether a bare term is `NOT`, then any layout, then `-`: the word that
/// cannot sit directly before a backtick (ADR-0061 ruling 5). Layout here
/// can only be non-ASCII, because ASCII whitespace ends the term.
fn is_not_dash(term: &str) -> bool {
    term.strip_prefix("NOT")
        .and_then(|rest| rest.strip_suffix('-'))
        .is_some_and(|layout| layout.chars().all(comment::is_layout))
}

fn earliest_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    just("earliest=")
        .ignore_then(quoted_string())
        .map(SearchToken::EarliestFilter)
        .labelled("earliest filter")
}

fn latest_filter<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    just("latest=")
        .ignore_then(quoted_string())
        .map(SearchToken::LatestFilter)
        .labelled("latest filter")
}

fn time_clause<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    choice((time_filter(), earliest_filter(), latest_filter()))
}

/// The top level ([`search_stage`]) reads a time clause before it asks
/// this parser, so a clause reaches this arm only from inside a group or
/// a `NOT` (ADR-0061 ruling 2).
fn search_token<'src>()
-> impl Parser<'src, ParserInput<'src>, SearchToken, ParserExtra<'src>> + Clone {
    recursive(|token| {
        // Use rewind-based lookahead: NOT must be followed by something
        // that's a valid token start, otherwise treat "NOT" as text search.
        let not_token = keyword("NOT")
            .then(
                any()
                    .filter(|c: &char| {
                        c.is_alphanumeric()
                            || *c == '_'
                            || *c == '"'
                            || *c == '-'
                            || *c == '('
                            || *c == '`'
                    })
                    .rewind()
                    .spaced(),
            )
            .ignore_then(spanned(token.clone()))
            .map(|inner| SearchToken::Not(Box::new(inner)));

        // Parenthesized group: (tokens OR tokens). The close is read apart
        // from the padding after it, so a group of only `OR` separators is
        // refused over exactly its own text (ADR-0061 ruling 4).
        let or_marker = keyword("OR")
            .or(keyword("or"))
            .to(TokenOrSep::<Spanned<SearchToken>>::Or);
        let paren_token = spanned(token).map(TokenOrSep::Token);
        let paren_group = just('(')
            .map_with(|_, e| -> SimpleSpan { e.span() })
            .spaced()
            .then(
                choice((or_marker, paren_token))
                    .spaced()
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .then(just(')').map_with(|_, e| -> SimpleSpan { e.span() }))
            .then_ignore(comment::ws())
            .validate(|((open, items), close), _, emitter| {
                if !items
                    .iter()
                    .any(|item| matches!(item, TokenOrSep::Token(_)))
                {
                    emitter.emit(Rich::custom(
                        (open.start..close.end).into(),
                        MSG_OR_ONLY_GROUP,
                    ));
                }
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

        let nested_time_clause = time_clause().validate(|clause, extra, emitter| {
            emitter.emit(Rich::custom(extra.span(), MSG_NESTED_TIME_CLAUSE));
            clause
        });
        let leaf = choice((
            nested_time_clause,
            quoted_search(),
            field_filter(),
            text_search(),
        ));

        choice((not_token, paren_group, leaf)).labelled("search token")
    })
}

#[derive(Clone)]
enum TokenOrSep<T> {
    Token(T),
    Or,
}

/// The search stage's time clauses, taken out of its groups in text
/// order, the last of each kind winning its slot.
///
/// the three slots keep only the last of each kind.
///
/// Each clause is checked as it is taken, because a later clause of its
/// kind overwrites the slot and `clauses` keeps no value: an unreadable
/// bound is refused over its own span, and the first clause that puts
/// `last=` beside an absolute bound is refused over its span. A refused
/// bound leaves its slot as it was, and the parse fails.
#[derive(Default)]
struct Hoist {
    time_filter: Option<Spanned<TimeFilter>>,
    earliest: Option<Spanned<TimeBound>>,
    latest: Option<Spanned<TimeBound>>,
    clauses: Vec<Spanned<TimeClause>>,
    seen_last: bool,
    seen_absolute: bool,
    paired: bool,
}

impl Hoist {
    fn take(
        &mut self,
        tokens: &mut Vec<Spanned<SearchToken>>,
        emitter: &mut chumsky::input::Emitter<Rich<'_, char>>,
    ) {
        tokens.retain(|t| {
            let span = t.span.clone();
            let clause = match &t.node {
                SearchToken::TimeFilter(tf) => {
                    self.time_filter = Some(Spanned::new(tf.clone(), span.clone()));
                    TimeClause::Last
                }
                SearchToken::EarliestFilter(text) => {
                    if let Some(bound) = read_bound(text, &span, emitter) {
                        self.earliest = Some(bound);
                    }
                    TimeClause::Earliest
                }
                SearchToken::LatestFilter(text) => {
                    if let Some(bound) = read_bound(text, &span, emitter) {
                        self.latest = Some(bound);
                    }
                    TimeClause::Latest
                }
                _ => return true,
            };
            if clause == TimeClause::Last {
                self.seen_last = true;
            } else {
                self.seen_absolute = true;
            }
            if self.seen_last && self.seen_absolute && !self.paired {
                self.paired = true;
                emitter.emit(Rich::custom(
                    (span.start..span.end).into(),
                    MSG_LAST_WITH_BOUND,
                ));
            }
            self.clauses.push(Spanned::new(clause, span));
            false
        });
    }
}

fn read_bound(
    text: &str,
    span: &std::ops::Range<usize>,
    emitter: &mut chumsky::input::Emitter<Rich<'_, char>>,
) -> Option<Spanned<TimeBound>> {
    let bound = TimeBound::read(text);
    if bound.is_none() {
        emitter.emit(Rich::custom(
            (span.start..span.end).into(),
            MSG_UNREADABLE_BOUND,
        ));
    }
    bound.map(|bound| Spanned::new(bound, span.clone()))
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

    // A time clause is read here, before the token grammar, so only the
    // top level reaches this arm (ADR-0061 ruling 2).
    let token = spanned(choice((time_clause(), search_token()))).map(TokenOrSep::Token);

    choice((or_marker, token))
        .spaced()
        .repeated()
        .collect::<Vec<_>>()
        .validate(|items, _, emitter| {
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

            // Hoist time clauses out of the OR groups — they apply
            // globally. If multiple clauses of the same kind appear, the
            // last one wins, in text order across the groups.
            let mut hoist = Hoist::default();
            for group in &mut groups {
                hoist.take(group, emitter);
            }
            groups.retain(|g| !g.is_empty());

            SearchStage {
                groups,
                time_filter: hoist.time_filter,
                earliest: hoist.earliest,
                latest: hoist.latest,
                hoisted: hoist.clauses,
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
        assert_eq!(
            only_filter("f=x,/a,b/"),
            filter(
                "f",
                FilterOp::Eq,
                FilterValue::List(vec![exact("x"), regex("a,b")])
            )
        );
    }

    fn one_error(dsl: &str) -> (crate::parser::ParseError, &str) {
        let errors = crate::parser::parse(dsl).expect_err(dsl);
        assert_eq!(errors.len(), 1, "{dsl}: {errors:?}");
        let error = errors.into_iter().next().expect("one error");
        let marked = &dsl[error.span.clone()];
        (error, marked)
    }

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
        assert_eq!(result.groups[0].len(), 2);
        assert!(result.time_filter.is_some());
    }

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

    #[test]
    fn no_unquoted_position_absorbs_a_backtick() {
        for dsl in [
            "er`ror",
            "service=`nginx`",
            "service=`my service`",
            "status=200,`a b` more",
            "host=a+`b` more",
            "-`http-status`=500",
            "-`a b`",
        ] {
            let result = search_stage().then_ignore(end()).parse(dsl).into_result();
            assert!(
                result.is_err(),
                "{dsl} must be a loud parse error: {result:?}"
            );
        }
    }

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
        let result = search_stage()
            .parse("service=nginx last=2h OR service=postgres")
            .into_result()
            .unwrap();
        assert_eq!(result.groups.len(), 2);
        for group in &result.groups {
            for token in group {
                assert!(
                    !matches!(token.node, SearchToken::TimeFilter(_)),
                    "time filter should be hoisted out of groups"
                );
            }
        }
        let tf = result.time_filter.expect("time_filter should be hoisted");
        assert_eq!(tf.node.duration.quantity, 2);
        assert_eq!(tf.node.duration.unit, TimeUnit::Hours);
    }

    #[test]
    fn test_last_time_filter_wins() {
        let result = search_stage()
            .parse("last=1h service=nginx last=2h")
            .into_result()
            .unwrap();
        let tf = result.time_filter.expect("time_filter should be hoisted");
        assert_eq!(tf.node.duration.quantity, 2);
        assert_eq!(tf.node.duration.unit, TimeUnit::Hours);
        assert_eq!(result.groups[0].len(), 1);
    }

    #[test]
    fn every_hoisted_clause_is_recorded_with_its_span() {
        let hoisted = |input: &'static str| -> (SearchStage, Vec<(TimeClause, &'static str)>) {
            let result = search_stage().parse(input).into_result().unwrap();
            let clauses = result
                .hoisted
                .iter()
                .map(|c| (c.node, &input[c.span.clone()]))
                .collect();
            (result, clauses)
        };

        let input =
            r#"earliest="2026-03-14" (a OR b) latest="2026-03-16" OR b earliest="2026-03-15""#;
        let (result, clauses) = hoisted(input);
        assert_eq!(
            clauses,
            [
                (TimeClause::Earliest, r#"earliest="2026-03-14""#),
                (TimeClause::Latest, r#"latest="2026-03-16""#),
                (TimeClause::Earliest, r#"earliest="2026-03-15""#),
            ]
        );
        let earliest = result.earliest.expect("earliest should be hoisted");
        assert_eq!(
            &input[earliest.span], r#"earliest="2026-03-15""#,
            "the last `earliest=` is the slot's"
        );

        let input = "last=1h (a OR b) OR b last=3h";
        let (result, clauses) = hoisted(input);
        assert_eq!(
            clauses,
            [(TimeClause::Last, "last=1h"), (TimeClause::Last, "last=3h")]
        );
        let tf = result.time_filter.expect("time_filter should be hoisted");
        assert_eq!(&input[tf.span], "last=3h", "the last `last=` is the slot's");

        let plain = search_stage().parse("a b").into_result().unwrap();
        assert!(plain.hoisted.is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn absolute_bounds_are_checked_and_spanned() {
        use crate::compare::literal_timestamp;
        use comment::{HINT_LAST_WITH_BOUND, HINT_UNREADABLE_BOUND};

        let refused = |input: &str| -> Vec<(std::ops::Range<usize>, String, Option<String>)> {
            crate::parser::parse(input)
                .expect_err(input)
                .into_iter()
                .map(|e| (e.span, e.message, e.hint))
                .collect()
        };
        let unreadable = |span: std::ops::Range<usize>| {
            (
                span,
                MSG_UNREADABLE_BOUND.to_string(),
                Some(HINT_UNREADABLE_BOUND.to_string()),
            )
        };
        let with_last = |span: std::ops::Range<usize>| {
            (
                span,
                MSG_LAST_WITH_BOUND.to_string(),
                Some(HINT_LAST_WITH_BOUND.to_string()),
            )
        };

        let cases: [(&str, Vec<_>); 14] = [
            (r#"earliest="yesterday""#, vec![unreadable(0..20)]),
            // Read, but its cast text is `+10000-…`, NULL to DuckDB.
            (
                r#"service=api latest="10000-01-01""#,
                vec![unreadable(12..32)],
            ),
            (r#"latest="300000-01-01""#, vec![unreadable(0..21)]),
            (
                r#"earliest="bad" earliest="2026-03-14""#,
                vec![unreadable(0..14)],
            ),
            (
                r#"earliest="2026-03-14" latest="soon""#,
                vec![unreadable(22..35)],
            ),
            (
                r#"earliest="bad" latest="worse""#,
                vec![unreadable(0..14), unreadable(15..29)],
            ),
            (r#""é" earliest="yesterday""#, vec![unreadable(5..25)]),
            (r#"a OR earliest="nope""#, vec![unreadable(5..20)]),
            (r#"last=1h earliest="2026-03-14""#, vec![with_last(8..29)]),
            (r#"last=1h latest="2026-03-14""#, vec![with_last(8..27)]),
            (r#"earliest="2026-03-14" last=1h"#, vec![with_last(22..29)]),
            (
                r#"last=1h earliest="2026-03-14" latest="2026-03-15""#,
                vec![with_last(8..29)],
            ),
            (
                r#"last=1h a OR b last=2h latest="2026-03-14""#,
                vec![with_last(23..42)],
            ),
            (
                r#"last=1h earliest="bad""#,
                vec![unreadable(8..22), with_last(8..22)],
            ),
        ];
        for (input, want) in cases {
            assert_eq!(refused(input), want, "{input:?}");
        }

        let parsed = |input: &str| {
            crate::parser::parse(input)
                .unwrap_or_else(|e| panic!("{input:?} must parse: {e:?}"))
                .search
        };
        let at = |text: &str| literal_timestamp(text).expect("a timestamp");

        for (text, instant) in [
            ("2026-03-14", "2026-03-14 00:00:00"),
            ("2026-03-14T09:00:00", "2026-03-14 09:00:00"),
            ("2026-03-14 09:00:00", "2026-03-14 09:00:00"),
            ("2026-03-14T09:00:00.000001", "2026-03-14 09:00:00.000001"),
            ("2026-03-14T09:00:00Z", "2026-03-14 09:00:00"),
            // The offset is read, then discarded: the wall clock stays.
            ("2026-03-14T09:00:00+05:30", "2026-03-14 09:00:00"),
            ("epoch", "1970-01-01 00:00:00"),
            ("infinity", "infinity"),
            ("-infinity", "-infinity"),
        ] {
            let search = parsed(&format!(r#"earliest="{text}" latest="{text}""#));
            for bound in [search.earliest, search.latest] {
                let bound = bound.expect("the bound").node;
                assert_eq!(bound.text(), text);
                assert_eq!(bound.instant().cast_text(), instant, "{text:?}");
            }
        }

        let search = parsed(r#"earliest="2026-03-14" earliest="2026-03-15""#);
        let earliest = search.earliest.expect("the bound").node;
        assert_eq!(earliest.text(), "2026-03-15");
        assert_eq!(earliest.instant(), at("2026-03-15"));
        assert_eq!(search.hoisted.len(), 2);

        for input in ["earliest=2026-03-14", "earliest=yesterday"] {
            let search = parsed(input);
            assert!(search.earliest.is_none(), "{input:?}");
            assert!(search.hoisted.is_empty(), "{input:?}");
            assert!(
                matches!(
                    &search.groups[0][0].node,
                    SearchToken::FieldFilter(f) if f.field == "earliest"
                ),
                "{input:?}"
            );
        }

        let search = parsed(r#"earliest="2026-03-15" latest="2026-03-14""#);
        assert!(
            search.earliest.expect("earliest").node.instant()
                > search.latest.expect("latest").node.instant()
        );
    }

    #[test]
    fn top_level_or_time_clauses_keep_last_wins_order() {
        let input = "a last=1h OR b OR last=2h";
        let result = search_stage().parse(input).into_result().unwrap();
        assert_eq!(result.groups.len(), 2);
        let tf = result.time_filter.expect("the window");
        assert_eq!(&input[tf.span], "last=2h");
        assert_eq!(tf.node.duration.quantity, 2);
    }

    #[test]
    fn test_only_time_filter_produces_empty_groups() {
        let result = search_stage().parse("last=5m").into_result().unwrap();
        assert!(result.groups.is_empty());
        assert!(result.time_filter.is_some());
    }
}
