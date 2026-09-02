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
    QuotedSearch, SearchToken, TextSearch,
};
use trawl_core::parser::{ParseError, parse};

/// The messages a comment opener inside an unquoted token carries. One
/// rule, but the working spelling differs by position, so the position
/// rides in the message and each hint offers an escape that keeps the
/// query meaning what it meant.
const MSG_OPENER: &str = "'#' inside an unquoted token";
const MSG_OPENER_VALUE: &str = "'#' inside an unquoted value";
const MSG_OPENER_NEGATED: &str = "'#' inside a negated search term";
const MSG_OPENER_COMMAND: &str = "'#' inside a pipe stage name";
/// The message a bare term opening with `//` carries.
const MSG_SLASHES: &str = "'//' does not start a comment — '#' is the comment character";

fn ok(dsl: &str) -> Query {
    parse(dsl).unwrap_or_else(|e| panic!("{dsl:?} must parse: {e:?}"))
}

/// The single error `dsl` produces. Exactly one: the server surfaces the
/// first error, so a shape that reports two would show whichever chumsky
/// happened to rank first.
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
        value: FilterValue::Literal(value.to_string()),
    }
}

/// The search tokens of a query, in order — so a hint's advised rewrite
/// can be checked for what it means, not merely that it parsed.
fn tokens(query: &Query) -> Vec<&SearchToken> {
    query.search.groups[0].iter().map(|t| &t.node).collect()
}

/// The one advice that holds in every position, and the tail of every
/// hint here.
const COMMENT_HALF: &str = "put whitespace before the '#' to start a comment";

/// The position-blind net's whole hint: it knows neither the token's
/// bounds nor which quoted context the position takes, so it names both
/// escapes and mints no spelling.
const GENERIC_ADVICE: &str = "put whitespace before the '#' to start a comment, \
     or carry the '#' inside a quoted value (\"…\") or a backticked field name (`…`)";

// ── the shapes that parse, and what they mean ────────────────────────

/// A comment after whitespace is a comment, and only the comment.
#[test]
fn a_whitespace_preceded_hash_opens_a_comment() {
    // `a=1 # note` — the filter survives, the comment does not reach the AST
    let query = ok("a=1 # note");
    assert_eq!(filters(&query), vec![&eq("a", "1")]);
    assert!(query.pipeline.is_empty());

    // a comment ends at the newline, so the next line is grammar again
    let query = ok("status=200 # c\nhost=x");
    assert_eq!(
        filters(&query),
        vec![&eq("status", "200"), &eq("host", "x")]
    );

    // …and a whole comment line between two stages leaves both stages
    let query = ok("* | stats count()\n# note\n| sort -count");
    assert_eq!(query.pipeline.len(), 2);
    assert!(matches!(query.pipeline[0].node, PipeStage::Stats(_)));
    assert!(matches!(query.pipeline[1].node, PipeStage::Sort(_)));

    // start of input is the one comment site whitespace cannot precede
    let query = ok("# note\na=1");
    assert_eq!(filters(&query), vec![&eq("a", "1")]);

    // comment-only and whitespace-only input behave exactly as empty input
    for dsl in ["", "   ", "# just a comment", "# a\n# b\n"] {
        let query = ok(dsl);
        assert!(query.search.groups.is_empty(), "{dsl:?}");
        assert!(query.pipeline.is_empty(), "{dsl:?}");
    }
}

/// The three quoted contexts carry `#` verbatim — they contain no
/// whitespace-skip site, so the comment production cannot reach inside.
#[test]
fn quoted_contexts_carry_the_opener_verbatim() {
    // `message=/a#b/` — the regex, not an equality on the literal "/a"
    let query = ok("message=/a#b/");
    assert_eq!(
        filters(&query),
        vec![&FieldFilter {
            field: "message".to_string(),
            op: FilterOp::Regex,
            value: FilterValue::Literal("a#b".to_string()),
        }]
    );

    // a backtick-quoted name (ADR-0013 ruling 7)
    let query = ok("`a#b`=1");
    assert_eq!(filters(&query), vec![&eq("a#b", "1")]);

    // a double-quoted value
    let query = ok(r#"host="a#b""#);
    assert_eq!(filters(&query), vec![&eq("host", "a#b")]);

    // …and a regex in expression position — the whole condition, because
    // "it is a Where stage" would hold just as well for a stage that
    // matched the literal "/foo".
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

/// `//` is ordinary data (ruling 3) in every value position, so a filter
/// that follows one on the same line still parses.
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

    // `referrer=https://a.b/c status=200` — both filters, correctly
    let query = ok("referrer=https://a.b/c status=200");
    assert_eq!(
        filters(&query),
        vec![&eq("referrer", "https://a.b/c"), &eq("status", "200")]
    );

    // a `//` inside a bare term is fine too — only a term that opens
    // with it is refused, and a negated one is never a comment
    let query = ok("-//cdn.example.com/x");
    assert_eq!(
        query.search.groups[0][0].node,
        SearchToken::TextSearch(TextSearch {
            term: "//cdn.example.com/x".to_string(),
            negated: true,
        })
    );
}

// ── the shapes that error, and exactly how ───────────────────────────

/// `foo#bar`, `color=#ff0000`, `a=1# note` — one error each, spanning
/// the `#` byte, with a hint whose advice a user can follow (ruling 2).
///
/// The hint is position-aware because the working spelling is: quoting
/// the whole of `color=#ff0000` produces a phrase search, which is a
/// different query, and following a hint must never change what a query
/// means.
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

    // …and every spelling those hints offer is a query the grammar reads
    // back as the thing the hint claims it is — a term hint claims a text
    // search, a value hint claims a filter on that value.
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

/// The value hint names the value the production held, never a slice
/// re-derived from the raw text.
///
/// `=`, `<`, `>` and `!` are all legal inside a bare value, so a left
/// boundary found by scanning backwards for one of them would cut a URL
/// at its own query string: `url=…?a=b#frag` would be hinted `quote the
/// value ("b#frag")`, a different filter on a different value. The
/// emitting production knows the exact bounds, so it sends them.
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
    // …and the spelling it offers is the query the user meant, with the
    // operator the hint claims
    assert_eq!(
        filters(&ok("url=\"https://example.test/p?a=b#frag\"")),
        vec![&eq("url", "https://example.test/p?a=b#frag")]
    );
}

/// A quoted value is never a pattern, so a hint that says "quote it" to a
/// value carrying `*`/`?` is advising an operator change. The advice
/// stands (the `#` leaves no unquoted spelling), and it says what it
/// costs in the one wording that holds under every operator.
///
/// "match it exactly" does not hold: the glob auto-detect overrides the
/// operator the user typed, so `f>#a*` is a Glob whose quoted rewrite is
/// a Gt comparison. The promise is lexical (the `#` reaches the value),
/// never that the field's pin admits the result: `_severity="#warn*"`
/// carries the `#` and is refused by the severity vocabulary at
/// emission.
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

        // …and under `=` the rewrite is the exact match it always was
        assert_eq!(
            filters(&ok(&format!("f=\"{value}\""))),
            vec![&eq("f", value)],
            "{dsl:?}: the advised rewrite must be FilterOp::Eq"
        );
    }

    // …while under an ordered operator the same hint's rewrite is that
    // operator's comparison, which is why the wording may not say
    // "exactly". The unquoted form is a Glob — the auto-detect discards
    // the typed operator — so quoting is an operator change either way.
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
                value: FilterValue::Literal(value.clone()),
            }],
            "{quoted:?}: the advised rewrite keeps the typed operator"
        );
    }

    // …while a value with no wildcard keeps the plain wording, and its
    // rewrite is Eq because the unquoted spelling was Eq too
    let err = one_error("f=#a");
    assert_eq!(
        err.hint.as_deref(),
        Some(format!("quote the value (\"#a\"), or {COMMENT_HALF}").as_str())
    );
    assert_eq!(filters(&ok("f=\"#a\"")), vec![&eq("f", "#a")]);
}

/// A `,` is a word byte in a bare term — nothing splits on it there — so
/// the term hint quotes the whole of `foo,#bar`, where the value
/// position's element bounding would cut at the comma and advise a term
/// the user never wrote. `"foo,#bar"` is the quoted search that means the
/// same thing.
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

    // …and the rewrite parses as the substring search the hint claims
    assert_eq!(
        tokens(&ok("\"foo,#bar\"")),
        vec![&SearchToken::QuotedSearch(QuotedSearch {
            phrase: "foo,#bar".to_string()
        })]
    );

    // …the same under `-`, whose working spelling is the `NOT` form
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

/// …and an IN list is hinted per element, because an element is its own
/// value production: the comma bounding comes free with the exact slice,
/// in every position of the list.
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

    // …and a sole element, and a middle one whose neighbours are clean
    for (dsl, value) in [("f=#a", "#a"), ("f=ok,#b,ok", "#b")] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_OPENER_VALUE, "{dsl:?}");
        assert_eq!(err.hint.as_deref(), expected(value).as_deref(), "{dsl:?}");
    }

    // …and quoting each element as advised gives back the IN list the
    // user meant, element for element — the claim the per-element hint
    // makes is that the list survives.
    let query = ok("f=\"#a\",\"#b\",\"#c\"");
    assert_eq!(
        filters(&query),
        vec![&FieldFilter {
            field: "f".to_string(),
            op: FilterOp::Eq,
            value: FilterValue::List(vec!["#a".to_string(), "#b".to_string(), "#c".to_string()]),
        }]
    );
}

/// A concrete rewrite is offered only where the emitting production held
/// the slice and the rewrite means what the hint claims. Two shapes where
/// it does not, and neither prints advice that cannot be followed:
///
/// * a token carrying a `"` or a `\` would need an escape the hint does
///   not spell, so the pasted string would end at its own second quote;
/// * the position-blind net knows only an offset — the run around it can
///   span several grammar tokens, and each position escapes a `#`
///   differently — so it names the escapes and spells none.
///
/// The whitespace half is always true, so it is what always remains.
#[test]
fn a_hint_drops_advice_the_grammar_cannot_read() {
    // an embedded quote inside a value: the position is known, but no
    // quoted spelling reads back unchanged, so only the true half remains
    let err = one_error("f=a\"b#c");
    assert_eq!(err.message, MSG_OPENER_VALUE);
    assert_eq!(err.hint.as_deref(), Some(COMMENT_HALF));

    // …and the position-blind net mints no spelling at all. `"a#b"` does
    // not parse as a sort key and quietly becomes a string literal in an
    // expression, whose escape is a backticked name — so the net names
    // both escapes and picks neither.
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

    // …while a term the production held is quotable even when the text
    // around it is not: the exact slice after a no-break space is the
    // `#` alone, and `"#"` is a phrase search that parses.
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

/// Quoting is no escape under `-`: `-"a#b"` is a bare negated term that
/// spells literal quote characters, so a hint saying `quote it ("-"a#b"")`
/// renders something the grammar cannot read at all. `NOT "a#b"` is the
/// working form, and it parses.
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
        // …and the spelling the hint offers is one the grammar accepts,
        // as the negated phrase search the hint claims it is
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

/// A `#` in a pipe stage name is the comment rule, not a typo. chumsky
/// reports the start of the word (the stage word ends at the `#`), so
/// without this rule the unknown-command diagnostic invents a command the
/// user never wrote: `unknown command 'co'`, "did you mean 'top'?".
#[test]
fn an_opener_in_a_stage_name_is_not_an_unknown_command() {
    let err = one_error("* | co#unt()");
    assert_eq!(err.message, MSG_OPENER_COMMAND);
    assert_eq!(err.span, 6..7);
    assert_eq!(
        err.hint.as_deref(),
        Some("put whitespace before the '#' to start a comment")
    );

    // …and the rule is narrow: the token holding the offset must be the
    // stage word, so a shape whose real problem is the `,` keeps its own
    // diagnostic on its own byte.
    let err = one_error("* | stats count(),# x");
    assert_eq!(err.message, MSG_OPENER);
    assert_eq!(err.span, 18..19);
    let err = one_error("* | staats count()");
    assert_eq!(err.message, "unknown command 'staats'");
}

/// A bare term opening with `//` is refused and names `#` as the comment
/// character. `//` opens no comment (ruling 3), so the alternative is
/// AND-ing the slashes into the search as text terms, which narrows the
/// result set to nothing without saying why.
#[test]
fn a_term_opening_with_slashes_names_the_real_opener() {
    // …in the search stage (the validator's own error) and in the
    // pipeline (the generic net). Both slashes are the span in either
    // lane: underlining one of two points at nothing the user can act on.
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

/// The comment boundary is structural: whitespace or start of input, and
/// nothing else. "After a delimiter" is deliberately not admitted — the
/// delimiter set is unenumerable, and admitting `=` would stop
/// `color=#ff0000` erroring. These are parse errors, not comments.
#[test]
fn a_delimiter_does_not_open_a_comment() {
    for dsl in ["count(),# x", "* | stats count(),# x"] {
        assert!(parse(dsl).is_err(), "{dsl:?} must be a parse error");
    }
}

// ── spans index the original input ───────────────────────────────────

/// Chumsky's spans index the real input text, so an error after a comment
/// lands where the user can see it.
#[test]
fn a_span_after_a_comment_indexes_the_original_input() {
    let input = "# note\nfoo bar baz |";
    let err = one_error(input);
    assert_eq!(
        err.span,
        input.len()..input.len(),
        "the dangling pipe is at end of input"
    );

    // …and one inside the line the comment precedes
    let input = "# a longer note\nfoo#bar";
    let err = one_error(input);
    let at = input.find("foo#bar").expect("present") + 3;
    assert_eq!(err.span, at..at + 1);
    assert_eq!(&input[err.span.clone()], "#");

    // …and a multi-line comment run before the offending token
    let input = "# one\n# two\ncolor=#ff0000";
    let err = one_error(input);
    let at = input.rfind('#').expect("present");
    assert_eq!(err.span, at..at + 1);
    assert_eq!(err.message, MSG_OPENER_VALUE);
}

// ── the boundary is ASCII whitespace ─────────────────────────────────

/// A comment opens after ASCII whitespace or at the start of input and
/// after nothing else, because that is exactly where the unquoted token
/// charsets end. A no-break space is an ordinary character inside a bare
/// word or value, so a `#` behind one is inside a token, and the outcome
/// is the token error: loud, never a silently different query.
#[test]
fn a_comment_opens_only_after_ascii_whitespace() {
    for ws in [" ", "\t", "\n", "\r\n"] {
        let dsl = format!("a=1{ws}# note");
        let query = ok(&dsl);
        assert_eq!(filters(&query), vec![&eq("a", "1")], "{dsl:?}");
    }

    // …and a Unicode space is not a comment boundary in either lane.
    for (dsl, msg) in [
        ("message=\"x\"\u{a0}# note", MSG_OPENER),
        ("foo\u{a0}# note", MSG_OPENER),
        // …inside the value, whose own escape quotes the value alone
        ("a=1\u{2003}# note", MSG_OPENER_VALUE),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, msg, "{dsl:?}");
        assert_eq!(&dsl[err.span.clone()], "#", "{dsl:?}");
    }

    // A no-break space separates nothing, so `a=1\u{a0}host=x` is one
    // filter whose value carries the space.
    let query = ok("a=1\u{a0}host=x");
    assert_eq!(filters(&query), vec![&eq("a", "1\u{a0}host=x")]);
}

// ── diagnostics are bounded (parse-time, not just on the wire) ───────

/// One query can carry unboundedly many independent violations, and each
/// one renders a hint quoting the input. Both halves are bounded: the
/// hint is cut to the comma-delimited element the user must quote, and
/// the reported list is capped before anything is rendered — so a
/// 64 KiB list of `#`-bearing elements costs a constant number of
/// constant-sized diagnostics, not their product.
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

    // …and the hint still names the element, not the rest of the list
    let err = &errors[0];
    assert_eq!(err.message, MSG_OPENER_VALUE);
    assert_eq!(
        err.hint.as_deref(),
        Some("quote the value (\"#a\"), or put whitespace before the '#' to start a comment")
    );
}

/// The cap keeps the error that ended the parse, whatever the emitted
/// list does to the budget.
///
/// The list is not purely positional: chumsky appends the terminal
/// failure last, so a plain truncation would keep eight value diagnostics
/// and drop `unknown command 'bogus_stage'`, the one error the query
/// cannot succeed without fixing. The report is the first `CAP - 1` plus
/// that final one, whose message says how many were left out.
#[test]
fn the_cap_keeps_the_terminal_error() {
    /// Mirrors `parser::MAX_REPORTED_ERRORS`, which is private.
    const CAP: usize = 8;

    // Eight value diagnostics ahead of a `#` in the stage name.
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

    // …and twelve ahead of an unknown stage, whose message is the one a
    // user must act on.
    let filters: Vec<String> = (0..12).map(|i| format!("f{i}=#a")).collect();
    let dsl = format!("{} | bogus_stage", filters.join(" "));
    let errors = parse(&dsl).expect_err("thirteen violations");
    assert_eq!(errors.len(), CAP);
    assert_eq!(
        errors[CAP - 1].message,
        "unknown command 'bogus_stage' (5 earlier errors omitted)"
    );
    assert_eq!(&dsl[errors[CAP - 1].span.clone()], "b");

    // …and an uncapped list is untouched: no annotation, no reordering.
    let errors = parse("f=#a,#b | bogus_stage").expect_err("three violations");
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[0].message, MSG_OPENER_VALUE);
    assert_eq!(errors[2].message, "unknown command 'bogus_stage'");
}
