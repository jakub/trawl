// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a search-stage field filter means (ADR-0060), as a table.
//!
//! Each row maps query text to its meaning, in a notation this file
//! prints from the public AST. Nothing here calls `format`: a misreading
//! the parser and the formatter make the same way twice would pass a
//! format-then-reparse check, so the parser is checked against what the
//! author wrote instead.
//!
//! The notation (spans are ignored):
//!
//! - `ff(field op scalar(kind,"text"))`: a field filter. The operator is
//!   the one typed, and `!=` stays `!=`; it is never folded into `not`.
//! - `ff(field op list[scalar(..), …])`: a comma list, in written order.
//! - `text("t")`, `-text("t")`, `phrase("p")`: text searches.
//! - `not(..)`: `NOT`.
//! - `grp(a b|c)`: a group. A space joins with AND and `|` with OR.
//! - `… |> head(1)`: a pipe stage after the search stage.
//! - `err("fragment" @ a..b)`: the query's one error, a fragment of its
//!   message and its byte span.

use std::fmt;
use std::ops::Range;

use trawl_core::ast::{FilterOp, FilterValue, PipeStage, Query, Scalar, SearchToken, Spanned};
use trawl_core::parser::parse;

// ── The notation ──────────────────────────────────────────────────────

fn operator(op: FilterOp) -> &'static str {
    match op {
        FilterOp::Eq => "=",
        FilterOp::Ne => "!=",
        FilterOp::Lt => "<",
        FilterOp::Lte => "<=",
        FilterOp::Gt => ">",
        FilterOp::Gte => ">=",
    }
}

fn scalar(s: &Scalar) -> String {
    match s {
        Scalar::Exact(t) => format!("scalar(exact,{t:?})"),
        Scalar::Glob(t) => format!("scalar(glob,{t:?})"),
        Scalar::Regex(t) => format!("scalar(regex,{t:?})"),
    }
}

fn value(v: &FilterValue) -> String {
    match v {
        FilterValue::Single(s) => scalar(s),
        FilterValue::List(items) => {
            let items: Vec<String> = items.iter().map(scalar).collect();
            format!("list[{}]", items.join(", "))
        }
    }
}

fn token(t: &SearchToken) -> String {
    match t {
        SearchToken::FieldFilter(ff) => {
            format!("ff({} {} {})", ff.field, operator(ff.op), value(&ff.value))
        }
        SearchToken::TextSearch(ts) if ts.negated => format!("-text({:?})", ts.term),
        SearchToken::TextSearch(ts) => format!("text({:?})", ts.term),
        SearchToken::QuotedSearch(q) => format!("phrase({:?})", q.phrase),
        SearchToken::Not(inner) => format!("not({})", token(&inner.node)),
        SearchToken::Group(groups) => format!("grp({})", or_groups(groups)),
        other => panic!("the table has no notation for {other:?}"),
    }
}

fn and_group(tokens: &[Spanned<SearchToken>]) -> String {
    let tokens: Vec<String> = tokens.iter().map(|t| token(&t.node)).collect();
    tokens.join(" ")
}

fn or_groups(groups: &[Vec<Spanned<SearchToken>>]) -> String {
    let groups: Vec<String> = groups.iter().map(|g| and_group(g)).collect();
    groups.join("|")
}

fn stage(s: &PipeStage) -> String {
    match s {
        PipeStage::Limit(limit) => format!("{}({})", limit.keyword, limit.count),
        other => panic!("the table has no notation for {other:?}"),
    }
}

fn meaning(query: &Query) -> String {
    let mut out = or_groups(&query.search.groups);
    for s in &query.pipeline {
        out.push_str(" |> ");
        out.push_str(&stage(&s.node));
    }
    out
}

/// What a query is expected to read as.
#[derive(Clone)]
enum Want {
    Means(String),
    Error {
        fragment: &'static str,
        span: Range<usize>,
    },
}

impl fmt::Display for Want {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Means(m) => f.write_str(m),
            Self::Error { fragment, span } => {
                write!(f, "err({fragment:?} @ {}..{})", span.start, span.end)
            }
        }
    }
}

fn means(m: impl Into<String>) -> Want {
    Want::Means(m.into())
}

fn err(fragment: &'static str, span: Range<usize>) -> Want {
    Want::Error { fragment, span }
}

/// How `dsl` actually reads, in the notation.
fn read(dsl: &str) -> String {
    match parse(dsl) {
        Ok(query) => meaning(&query),
        Err(errors) => {
            let errors: Vec<String> = errors
                .iter()
                .map(|e| format!("err({:?} @ {}..{})", e.message, e.span.start, e.span.end))
                .collect();
            errors.join(" ")
        }
    }
}

/// Whether `dsl` reads as `want`. An error row holds when the query
/// reports exactly one error, whose message holds the fragment and whose
/// span is the one stated.
fn holds(dsl: &str, want: &Want) -> bool {
    match (want, parse(dsl)) {
        (Want::Means(m), Ok(query)) => meaning(&query) == *m,
        (Want::Error { fragment, span }, Err(errors)) => match &errors[..] {
            [one] => one.message.contains(fragment) && one.span == *span,
            _ => false,
        },
        _ => false,
    }
}

fn check_all(rows: &[(String, Want)]) {
    let failures: Vec<String> = rows
        .iter()
        .filter(|(dsl, want)| !holds(dsl, want))
        .map(|(dsl, want)| format!("{dsl:?}\n   want {want}\n    got {}", read(dsl)))
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} rows read wrong:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
}

// ── The table ─────────────────────────────────────────────────────────

const OPERATORS: [&str; 6] = ["=", "!=", "<", "<=", ">", ">="];

/// The message fragments of the errors the table expects.
const ORDERED_GLOB: &str = "glob patterns only support `=` and `!=`";
const ORDERED_REGEX: &str = "regex patterns only support `=` and `!=`";
const ORDERED_LIST: &str = "comma-separated lists only support `=` and `!=`";
const INVALID_REGEX: &str = "invalid regex";
const QUOTE_AFTER_REGEX: &str = "a regex ends at its closing '/'";
const OPENER_IN_VALUE: &str = "'#' inside an unquoted value";

/// A regex body over the 1024-byte limit in bytes, but not in characters:
/// 513 two-byte characters.
fn long_multibyte_body() -> String {
    "é".repeat(513)
}

/// How a value reads under an ordered operator, given how it reads
/// under `=`/`!=`.
#[derive(Clone, Copy)]
enum Ordered {
    /// The same value: an ordered comparison of it.
    Compares,
    /// An error from the operator through the end of the value.
    Refused(&'static str),
    /// The list error, over the whole filter.
    RefusedList,
}

/// How a value reads under every operator.
enum Reads {
    /// A value, under `=`/`!=`, with what an ordered operator does to it.
    Value(String, Ordered),
    /// The same error under every operator, over the value alone.
    Error(&'static str),
}

/// Operator × value kind (ADR-0060): every operator, with every kind of
/// value the search stage can hold.
fn operator_rows() -> Vec<(String, Want)> {
    let exact = |t: &str| format!("scalar(exact,{t:?})");
    let glob = |t: &str| format!("scalar(glob,{t:?})");
    let regex = |t: &str| format!("scalar(regex,{t:?})");
    let list = |items: &[String]| format!("list[{}]", items.join(", "));
    let long = format!("/{}/", long_multibyte_body());

    let values: Vec<(String, Reads)> = vec![
        // bare exact
        ("web".into(), Reads::Value(exact("web"), Ordered::Compares)),
        // quoted exact
        (
            r#""a b""#.into(),
            Reads::Value(exact("a b"), Ordered::Compares),
        ),
        // glob
        (
            "web*".into(),
            Reads::Value(glob("web*"), Ordered::Refused(ORDERED_GLOB)),
        ),
        // a quoted glob is exact text, so an ordered operator compares it
        (
            r#""web*""#.into(),
            Reads::Value(exact("web*"), Ordered::Compares),
        ),
        // regex
        (
            "/x+/".into(),
            Reads::Value(regex("x+"), Ordered::Refused(ORDERED_REGEX)),
        ),
        // a quoted regex shape is exact text too
        (
            r#""/x+/""#.into(),
            Reads::Value(exact("/x+/"), Ordered::Compares),
        ),
        // an invalid regex is an error under every operator, and the only
        // one: no ordered-pattern error on top
        ("/[/".into(), Reads::Error(INVALID_REGEX)),
        // over the limit in bytes, not in characters
        (
            long,
            Reads::Error("regex pattern too long (1026 bytes, max 1024)"),
        ),
        // a list of exact values
        (
            r#"a,"b c",d"#.into(),
            Reads::Value(
                list(&[exact("a"), exact("b c"), exact("d")]),
                Ordered::RefusedList,
            ),
        ),
        // a list with a glob element
        (
            "a,b*".into(),
            Reads::Value(list(&[exact("a"), glob("b*")]), Ordered::RefusedList),
        ),
        // a list with a regex element, first, in the middle and last
        (
            "/x+/,b".into(),
            Reads::Value(list(&[regex("x+"), exact("b")]), Ordered::RefusedList),
        ),
        (
            "a,/x+/,c".into(),
            Reads::Value(
                list(&[exact("a"), regex("x+"), exact("c")]),
                Ordered::RefusedList,
            ),
        ),
        (
            "a,/x+/".into(),
            Reads::Value(list(&[exact("a"), regex("x+")]), Ordered::RefusedList),
        ),
    ];

    let mut rows = Vec::new();
    for op in OPERATORS {
        let ordered = !matches!(op, "=" | "!=");
        for (text, reads) in &values {
            let dsl = format!("f{op}{text}");
            let value_start = "f".len() + op.len();
            let want = match reads {
                Reads::Error(fragment) => err(fragment, value_start..dsl.len()),
                Reads::Value(v, _) if !ordered => means(format!("ff(f {op} {v})")),
                Reads::Value(v, Ordered::Compares) => means(format!("ff(f {op} {v})")),
                Reads::Value(_, Ordered::Refused(fragment)) => err(fragment, "f".len()..dsl.len()),
                Reads::Value(_, Ordered::RefusedList) => err(ORDERED_LIST, 0..dsl.len()),
            };
            rows.push((dsl, want));
        }
    }
    rows
}

/// What follows a closing `/` decides whether it closes a regex
/// (ADR-0060's table): a byte that ends a bare value, or the end, makes a
/// regex; `"` is an error; `#` is the opener error; anything else makes
/// the whole run a bare value.
fn follower_rows() -> Vec<(String, Want)> {
    let rows: Vec<(&str, Want)> = vec![
        // ASCII whitespace, and the end
        ("m=/x/", means(r#"ff(m = scalar(regex,"x"))"#)),
        ("m=/x/ a", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        ("m=/x/\ta", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        ("m=/x/\na", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        ("m=/x/\ra", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        (
            "m=/x/\x0ca",
            means(r#"ff(m = scalar(regex,"x")) text("a")"#),
        ),
        // `)`: a group closes a regex tight
        ("(m=/x/)", means(r#"grp(ff(m = scalar(regex,"x")))"#)),
        (
            "(a OR msg=/x/)",
            means(r#"grp(text("a")|ff(msg = scalar(regex,"x")))"#),
        ),
        // `(` and a backtick begin the next token
        (
            "m=/x/(a)",
            means(r#"ff(m = scalar(regex,"x")) grp(text("a"))"#),
        ),
        (
            "m=/x/`f`=1",
            means(r#"ff(m = scalar(regex,"x")) ff(f = scalar(exact,"1"))"#),
        ),
        // `,`: the list continues, and the regex is read before the list
        // splits, so a body may hold a `,`
        (
            "m=/x/,b",
            means(r#"ff(m = list[scalar(regex,"x"), scalar(exact,"b")])"#),
        ),
        ("m=/a,b/", means(r#"ff(m = scalar(regex,"a,b"))"#)),
        (
            "m=/a,b/,/c/",
            means(r#"ff(m = list[scalar(regex,"a,b"), scalar(regex,"c")])"#),
        ),
        // the body rule is unchanged, so a body may hold whitespace: this
        // is one regex, `tmp OR dir=`, closed by the group
        (
            "(dir=/tmp OR dir=/)",
            means(r#"grp(ff(dir = scalar(regex,"tmp OR dir=")))"#),
        ),
        // `"`: an error over the regex and the quote, and it wins over an
        // invalid body, which is never validated
        (r#"m=/x/"y""#, err(QUOTE_AFTER_REGEX, 2..6)),
        (r#"m=/[/""#, err(QUOTE_AFTER_REGEX, 2..6)),
        // `#`: the ADR-0014 opener error, through the bare value
        ("m=/x/#", err(OPENER_IN_VALUE, 5..6)),
        // anything else: the whole run is a bare value, never validated
        ("path=/api/*", means(r#"ff(path = scalar(glob,"/api/*"))"#)),
        (
            "path=/var/log",
            means(r#"ff(path = scalar(exact,"/var/log"))"#),
        ),
        ("m=/x/1", means(r#"ff(m = scalar(exact,"/x/1"))"#)),
        ("m=/x//", means(r#"ff(m = scalar(exact,"/x//"))"#)),
        ("m=/x/]", means(r#"ff(m = scalar(exact,"/x/]"))"#)),
        ("m=/x/=y", means(r#"ff(m = scalar(exact,"/x/=y"))"#)),
        ("m=/x/?", means(r#"ff(m = scalar(glob,"/x/?"))"#)),
        (
            "m=/x/\u{a0}a",
            means("ff(m = scalar(exact,\"/x/\\u{a0}a\"))"),
        ),
        ("m=/[a/]/", means(r#"ff(m = scalar(exact,"/[a/]/"))"#)),
        ("m=/tmp/s", means(r#"ff(m = scalar(exact,"/tmp/s"))"#)),
        // no trailing flags: the `i` continues the word, so this is a glob
        ("m=/err.*/i", means(r#"ff(m = scalar(glob,"/err.*/i"))"#)),
        // a recognised regex that does not compile is an error, never a
        // glob
        ("msg=/*/", err(INVALID_REGEX, 4..7)),
        ("(a OR msg=/*/)", err(INVALID_REGEX, 10..13)),
        // `!=` with a pattern keeps its operator
        ("host!=*", means(r#"ff(host != scalar(glob,"*"))"#)),
        // #321's spelling: a bare `"*` is a glob, refused under `<`
        (r#"host<"*"#, err(ORDERED_GLOB, 4..7)),
        // spans count bytes after a multibyte prefix (`"é" ` is five)
        (r#""é" m=/[/"#, err(INVALID_REGEX, 7..10)),
        (r#""é" m=/x/"y""#, err(QUOTE_AFTER_REGEX, 7..11)),
        (r#""é" m=/x/#"#, err(OPENER_IN_VALUE, 10..11)),
    ];
    rows.into_iter().map(|(d, w)| (d.to_string(), w)).collect()
}

/// A tight `|` closes a regex too. Its row holds a pipe stage, so it is
/// not a filter the context check can wrap.
const PIPE_FOLLOWER: (&str, &str) = ("m=/x/|head 1", r#"ff(m = scalar(regex,"x")) |> head(1)"#);

fn table() -> Vec<(String, Want)> {
    let mut rows = operator_rows();
    rows.extend(follower_rows());
    rows
}

#[test]
fn meaning_table() {
    let mut rows = table();
    rows.push((PIPE_FOLLOWER.0.to_string(), means(PIPE_FOLLOWER.1)));
    check_all(&rows);
}

// ── Context invariance ────────────────────────────────────────────────

/// The contexts a filter `F` must mean the same in: the text before and
/// after it, and how its tokens read there given how they read alone.
struct Context {
    before: &'static str,
    after: &'static str,
    /// The meaning of the context, from the top-level tokens `F` reads as
    /// alone.
    wrap: fn(&[String]) -> String,
}

const CONTEXTS: [Context; 9] = [
    Context {
        before: "(",
        after: ")",
        wrap: |t| format!("grp({})", t.join(" ")),
    },
    Context {
        before: "(a OR ",
        after: ")",
        wrap: |t| format!("grp(text(\"a\")|{})", t.join(" ")),
    },
    Context {
        before: "(",
        after: " OR a)",
        wrap: |t| format!("grp({}|text(\"a\"))", t.join(" ")),
    },
    Context {
        before: "((",
        after: "))",
        wrap: |t| format!("grp(grp({}))", t.join(" ")),
    },
    // `NOT` takes the first token alone
    Context {
        before: "NOT ",
        after: "",
        wrap: |t| {
            let mut out = format!("not({})", t[0]);
            for rest in &t[1..] {
                out.push(' ');
                out.push_str(rest);
            }
            out
        },
    },
    Context {
        before: "NOT(",
        after: ")",
        wrap: |t| format!("not(grp({}))", t.join(" ")),
    },
    Context {
        before: "",
        after: " | head 1",
        wrap: |t| format!("{} |> head(1)", t.join(" ")),
    },
    Context {
        before: "",
        after: " # c",
        wrap: |t| t.join(" "),
    },
    Context {
        before: "",
        after: "\n| head 1",
        wrap: |t| format!("{} |> head(1)", t.join(" ")),
    },
];

/// The top-level tokens `filter` reads as alone, each in the notation.
fn tokens_alone(filter: &str) -> Vec<String> {
    let query = parse(filter).unwrap_or_else(|e| panic!("{filter:?} must parse: {e:?}"));
    assert!(query.pipeline.is_empty(), "{filter:?} is not a filter");
    match &query.search.groups[..] {
        [group] => group.iter().map(|t| token(&t.node)).collect(),
        other => panic!("{filter:?} reads as {} OR groups", other.len()),
    }
}

/// Every non-error row means the same in every context.
#[test]
fn context_every_meaning_holds_in_every_context() {
    let mut rows = Vec::new();
    for (filter, want) in table() {
        let Want::Means(alone) = &want else { continue };
        let tokens = tokens_alone(&filter);
        // The wrapped meanings come from the tokens read alone, so the
        // alone reading must be the row's own.
        rows.push((filter.clone(), means(alone.clone())));
        for c in &CONTEXTS {
            let dsl = format!("{}{filter}{}", c.before, c.after);
            rows.push((dsl, means((c.wrap)(&tokens))));
        }
    }
    check_all(&rows);
}

/// Every error row gives the same one error in every context, its span
/// moved by the text before the filter.
#[test]
fn context_every_error_holds_in_every_context() {
    let mut rows = Vec::new();
    for (filter, want) in table() {
        let Want::Error { fragment, span } = &want else {
            continue;
        };
        for c in &CONTEXTS {
            let dsl = format!("{}{filter}{}", c.before, c.after);
            let shift = c.before.len();
            rows.push((dsl, err(fragment, span.start + shift..span.end + shift)));
        }
    }
    check_all(&rows);
}

/// The defect that opened ADR-0060: a regex a group closes read as text.
#[test]
fn context_a_regex_a_group_closes_is_a_regex() {
    assert_eq!(
        read("(a OR msg=/x/)"),
        r#"grp(text("a")|ff(msg = scalar(regex,"x")))"#
    );
}
