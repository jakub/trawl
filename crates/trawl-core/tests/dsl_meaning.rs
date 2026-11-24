// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a search-stage field filter means (ADR-0060), and what the search
//! stage admits (ADR-0061), as tables.
//!
//! Each row maps query text to its meaning, in a notation this file
//! prints from the public AST. Nothing here calls `format`: a misreading
//! the parser and the formatter make the same way twice would pass a
//! format-then-reparse check, so the parser is checked against what the
//! author wrote instead.

use std::fmt;
use std::ops::Range;

use trawl_core::ast::{
    Expr, FilterOp, FilterValue, PipeStage, Query, Scalar, SearchToken, Spanned,
};
use trawl_core::parser::parse;

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

fn expr(e: &Expr) -> String {
    match e {
        Expr::FieldRef(name) => name.clone(),
        Expr::Literal(v) => v.to_string(),
        Expr::Binary { lhs, op, rhs } => {
            format!("({} {op} {})", expr(&lhs.node), expr(&rhs.node))
        }
        Expr::FunctionCall { name, args } => {
            let args: Vec<String> = args.iter().map(|a| expr(&a.node)).collect();
            format!("{name}({})", args.join(", "))
        }
        other => panic!("the table has no notation for {other:?}"),
    }
}

fn stage(s: &PipeStage) -> String {
    match s {
        PipeStage::Limit(limit) => format!("{}({})", limit.keyword, limit.count),
        PipeStage::Where(w) => format!("where({})", expr(&w.condition.node)),
        other => panic!("the table has no notation for {other:?}"),
    }
}

fn meaning(query: &Query) -> String {
    let mut out = or_groups(&query.search.groups);
    let search = &query.search;
    let slots = [
        search
            .time_filter
            .as_ref()
            .map(|t| format!("@last({})", t.node.duration)),
        search
            .earliest
            .as_ref()
            .map(|t| format!("@earliest({:?})", t.node.text())),
        search
            .latest
            .as_ref()
            .map(|t| format!("@latest({:?})", t.node.text())),
    ];
    for slot in slots.into_iter().flatten() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&slot);
    }
    for s in &query.pipeline {
        out.push_str(" |> ");
        out.push_str(&stage(&s.node));
    }
    out
}

#[derive(Clone)]
enum Want {
    Means(String),
    Error {
        fragment: &'static str,
        span: Range<usize>,
    },
    Errors(Vec<(&'static str, Range<usize>)>),
}

impl fmt::Display for Want {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Means(m) => f.write_str(m),
            Self::Error { fragment, span } => {
                write!(f, "err({fragment:?} @ {}..{})", span.start, span.end)
            }
            Self::Errors(errors) => {
                let errors: Vec<String> = errors
                    .iter()
                    .map(|(fragment, span)| format!("{fragment:?} @ {}..{}", span.start, span.end))
                    .collect();
                write!(f, "errs({})", errors.join(", "))
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

fn errs(errors: &[(&'static str, Range<usize>)]) -> Want {
    Want::Errors(errors.to_vec())
}

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

fn holds(dsl: &str, want: &Want) -> bool {
    match (want, parse(dsl)) {
        (Want::Means(m), Ok(query)) => meaning(&query) == *m,
        (Want::Error { fragment, span }, Err(errors)) => match &errors[..] {
            [one] => one.message.contains(fragment) && one.span == *span,
            _ => false,
        },
        (Want::Errors(want), Err(errors)) => {
            errors.len() == want.len()
                && errors
                    .iter()
                    .zip(want)
                    .all(|(e, (fragment, span))| e.message.contains(fragment) && e.span == *span)
        }
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

const OPERATORS: [&str; 6] = ["=", "!=", "<", "<=", ">", ">="];

const ORDERED_GLOB: &str = "glob patterns only support `=` and `!=`";
const ORDERED_REGEX: &str = "regex patterns only support `=` and `!=`";
const ORDERED_LIST: &str = "comma-separated lists only support `=` and `!=`";
const INVALID_REGEX: &str = "invalid regex";
const QUOTE_AFTER_REGEX: &str = "a regex ends at its closing '/'";
const OPENER_IN_VALUE: &str = "'#' inside an unquoted value";
const REGEX_LINE_BREAK: &str = "a regex cannot hold a raw line break";
const REGEX_PIPE_STAGE: &str = "a regex cannot hold '|' after whitespace";
const QUOTE_OPENS_VALUE: &str = "a value that opens with '\"' must be a complete quoted string";
const NESTED_TIME_CLAUSE: &str = "a time clause cannot sit inside parentheses or under NOT";
const OR_ONLY_GROUP: &str = "a group needs a search term or filter, not only OR";
const NOT_DASH_BEFORE_BACKTICK: &str = "'NOT-' cannot sit directly before a backticked name";
const NEGATED_BACKTICK: &str = "found '`'";
const UNKNOWN_ESCAPE: &str = r"unknown escape \s: a quoted string decodes only";

/// A regex body over the 1024-byte limit in bytes, but not in characters:
/// 513 two-byte characters.
fn long_multibyte_body() -> String {
    "é".repeat(513)
}

#[derive(Clone, Copy)]
enum Ordered {
    Compares,
    Refused(&'static str),
    RefusedList,
}

enum Reads {
    Value(String, Ordered),
    Error(&'static str),
}

fn operator_rows() -> Vec<(String, Want)> {
    let exact = |t: &str| format!("scalar(exact,{t:?})");
    let glob = |t: &str| format!("scalar(glob,{t:?})");
    let regex = |t: &str| format!("scalar(regex,{t:?})");
    let list = |items: &[String]| format!("list[{}]", items.join(", "));
    let long = format!("/{}/", long_multibyte_body());

    let values: Vec<(String, Reads)> = vec![
        ("web".into(), Reads::Value(exact("web"), Ordered::Compares)),
        (
            r#""a b""#.into(),
            Reads::Value(exact("a b"), Ordered::Compares),
        ),
        (
            "web*".into(),
            Reads::Value(glob("web*"), Ordered::Refused(ORDERED_GLOB)),
        ),
        (
            r#""web*""#.into(),
            Reads::Value(exact("web*"), Ordered::Compares),
        ),
        (
            "/x+/".into(),
            Reads::Value(regex("x+"), Ordered::Refused(ORDERED_REGEX)),
        ),
        (
            r#""/x+/""#.into(),
            Reads::Value(exact("/x+/"), Ordered::Compares),
        ),
        ("/[/".into(), Reads::Error(INVALID_REGEX)),
        (
            long,
            Reads::Error("regex pattern too long (1026 bytes, max 1024)"),
        ),
        (
            r#"a,"b c",d"#.into(),
            Reads::Value(
                list(&[exact("a"), exact("b c"), exact("d")]),
                Ordered::RefusedList,
            ),
        ),
        (
            "a,b*".into(),
            Reads::Value(list(&[exact("a"), glob("b*")]), Ordered::RefusedList),
        ),
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

fn follower_rows() -> Vec<(String, Want)> {
    let rows: Vec<(&str, Want)> = vec![
        ("m=/x/", means(r#"ff(m = scalar(regex,"x"))"#)),
        ("m=/x/ a", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        ("m=/x/\ta", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        ("m=/x/\na", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        ("m=/x/\ra", means(r#"ff(m = scalar(regex,"x")) text("a")"#)),
        (
            "m=/x/\x0ca",
            means(r#"ff(m = scalar(regex,"x")) text("a")"#),
        ),
        ("(m=/x/)", means(r#"grp(ff(m = scalar(regex,"x")))"#)),
        (
            "(a OR msg=/x/)",
            means(r#"grp(text("a")|ff(msg = scalar(regex,"x")))"#),
        ),
        (
            "m=/x/(a)",
            means(r#"ff(m = scalar(regex,"x")) grp(text("a"))"#),
        ),
        (
            "m=/x/`f`=1",
            means(r#"ff(m = scalar(regex,"x")) ff(f = scalar(exact,"1"))"#),
        ),
        (
            "m=/x/,b",
            means(r#"ff(m = list[scalar(regex,"x"), scalar(exact,"b")])"#),
        ),
        ("m=/a,b/", means(r#"ff(m = scalar(regex,"a,b"))"#)),
        (
            "m=/a,b/,/c/",
            means(r#"ff(m = list[scalar(regex,"a,b"), scalar(regex,"c")])"#),
        ),
        (
            "(dir=/tmp OR dir=/)",
            means(r#"grp(ff(dir = scalar(regex,"tmp OR dir=")))"#),
        ),
        (
            "msg=/err|warn/",
            means(r#"ff(msg = scalar(regex,"err|warn"))"#),
        ),
        ("m=/a| b/", means(r#"ff(m = scalar(regex,"a| b"))"#)),
        ("m=/a\tb/", means(r#"ff(m = scalar(regex,"a\tb"))"#)),
        ("msg=/a\nb/", err(REGEX_LINE_BREAK, 4..9)),
        ("msg=/a\r\nb/", err(REGEX_LINE_BREAK, 4..10)),
        ("f=x,/a\nb/", err(REGEX_LINE_BREAK, 4..9)),
        (r#"m=/x/"y""#, err(QUOTE_AFTER_REGEX, 2..6)),
        (r#"m=/[/""#, err(QUOTE_AFTER_REGEX, 2..6)),
        ("m=/x/#", err(OPENER_IN_VALUE, 5..6)),
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
        ("m=/err.*/i", means(r#"ff(m = scalar(glob,"/err.*/i"))"#)),
        ("msg=/*/", err(INVALID_REGEX, 4..7)),
        ("(a OR msg=/*/)", err(INVALID_REGEX, 10..13)),
        ("host!=*", means(r#"ff(host != scalar(glob,"*"))"#)),
        (r#"host<"*"#, err(QUOTE_OPENS_VALUE, 5..7)),
        (r#"host="*"#, err(QUOTE_OPENS_VALUE, 5..7)),
        (r#"f=a,"b"#, err(QUOTE_OPENS_VALUE, 4..6)),
        // spans count bytes after a multibyte prefix (`"é" ` is five)
        (r#""é" m=/[/"#, err(INVALID_REGEX, 7..10)),
        (r#""é" m=/x/"y""#, err(QUOTE_AFTER_REGEX, 7..11)),
        (r#""é" m=/x/#"#, err(OPENER_IN_VALUE, 10..11)),
        (r#"host="a\sb""#, err(UNKNOWN_ESCAPE, 7..9)),
        (r#"host=a,"b\sb""#, err(UNKNOWN_ESCAPE, 9..11)),
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

/// These rows hold pipe stages, time clauses and groups, so they are not
/// filters the context check can wrap.
fn admission_rows() -> Vec<(String, Want)> {
    let glob_then_division = r#"ff(path = scalar(glob,"/tmp*")) |> where(((x / 2) == 1))"#;
    let rows: Vec<(&str, Want)> = vec![
        ("path=/tmp* | where x / 2 == 1", means(glob_then_division)),
        ("path=/tmp*\n| where x / 2 == 1", means(glob_then_division)),
        (
            "path=/tmp*\n  | where x / 2 == 1",
            means(glob_then_division),
        ),
        (
            "msg=/err | warn/",
            errs(&[
                (REGEX_PIPE_STAGE, 4..16),
                ("unknown command 'warn'", 12..13),
            ]),
        ),
        ("a last=1h OR b", means(r#"text("a")|text("b") @last(1h)"#)),
        (
            "last=1h a OR b last=2h",
            means(r#"text("a")|text("b") @last(2h)"#),
        ),
        ("last=1h NOT", means(r#"text("NOT") @last(1h)"#)),
        ("(last=1h)", err(NESTED_TIME_CLAUSE, 1..8)),
        ("(a OR last=1h)", err(NESTED_TIME_CLAUSE, 6..13)),
        ("NOT last=1h", err(NESTED_TIME_CLAUSE, 4..11)),
        ("NOT (last=1h)", err(NESTED_TIME_CLAUSE, 5..12)),
        ("NOT NOT last=1h", err(NESTED_TIME_CLAUSE, 8..15)),
        (
            r#"NOT earliest="2026-01-01T00:00:00Z""#,
            err(NESTED_TIME_CLAUSE, 4..35),
        ),
        ("(OR)", err(OR_ONLY_GROUP, 0..4)),
        ("a (or OR) b", err(OR_ONLY_GROUP, 2..9)),
        ("(a OR)", means(r#"grp(text("a"))"#)),
        ("NOT-`x`=1", err(NOT_DASH_BEFORE_BACKTICK, 0..4)),
        (
            "NOT- `x`=1",
            means(r#"not(text("-")) ff(x = scalar(exact,"1"))"#),
        ),
        ("-`x`=1", err(NEGATED_BACKTICK, 1..2)),
    ];
    rows.into_iter().map(|(d, w)| (d.to_string(), w)).collect()
}

#[test]
fn admission_table() {
    check_all(&admission_rows());
}

const PATTERN_NOT_LITERAL: &str = "a pattern must be literal text in the query";
const REGEX_TOO_LONG: &str = "regex pattern too long (1025 bytes, max 1024)";
const REGEX_TOO_LARGE: &str = "regex pattern too large once compiled";
const BATCH_TOO_LARGE: &str = "regex pattern too large for the batch engine once compiled";
const CLASS_SET_SYNTAX: &str = "nested character classes and the class set operators";

fn refused_at(dsl: &str, part: &str, fragment: &'static str) -> (String, Want) {
    let start = dsl
        .find(part)
        .unwrap_or_else(|| panic!("{part:?} is not in {dsl:?}"));
    (dsl.to_string(), err(fragment, start..start + part.len()))
}

#[allow(clippy::too_many_lines)]
fn pattern_rows() -> Vec<(String, Want)> {
    let over = "a".repeat(1025);
    let quoted_over = format!("\"{over}\"");
    let four_bytes = "\u{1F600}".repeat(62);
    let four_bytes_over = "\u{1F600}".repeat(63);
    let glob_at_limit = format!("*[{}]", "a".repeat(1021));
    vec![
        refused_at(
            r#"* | where message matches lower("a")"#,
            r#"lower("a")"#,
            PATTERN_NOT_LITERAL,
        ),
        refused_at(
            "* | where message matches field",
            "field",
            PATTERN_NOT_LITERAL,
        ),
        refused_at("* | where host like field", "field", PATTERN_NOT_LITERAL),
        refused_at(
            r#"* | where host ilike concat("a", "%")"#,
            r#"concat("a", "%")"#,
            PATTERN_NOT_LITERAL,
        ),
        refused_at(
            r#"* | where message matches "a" + "b""#,
            r#""a" + "b""#,
            PATTERN_NOT_LITERAL,
        ),
        refused_at("* | where code like 5", "5", PATTERN_NOT_LITERAL),
        (
            "x | where message matches /a/".to_string(),
            means(r#"text("x") |> where((message matches "a"))"#),
        ),
        (
            r#"x | where message matches "a""#.to_string(),
            means(r#"text("x") |> where((message matches "a"))"#),
        ),
        (
            r#"x | where host like "a%""#.to_string(),
            means(r#"text("x") |> where((host like "a%"))"#),
        ),
        (
            r#"x | where host ilike ("a%")"#.to_string(),
            means(r#"text("x") |> where((host ilike "a%"))"#),
        ),
        refused_at(r#"x | where m matches "[""#, r#""[""#, INVALID_REGEX),
        refused_at(
            &format!("x | where m matches {quoted_over}"),
            &quoted_over,
            REGEX_TOO_LONG,
        ),
        refused_at(
            r#"x | where m matches "\\w{64}""#,
            r#""\\w{64}""#,
            REGEX_TOO_LARGE,
        ),
        refused_at(r#"x | extract "[""#, r#""[""#, INVALID_REGEX),
        refused_at(
            &format!("x | extract {quoted_over}"),
            &quoted_over,
            REGEX_TOO_LONG,
        ),
        // The named group is one position, so `\w{63}` fills the budget.
        refused_at(
            r#"x | extract "(?P<w>\w{63})""#,
            r#""(?P<w>\w{63})""#,
            REGEX_TOO_LARGE,
        ),
        refused_at(
            r#"x | where m matches "[\\pL&&a]""#,
            r#""[\\pL&&a]""#,
            CLASS_SET_SYNTAX,
        ),
        refused_at(
            "x | where m matches /[a-z--b]/",
            "/[a-z--b]/",
            CLASS_SET_SYNTAX,
        ),
        refused_at(
            r#"x | extract "(?P<x>[a[b]])""#,
            r#""(?P<x>[a[b]])""#,
            CLASS_SET_SYNTAX,
        ),
        refused_at(
            r"x | where m matches /\B(?:\pL?){15}\PL/",
            r"/\B(?:\pL?){15}\PL/",
            BATCH_TOO_LARGE,
        ),
        refused_at(
            r#"x | extract "(?P<x>\B(?:\pL?){15}\PL)""#,
            r#""(?P<x>\B(?:\pL?){15}\PL)""#,
            BATCH_TOO_LARGE,
        ),
        refused_at(
            &format!("host=*{over}"),
            &format!("*{over}"),
            "glob pattern too long (1026 bytes, max 1024)",
        ),
        refused_at(
            &format!("x | where host like {quoted_over}"),
            &quoted_over,
            "like pattern too long (1025 bytes, max 1024)",
        ),
        refused_at(
            &format!("x | where host ilike {quoted_over}"),
            &quoted_over,
            "ilike pattern too long (1025 bytes, max 1024)",
        ),
        (
            format!("host={glob_at_limit}"),
            means(format!("ff(host = scalar(glob,{glob_at_limit:?}))")),
        ),
        (
            format!("x | where host like \"{four_bytes}\""),
            means(format!(
                "text(\"x\") |> where((host like \"{four_bytes}\"))"
            )),
        ),
        (
            format!("x | where host ilike \"{four_bytes}\""),
            means(format!(
                "text(\"x\") |> where((host ilike \"{four_bytes}\"))"
            )),
        ),
        refused_at(
            &format!("x | where host like \"{four_bytes_over}\""),
            &format!("\"{four_bytes_over}\""),
            "pattern repeats too much (65 positions, max 64)",
        ),
    ]
}

#[test]
fn pattern_table() {
    check_all(&pattern_rows());
}

struct Context {
    before: &'static str,
    after: &'static str,
    /// The meaning of the context, from the tokens the filter reads as alone.
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

fn tokens_alone(filter: &str) -> Vec<String> {
    let query = parse(filter).unwrap_or_else(|e| panic!("{filter:?} must parse: {e:?}"));
    assert!(query.pipeline.is_empty(), "{filter:?} is not a filter");
    match &query.search.groups[..] {
        [group] => group.iter().map(|t| token(&t.node)).collect(),
        other => panic!("{filter:?} reads as {} OR groups", other.len()),
    }
}

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

#[test]
fn context_a_regex_a_group_closes_is_a_regex() {
    assert_eq!(
        read("(a OR msg=/x/)"),
        r#"grp(text("a")|ff(msg = scalar(regex,"x")))"#
    );
}
