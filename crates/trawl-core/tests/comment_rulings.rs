// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ADR-0014's ruling table, as a test.
//!
//! Every row of the table appears here with its exact AST or its exact
//! error. A shape that must parse is checked for what it means, not
//! merely that it parsed: the failure class the ruling guards against is
//! a query that parses successfully as a different query.

use trawl_core::ast::{
    BinaryOp, Expr, FieldFilter, FilterOp, FilterValue, LiteralValue, PipeStage, Query,
    QuotedSearch, Scalar, SearchToken, TextSearch,
};
use trawl_core::parser::{ParseError, parse};

const MSG_OPENER: &str = "'#' inside an unquoted token";
const MSG_OPENER_VALUE: &str = "'#' inside an unquoted value";
const MSG_OPENER_NEGATED: &str = "'#' inside a negated search term";
const MSG_OPENER_COMMAND: &str = "'#' inside a pipe stage name";
const MSG_SLASHES: &str = "'//' does not start a comment — '#' is the comment character";

fn ok(dsl: &str) -> Query {
    parse(dsl).unwrap_or_else(|e| panic!("{dsl:?} must parse: {e:?}"))
}

/// The single error `dsl` produces. Exactly one: a shape that reports two
/// would show whichever chumsky happened to rank first.
fn one_error(dsl: &str) -> ParseError {
    let mut errors = parse(dsl)
        .map(|_| ())
        .expect_err(&format!("{dsl:?} must fail"));
    assert_eq!(
        errors.len(),
        1,
        "{dsl:?} must report exactly one error: {errors:?}"
    );
    errors.remove(0)
}

fn filters(query: &Query) -> Vec<&FieldFilter> {
    query.search.groups[0]
        .iter()
        .filter_map(|t| match &t.node {
            SearchToken::FieldFilter(ff) => Some(ff),
            _ => None,
        })
        .collect()
}

fn eq(field: &str, value: &str) -> FieldFilter {
    FieldFilter {
        field: field.to_string(),
        op: FilterOp::Eq,
        value: FilterValue::Single(Scalar::Exact(value.to_string())),
    }
}

fn tokens(query: &Query) -> Vec<&SearchToken> {
    query.search.groups[0].iter().map(|t| &t.node).collect()
}

const COMMENT_HALF: &str = "put whitespace before the '#' to start a comment";

const GENERIC_ADVICE: &str = "put whitespace before the '#' to start a comment, \
     or carry the '#' inside a quoted value (\"…\") or a backticked field name (`…`)";

#[test]
fn a_whitespace_preceded_hash_opens_a_comment() {
    let query = ok("a=1 # note");
    assert_eq!(filters(&query), vec![&eq("a", "1")]);
    assert!(query.pipeline.is_empty());

    let query = ok("status=200 # c\nhost=x");
    assert_eq!(
        filters(&query),
        vec![&eq("status", "200"), &eq("host", "x")]
    );

    let query = ok("* | stats count()\n# note\n| sort -count");
    assert_eq!(query.pipeline.len(), 2);
    assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
    assert!(matches!(query.pipeline[1].node, PipeStage::Sort(_)));

    let query = ok("# note\na=1");
    assert_eq!(filters(&query), vec![&eq("a", "1")]);

    for dsl in ["", "   ", "# just a comment", "# a\n# b\n"] {
        let query = ok(dsl);
        assert!(query.search.groups.is_empty(), "{dsl:?}");
        assert!(query.pipeline.is_empty(), "{dsl:?}");
    }
}

#[test]
fn quoted_contexts_carry_the_opener_verbatim() {
    let query = ok("message=/a#b/");
    assert_eq!(
        filters(&query),
        vec![&FieldFilter {
            field: "message".to_string(),
            op: FilterOp::Eq,
            value: FilterValue::Single(Scalar::Regex("a#b".to_string())),
        }]
    );

    let query = ok("`a#b`=1");
    assert_eq!(filters(&query), vec![&eq("a#b", "1")]);

    let query = ok(r#"host="a#b""#);
    assert_eq!(filters(&query), vec![&eq("host", "a#b")]);

    let query = ok("* | where message matches /foo#bar/");
    assert_eq!(query.pipeline.len(), 1);
    let PipeStage::Where(w) = &query.pipeline[0].node else {
        panic!("expected Where, got {:?}", query.pipeline[0].node);
    };
    let Expr::Binary { lhs, op, rhs } = &w.condition.node else {
        panic!("expected a binary condition, got {:?}", w.condition.node);
    };
    assert_eq!(*op, BinaryOp::Matches);
    assert_eq!(lhs.node, Expr::FieldRef("message".to_string()));
    assert_eq!(
        rhs.node,
        Expr::Literal(LiteralValue::String("foo#bar".to_string()))
    );
}

#[test]
fn slashes_in_a_value_are_data() {
    for (dsl, want) in [
        (
            "url=https://example.com/x",
            eq("url", "https://example.com/x"),
        ),
        ("path=/api//v1", eq("path", "/api//v1")),
        ("url=//cdn.example.com/x", eq("url", "//cdn.example.com/x")),
    ] {
        assert_eq!(filters(&ok(dsl)), vec![&want], "{dsl:?}");
    }

    let query = ok("referrer=https://a.b/c status=200");
    assert_eq!(
        filters(&query),
        vec![&eq("referrer", "https://a.b/c"), &eq("status", "200")]
    );

    let query = ok("-//cdn.example.com/x");
    assert_eq!(
        query.search.groups[0][0].node,
        SearchToken::TextSearch(TextSearch {
            term: "//cdn.example.com/x".to_string(),
            negated: true,
        })
    );
}

#[test]
fn an_opener_inside_an_unquoted_token_is_a_loud_error() {
    for (dsl, at, msg, hint_fragment) in [
        (
            "foo#bar",
            3,
            MSG_OPENER,
            "quote it (\"foo#bar\") to search for it",
        ),
        (
            "color=#ff0000",
            6,
            MSG_OPENER_VALUE,
            "quote the value (\"#ff0000\")",
        ),
        ("a=1# note", 3, MSG_OPENER_VALUE, "quote the value (\"1#\")"),
        (
            "1=/foo#bar/",
            6,
            MSG_OPENER,
            "quote it (\"1=/foo#bar/\") to search for it",
        ),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, msg, "{dsl:?}");
        assert_eq!(err.span, at..at + 1, "{dsl:?}: span must be the '#' byte");
        assert_eq!(&dsl[err.span.clone()], "#", "{dsl:?}");
        let hint = err.hint.unwrap_or_else(|| panic!("{dsl:?} must hint"));
        assert_eq!(
            hint,
            format!("{hint_fragment}, or {COMMENT_HALF}"),
            "{dsl:?}"
        );
    }

    assert_eq!(
        tokens(&ok("\"foo#bar\"")),
        vec![&SearchToken::QuotedSearch(QuotedSearch {
            phrase: "foo#bar".to_string()
        })]
    );
    assert_eq!(
        filters(&ok("color=\"#ff0000\"")),
        vec![&eq("color", "#ff0000")]
    );
    assert_eq!(filters(&ok("a=\"1#\"")), vec![&eq("a", "1#")]);
    assert_eq!(
        tokens(&ok("\"1=/foo#bar/\"")),
        vec![&SearchToken::QuotedSearch(QuotedSearch {
            phrase: "1=/foo#bar/".to_string()
        })]
    );
}

#[test]
fn the_value_hint_names_the_whole_value() {
    let dsl = "url=https://example.test/p?a=b#frag";
    let err = one_error(dsl);
    assert_eq!(err.message, MSG_OPENER_VALUE);
    assert_eq!(&dsl[err.span.clone()], "#");
    assert_eq!(
        err.hint.as_deref(),
        Some(
            format!(
                "quote the value (\"https://example.test/p?a=b#frag\") to carry the '#' \
                 (a quoted value is never a pattern), or {COMMENT_HALF}"
            )
            .as_str()
        ),
        "the value carries a '?', so quoting it is not operator-neutral"
    );
    assert_eq!(
        filters(&ok("url=\"https://example.test/p?a=b#frag\"")),
        vec![&eq("url", "https://example.test/p?a=b#frag")]
    );
}

#[test]
fn a_value_hint_admits_that_quoting_a_glob_drops_the_pattern() {
    for dsl in ["f=#a*", "f=#a?"] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_OPENER_VALUE, "{dsl:?}");
        let value = &dsl["f=".len()..];
        assert_eq!(
            err.hint.as_deref(),
            Some(
                format!(
                    "quote the value (\"{value}\") to carry the '#' \
                     (a quoted value is never a pattern), or {COMMENT_HALF}"
                )
                .as_str()
            ),
            "{dsl:?}"
        );

        assert_eq!(
            filters(&ok(&format!("f=\"{value}\""))),
            vec![&eq("f", value)],
            "{dsl:?}: the advised rewrite must be FilterOp::Eq"
        );
    }

    for (dsl, op) in [
        ("f>#a*", FilterOp::Gt),
        ("f<=#a?", FilterOp::Lte),
        ("f!=#a*", FilterOp::Ne),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_OPENER_VALUE, "{dsl:?}");
        let value = dsl
            .rsplit_once('#')
            .map(|(_, v)| format!("#{v}"))
            .expect("#");
        assert_eq!(
            err.hint.as_deref(),
            Some(
                format!(
                    "quote the value (\"{value}\") to carry the '#' \
                     (a quoted value is never a pattern), or {COMMENT_HALF}"
                )
                .as_str()
            ),
            "{dsl:?}"
        );
        let quoted = dsl.replace(&value, &format!("\"{value}\""));
        assert_eq!(
            filters(&ok(&quoted)),
            vec![&FieldFilter {
                field: "f".to_string(),
                op,
                value: FilterValue::Single(Scalar::Exact(value.clone())),
            }],
            "{quoted:?}: the advised rewrite keeps the typed operator"
        );

        let unhashed = dsl.replace('#', "");
        if op == FilterOp::Ne {
            assert_eq!(
                filters(&ok(&unhashed)),
                vec![&FieldFilter {
                    field: "f".to_string(),
                    op,
                    value: FilterValue::Single(Scalar::Glob(value[1..].to_string())),
                }],
                "{unhashed:?}"
            );
        } else {
            assert_eq!(
                one_error(&unhashed).message,
                "glob patterns only support `=` and `!=`",
                "{unhashed:?}"
            );
        }
    }

    let err = one_error("f=#a");
    assert_eq!(
        err.hint.as_deref(),
        Some(format!("quote the value (\"#a\"), or {COMMENT_HALF}").as_str())
    );
    assert_eq!(filters(&ok("f=\"#a\"")), vec![&eq("f", "#a")]);
}

#[test]
fn a_term_hint_admits_a_comma() {
    let dsl = "foo,#bar";
    let err = one_error(dsl);
    assert_eq!(err.message, MSG_OPENER);
    assert_eq!(&dsl[err.span.clone()], "#");
    assert_eq!(
        err.hint.as_deref(),
        Some(format!("quote it (\"foo,#bar\") to search for it, or {COMMENT_HALF}").as_str())
    );

    assert_eq!(
        tokens(&ok("\"foo,#bar\"")),
        vec![&SearchToken::QuotedSearch(QuotedSearch {
            phrase: "foo,#bar".to_string()
        })]
    );

    let err = one_error("-foo,#bar");
    assert_eq!(err.message, MSG_OPENER_NEGATED);
    assert_eq!(
        err.hint.as_deref(),
        Some(format!("write it as NOT \"foo,#bar\", or {COMMENT_HALF}").as_str())
    );
    let rewritten = ok("NOT \"foo,#bar\"");
    let advised = tokens(&rewritten);
    assert_eq!(advised.len(), 1, "the advised rewrite is one negated token");
    let SearchToken::Not(inner) = advised[0] else {
        panic!("expected a NOT token, got {:?}", advised[0]);
    };
    assert_eq!(
        inner.node,
        SearchToken::QuotedSearch(QuotedSearch {
            phrase: "foo,#bar".to_string()
        })
    );
}

#[test]
fn an_in_list_is_hinted_one_element_at_a_time() {
    let dsl = "f=#a,#b,#c";
    let errors = parse(dsl).expect_err("every element carries a '#'");
    let hints: Vec<Option<&str>> = errors.iter().map(|e| e.hint.as_deref()).collect();
    let expected = |v: &str| {
        Some(format!(
            "quote the value (\"{v}\"), or put whitespace before the '#' to start a comment"
        ))
    };
    assert_eq!(
        hints,
        vec![
            expected("#a").as_deref(),
            expected("#b").as_deref(),
            expected("#c").as_deref(),
        ]
    );

    for (dsl, value) in [("f=#a", "#a"), ("f=ok,#b,ok", "#b")] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_OPENER_VALUE, "{dsl:?}");
        assert_eq!(err.hint.as_deref(), expected(value).as_deref(), "{dsl:?}");
    }

    let query = ok("f=\"#a\",\"#b\",\"#c\"");
    assert_eq!(
        filters(&query),
        vec![&FieldFilter {
            field: "f".to_string(),
            op: FilterOp::Eq,
            value: FilterValue::List(vec![
                Scalar::Exact("#a".to_string()),
                Scalar::Exact("#b".to_string()),
                Scalar::Exact("#c".to_string()),
            ]),
        }]
    );
}

#[test]
fn a_hint_drops_advice_the_grammar_cannot_read() {
    let err = one_error("f=a\"b#c");
    assert_eq!(err.message, MSG_OPENER_VALUE);
    assert_eq!(err.hint.as_deref(), Some(COMMENT_HALF));

    for dsl in [
        "* | stats count(),# x",
        "* | sort a#b",
        "* | where a#b > 1",
        "* | table a#b",
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_OPENER, "{dsl:?}");
        assert_eq!(err.hint.as_deref(), Some(GENERIC_ADVICE), "{dsl:?}");
    }

    let dsl = "message=\"x\"\u{a0}# note";
    let err = one_error(dsl);
    assert_eq!(err.message, MSG_OPENER);
    assert_eq!(
        err.hint.as_deref(),
        Some(
            "quote it (\"#\") to search for it, or put whitespace before the '#' to start a comment"
        )
    );
    let rewritten = ok("message=\"x\"\u{a0}\"#\" note");
    assert_eq!(
        tokens(&rewritten),
        vec![
            &SearchToken::FieldFilter(eq("message", "x")),
            &SearchToken::QuotedSearch(QuotedSearch {
                phrase: "#".to_string()
            }),
            &SearchToken::TextSearch(TextSearch {
                term: "note".to_string(),
                negated: false,
            }),
        ]
    );
}

#[test]
fn a_negated_term_is_pointed_at_the_not_spelling() {
    for (dsl, at, working) in [
        ("-\"a#b\"", 3, "NOT \"a#b\""),
        ("-foo#bar", 4, "NOT \"foo#bar\""),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_OPENER_NEGATED, "{dsl:?}");
        assert_eq!(err.span, at..at + 1, "{dsl:?}: span must be the '#' byte");
        assert_eq!(&dsl[err.span.clone()], "#", "{dsl:?}");
        let hint = err.hint.unwrap_or_else(|| panic!("{dsl:?} must hint"));
        assert_eq!(
            hint,
            format!("write it as {working}, or put whitespace before the '#' to start a comment"),
            "{dsl:?}"
        );
        let rewritten = ok(working);
        let advised = tokens(&rewritten);
        assert_eq!(advised.len(), 1, "{working:?}");
        let SearchToken::Not(inner) = advised[0] else {
            panic!("{working:?}: expected a NOT token, got {:?}", advised[0]);
        };
        let SearchToken::QuotedSearch(phrase) = &inner.node else {
            panic!(
                "{working:?}: expected a quoted search, got {:?}",
                inner.node
            );
        };
        assert_eq!(
            phrase.phrase,
            dsl.trim_start_matches('-').trim_matches('"'),
            "{working:?}: the rewrite must search for the term the user wrote"
        );
    }
}

#[test]
fn an_opener_in_a_stage_name_is_not_an_unknown_command() {
    let err = one_error("* | co#unt()");
    assert_eq!(err.message, MSG_OPENER_COMMAND);
    assert_eq!(err.span, 6..7);
    assert_eq!(
        err.hint.as_deref(),
        Some("put whitespace before the '#' to start a comment")
    );

    let err = one_error("* | stats count(),# x");
    assert_eq!(err.message, MSG_OPENER);
    assert_eq!(err.span, 18..19);
    let err = one_error("* | staats count()");
    assert_eq!(err.message, "unknown command 'staats'");
}

#[test]
fn a_term_opening_with_slashes_names_the_real_opener() {
    for dsl in [
        "// note",
        "service=x // filter by service",
        "* | stats count() // note",
        "* | stats count()\n// note",
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_SLASHES, "{dsl:?}");
        assert_eq!(err.span.len(), 2, "{dsl:?}: span must be both slashes");
        assert_eq!(&dsl[err.span.clone()], "//", "{dsl:?}");
        assert!(
            err.hint.is_some_and(|h| h.contains('#')),
            "{dsl:?}: hint must name '#'"
        );
    }
}

#[test]
fn a_delimiter_does_not_open_a_comment() {
    for dsl in ["count(),# x", "* | stats count(),# x"] {
        assert!(parse(dsl).is_err(), "{dsl:?} must be a parse error");
    }
}

#[test]
fn a_span_after_a_comment_indexes_the_original_input() {
    let input = "# note\nfoo bar baz |";
    let err = one_error(input);
    assert_eq!(
        err.span,
        input.len()..input.len(),
        "the dangling pipe is at end of input"
    );

    let input = "# a longer note\nfoo#bar";
    let err = one_error(input);
    let at = input.find("foo#bar").expect("present") + 3;
    assert_eq!(err.span, at..at + 1);
    assert_eq!(&input[err.span.clone()], "#");

    let input = "# one\n# two\ncolor=#ff0000";
    let err = one_error(input);
    let at = input.rfind('#').expect("present");
    assert_eq!(err.span, at..at + 1);
    assert_eq!(err.message, MSG_OPENER_VALUE);
}

#[test]
fn a_comment_opens_only_after_ascii_whitespace() {
    for ws in [" ", "\t", "\n", "\r\n"] {
        let dsl = format!("a=1{ws}# note");
        let query = ok(&dsl);
        assert_eq!(filters(&query), vec![&eq("a", "1")], "{dsl:?}");
    }

    for (dsl, msg) in [
        ("message=\"x\"\u{a0}# note", MSG_OPENER),
        ("foo\u{a0}# note", MSG_OPENER),
        ("a=1\u{2003}# note", MSG_OPENER_VALUE),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, msg, "{dsl:?}");
        assert_eq!(&dsl[err.span.clone()], "#", "{dsl:?}");
    }

    let query = ok("a=1\u{a0}host=x");
    assert_eq!(filters(&query), vec![&eq("a", "1\u{a0}host=x")]);
}

#[test]
fn a_pathological_query_reports_a_bounded_number_of_diagnostics() {
    /// Mirrors `parser::MAX_REPORTED_ERRORS`, which is private.
    const CAP: usize = 8;

    let mut dsl = String::from("f=");
    while dsl.len() < 60_000 {
        dsl.push_str("#a,");
    }
    dsl.pop();

    let errors = parse(&dsl).expect_err("every element carries a '#'");
    assert!(
        errors.len() <= CAP,
        "{} diagnostics for a {}-byte query",
        errors.len(),
        dsl.len()
    );
    let rendered: usize = errors
        .iter()
        .map(|e| e.message.len() + e.hint.as_ref().map_or(0, String::len))
        .sum();
    assert!(
        rendered < 4 * 1024,
        "{rendered} bytes of diagnostics for a {}-byte query",
        dsl.len()
    );

    let err = &errors[0];
    assert_eq!(err.message, MSG_OPENER_VALUE);
    assert_eq!(
        err.hint.as_deref(),
        Some("quote the value (\"#a\"), or put whitespace before the '#' to start a comment")
    );
}

#[test]
fn the_cap_keeps_the_terminal_error() {
    /// Mirrors `parser::MAX_REPORTED_ERRORS`, which is private.
    const CAP: usize = 8;

    let dsl = "f=#0,#1,#2,#3,#4,#5,#6,#7 | co#unt()";
    let errors = parse(dsl).expect_err("nine violations");
    assert_eq!(errors.len(), CAP);
    let last = errors.last().expect("capped list is non-empty");
    assert_eq!(
        last.message,
        format!("{MSG_OPENER_COMMAND} (1 earlier error omitted)")
    );
    assert_eq!(&dsl[last.span.clone()], "#");
    assert_eq!(
        last.hint.as_deref(),
        Some("put whitespace before the '#' to start a comment")
    );

    let filters: Vec<String> = (0..12).map(|i| format!("f{i}=#a")).collect();
    let dsl = format!("{} | bogus_stage", filters.join(" "));
    let errors = parse(&dsl).expect_err("thirteen violations");
    assert_eq!(errors.len(), CAP);
    assert_eq!(
        errors[CAP - 1].message,
        "unknown command 'bogus_stage' (5 earlier errors omitted)"
    );
    assert_eq!(&dsl[errors[CAP - 1].span.clone()], "b");

    let errors = parse("f=#a,#b | bogus_stage").expect_err("three violations");
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[0].message, MSG_OPENER_VALUE);
    assert_eq!(errors[2].message, "unknown command 'bogus_stage'");
}
