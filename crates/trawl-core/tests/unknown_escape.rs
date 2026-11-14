// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An unknown escape in a quoted string is one parse error at the
//! backslash pair (ADR-0068, "An unknown escape is reported where it is
//! lexed").
//!
//! A quoted string decodes `\\`, `\"`, `\n`, `\t` and `\r`. Any other
//! backslash is an emitted error over the backslash and the character
//! after it, and the string still closes, so no later arm reads the same
//! text again. The raw positions (extract and rex bodies, `/…/` regexes,
//! bare words and values, comments) decode nothing and do not change.

use std::ops::Range;

use trawl_core::ast::{
    ExtractMode, FieldFilter, FilterOp, FilterValue, Scalar, SearchToken, TextSearch,
};
use trawl_core::parser::{ParseError, parse};

/// The head of the unknown-escape message for `\s`.
const UNKNOWN_ESCAPE_S: &str =
    r#"unknown escape \s: a quoted string decodes only \\, \", \n, \t and \r"#;
const UNREADABLE_BOUND: &str = "cannot read this time bound as a timestamp";
const UNTERMINATED: &str = "unterminated string literal";
const QUOTE_OPENS_VALUE: &str = "a value that opens with '\"' must be a complete quoted string";

/// One expected error: a message and its span.
type Expected = (&'static str, Range<usize>);

fn errors(dsl: &str) -> Vec<ParseError> {
    match parse(dsl) {
        Ok(query) => panic!("{dsl:?} must not parse, got {query:?}"),
        Err(errors) => errors,
    }
}

/// The whole error list, as (message, span) pairs.
fn error_list(dsl: &str) -> Vec<(String, Range<usize>)> {
    errors(dsl)
        .into_iter()
        .map(|e| (e.message, e.span))
        .collect()
}

fn want(list: &[Expected]) -> Vec<(String, Range<usize>)> {
    list.iter()
        .map(|(m, span)| ((*m).to_string(), span.clone()))
        .collect()
}

/// The survey query: one error, at the pair, naming the three ways out.
#[test]
fn unknown_escape_in_matches_operand() {
    let dsl = r#"* | where m matches "^\s+at ""#;
    let errors = errors(dsl);
    let [one] = &errors[..] else {
        panic!("{dsl:?} must report exactly one error: {errors:?}");
    };
    assert_eq!(one.message, UNKNOWN_ESCAPE_S);
    assert_eq!(one.span, 22..24);
    assert_eq!(&dsl[one.span.clone()], r"\s");
    let hint = one.hint.as_deref().expect("the escape error has a hint");
    assert!(hint.contains(r"\\s"), "{hint:?}");
    assert!(hint.contains("a /…/ regex"), "{hint:?}");
    assert!(hint.contains(r"matches /^\s+at /"), "{hint:?}");
    assert!(hint.contains("extract"), "{hint:?}");
}

/// Every ordinary quoted string reports the escape first, at the pair, and
/// each site's whole error list is pinned: a closed string never falls
/// back to a scanner that reports it again.
#[test]
fn unknown_escape_reports_at_every_quoted_site() {
    let rows: Vec<(&str, Vec<Expected>)> = vec![
        // the search stage
        (r#""a\sb""#, vec![(UNKNOWN_ESCAPE_S, 2..4)]),
        (r#"x "a\sb" y"#, vec![(UNKNOWN_ESCAPE_S, 4..6)]),
        (r#"NOT "a\sb""#, vec![(UNKNOWN_ESCAPE_S, 6..8)]),
        (r#"(a OR "a\sb")"#, vec![(UNKNOWN_ESCAPE_S, 8..10)]),
        (r#"host="a\sb""#, vec![(UNKNOWN_ESCAPE_S, 7..9)]),
        (r#"host!="a\sb""#, vec![(UNKNOWN_ESCAPE_S, 8..10)]),
        // an ordered operator with a quoted value adds nothing on top
        (r#"host>="a\sb""#, vec![(UNKNOWN_ESCAPE_S, 8..10)]),
        (r#"host=a,"b\sb""#, vec![(UNKNOWN_ESCAPE_S, 9..11)]),
        (r#"host="a\sb",c"#, vec![(UNKNOWN_ESCAPE_S, 7..9)]),
        // an ordered operator with a list: the escape is the one error
        (r#"host<a,"b\sb""#, vec![(UNKNOWN_ESCAPE_S, 9..11)]),
        // two escapes, two errors, in source order
        (
            r#"host="a\sb","c\sd""#,
            vec![(UNKNOWN_ESCAPE_S, 7..9), (UNKNOWN_ESCAPE_S, 14..16)],
        ),
        // a time bound: the escape first, then the bound the kept value
        // cannot be read as, which is true of the corrected query too
        (
            r#"earliest="2026\s""#,
            vec![(UNKNOWN_ESCAPE_S, 14..16), (UNREADABLE_BOUND, 0..17)],
        ),
        (
            r#"latest="2026\s""#,
            vec![(UNKNOWN_ESCAPE_S, 12..14), (UNREADABLE_BOUND, 0..15)],
        ),
        // pipe stages
        (r#"* | from saved "a\sb""#, vec![(UNKNOWN_ESCAPE_S, 17..19)]),
        (r#"| from saved "a\sb""#, vec![(UNKNOWN_ESCAPE_S, 15..17)]),
        (r#"* | where m == "a\sb""#, vec![(UNKNOWN_ESCAPE_S, 17..19)]),
        (r#"* | where "a\sb" == m"#, vec![(UNKNOWN_ESCAPE_S, 12..14)]),
        (
            r#"* | where m like "a\sb""#,
            vec![(UNKNOWN_ESCAPE_S, 19..21)],
        ),
        (
            r#"* | where m ilike "a\sb""#,
            vec![(UNKNOWN_ESCAPE_S, 20..22)],
        ),
        (r#"* | let x = "a\sb""#, vec![(UNKNOWN_ESCAPE_S, 14..16)]),
        (r#"* | eval x = "a\sb""#, vec![(UNKNOWN_ESCAPE_S, 15..17)]),
        (
            r#"* | where contains(m, "a\sb")"#,
            vec![(UNKNOWN_ESCAPE_S, 24..26)],
        ),
        (
            r#"* | stats count_if(m == "a\sb")"#,
            vec![(UNKNOWN_ESCAPE_S, 26..28)],
        ),
        (
            r#"* | stats max(coalesce(m, "a\sb"))"#,
            vec![(UNKNOWN_ESCAPE_S, 28..30)],
        ),
    ];
    let failures: Vec<String> = rows
        .iter()
        .filter_map(|(dsl, list)| {
            let got = error_list(dsl);
            (got != want(list)).then(|| format!("{dsl:?}\n   want {list:?}\n    got {got:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    // the span covers the backslash and the whole following character
    let dsl = "\"a\\éb\"";
    let got = error_list(dsl);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(&dsl[got[0].1.clone()], "\\é");
    assert!(got[0].0.starts_with("unknown escape \\é:"), "{got:?}");
}

/// What the escape arm must not change: the end of input, an unclosed
/// filter value's fallback, and the five escapes.
#[test]
fn unknown_escape_edges_keep_existing_errors() {
    // a backslash at the end of input is still an unterminated string
    let got = errors(r#""a\"#);
    let [one] = &got[..] else { panic!("{got:?}") };
    assert_eq!(one.message, UNTERMINATED);
    assert_eq!(one.span, 3..3);

    // an unclosed quoted filter value falls back to the bare scanner, and
    // its error is the one it had; the escape the failed quoted arm saw is
    // discarded with that arm
    let got = errors(r#"host="a\sb"#);
    let [one] = &got[..] else { panic!("{got:?}") };
    assert_eq!(one.message, QUOTE_OPENS_VALUE);
    assert_eq!(one.span, 5..10);
    assert_eq!(one.hint.as_deref(), Some("close the quote"));

    // an unclosed quoted search or expression string has no fallback
    assert_eq!(
        error_list(r#""a\sb"#),
        want(&[(UNTERMINATED, 5..5)]),
        "quoted search"
    );
    assert_eq!(
        error_list(r#"* | where m == "a\sb"#),
        want(&[(UNTERMINATED, 20..20)]),
        "expression string"
    );

    // the five escapes decode as before
    let query = parse(r#""\\ \" \n \t \r""#).expect("the five escapes parse");
    let [group] = &query.search.groups[..] else {
        panic!("{query:?}")
    };
    let [token] = &group[..] else {
        panic!("{query:?}")
    };
    let SearchToken::QuotedSearch(q) = &token.node else {
        panic!("{query:?}")
    };
    assert_eq!(q.phrase, "\\ \" \n \t \r");
}

/// Assert `dsl` parses and its tree holds `node`, compared by `Debug`.
fn holds_node(dsl: &str, node: &impl std::fmt::Debug) {
    let query = parse(dsl).unwrap_or_else(|e| panic!("{dsl:?} must parse: {e:?}"));
    let tree = format!("{query:?}");
    let node = format!("{node:?}");
    assert!(tree.contains(&node), "{dsl:?}: {node} not in {tree}");
}

/// The raw positions decode nothing, so a backslash there stays valid and
/// stays in the tree as written.
#[test]
fn unknown_escape_leaves_raw_positions_alone() {
    // extract and rex bodies are raw strings
    for kw in ["extract", "rex"] {
        holds_node(
            &format!(r#"* | {kw} "(?<x>\d+)" from m"#),
            &ExtractMode::Regex(r"(?<x>\d+)".to_string()),
        );
    }
    // a negated bare term lexes `-` and every non-space byte, quotes too
    holds_node(
        r#"-"a\sb""#,
        &SearchToken::TextSearch(TextSearch {
            term: r#""a\sb""#.to_string(),
            negated: true,
        }),
    );
    holds_node(
        r"a\sb",
        &SearchToken::TextSearch(TextSearch {
            term: r"a\sb".to_string(),
            negated: false,
        }),
    );
    holds_node(
        r"host=a\sb",
        &SearchToken::FieldFilter(FieldFilter {
            field: "host".to_string(),
            op: FilterOp::Eq,
            value: FilterValue::Single(Scalar::Exact(r"a\sb".to_string())),
        }),
    );
    holds_node(
        r#"f=/"\s"/"#,
        &SearchToken::FieldFilter(FieldFilter {
            field: "f".to_string(),
            op: FilterOp::Eq,
            value: FilterValue::Single(Scalar::Regex(r#""\s""#.to_string())),
        }),
    );
    holds_node(
        r#"* | where m matches /"\s"/"#,
        &trawl_core::ast::LiteralValue::String(r#""\s""#.to_string()),
    );
    // a comment is not lexed at all
    assert_eq!(
        parse("a # \"\\s\"").expect("a comment holding an escape parses"),
        parse("a").expect("a parses")
    );
}
