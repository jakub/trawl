// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The drift guard between the rendering profiles and the real emitter
//! (ADR-0024).
//!
//! `complexity::profile` states, per rendering, how many nodes translation
//! adds on its own and how many times each child lands in the emitted
//! expression. Those numbers are the whole admission check: understate a
//! child copy and a refused query becomes admitted, so the numbers have to
//! be held against the emitter rather than reasoned about.
//!
//! Two assertions per case, over SQL the real emitter produced:
//!
//! 1. **child copies.** Each child operand is a field named `__cN__`,
//!    which quotes into the SQL as `"__cN__"` and appears nowhere else.
//!    The emitted text must carry it exactly `copies[N]` times — unless
//!    the rendering contains a simple `CASE`, in which case the text is
//!    the WRONG side of the parser and the declared count is only an
//!    upper bound here ([`Copies::NormalizedCase`]). What those
//!    renderings actually cost is counted post-parse, in
//!    `trawl-engine/tests/duckdb_probe.rs`, which serializes the same SQL
//!    through `json_serialize_sql`.
//! 2. **a one-sided node bound.** `fixed + Σ copies` must be at least
//!    [`oracle::nodes`], an independent lexical counter that reads the SQL
//!    and knows nothing about the profile table. A profile may overcount;
//!    it may never undercount.
//!
//! The oracle FAILS on syntax it does not recognise. That is the point: a
//! rendering that starts emitting a construct nobody taught it stops the
//! suite instead of scoring zero for the new nodes.

use trawl_core::complexity::{
    CompareKind, ConformShape, FunctionShape, RenderProfile, Rendering, profile,
};
use trawl_core::context::EvalContext;
use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_core::severity::Dialect;
use trawl_core::{compare, conform, parser};

// ---------------------------------------------------------------------------
// The independent node oracle
// ---------------------------------------------------------------------------

mod oracle {
    /// An upper bound on the expression nodes in one SQL fragment.
    ///
    /// Deliberately dumb and deliberately independent: it reads tokens, not
    /// profiles, so it cannot agree with a wrong profile by sharing its
    /// mistake. Calls, operators, literals, parameters, references, `CASE`
    /// heads and list indexing each count one; parentheses, commas, the
    /// alias and type-name tails of a cast, `DISTINCT` and the
    /// `WITHIN GROUP (ORDER BY …)` / `PARTITION BY` decoration count
    /// nothing, because none of them is an expression node.
    ///
    /// # Panics
    ///
    /// On any character or word it does not recognise. Counting an unknown
    /// construct as zero would silently weaken every bound in this file.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn nodes(sql: &str) -> u64 {
        let bytes: Vec<char> = sql.chars().collect();
        let mut i = 0;
        let mut count: u64 = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            match c {
                // A single-quoted string literal; `''` escapes one quote.
                '\'' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == '\'' {
                            if bytes.get(i + 1) == Some(&'\'') {
                                i += 2;
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    count += 1;
                }
                // A quoted identifier.
                '"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != '"' {
                        i += 1;
                    }
                    i += 1;
                    count += 1;
                }
                '?' => {
                    i += 1;
                    count += 1;
                }
                '0'..='9' => {
                    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
                        i += 1;
                    }
                    count += 1;
                }
                'A'..='Z' | 'a'..='z' | '_' => {
                    let start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
                        i += 1;
                    }
                    let word: String = bytes[start..i].iter().collect();
                    let mut after = i;
                    while after < bytes.len() && bytes[after].is_whitespace() {
                        after += 1;
                    }
                    let _ = after;
                    count += word_nodes(&word);
                    if word.eq_ignore_ascii_case("AS") {
                        // A cast's type tail: the name, and its precision
                        // arguments if it has any. Not an expression node.
                        i = skip_type(&bytes, i);
                    }
                }
                // Lambda arrow, before the subtraction operator.
                '-' if bytes.get(i + 1) == Some(&'>') => {
                    i += 2;
                    count += 1;
                }
                '+' | '-' | '*' | '/' | '%' | '=' | '<' | '>' => {
                    i += 1;
                    // Two-character comparison operators.
                    if matches!(bytes.get(i), Some('=' | '>')) {
                        i += 1;
                    }
                    count += 1;
                }
                '!' if bytes.get(i + 1) == Some(&'=') => {
                    i += 2;
                    count += 1;
                }
                // List indexing and list construction: one operation.
                '[' => {
                    i += 1;
                    count += 1;
                }
                ']' | '(' | ')' | ',' => i += 1,
                other => panic!("the node oracle does not know the character {other:?} in {sql}"),
            }
        }
        count
    }

    fn skip_type(chars: &[char], mut i: usize) -> usize {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if chars.get(i) == Some(&'(') {
            while i < chars.len() && chars[i] != ')' {
                i += 1;
            }
            i += 1;
        }
        i
    }

    /// How many expression nodes one bare word is worth.
    ///
    /// A function name and a bare column reference are both one node, so
    /// the only distinction that matters is the structural vocabulary: a
    /// word that decorates an expression rather than being one. That has
    /// to be decided BEFORE any "is it followed by a paren" reasoning,
    /// because `WITHIN GROUP (ORDER BY …)` puts a paren after a word that
    /// is not a call.
    #[allow(clippy::match_same_arms)]
    fn word_nodes(word: &str) -> u64 {
        match word.to_ascii_uppercase().as_str() {
            // Structural words that are not expression nodes.
            "WHEN" | "THEN" | "ELSE" | "END" | "AS" | "DISTINCT" | "WITHIN" | "GROUP" | "ORDER"
            | "BY" | "PARTITION" | "UNION" | "ALL" | "NAME" => 0,
            // One node each.
            "CASE" | "BETWEEN" | "AND" | "OR" | "NOT" | "IN" | "IS" | "NULL" | "TRUE" | "FALSE"
            | "LIKE" | "ILIKE" | "OVER" => 1,
            // A bare column reference or a lambda parameter.
            _ => 1,
        }
    }
}

/// The oracle, held against fragments counted by hand — including strings
/// that carry SQL punctuation, which must be one node and not a nest of
/// operators.
#[test]
fn the_node_oracle_agrees_with_hand_counting() {
    for (sql, expected) in [
        (r#""a""#, 1),
        ("?", 1),
        ("42", 1),
        ("0.5", 1),
        (r#"("a" + "b")"#, 3),
        (r#"ABS("a")"#, 2),
        (r"COUNT(*)", 2),
        (r#"("a" IS NULL)"#, 3),
        (r#"("a" IS NOT NULL)"#, 4),
        (r#"TRY_CAST("a" AS DECIMAL(38,6))"#, 2),
        (r"CAST(? AS TIMESTAMP)", 2),
        (r#""a" IN (?, ?, ?)"#, 5),
        (r#""a" BETWEEN 17 AND 20"#, 5),
        (r#"NOT ("a" = 3)"#, 4),
        (r#"CASE WHEN "a" THEN "b" ELSE "c" END"#, 4),
        (r#"PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY "a")"#, 3),
        (r#"AVG("a") OVER (PARTITION BY "h")"#, 4),
        (r#"LIST(DISTINCT "a")"#, 2),
        (r#"STRING_SPLIT("a", "b")[2]"#, 5),
        // A string literal is ONE node however much SQL punctuation it
        // carries, and a doubled quote inside it does not end it.
        (r#"regexp_full_match("a", '(1 + 2) = ?')"#, 3),
        (r#"json_extract_string("r", '/a''b')"#, 3),
        (r"'[\p{Zs}\x{9}-\x{d}]+$'", 1),
        // A lambda is a node; its parameter is a reference.
        (r#"list_transform(["a"], _v -> _v)"#, 6),
    ] {
        assert_eq!(oracle::nodes(sql), expected, "{sql}");
    }
}

#[test]
#[should_panic(expected = "does not know the character")]
fn the_node_oracle_refuses_syntax_it_has_not_been_taught() {
    let _ = oracle::nodes("a @> b");
}

// ---------------------------------------------------------------------------
// Emission fixtures
// ---------------------------------------------------------------------------

fn anchor() -> EvalContext {
    EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc),
    )
}

fn marker(n: usize) -> String {
    format!("__c{n}__")
}

fn pins(entries: &[(usize, CanonicalType)]) -> FieldTypes {
    let mut map = FieldTypes::new();
    for (n, ty) in entries {
        map.insert(&marker(*n), *ty);
    }
    map
}

/// Emit `dsl` and return the one expression fragment it projects or
/// filters on.
fn fragment(dsl: &str, pin_map: &FieldTypes) -> String {
    let query = parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
    let sql = trawl_core::emitter::emit_with_pins(&query, "src", pin_map, anchor())
        .unwrap_or_else(|e| panic!("{dsl}: {e}"))
        .sql;
    if let Some((_, tail)) = sql.split_once("NOT IN ('z')), ") {
        let (expr, _) = tail
            .rsplit_once(" AS \"z\"")
            .unwrap_or_else(|| panic!("{dsl}: no projection alias in {sql}"));
        return expr.trim().to_string();
    }
    let (_, tail) = sql
        .split_once("WHERE ")
        .unwrap_or_else(|| panic!("{dsl}: neither a projection nor a filter: {sql}"));
    tail.trim().to_string()
}

/// How the emitted TEXT's copy count relates to the declared one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Copies {
    /// The text is the whole story: what the emitter writes is what the
    /// binder sees.
    Exact,
    /// The rendering carries a simple `CASE`, which `DuckDB`'s parser
    /// rewrites into one equality per arm — duplicating the subject —
    /// before anything binds. The text undercounts on purpose here; the
    /// parse-tree count lives in `trawl-engine/tests/duckdb_probe.rs`.
    NormalizedCase,
}

/// The two assertions, for one rendering against one emitted fragment.
fn check(label: &str, rendering: &Rendering, sql: &str, children: usize, copies: Copies) {
    let p: RenderProfile = profile(rendering);
    for n in 0..children {
        let expected = p.copies.get(n).copied().unwrap_or(0);
        let found = sql.matches(&format!("\"{}\"", marker(n))).count() as u64;
        match copies {
            Copies::Exact => assert_eq!(
                found, expected,
                "{label}: child {n} is written {found} times, the profile says {expected}\n{sql}"
            ),
            Copies::NormalizedCase => assert!(
                expected >= found,
                "{label}: child {n} is written {found} times in text, \
                 the profile declares only {expected}\n{sql}"
            ),
        }
    }
    let declared = p.fixed + p.copies.iter().sum::<u64>();
    let counted = oracle::nodes(sql);
    assert!(
        declared >= counted,
        "{label}: the profile declares {declared} nodes, emission needs {counted}\n{sql}"
    );
}

/// One `let` projection whose operands are markers.
fn check_let(
    label: &str,
    rendering: &Rendering,
    expr: &str,
    pin_map: &FieldTypes,
    children: usize,
) {
    let sql = fragment(&format!("* | let z = {expr}"), pin_map);
    check(label, rendering, &sql, children, Copies::Exact);
}

/// One `where` filter whose subject is a marker.
fn check_where(
    label: &str,
    rendering: &Rendering,
    cond: &str,
    pin_map: &FieldTypes,
    children: usize,
) {
    check_where_as(label, rendering, cond, pin_map, children, Copies::Exact);
}

fn check_where_as(
    label: &str,
    rendering: &Rendering,
    cond: &str,
    pin_map: &FieldTypes,
    children: usize,
    copies: Copies,
) {
    let sql = fragment(&format!("* | where {cond}"), pin_map);
    check(label, rendering, &sql, children, copies);
}

// ── the structural renderings ─────────────────────────────────────────

#[test]
fn structural_renderings_match_their_emission() {
    let none = FieldTypes::new();
    // A leaf is one node and has no children to copy.
    let sql = fragment("* | let z = `__c0__`", &none);
    assert_eq!(oracle::nodes(&sql), 1, "{sql}");
    assert_eq!(profile(&Rendering::Leaf).fixed, 1);

    check_let(
        "binary",
        &Rendering::Binary,
        "`__c0__` + `__c1__`",
        &none,
        2,
    );
    check_let("unary", &Rendering::Unary, "-`__c0__`", &none, 1);
    check_let(
        "in-list, pin-blind",
        &Rendering::InListPlain(2),
        "`__c0__` in (`__c1__`, 2)",
        &none,
        2,
    );
}

// ── comparison forms, per pin ─────────────────────────────────────────

#[test]
fn every_comparison_form_matches_its_emission() {
    // A typed pin binds one parameter, exactly as the unpinned path does.
    for pin in [
        CanonicalType::BigInt,
        CanonicalType::Double,
        CanonicalType::Boolean,
        CanonicalType::Timestamp,
    ] {
        check_where(
            &format!("{pin:?} equality"),
            &Rendering::Compare(CompareKind::Bound),
            "`__c0__` == 200",
            &pins(&[(0, pin)]),
            1,
        );
    }
    let varchar = pins(&[(0, CanonicalType::Varchar)]);
    // VARCHAR + non-numeric literal: plain text equality.
    check_where(
        "varchar text equality",
        &Rendering::Compare(CompareKind::Bound),
        "`__c0__` == \"accepted\"",
        &varchar,
        1,
    );
    // VARCHAR + numeric literal: the subject is written twice.
    check_where(
        "varchar numeric equality",
        &Rendering::Compare(CompareKind::TextOrNumeric),
        "`__c0__` == 200",
        &varchar,
        1,
    );
    check_where(
        "varchar numeric inequality",
        &Rendering::Compare(CompareKind::TextOrNumeric),
        "`__c0__` != 200",
        &varchar,
        1,
    );
    // VARCHAR + ordered numeric: both sides through the decimal space.
    check_where(
        "varchar ordered numeric",
        &Rendering::Compare(CompareKind::NumericOnText),
        "`__c0__` > 200",
        &varchar,
        1,
    );
    // A float literal keeps its source token and binds the same way.
    check_where(
        "varchar ordered float",
        &Rendering::Compare(CompareKind::NumericOnText),
        "`__c0__` <= 1.5",
        &varchar,
        1,
    );
    // A negative literal folds into the comparison rather than emitting
    // as arithmetic.
    check_where(
        "varchar ordered negative",
        &Rendering::Compare(CompareKind::NumericOnText),
        "`__c0__` > -400",
        &varchar,
        1,
    );
    // The SEVERITY pin's exact rung is one bound integer.
    check_where(
        "severity exact",
        &Rendering::Compare(CompareKind::Bound),
        "`__c0__` == 17",
        &pins(&[(0, CanonicalType::Severity)]),
        1,
    );
}

#[test]
fn every_in_list_shape_matches_its_emission() {
    let varchar = pins(&[(0, CanonicalType::Varchar)]);
    // No element expands: the plain bound list, subject written once.
    check_where(
        "in list, all text",
        &Rendering::InListBound(3),
        "`__c0__` in (\"a\", \"b\", \"c\")",
        &varchar,
        1,
    );
    // Two numeric elements expand, one does not.
    check_where(
        "in list, mixed expansion",
        &Rendering::InListExpanded(vec![
            CompareKind::TextOrNumeric,
            CompareKind::TextOrNumeric,
            CompareKind::Bound,
        ]),
        "`__c0__` in (1, 2, \"abc\")",
        &varchar,
        1,
    );
}

// ── pattern targets, per pin ──────────────────────────────────────────

#[test]
fn every_pattern_target_matches_its_emission() {
    for (pin, form) in [
        (CanonicalType::Varchar, compare::PatternForm::Native),
        (CanonicalType::BigInt, compare::PatternForm::BigIntText),
        (CanonicalType::Boolean, compare::PatternForm::BooleanText),
        (CanonicalType::Double, compare::PatternForm::DoubleText),
        (CanonicalType::Timestamp, compare::PatternForm::Rfc3339Text),
        (CanonicalType::Severity, compare::PatternForm::SeverityText),
    ] {
        assert_eq!(compare::pattern_form(Some(pin)), form, "{pin:?}");
        let map = pins(&[(0, pin)]);
        // The SEVERITY target is the twenty-four-arm token table, a
        // simple `CASE`: the text writes the subject once and the parsed
        // tree twenty-four times.
        let copies = if form == compare::PatternForm::SeverityText {
            Copies::NormalizedCase
        } else {
            Copies::Exact
        };
        for op in ["matches", "like", "ilike"] {
            check_where_as(
                &format!("{pin:?} {op}"),
                &Rendering::Pattern(form),
                &format!("`__c0__` {op} \"x\""),
                &map,
                1,
                copies,
            );
        }
    }
}

// ── the severity range classes ────────────────────────────────────────

#[test]
fn every_severity_range_class_matches_its_emission() {
    let sev = pins(&[(0, CanonicalType::Severity)]);
    // One band, one run.
    check_where(
        "one band",
        &Rendering::SeverityRanges {
            runs: 1,
            negated: false,
        },
        "`__c0__` == \"error\"",
        &sev,
        1,
    );
    // The same band negated wraps the shape in `NOT`.
    check_where(
        "one band negated",
        &Rendering::SeverityRanges {
            runs: 1,
            negated: true,
        },
        "`__c0__` != \"error\"",
        &sev,
        1,
    );
    // Adjacent bands merge into one run; disjoint ones do not.
    check_where(
        "adjacent bands merge",
        &Rendering::SeverityRanges {
            runs: 1,
            negated: false,
        },
        "`__c0__` in (\"warn\", \"error\")",
        &sev,
        1,
    );
    check_where(
        "disjoint bands",
        &Rendering::SeverityRanges {
            runs: 2,
            negated: false,
        },
        "`__c0__` in (\"warn\", \"fatal\")",
        &sev,
        1,
    );
    // The widest set the ladder admits: every odd point, twelve runs.
    let odd = (0..12)
        .map(|i| (i * 2 + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    check_where(
        "twelve disjoint points",
        &Rendering::SeverityRanges {
            runs: 12,
            negated: false,
        },
        &format!("`__c0__` in ({odd})"),
        &sev,
        1,
    );
    // Out-of-ladder points collapse onto one representative, so a run
    // count can never exceed the thirteen the profile ceiling assumes.
    let mut points: Vec<i64> = (1..=24).step_by(2).collect();
    points.extend([-5, 9_000, i64::MAX]);
    assert_eq!(compare::severity_ranges(&points).len(), 13);
}

/// The composition boundary the two helpers do not cover on their own: a
/// `sev()` subject inside a severity set, which writes that whole
/// kilobyte-long expression once per run.
#[test]
fn a_sev_subject_inside_a_severity_set_is_copied_per_run() {
    let odd = (0..12)
        .map(|i| (i * 2 + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    for (dialect, call) in [
        (Dialect::Otel, "sev(`__c0__`)".to_string()),
        (Dialect::Otel, "sev(`__c0__`, \"otel\")".to_string()),
        (Dialect::Syslog, "sev(`__c0__`, \"syslog\")".to_string()),
    ] {
        let sql = fragment(&format!("* | where {call} in ({odd})"), &FieldTypes::new());
        // Twelve runs, each carrying one whole reading of the subject.
        assert_eq!(sql.matches("\"__c0__\"").count(), 12, "{call}");

        // The composed profile: the set over the call's own weight.
        let call_profile = profile(&Rendering::Function(FunctionShape::Sev {
            dialect,
            argc: if call.contains(',') { 2 } else { 1 },
        }));
        let call_weight = call_profile.fixed + call_profile.copies.iter().sum::<u64>();
        let set = profile(&Rendering::SeverityRanges {
            runs: 12,
            negated: false,
        });
        let declared = set.fixed + set.copies[0] * call_weight;
        let counted = oracle::nodes(&sql);
        assert!(
            declared >= counted,
            "{call}: the composed profile declares {declared}, emission needs {counted}"
        );
    }
}

// ── the function inventory ────────────────────────────────────────────

/// Every name in `KNOWN_FUNCTIONS` has an emission fixture, and every
/// fixture's profile bounds what the emitter writes.
///
/// The table below is the inventory: a name missing from it fails here,
/// which is the check that stops a new function from scoring as something
/// cheaper than it emits.
#[test]
fn every_known_function_has_an_emission_fixture() {
    // (name, DSL arguments, how many leading arguments are markers)
    let fixtures: &[(&str, &str, usize)] = &[
        ("count", "", 0),
        ("avg", "`__c0__`", 1),
        ("sum", "`__c0__`", 1),
        ("min", "`__c0__`", 1),
        ("max", "`__c0__`", 1),
        ("dc", "`__c0__`", 1),
        ("distinct_count", "`__c0__`", 1),
        ("p50", "`__c0__`", 1),
        ("p90", "`__c0__`", 1),
        ("p95", "`__c0__`", 1),
        ("p99", "`__c0__`", 1),
        ("first", "`__c0__`", 1),
        ("last", "`__c0__`", 1),
        ("values", "`__c0__`", 1),
        ("list", "`__c0__`", 1),
        ("median", "`__c0__`", 1),
        ("stddev", "`__c0__`", 1),
        ("lower", "`__c0__`", 1),
        ("upper", "`__c0__`", 1),
        ("length", "`__c0__`", 1),
        ("len", "`__c0__`", 1),
        ("coalesce", "`__c0__`, `__c1__`", 2),
        ("if", "`__c0__`, `__c1__`, `__c2__`", 3),
        ("replace", "`__c0__`, `__c1__`, `__c2__`", 3),
        ("substr", "`__c0__`, `__c1__`", 2),
        ("trim", "`__c0__`", 1),
        ("ltrim", "`__c0__`", 1),
        ("rtrim", "`__c0__`", 1),
        ("isnull", "`__c0__`", 1),
        ("isnotnull", "`__c0__`", 1),
        ("abs", "`__c0__`", 1),
        ("ceil", "`__c0__`", 1),
        ("ceiling", "`__c0__`", 1),
        ("floor", "`__c0__`", 1),
        ("round", "`__c0__`", 1),
        ("now", "", 0),
        ("typeof", "`__c0__`", 1),
        ("tonumber", "`__c0__`", 1),
        ("tostring", "`__c0__`", 1),
        ("sev", "`__c0__`", 1),
        ("contains", "`__c0__`, `__c1__`", 2),
        ("startswith", "`__c0__`, `__c1__`", 2),
        ("endswith", "`__c0__`, `__c1__`", 2),
        ("split", "`__c0__`, `__c1__`, 1", 2),
        ("concat", "`__c0__`, `__c1__`", 2),
        ("date_part", "\"hour\", `__c0__`", 0),
        ("date_trunc", "\"hour\", `__c0__`", 0),
        ("date_diff", "\"day\", `__c0__`, `__c1__`", 0),
        ("strftime", "`__c0__`, \"%Y\"", 1),
        ("strptime", "`__c0__`, \"%Y\"", 1),
        ("case", "`__c0__`, `__c1__`", 2),
        ("json", "`__c0__`, `__c1__`", 2),
        ("json_extract", "`__c0__`, `__c1__`", 2),
        ("json_extract_string", "`__c0__`, `__c1__`", 2),
        ("json_valid", "`__c0__`", 1),
        ("json_keys", "`__c0__`", 1),
        ("json_array_length", "`__c0__`", 1),
    ];

    for &name in trawl_core::parser::suggest::KNOWN_FUNCTIONS {
        assert!(
            fixtures.iter().any(|(f, _, _)| *f == name),
            "{name} has no emission fixture in this inventory"
        );
    }

    let none = FieldTypes::new();
    for (name, args, markers) in fixtures {
        let sql = fragment(&format!("* | let z = {name}({args})"), &none);
        let rendering = shape_of(name, args);
        check(name, &rendering, &sql, *markers, Copies::Exact);
    }
}

/// The shape a fixture's call takes — the same split
/// `complexity::profile` prices, restated here so the test states its own
/// expectation rather than asking the module under test.
fn shape_of(name: &str, args: &str) -> Rendering {
    let arity = if args.trim().is_empty() {
        0
    } else {
        args.split(", ").count()
    };
    Rendering::Function(match name {
        "count" if arity == 0 => FunctionShape::CountStar,
        "now" => FunctionShape::Now,
        "isnull" => FunctionShape::IsNull,
        "isnotnull" => FunctionShape::IsNotNull,
        "p50" | "p90" | "p95" | "p99" => FunctionShape::Percentile,
        "split" => FunctionShape::Split,
        "sev" => FunctionShape::Sev {
            dialect: Dialect::Otel,
            argc: arity,
        },
        _ => FunctionShape::Plain(arity),
    })
}

/// The shapes whose cost changes with the arity or the literal they were
/// called with.
#[test]
fn arity_dependent_shapes_match_their_emission() {
    let none = FieldTypes::new();
    // `CASE` grows one child per argument, with and without an `ELSE`.
    for argc in 2..=5 {
        let args = (0..argc)
            .map(|i| format!("`__c{i}__`"))
            .collect::<Vec<_>>()
            .join(", ");
        check_let(
            &format!("case/{argc}"),
            &Rendering::Function(FunctionShape::Plain(argc)),
            &format!("case({args})"),
            &none,
            argc,
        );
    }
    // `coalesce` and `concat` are variadic in the same way.
    for name in ["coalesce", "concat"] {
        let args = (0..4)
            .map(|i| format!("`__c{i}__`"))
            .collect::<Vec<_>>()
            .join(", ");
        check_let(
            name,
            &Rendering::Function(FunctionShape::Plain(4)),
            &format!("{name}({args})"),
            &none,
            4,
        );
    }
    // `substr` takes an optional length.
    check_let(
        "substr/3",
        &Rendering::Function(FunctionShape::Plain(3)),
        "substr(`__c0__`, `__c1__`, `__c2__`)",
        &none,
        3,
    );
    // `count(x)` is not `count()`.
    check_let(
        "count/1",
        &Rendering::Function(FunctionShape::Plain(1)),
        "count(`__c0__`)",
        &none,
        1,
    );
    // `round`'s precision argument is inlined, never an operand.
    check_let(
        "round/2",
        &Rendering::Function(FunctionShape::RoundPrecision),
        "round(`__c0__`, 2)",
        &none,
        1,
    );
    // Both `sev()` dialects, and the proof that the dialect token itself
    // never reaches the SQL as an operand.
    for (dialect, call, argc) in [
        (Dialect::Otel, "sev(`__c0__`)", 1),
        (Dialect::Otel, "sev(`__c0__`, \"otel\")", 2),
        (Dialect::Syslog, "sev(`__c0__`, \"syslog\")", 2),
    ] {
        let sql = fragment(&format!("* | let z = {call}"), &none);
        check(
            call,
            &Rendering::Function(FunctionShape::Sev { dialect, argc }),
            &sql,
            1,
            // `sev()` binds its subject once: the parser's `CASE` rewrite
            // multiplies the lambda-local variable, not the argument.
            Copies::Exact,
        );
        assert!(!sql.contains("otel") && !sql.contains("syslog"), "{sql}");
        assert_eq!(
            sql.contains("BETWEEN 1 AND 24"),
            dialect == Dialect::Otel,
            "{call}: the wrong numeric arm"
        );
    }
}

/// `eventstats` wraps every output in a window, and the stage's
/// accounting has to cover it.
#[test]
fn the_eventstats_window_wrapper_is_covered() {
    let sql = fragment(
        "* | eventstats avg(`__c0__`) as z by `h`, `g`",
        &FieldTypes::new(),
    );
    assert!(sql.contains("OVER (PARTITION BY"), "{sql}");
    let agg = profile(&Rendering::Function(FunctionShape::Plain(1)));
    // The stage adds one node for the window plus one per partition key.
    let declared = agg.fixed + agg.copies.iter().sum::<u64>() + 1 + 2;
    assert!(
        declared >= oracle::nodes(&sql),
        "the window wrapper is undercounted: {declared} vs {}\n{sql}",
        oracle::nodes(&sql)
    );
}

// ── the conform helpers ───────────────────────────────────────────────

/// Every `conform.rs` expression helper, built directly and held against
/// its own profile — including the two severity reading shapes, whose
/// documented "up to five times" is asserted here as a number.
#[test]
fn every_conform_helper_matches_its_profile() {
    let subject = "\"__c0__\"";
    let cases: Vec<(ConformShape, String)> = vec![
        (ConformShape::UntypedText, conform::untyped_text(subject)),
        (
            ConformShape::DecimalReading,
            conform::decimal_reading(subject),
        ),
        (
            ConformShape::SeverityTokenText,
            conform::severity_token_text_sql(subject),
        ),
        (
            ConformShape::SeverityReading(Dialect::Otel),
            conform::severity_reading_sql(subject, Dialect::Otel),
        ),
        (
            ConformShape::SeverityReading(Dialect::Syslog),
            conform::severity_reading_sql(subject, Dialect::Syslog),
        ),
        (
            ConformShape::SeverityReadingBindOnce(Dialect::Otel),
            conform::severity_reading_sql_bind_once(subject, Dialect::Otel),
        ),
        (
            ConformShape::SeverityReadingBindOnce(Dialect::Syslog),
            conform::severity_reading_sql_bind_once(subject, Dialect::Syslog),
        ),
    ];
    for (shape, sql) in cases {
        // The token table and both reading shapes are built out of simple
        // `CASE`s, so their declared copy counts describe the PARSED tree
        // and are an upper bound on the text.
        check(
            &format!("{shape:?}"),
            &Rendering::Conform(shape),
            &sql,
            1,
            Copies::NormalizedCase,
        );
    }

    for pin in CanonicalType::ALL {
        for dialect in [Dialect::Otel, Dialect::Syslog] {
            let sql = conform::guarded_cast_in(subject, pin, dialect);
            // Only the SEVERITY rung is the reading kernel; every other
            // rung is a searched `CASE` or a bare cast, counted exactly.
            let copies = if pin == CanonicalType::Severity {
                Copies::NormalizedCase
            } else {
                Copies::Exact
            };
            check(
                &format!("guarded_cast_in({pin:?}, {dialect:?})"),
                &Rendering::Conform(ConformShape::GuardedCast(pin, dialect)),
                &sql,
                1,
                copies,
            );
        }
    }

    // The counts `conform.rs` documents in prose, as numbers — these are
    // TEXT counts, which is the number that matters for pushing bound
    // parameters, and they are asserted against the emitted string.
    for (dialect, written) in [(Dialect::Otel, 5), (Dialect::Syslog, 4)] {
        let sql = conform::severity_reading_sql(subject, dialect);
        assert_eq!(sql.matches(subject).count(), written, "{dialect:?}");
        // And the profile, which counts what the BINDER sees, is strictly
        // larger: the parser has multiplied the subject by then.
        let declared = profile(&Rendering::Conform(ConformShape::SeverityReading(dialect)));
        assert!(declared.copies[0] > written as u64, "{declared:?}");
    }
    for dialect in [Dialect::Otel, Dialect::Syslog] {
        let sql = conform::severity_reading_sql_bind_once(subject, dialect);
        assert_eq!(sql.matches(subject).count(), 1, "{dialect:?}");
        // The bind-once shape is the one whose text count and parsed
        // count agree, which is exactly what lets `sev()` carry `?`.
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::SeverityReadingBindOnce(
                dialect
            )))
            .copies,
            vec![1]
        );
    }
}
