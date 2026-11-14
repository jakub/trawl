// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The JSON path grammar of `json`, `json_extract_string` and
//! `json_extract`: [`parse`] reads a path into typed [`Step`]s, and
//! [`select`] walks a `serde_json` document by them.
//!
//! The grammar is the subset of `DuckDB`'s that both lanes read the same
//! way, as ADR-0017's amendment of 2026-11-13 lists it:
//!
//! - `$` or the empty string: the root;
//! - `$` then a chain of `.member`, `."quoted member"`, `[n]`, `[-n]` and
//!   `[#-n]` steps;
//! - an RFC 6901 pointer that starts with `/`;
//! - one bare key.
//!
//! Anything else is refused as [`PathErrorKind::Malformed`] (`DuckDB`
//! refuses it too, or it has no reading) or [`PathErrorKind::Unsupported`]
//! (`DuckDB` reads it in a way the evaluator does not copy). The parser
//! reads the whole path or refuses it: it never trims, never keeps a
//! parsed prefix, and never reads a failed rooted path as a bare key. It
//! is one loop over the characters, so a long path cannot exhaust the
//! stack.

use std::iter::Peekable;
use std::str::Chars;

use serde_json::Value;

/// One step of a parsed JSON path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// An object key: `.k`, `."k"`, or a bare key. Misses on anything but
    /// an object.
    Member(String),
    /// A zero-based array element, at most `i64::MAX`: `[n]`. Misses on
    /// anything but an array.
    Index(u64),
    /// The n-th array element from the end, n from 1 to `i64::MAX`:
    /// `[-n]` or `[#-n]`. Misses on anything but an array.
    FromEnd(u64),
    /// A decoded RFC 6901 pointer token. It reads an object key, or an
    /// array element when the token is a canonical decimal (`0` or a
    /// digit string without a leading zero), as `DuckDB` does.
    Token(String),
}

/// Whether a refused path is broken or names a form trawl does not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathErrorKind {
    /// The path has no reading: `$[`, `$.k.`, an unknown escape, an index
    /// out of range or longer than 19 digits, a control character.
    Malformed,
    /// `DuckDB` reads the path, but not as a reader expects or not as a
    /// single value: wildcards, recursive descent, `[-0]`, `[#]`, a bare
    /// key holding `.`.
    Unsupported,
}

/// Why [`parse`] refused a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    /// Malformed or unsupported.
    pub kind: PathErrorKind,
    /// The fault, in words: `expected an array index after [`.
    pub reason: &'static str,
    /// The accepted spelling a refused form most likely meant, when there
    /// is one: `use [#-1] for the last element`.
    pub fix: Option<&'static str>,
}

/// The most digits an index may have, leading zeros included: `DuckDB`
/// refuses a 20-digit index even when its value is small.
const MAX_INDEX_DIGITS: usize = 19;

const QUOTE_THE_MEMBER: &str = r#"quote the member: $."a b""#;
const POINTER_STAR: &str = "a pointer /* reads a key named *";

fn malformed(reason: &'static str, fix: Option<&'static str>) -> PathError {
    PathError {
        kind: PathErrorKind::Malformed,
        reason,
        fix,
    }
}

fn unsupported(reason: &'static str, fix: Option<&'static str>) -> PathError {
    PathError {
        kind: PathErrorKind::Unsupported,
        reason,
        fix,
    }
}

/// Read `path` into steps, or refuse it. An empty step list is the root.
pub fn parse(path: &str) -> Result<Vec<Step>, PathError> {
    if path.is_empty() {
        Ok(Vec::new())
    } else if let Some(rest) = path.strip_prefix('$') {
        parse_rooted(rest)
    } else if let Some(rest) = path.strip_prefix('/') {
        parse_pointer(rest)
    } else {
        parse_bare(path).map(|key| vec![Step::Member(key)])
    }
}

/// Walk `doc` by `steps`. `None` is a miss: a step met the wrong kind of
/// value, an absent key, or an index past the end. A present JSON `null`
/// is `Some(&Value::Null)`.
pub fn select<'v>(steps: &[Step], doc: &'v Value) -> Option<&'v Value> {
    let mut at = doc;
    for step in steps {
        at = match (step, at) {
            (Step::Member(key) | Step::Token(key), Value::Object(map)) => map.get(key)?,
            (Step::Index(i), Value::Array(items)) => items.get(usize::try_from(*i).ok()?)?,
            (Step::FromEnd(n), Value::Array(items)) => {
                items.get(items.len().checked_sub(usize::try_from(*n).ok()?)?)?
            }
            (Step::Token(token), Value::Array(items)) => items.get(canonical_index(token)?)?,
            _ => return None,
        };
    }
    Some(at)
}

/// A pointer token as an array index: `0`, or digits without a leading
/// zero that fit `usize`. `01`, `+1`, `-1`, `1e0` and the empty token
/// are not indexes.
fn canonical_index(token: &str) -> Option<usize> {
    let canonical = token == "0"
        || (token.starts_with(|c: char| matches!(c, '1'..='9'))
            && token.bytes().all(|b| b.is_ascii_digit()));
    if canonical { token.parse().ok() } else { None }
}

/// The steps after a leading `$`.
fn parse_rooted(rest: &str) -> Result<Vec<Step>, PathError> {
    let mut steps = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        let step = match c {
            '.' => parse_member(&mut chars)?,
            '[' => parse_index(&mut chars)?,
            _ => return Err(malformed("a step starts with . or [", None)),
        };
        steps.push(step);
    }
    Ok(steps)
}

/// A member after its `.`: quoted or unquoted.
fn parse_member(chars: &mut Peekable<Chars<'_>>) -> Result<Step, PathError> {
    match chars.peek() {
        Some('.') => {
            return Err(unsupported("recursive descent (..) is not supported", None));
        }
        Some('*') => {
            return Err(unsupported(
                "a wildcard member (.*) is not supported",
                Some(POINTER_STAR),
            ));
        }
        Some('"') => {
            chars.next();
            return parse_quoted_member(chars);
        }
        _ => {}
    }
    let mut key = String::new();
    while let Some(&c) = chars.peek() {
        if c == '.' || c == '[' {
            break;
        }
        if c.is_control() {
            return Err(malformed(
                "a member name cannot hold a control character",
                None,
            ));
        }
        if c.is_whitespace() {
            return Err(unsupported(
                "an unquoted member cannot hold whitespace",
                Some(QUOTE_THE_MEMBER),
            ));
        }
        match c {
            '*' => {
                return Err(unsupported(
                    "an unquoted member cannot hold *",
                    Some(r#"quote the member: $."a*b""#),
                ));
            }
            ']' => {
                return Err(malformed(
                    "an unquoted member cannot hold ]",
                    Some(r#"quote the member: $."a]""#),
                ));
            }
            '"' => {
                return Err(malformed(
                    "a quote may only open a member",
                    Some(r#"quote the whole member, escaping the quote: $."a\"b""#),
                ));
            }
            '\\' => {
                return Err(malformed(
                    "an unquoted member cannot hold a backslash",
                    Some(r#"quote the member, escaping the backslash: $."a\\b""#),
                ));
            }
            _ => key.push(c),
        }
        chars.next();
    }
    if key.is_empty() {
        return Err(malformed("expected a member name after .", None));
    }
    Ok(Step::Member(key))
}

/// A quoted member after its opening `"`, through its closing `"`.
fn parse_quoted_member(chars: &mut Peekable<Chars<'_>>) -> Result<Step, PathError> {
    let mut key = String::new();
    loop {
        match chars.next() {
            None => return Err(malformed("unterminated quoted member", None)),
            Some('"') => break,
            Some('\\') => match chars.next() {
                Some(c @ ('"' | '\\')) => key.push(c),
                None => return Err(malformed("unterminated quoted member", None)),
                Some(_) => {
                    return Err(malformed(
                        r#"a quoted member allows only the \" and \\ escapes"#,
                        None,
                    ));
                }
            },
            Some(c) if c.is_control() => {
                return Err(malformed(
                    "a quoted member cannot hold a control character",
                    None,
                ));
            }
            Some(c) => key.push(c),
        }
    }
    if key.is_empty() {
        return Err(malformed("a quoted member cannot be empty", None));
    }
    if key == "*" {
        return Err(unsupported(
            "a quoted * is a wildcard, which is not supported",
            Some(POINTER_STAR),
        ));
    }
    Ok(Step::Member(key))
}

/// An index after its `[`, through its `]`.
fn parse_index(chars: &mut Peekable<Chars<'_>>) -> Result<Step, PathError> {
    let step = match chars.peek() {
        Some('*') => {
            return Err(unsupported("a wildcard index ([*]) is not supported", None));
        }
        Some('\'' | '"') => {
            return Err(malformed(
                "a key in brackets is not supported",
                Some(r#"use a quoted member: $."k""#),
            ));
        }
        Some('#') => {
            chars.next();
            match chars.peek() {
                Some(']') => {
                    return Err(unsupported(
                        "[#] is one past the last element",
                        Some("use [#-1] for the last element"),
                    ));
                }
                Some('-') => {
                    chars.next();
                    from_end(parse_digits(chars)?)?
                }
                _ => return Err(malformed("expected - after # in an index", None)),
            }
        }
        Some('-') => {
            chars.next();
            from_end(parse_digits(chars)?)?
        }
        Some(c) if c.is_ascii_digit() => Step::Index(parse_digits(chars)?),
        None | Some(_) => return Err(malformed("expected an array index after [", None)),
    };
    match chars.next() {
        Some(']') => Ok(step),
        _ => Err(malformed("expected ] after the index", None)),
    }
}

fn from_end(n: u64) -> Result<Step, PathError> {
    if n == 0 {
        return Err(unsupported(
            "a reverse index of 0 is not supported: DuckDB reads it as the first element",
            Some("use [0] for the first element, [#-1] for the last"),
        ));
    }
    Ok(Step::FromEnd(n))
}

/// The ASCII digits of an index: one to 19 of them, leading zeros
/// included, worth at most `i64::MAX`.
fn parse_digits(chars: &mut Peekable<Chars<'_>>) -> Result<u64, PathError> {
    let mut digits = 0_usize;
    let mut value = 0_u64;
    while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
        chars.next();
        digits += 1;
        if digits > MAX_INDEX_DIGITS {
            return Err(malformed(
                "an index has at most 19 digits, leading zeros included",
                None,
            ));
        }
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(d)))
            .ok_or_else(index_too_large)?;
    }
    if digits == 0 {
        return Err(malformed("expected digits in the index", None));
    }
    if i64::try_from(value).is_err() {
        return Err(index_too_large());
    }
    Ok(value)
}

fn index_too_large() -> PathError {
    malformed("an index cannot exceed 9223372036854775807", None)
}

/// The tokens of an RFC 6901 pointer after its leading `/`. Splitting on
/// `/` first and decoding each token once, left to right, reads `~01` as
/// `~1`.
fn parse_pointer(rest: &str) -> Result<Vec<Step>, PathError> {
    let mut steps = Vec::new();
    for raw in rest.split('/') {
        let mut token = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            match c {
                '~' => match chars.next() {
                    Some('0') => token.push('~'),
                    Some('1') => token.push('/'),
                    _ => {
                        return Err(malformed(
                            "a pointer allows only the ~0 and ~1 escapes",
                            Some("write ~ as ~0 and / as ~1"),
                        ));
                    }
                },
                c if c.is_control() => {
                    return Err(malformed("a pointer cannot hold a control character", None));
                }
                c => token.push(c),
            }
        }
        steps.push(Step::Token(token));
    }
    Ok(steps)
}

/// One bare key: a path that starts with neither `$` nor `/`. A control
/// character anywhere makes it malformed; otherwise the first character
/// both lanes would read differently from a reader decides the refusal.
fn parse_bare(path: &str) -> Result<String, PathError> {
    if path.chars().any(char::is_control) {
        return Err(malformed(
            "a bare key cannot hold a control character",
            None,
        ));
    }
    if path == "*" {
        return Err(unsupported(
            "a bare * is a wildcard, which is not supported",
            Some(POINTER_STAR),
        ));
    }
    for c in path.chars() {
        let refusal = match c {
            '.' => unsupported(
                "a bare key cannot hold .",
                Some(r#"use $.a.b to read nested keys, or $."a.b" for one key named a.b"#),
            ),
            '[' | ']' => unsupported(
                "a bare key cannot hold [ or ]",
                Some(r#"use $[0] to index an array, or $."a[0]" for one key"#),
            ),
            '"' | '\\' => unsupported(
                "a bare key cannot hold a quote or backslash",
                Some(r#"use a quoted member, escaping \" and \\: $."a\"b""#),
            ),
            '*' => unsupported(
                "a bare key cannot hold *",
                Some("a pointer such as /a*b reads a key holding *"),
            ),
            c if c.is_whitespace() => unsupported(
                "a bare key cannot hold whitespace",
                Some(r#"use a quoted member: $." k""#),
            ),
            _ => continue,
        };
        return Err(refusal);
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{PathErrorKind, Step, parse, select};
    use serde_json::{Value, json};

    fn m(key: &str) -> Step {
        Step::Member(key.to_owned())
    }

    fn t(token: &str) -> Step {
        Step::Token(token.to_owned())
    }

    #[test]
    fn json_path_grammar_accepts_each_documented_form() {
        let max = 9_223_372_036_854_775_807_u64;
        let cases: Vec<(&str, Vec<Step>)> = vec![
            ("", vec![]),
            ("$", vec![]),
            ("$.k", vec![m("k")]),
            ("$.a.b", vec![m("a"), m("b")]),
            ("$.user-agent", vec![m("user-agent")]),
            ("$.@t", vec![m("@t")]),
            ("$.é", vec![m("é")]),
            ("$.$", vec![m("$")]),
            ("$.a'b/~#", vec![m("a'b/~#")]),
            ("$.0", vec![m("0")]),
            (r#"$."a.b""#, vec![m("a.b")]),
            (r#"$."a b""#, vec![m("a b")]),
            (r#"$."q\"x""#, vec![m("q\"x")]),
            (r#"$."a\\b""#, vec![m("a\\b")]),
            (r#"$."a*b""#, vec![m("a*b")]),
            (r#"$."a]""#, vec![m("a]")]),
            (r#"$."[0]""#, vec![m("[0]")]),
            ("$[0]", vec![Step::Index(0)]),
            ("$[01]", vec![Step::Index(1)]),
            ("$[-1]", vec![Step::FromEnd(1)]),
            ("$[#-2]", vec![Step::FromEnd(2)]),
            ("$.m[0][1]", vec![m("m"), Step::Index(0), Step::Index(1)]),
            (r#"$[0]."k".j"#, vec![Step::Index(0), m("k"), m("j")]),
            ("$[0000000000000000001]", vec![Step::Index(1)]),
            ("$[-0000000000000000001]", vec![Step::FromEnd(1)]),
            ("$[#-0000000000000000001]", vec![Step::FromEnd(1)]),
            ("$[0000000000000000000]", vec![Step::Index(0)]),
            ("$[9223372036854775807]", vec![Step::Index(max)]),
            ("$[-9223372036854775807]", vec![Step::FromEnd(max)]),
            ("$[#-9223372036854775807]", vec![Step::FromEnd(max)]),
            ("/k", vec![t("k")]),
            ("/m/0/1", vec![t("m"), t("0"), t("1")]),
            ("/a~1b", vec![t("a/b")]),
            ("/a~0b", vec![t("a~b")]),
            ("/~01", vec![t("~1")]),
            ("/~10", vec![t("/0")]),
            ("/", vec![t("")]),
            ("//", vec![t(""), t("")]),
            ("/*", vec![t("*")]),
            ("/0", vec![t("0")]),
            ("/ 1", vec![t(" 1")]),
            (r#"/a.b[0]"'\"#, vec![t(r#"a.b[0]"'\"#)]),
            ("level", vec![m("level")]),
            ("user-agent", vec![m("user-agent")]),
            ("0", vec![m("0")]),
            ("@t", vec![m("@t")]),
            ("é", vec![m("é")]),
            ("a$", vec![m("a$")]),
            ("a/b", vec![m("a/b")]),
        ];
        for (path, steps) in cases {
            assert_eq!(parse(path), Ok(steps), "path {path:?}");
        }
    }

    #[test]
    fn json_path_grammar_refuses_malformed_and_unsupported() {
        use PathErrorKind::{Malformed, Unsupported};
        let cases: &[(&str, PathErrorKind, &str)] = &[
            ("$[", Malformed, "expected an array index after ["),
            ("$.k.", Malformed, "expected a member name after ."),
            ("$.", Malformed, "expected a member name after ."),
            ("$.[0]", Malformed, "expected a member name after ."),
            ("$x", Malformed, "a step starts with . or ["),
            (r#"$."a"b"#, Malformed, "a step starts with . or ["),
            ("$[0]x", Malformed, "a step starts with . or ["),
            ("$ ", Malformed, "a step starts with . or ["),
            ("$['k']", Malformed, "a key in brackets"),
            ("$[1:2]", Malformed, "expected ] after the index"),
            ("$[0", Malformed, "expected ] after the index"),
            ("$[0 ]", Malformed, "expected ] after the index"),
            (r#"$."k"#, Malformed, "unterminated quoted member"),
            (r#"$."a\"#, Malformed, "unterminated quoted member"),
            (r#"$."""#, Malformed, "a quoted member cannot be empty"),
            ("$[+1]", Malformed, "expected an array index after ["),
            ("$[ 1 ]", Malformed, "expected an array index after ["),
            ("$[x]", Malformed, "expected an array index after ["),
            ("$[-]", Malformed, "expected digits"),
            ("$[-x]", Malformed, "expected digits"),
            ("$[#-]", Malformed, "expected digits"),
            ("$[#+1]", Malformed, "expected - after #"),
            ("$[#1]", Malformed, "expected - after #"),
            ("$[9223372036854775808]", Malformed, "9223372036854775807"),
            ("$[-9223372036854775808]", Malformed, "9223372036854775807"),
            ("$[#-9999999999999999999]", Malformed, "9223372036854775807"),
            ("$[00000000000000000001]", Malformed, "at most 19 digits"),
            ("$[-00000000000000000001]", Malformed, "at most 19 digits"),
            ("$[#-00000000000000000001]", Malformed, "at most 19 digits"),
            ("$[99999999999999999999]", Malformed, "at most 19 digits"),
            ("$[٣]", Malformed, "expected an array index after ["),
            (r#"$."a\nb""#, Malformed, "escape"),
            (r#"$."a\é""#, Malformed, "escape"),
            ("$.\"a\nb\"", Malformed, "control character"),
            ("$.k\u{1}", Malformed, "control character"),
            ("$.a]", Malformed, "]"),
            (r#"$.a"b"#, Malformed, "quote"),
            (r"$.a\b", Malformed, "backslash"),
            ("/a~2b", Malformed, "~0 and ~1"),
            ("/a~", Malformed, "~0 and ~1"),
            ("/a\u{7}", Malformed, "control character"),
            ("a\u{1}", Malformed, "control character"),
            ("a.\u{1}", Malformed, "control character"),
            ("$.*", Unsupported, "wildcard"),
            ("$.*.k", Unsupported, "wildcard"),
            ("$[*]", Unsupported, "wildcard"),
            ("$..k", Unsupported, "recursive descent"),
            (r#"$."*""#, Unsupported, "wildcard"),
            ("*", Unsupported, "wildcard"),
            ("$.a*b", Unsupported, "*"),
            ("$.a b", Unsupported, "whitespace"),
            ("$.k ", Unsupported, "whitespace"),
            ("$[-0]", Unsupported, "reverse index of 0"),
            ("$[#-0]", Unsupported, "reverse index of 0"),
            ("$[-00]", Unsupported, "reverse index of 0"),
            ("$[#-000]", Unsupported, "reverse index of 0"),
            ("$[#]", Unsupported, "[#]"),
            ("a.b", Unsupported, "."),
            (".k", Unsupported, "."),
            ("[0]", Unsupported, "[ or ]"),
            ("a]", Unsupported, "[ or ]"),
            (" k", Unsupported, "whitespace"),
            ("k ", Unsupported, "whitespace"),
            ("a\"b", Unsupported, "quote"),
            ("a\\b", Unsupported, "quote"),
            ("a*b", Unsupported, "*"),
        ];
        for &(path, kind, fault) in cases {
            let err = parse(path).expect_err(path);
            assert_eq!(err.kind, kind, "path {path:?}: {err:?}");
            assert!(
                err.reason.contains(fault),
                "path {path:?}: reason {:?} should name {fault:?}",
                err.reason
            );
        }
    }

    /// Each refused form that has a better spelling points to it.
    #[test]
    fn json_path_refusals_point_to_the_fix() {
        let cases: &[(&str, &str)] = &[
            ("$[-0]", "[#-1]"),
            ("$[-0]", "[0]"),
            ("$[#]", "[#-1]"),
            ("$.*", "/*"),
            (r#"$."*""#, "/*"),
            ("*", "/*"),
            ("a.b", "$.a.b"),
            ("a.b", r#"$."a.b""#),
            ("[0]", "$[0]"),
            (" k", r#"$." k""#),
            ("$['k']", r#"$."k""#),
            ("$.a b", r#"$."a b""#),
        ];
        for &(path, fix) in cases {
            let err = parse(path).expect_err(path);
            let got = err.fix.unwrap_or_default();
            assert!(
                got.contains(fix),
                "path {path:?}: fix {got:?} should hold {fix:?}"
            );
        }
    }

    /// A rooted path that fails never falls back to a bare key, and a
    /// path is never trimmed.
    #[test]
    fn json_path_never_recovers_or_trims() {
        for path in ["$x", "$k", "$[", " $.k", "$.k ", "/a~", "$ .k"] {
            assert!(parse(path).is_err(), "path {path:?} must be refused");
        }
    }

    #[test]
    fn json_path_select_reads_by_container() {
        let cases: Vec<(&str, Value, Option<Value>)> = vec![
            ("$", json!(1), Some(json!(1))),
            ("", json!({"a": 1}), Some(json!({"a": 1}))),
            ("$.k", json!({"k": 1}), Some(json!(1))),
            ("$.k", json!({"k": null}), Some(Value::Null)),
            ("$.k", json!([1]), None),
            ("$.k", json!(1), None),
            ("$.k", json!("k"), None),
            ("$.k", Value::Null, None),
            ("$.k", json!(true), None),
            ("$.a.b", json!({"a": {"b": 2}}), Some(json!(2))),
            ("$.a.b", json!({"a": [2]}), None),
            ("$[0]", json!([7, 8]), Some(json!(7))),
            ("$[0]", json!({"0": 1}), None),
            ("$[0]", json!("x"), None),
            ("$[0]", json!(0), None),
            ("$[2]", json!([7, 8]), None),
            ("$[01]", json!([7, 8]), Some(json!(8))),
            ("$[9223372036854775807]", json!([1]), None),
            ("$[-1]", json!([7, 8, 9]), Some(json!(9))),
            ("$[#-2]", json!([7, 8, 9]), Some(json!(8))),
            ("$[-3]", json!([7, 8, 9]), Some(json!(7))),
            ("$[-4]", json!([7, 8, 9]), None),
            ("$[-1]", json!([]), None),
            ("$[-1]", json!({"-1": 1}), None),
            ("$[-9223372036854775807]", json!([1]), None),
            ("$.m[0][1]", json!({"m": [[1, 2]]}), Some(json!(2))),
            ("/k", json!({"k": 1}), Some(json!(1))),
            ("/k", json!([1]), None),
            ("/k", json!("k"), None),
            ("/0", json!({"0": "o"}), Some(json!("o"))),
            ("/0", json!([5]), Some(json!(5))),
            ("/1", json!([5, 6]), Some(json!(6))),
            ("/2", json!([5, 6]), None),
            ("/01", json!([5, 6]), None),
            ("/01", json!({"01": 1}), Some(json!(1))),
            ("/00", json!([5, 6]), None),
            ("/+1", json!([5, 6]), None),
            ("/-1", json!([5, 6]), None),
            ("/-", json!([5, 6]), None),
            ("/1e0", json!([5, 6]), None),
            ("/ 1", json!([5, 6]), None),
            ("/1 ", json!([5, 6]), None),
            ("/", json!([5, 6]), None),
            ("/99999999999999999999999999", json!([5, 6]), None),
            ("/", json!({"": 1}), Some(json!(1))),
            ("//", json!({"": {"": 2}}), Some(json!(2))),
            ("/a~1b", json!({"a/b": 1}), Some(json!(1))),
            ("/a~0b", json!({"a~b": 1}), Some(json!(1))),
            ("/*", json!({"*": 1}), Some(json!(1))),
            ("/m/0/1", json!({"m": [[1, 2]]}), Some(json!(2))),
            ("level", json!({"level": "info"}), Some(json!("info"))),
            ("0", json!([5]), None),
            ("0", json!({"0": 5}), Some(json!(5))),
        ];
        for (path, doc, want) in cases {
            let steps = parse(path).unwrap_or_else(|e| panic!("{path:?}: {e:?}"));
            assert_eq!(
                select(&steps, &doc),
                want.as_ref(),
                "path {path:?} on {doc}"
            );
        }
    }

    /// Steps built by hand, outside the parser, still miss rather than
    /// wrap or panic.
    #[test]
    fn json_path_select_misses_on_out_of_grammar_steps() {
        let doc = json!([1, 2]);
        assert_eq!(select(&[Step::FromEnd(0)], &doc), None);
        assert_eq!(select(&[Step::FromEnd(u64::MAX)], &doc), None);
        assert_eq!(select(&[Step::Index(u64::MAX)], &doc), None);
    }

    /// Every string up to four characters over the grammar's alphabet:
    /// the parser answers without panicking, and an accepted bare key is
    /// exactly one member holding the whole input.
    #[test]
    fn json_path_parse_never_panics_on_short_inputs() {
        const ALPHABET: [char; 18] = [
            '$', '.', '[', ']', '"', '\\', '*', '#', '-', '0', '1', '/', '~', 'a', ' ', '\n', '\'',
            'é',
        ];
        let docs = [json!({"a": [1, {"0": 2}]}), json!([[0], 1]), json!("a")];
        let mut frontier = vec![String::new()];
        let mut parsed = 0_usize;
        for depth in 0..=4 {
            let mut next = Vec::new();
            for path in &frontier {
                parsed += 1;
                match parse(path) {
                    Ok(steps) => {
                        if !path.is_empty() && !path.starts_with(['$', '/']) {
                            assert_eq!(steps, vec![Step::Member(path.clone())], "{path:?}");
                        }
                        for step in &steps {
                            if let Step::Index(n) | Step::FromEnd(n) = step {
                                assert!(i64::try_from(*n).is_ok(), "{path:?}");
                            }
                            assert_ne!(step, &Step::FromEnd(0), "{path:?}");
                        }
                        for doc in &docs {
                            let _ = select(&steps, doc);
                        }
                    }
                    Err(err) => assert!(!err.reason.is_empty(), "{path:?}"),
                }
                for c in ALPHABET.iter().copied().filter(|_| depth < 4) {
                    let mut longer = path.clone();
                    longer.push(c);
                    next.push(longer);
                }
            }
            frontier = next;
        }
        assert_eq!(parsed, 1 + 18 + 18 * 18 + 18 * 18 * 18 + 18 * 18 * 18 * 18);
    }

    /// The parser is a loop, not a recursion: a path of 100 000 steps
    /// parses and selects without growing the stack.
    #[test]
    fn json_path_long_paths_parse_iteratively() {
        let path = format!("${}", "[0]".repeat(100_000));
        let steps = parse(&path).expect("long index chain");
        assert_eq!(steps.len(), 100_000);
        assert_eq!(select(&steps, &json!([1])), None);

        let path = format!("${}", ".a".repeat(100_000));
        assert_eq!(parse(&path).map(|s| s.len()), Ok(100_000));

        let path = "/".repeat(100_000);
        assert_eq!(parse(&path).map(|s| s.len()), Ok(100_000));
    }
}
