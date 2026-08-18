// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ADR-0014's ruling table, as a test.
//!
//! Every row of the ADR preamble (the shapes the retired pre-parse
//! scanner answered SILENTLY wrong) and every row of the issue's
//! acceptance table appears here with its exact AST or its exact error.
//! Both directions are covered on purpose: a shape that must parse is
//! checked for what it MEANS, not merely that it parsed, because the
//! whole class this work retires is "parsed successfully as a different
//! query".

use trawl_core::ast::{
    BinaryOp, Expr, FieldFilter, FilterOp, FilterValue, LiteralValue, PipeStage, Query,
    SearchToken, TextSearch,
};
use trawl_core::parser::{ParseError, parse};

/// The messages a comment opener inside an unquoted token carries. One
/// rule, but the WORKING SPELLING differs by position, so the position
/// rides in the message and each one's hint offers an escape that keeps
/// the query MEANING what it meant.
const MSG_OPENER: &str = "'#' inside an unquoted token";
const MSG_OPENER_VALUE: &str = "'#' inside an unquoted value";
const MSG_OPENER_NEGATED: &str = "'#' inside a negated search term";
const MSG_OPENER_COMMAND: &str = "'#' inside a pipe stage name";
/// The one message a bare term opening with the retired `//` carries.
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

// ── the shapes that PARSE, and what they mean ────────────────────────

/// A comment after whitespace is a comment, and only the comment.
#[test]
fn a_whitespace_preceded_hash_opens_a_comment() {
    // `a=1 # note` — the filter survives, the comment does not reach the AST
    let query = ok("a=1 # note");
    assert_eq!(filters(&query), vec![&eq("a", "1")]);
    assert!(query.pipeline.is_empty());

    // a comment ENDS at the newline, so the next line is grammar again
    let query = ok("status=200 # c\nhost=x");
    assert_eq!(
        filters(&query),
        vec![&eq("status", "200"), &eq("host", "x")]
    );

    // …and the same one substitution covers a stage boundary
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
/// This is what retires the regex residual.
#[test]
fn quoted_contexts_carry_the_opener_verbatim() {
    // `message=/a#b/` — the regex, NOT an equality on the literal "/a"
    let query = ok("message=/a#b/");
    assert_eq!(
        filters(&query),
        vec![&FieldFilter {
            field: "message".to_string(),
            op: FilterOp::Regex,
            value: FilterValue::Literal("a#b".to_string()),
        }]
    );

    // a backtick-quoted NAME (ADR-0013 ruling 7)
    let query = ok("`a#b`=1");
    assert_eq!(filters(&query), vec![&eq("a#b", "1")]);

    // a double-quoted VALUE
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

/// `//` is ordinary data now (ruling 3), in every value position — and
/// the sibling filter the blanking scanner deleted comes back.
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

    // `referrer=https://a.b/c status=200` — BOTH filters, correctly
    let query = ok("referrer=https://a.b/c status=200");
    assert_eq!(
        filters(&query),
        vec![&eq("referrer", "https://a.b/c"), &eq("status", "200")]
    );

    // a `//` inside a bare TERM is fine too — only a term that OPENS
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

// ── the shapes that ERROR, and exactly how ───────────────────────────

/// `foo#bar`, `color=#ff0000`, `a=1# note` — one error each, spanning
/// the `#` byte, with a hint whose advice a user can FOLLOW (ruling 2).
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
            format!("{hint_fragment}, or put whitespace before the '#' to start a comment"),
            "{dsl:?}"
        );
    }
}

/// The value hint names the value the production HELD, never a slice
/// re-derived from the raw text.
///
/// `=`, `<`, `>` and `!` are all legal INSIDE a bare value, so a left
/// boundary found by scanning backwards for one of them cuts a URL at its
/// own query string: `url=…?a=b#frag` was hinted `quote the value
/// ("b#frag")`, which is a different filter on a different value. The
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
            "quote the value (\"https://example.test/p?a=b#frag\"), \
             or put whitespace before the '#' to start a comment"
        )
    );
    // …and the spelling it offers is the query the user meant
    ok("url=\"https://example.test/p?a=b#frag\"");
}

/// …and an IN list is still hinted per ELEMENT, because an element IS
/// its own value production: the comma bounding comes free with the exact
/// slice, in every position of the list.
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
}

/// A hint offers quoting only where `"…"` is a rewrite the grammar reads
/// back unchanged. Two shapes where it is not, and both drop that half
/// rather than print advice that cannot be followed:
///
/// * a token carrying a `"` or a `\` would need an escape the hint does
///   not spell, so the pasted string would end at its own second quote;
/// * a run the GENERIC net recovered can span several grammar tokens —
///   `count(),#` is not a token any position accepts.
///
/// The whitespace half is always true, so it is what remains.
#[test]
fn a_hint_drops_advice_the_grammar_cannot_read() {
    const COMMENT_HALF: &str = "put whitespace before the '#' to start a comment";

    for (dsl, msg) in [
        // an embedded quote inside a value
        ("f=a\"b#c", MSG_OPENER_VALUE),
        // …and a recovered run that is not one token, in a pipeline
        // position where quoting is not the fix at all
        ("* | stats count(),# x", MSG_OPENER),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, msg, "{dsl:?}");
        assert_eq!(err.hint.as_deref(), Some(COMMENT_HALF), "{dsl:?}");
    }

    // …while a term the production HELD is quotable even when the text
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
    ok("message=\"x\"\u{a0}\"#\" note");
}

/// Quoting is no escape under `-`: `-"a#b"` is a bare NEGATED term that
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
        // …and the spelling the hint offers is one the grammar accepts
        ok(working);
    }
}

/// A `#` in a pipe STAGE NAME is the comment rule, not a typo. chumsky
/// reports the start of the word — the stage word ends at the `#` — and
/// the unknown-command rule then invents a command the user never wrote
/// (`unknown command 'co'`, "did you mean 'top'?").
#[test]
fn an_opener_in_a_stage_name_is_not_an_unknown_command() {
    let err = one_error("* | co#unt()");
    assert_eq!(err.message, MSG_OPENER_COMMAND);
    assert_eq!(err.span, 6..7);
    assert_eq!(
        err.hint.as_deref(),
        Some("put whitespace before the '#' to start a comment")
    );

    // …and the rule is narrow: the token holding the offset must BE the
    // stage word, so a shape whose real problem is the `,` keeps its own
    // diagnostic on its own byte.
    let err = one_error("* | stats count(),# x");
    assert_eq!(err.message, MSG_OPENER);
    assert_eq!(err.span, 18..19);
    let err = one_error("* | staats count()");
    assert_eq!(err.message, "unknown command 'staats'");
}

/// A bare term OPENING with `//` names `#` as the comment character —
/// the loud half of dropping the second opener (ruling 3).
#[test]
fn a_term_opening_with_slashes_names_the_real_opener() {
    // …in the search stage (the validator's own error) and in the
    // pipeline (the generic net). BOTH slashes are the span in either
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

// ── spans without the length invariant (AC-5) ────────────────────────

/// The retired scanner was byte-length-preserving so that spans still
/// indexed the original input. Chumsky's spans index the real text, so
/// an error AFTER a comment lands where the user can see it.
#[test]
fn a_span_after_a_comment_indexes_the_original_input() {
    let input = "# note\nfoo bar baz |";
    let err = one_error(input);
    assert_eq!(
        err.span,
        input.len()..input.len(),
        "the dangling pipe is at end of input"
    );

    // …and one INSIDE the line the comment precedes
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

/// A comment opens after ASCII whitespace or at the start of input, and
/// after NOTHING else — because that is exactly where the unquoted token
/// charsets end. A no-break space is an ordinary character INSIDE a bare
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
        // …inside the VALUE, whose own escape quotes the value alone
        ("a=1\u{2003}# note", MSG_OPENER_VALUE),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, msg, "{dsl:?}");
        assert_eq!(&dsl[err.span.clone()], "#", "{dsl:?}");
    }

    // Layout CONSUMPTION is untouched: a no-break space separates
    // nothing, exactly as it did before comments were a production —
    // `a=1\u{a0}host=x` is one filter whose value carries the space.
    let query = ok("a=1\u{a0}host=x");
    assert_eq!(filters(&query), vec![&eq("a", "1\u{a0}host=x")]);
}

// ── diagnostics are bounded (parse-time, not just on the wire) ───────

/// One query can carry unboundedly many independent violations, and each
/// one renders a hint quoting the input. Both halves are bounded: the
/// hint is cut to the comma-delimited ELEMENT the user must quote, and
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

/// The cap keeps the error that ENDED the parse, whatever the emitted
/// list does to the budget.
///
/// The list is not purely positional: chumsky appends the terminal
/// failure LAST, so a plain truncation reported eight value diagnostics
/// and dropped `unknown command 'bogus_stage'` — the one error the query
/// cannot succeed without fixing. The report is the first `CAP - 1` plus
/// that final one, whose message says how many were left out.
#[test]
fn the_cap_keeps_the_terminal_error() {
    /// Mirrors `parser::MAX_REPORTED_ERRORS`, which is private.
    const CAP: usize = 8;

    // Eight value diagnostics ahead of a `#` in the STAGE NAME.
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
