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
    FieldFilter, FilterOp, FilterValue, PipeStage, Query, SearchToken, TextSearch,
};
use trawl_core::parser::{ParseError, parse};

/// The one message a comment opener inside an unquoted token carries.
const MSG_OPENER: &str = "'#' inside an unquoted token";
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

    // …and a regex in expression position
    let query = ok("* | where message matches /foo#bar/");
    assert_eq!(query.pipeline.len(), 1);
    assert!(matches!(query.pipeline[0].node, PipeStage::Where(_)));
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
/// the `#` byte, with a hint naming the quoted form (ruling 2).
#[test]
fn an_opener_inside_an_unquoted_token_is_a_loud_error() {
    for (dsl, at, quoted) in [
        ("foo#bar", 3, "foo#bar"),
        ("color=#ff0000", 6, "color=#ff0000"),
        ("a=1# note", 3, "a=1#"),
        ("1=/foo#bar/", 6, "1=/foo#bar/"),
    ] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_OPENER, "{dsl:?}");
        assert_eq!(err.span, at..at + 1, "{dsl:?}: span must be the '#' byte");
        assert_eq!(&dsl[err.span.clone()], "#", "{dsl:?}");
        let hint = err.hint.unwrap_or_else(|| panic!("{dsl:?} must hint"));
        assert!(
            hint.contains(&format!("\"{quoted}\"")),
            "{dsl:?}: hint must name the quoted form, got {hint:?}"
        );
    }
}

/// A bare term OPENING with `//` names `#` as the comment character —
/// the loud half of dropping the second opener (ruling 3).
#[test]
fn a_term_opening_with_slashes_names_the_real_opener() {
    for dsl in ["// note", "service=x // filter by service"] {
        let err = one_error(dsl);
        assert_eq!(err.message, MSG_SLASHES, "{dsl:?}");
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
    assert_eq!(err.message, MSG_OPENER);
}
