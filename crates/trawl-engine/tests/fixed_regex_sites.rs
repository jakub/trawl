// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The batch lane's RE2 allowance counts every regex call `DuckDB` binds
//! and runs for a statement (ADR-0063): the query's own patterns, bound
//! as parameters, and the patterns the emitter writes itself
//! (`trawl_core::conform::FIXED_REGEXES`), which `sev()` and the hot
//! branch's `SEVERITY` conform carry.
//!
//! Each shape below is emitted as a query lane emits it, and its calls
//! are read off `DuckDB`'s own plans with `json_serialize_plan`: the
//! bound plan, before the optimizer runs, and the optimized one, whose
//! calls each compile an RE2 on every thread that runs the statement.
//! Each call is read as the kind of allowance it takes: `fixed` when its
//! function and pattern are one of `FIXED_REGEXES`, `extract` for any
//! other `regexp_extract`, and `matching` for every other call.
//!
//! A plan's count is what one thread can hold at once. A thread runs one
//! branch of a `UNION` at a time, so a union counts its heaviest branch,
//! kind by kind, and every other operator counts its own calls and all
//! of its children's (`trawl_core::emitter::regex_calls` cites the
//! `DuckDB` source and the measurement). The invariant: no kind is
//! charged less than either plan's count of it, so no branch is charged
//! less than it holds.
//!
//! `json_serialize_plan` needs constants, so each statement runs with its
//! parameters inlined, and it cannot serialize a `read_json` scan, so the
//! hot branch reads the parquet fixture through the same projection. A
//! dynamic pivot gets the threshold's worth of values, its worst case.
//!
//! Two more tests hold the counter's tables against the bundled
//! `DuckDB`'s catalog, so an upgrade that adds a regex function or a macro
//! copying its argument fails here: `emitter::is_re2_function` must name
//! every regex function, and `emitter::macro_copies` must cover every
//! copy a built-in macro's body makes.

mod common;

use std::fmt::Write as _;

use duckdb::Connection;
use trawl_core::complexity::PIVOT_FILTER_THRESHOLD;
use trawl_core::conform::FIXED_REGEXES;
use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, EmittedQuery, InputColumns, SqlValue};
use trawl_core::pattern::memory::RegexSites;
use trawl_core::schema::{CanonicalType, FieldTypes};

/// Which source a shape reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// The parquet corpus alone.
    Cold,
    /// The hot+cold union, the hot branch conforming a `SEVERITY` pin.
    Hot,
}

/// What one shape binds and runs, and what the emitter charged it.
#[derive(Debug)]
struct Reading {
    bound: RegexSites,
    optimized: RegexSites,
    charged: RegexSites,
}

fn severity_pin() -> FieldTypes {
    let mut pins = FieldTypes::new();
    pins.insert("severity", CanonicalType::Severity);
    pins
}

fn hot_file() -> String {
    common::fixture_glob_json().replace("**/*.ndjson", "logs.ndjson")
}

fn parquet_file() -> String {
    common::fixture_glob().replace("**/*.parquet", "logs.parquet")
}

fn emit(dsl: &str, lane: Lane) -> EmittedQuery {
    let query = trawl_core::parser::parse(dsl).expect("the shape parses");
    let anchor = EvalContext::capture();
    match lane {
        Lane::Cold => emitter::emit(&query, &common::fixture_glob(), anchor),
        Lane::Hot => emitter::emit_with_hot_source(
            &query,
            &common::fixture_glob(),
            &hot_file(),
            &severity_pin(),
            &severity_pin(),
            &InputColumns::unprobed(),
            anchor,
        ),
    }
    .unwrap_or_else(|error| panic!("{dsl} emits: {error}"))
}

/// `sql` with every placeholder replaced by its value as a literal.
fn inline(sql: &str, params: &[SqlValue]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut params = params.iter();
    let mut quote: Option<char> = None;
    for ch in sql.chars() {
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some(ch),
            (Some(q), c) if c == q => quote = None,
            (None, '?') => {
                match params.next().expect("a value for every placeholder") {
                    SqlValue::String(s) => {
                        out.push('\'');
                        out.push_str(&s.replace('\'', "''"));
                        out.push('\'');
                    }
                    SqlValue::Int(i) => write!(out, "{i}").expect("a String takes writes"),
                    SqlValue::Float(f) => write!(out, "{f:?}").expect("a String takes writes"),
                    SqlValue::Bool(b) => write!(out, "{b}").expect("a String takes writes"),
                    SqlValue::Timestamp(t) => {
                        write!(out, "TIMESTAMP '{t}'").expect("a String takes writes");
                    }
                }
                continue;
            }
            _ => {}
        }
        out.push(ch);
    }
    out
}

/// The statement `json_serialize_plan` can read: parameters inlined, the
/// hot branch reading the parquet fixture, and a dynamic pivot over the
/// threshold's worth of values.
fn plannable(emitted: &EmittedQuery, lane: Lane) -> String {
    let mut sql = inline(&emitted.sql, &emitted.params);
    if lane == Lane::Hot {
        let reader = emitter::hot_source_reader(&hot_file()).expect("a valid hot path");
        assert!(
            sql.contains(&reader),
            "the hot branch reads {reader}: {sql}"
        );
        sql = sql.replace(&reader, &format!("read_parquet('{}')", parquet_file()));
    }
    let on = "ON \"service\"";
    if sql.contains("PIVOT") {
        assert_eq!(sql.matches(on).count(), 1, "one pivot over service: {sql}");
        let values = (0..PIVOT_FILTER_THRESHOLD)
            .map(|i| format!("'v{i}'"))
            .collect::<Vec<_>>()
            .join(", ");
        sql = sql.replace(on, &format!("{on} IN ({values})"));
    }
    sql
}

fn plus(a: RegexSites, b: RegexSites) -> RegexSites {
    RegexSites {
        extract: a.extract + b.extract,
        matching: a.matching + b.matching,
        fixed: a.fixed + b.fixed,
    }
}

fn heavier(a: RegexSites, b: RegexSites) -> RegexSites {
    RegexSites {
        extract: a.extract.max(b.extract),
        matching: a.matching.max(b.matching),
        fixed: a.fixed.max(b.fixed),
    }
}

/// The regex calls in a plan node that one thread can hold at once. A
/// `UNION`'s branches run one at a time on a thread (see
/// `trawl_core::emitter::regex_calls`), so a union holds its heaviest
/// branch, kind by kind; every other node holds its own calls and all of
/// its children's.
fn count(node: &serde_json::Value) -> RegexSites {
    match node {
        serde_json::Value::Object(map) => {
            let mut calls = RegexSites::default();
            let name = map.get("name").and_then(serde_json::Value::as_str);
            if let Some(name) = name.filter(|name| emitter::is_re2_function(name)) {
                let pattern = map
                    .get("children")
                    .and_then(|children| children.get(1))
                    .and_then(|pattern| pattern.get("value"))
                    .and_then(|value| value.get("value"))
                    .and_then(serde_json::Value::as_str);
                let fixed = pattern.is_some_and(|pattern| {
                    FIXED_REGEXES
                        .iter()
                        .any(|f| f.function == name && f.pattern == pattern)
                });
                if fixed {
                    calls.fixed += 1;
                } else if name.starts_with("regexp_extract") {
                    calls.extract += 1;
                } else {
                    calls.matching += 1;
                }
            }
            let union =
                map.get("type").and_then(serde_json::Value::as_str) == Some("LOGICAL_UNION");
            for (key, value) in map {
                let held = match value.as_array() {
                    Some(branches) if union && key == "children" => branches
                        .iter()
                        .map(count)
                        .fold(RegexSites::default(), heavier),
                    _ => count(value),
                };
                calls = plus(calls, held);
            }
            calls
        }
        serde_json::Value::Array(items) => {
            items.iter().map(count).fold(RegexSites::default(), plus)
        }
        _ => RegexSites::default(),
    }
}

/// The regex calls one thread can hold at once in `sql`'s bound plan, or
/// its optimized one.
fn plan_calls(sql: &str, optimize: bool) -> RegexSites {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let plan: String = conn
        .query_row(
            "SELECT json_serialize_plan(?::VARCHAR, skip_null := true, skip_empty := true, \
             optimize := ?)",
            duckdb::params![sql, optimize],
            |row| row.get(0),
        )
        .expect("the plan serializes");
    let plan: serde_json::Value = serde_json::from_str(&plan).expect("the plan is JSON");
    assert_ne!(
        plan.get("error"),
        Some(&serde_json::Value::Bool(true)),
        "{sql} must plan: {plan}"
    );
    count(&plan)
}

fn read(dsl: &str, lane: Lane) -> Reading {
    let emitted = emit(dsl, lane);
    let sql = plannable(&emitted, lane);
    Reading {
        bound: plan_calls(&sql, false),
        optimized: plan_calls(&sql, true),
        charged: emitted.regex_sites,
    }
}

/// Three `sev()` calls nothing downstream reads: they cost their own
/// calls on any source, beside whatever the source's conform costs.
const FEW_SEV: &str = "service=fresh | let a = sev(message), b = sev(host), c = sev(service)";

/// Every shape the allowance has to cover.
const SHAPES: &[(&str, Lane, &str)] = &[
    ("sev", Lane::Cold, "* | let s = sev(message)"),
    // `query_resources.rs` holds the server to this shape's charge.
    ("a few sev()", Lane::Cold, FEW_SEV),
    ("a few sev(), hot", Lane::Hot, FEW_SEV),
    (
        "sev, ordinary",
        Lane::Cold,
        "* | let s = sev(message), t = sev(severity_text) | where s >= 9",
    ),
    (
        "sev in a filter",
        Lane::Cold,
        "* | where sev(message) in (1, 3, 5, 7, 9)",
    ),
    ("hot conformance", Lane::Hot, "*"),
    ("hot conformance, compared", Lane::Hot, "severity >= 9"),
    (
        "hot conformance, a set",
        Lane::Hot,
        "* | where severity in (1, 3, 5, 9)",
    ),
    (
        "hot conformance, a pattern",
        Lane::Hot,
        "* | where severity matches \"e.r\"",
    ),
    (
        "lateral copies",
        Lane::Cold,
        "* | let a = sev(message), b = a",
    ),
    (
        "lateral copies, nested",
        Lane::Cold,
        "* | let a = sev(message), b = sev(a)",
    ),
    (
        "pushed into a filter",
        Lane::Cold,
        "* | let a = sev(message) | where a in (1, 3, 5)",
    ),
    (
        "pushed into a window",
        Lane::Cold,
        "* | let a = sev(message) | eventstats max(a) as m by a | where a > 3",
    ),
    (
        "pivot",
        Lane::Cold,
        "* | pivot max(sev(host)) on service by host",
    ),
    (
        "pivot and fill",
        Lane::Cold,
        "* | pivot max(sev(host)) on service by host | where zzz > 0",
    ),
    (
        "pivot and fill over a let",
        Lane::Cold,
        "* | let a = sev(message) | pivot max(a) on service by host | where zzz > 0",
    ),
    (
        "timechart",
        Lane::Cold,
        "* | timechart span=1h max(sev(host))",
    ),
    ("extract", Lane::Cold, "* | extract \"(?P<w>[a-z]+)\""),
    ("matches", Lane::Cold, "* | where message matches \"a.c\""),
    (
        "matches, a let filtered",
        Lane::Cold,
        "* | let a = message matches \"a.c\" | where a",
    ),
    (
        "matches, a let filtered, hot",
        Lane::Hot,
        "* | let a = message matches \"a.c\" | where a",
    ),
    (
        "matches over the union",
        Lane::Hot,
        "* | where message matches \"a.c\"",
    ),
    ("search over the union", Lane::Hot, "message=/e.r/"),
    (
        "matches pushed into a window",
        Lane::Cold,
        "* | let a = message matches \"a.c\" | eventstats count() as n by a | where a",
    ),
    (
        "extract, filtered",
        Lane::Cold,
        "* | extract \"(?P<w>[a-z]+)\" from message | where w == \"abc\"",
    ),
    (
        "extract, filtered, hot",
        Lane::Hot,
        "* | extract \"(?P<w>[a-z]+)\" from message | where w == \"abc\"",
    ),
    // A projection reads the columns it names: neither shape copies the
    // call beneath it.
    (
        "matches read by later lets",
        Lane::Cold,
        "* | let a = message matches \"a.c\" | let b = a and a | let c = b or a",
    ),
    (
        "extract, three groups",
        Lane::Cold,
        "* | extract \"(?P<x>[a-c])(?P<y>[d-f])(?P<z>[g-i])\"",
    ),
    (
        "pivot over matches",
        Lane::Cold,
        "* | pivot max(host matches \"w.b\") on service by host",
    ),
    (
        "mix",
        Lane::Cold,
        "message=/e.r/ | let a = sev(message), b = a | where message matches \"x.y\" \
         | extract \"(?P<w>[a-z]+)\" from message | stats max(b) as m by w",
    ),
    (
        "mix, hot",
        Lane::Hot,
        "message=/e.r/ severity >= 5 | let a = sev(message) | where severity matches \"w.r\" \
         | extract \"(?P<w>[a-z]+)\" from message | stats max(a) as m by w, severity",
    ),
];

/// The invariant, shape by shape and kind by kind: no kind of call is
/// charged less than one thread can hold of it in `DuckDB`'s bound plan
/// or its optimized plan.
#[test]
fn the_allowance_counts_every_regex_call_duckdb_binds_and_runs() {
    let mut failures = Vec::new();
    for &(label, lane, dsl) in SHAPES {
        let Reading {
            bound,
            optimized,
            charged,
        } = read(dsl, lane);
        println!(
            "{label:30} extract {:>2}/{:>2}/{:>2} | matching {:>3}/{:>3}/{:>3} | \
             fixed {:>4}/{:>4}/{:>5}  (bound/optimized/charged)",
            bound.extract,
            optimized.extract,
            charged.extract,
            bound.matching,
            optimized.matching,
            charged.matching,
            bound.fixed,
            optimized.fixed,
            charged.fixed,
        );
        let kinds = |sites: RegexSites| {
            [
                ("extract", sites.extract),
                ("matching", sites.matching),
                ("fixed", sites.fixed),
            ]
        };
        for (((kind, charged), (_, bound)), (_, optimized)) in kinds(charged)
            .into_iter()
            .zip(kinds(bound))
            .zip(kinds(optimized))
        {
            if charged < bound || charged < optimized {
                failures.push(format!(
                    "{label}: charged {charged} {kind} calls, the plans hold {bound} bound \
                     and {optimized} optimized"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The control: the plans hold the calls the invariant compares, so a
/// shape that binds none of the emitter's regexes reads zero and one that
/// does reads them.
#[test]
fn the_plan_count_sees_the_emitters_regexes() {
    let plain = read("* | where host == \"web01\"", Lane::Cold);
    assert_eq!(plain.bound, RegexSites::default());
    let sev = read("* | let s = sev(message)", Lane::Cold);
    assert_eq!(
        sev.bound,
        RegexSites {
            extract: 0,
            matching: 0,
            fixed: 41,
        }
    );
    let matches = read("* | where message matches \"a.c\"", Lane::Cold);
    assert_eq!(
        matches.bound,
        RegexSites {
            extract: 0,
            matching: 1,
            fixed: 0,
        }
    );
    let extract = read("* | extract \"(?P<w>[a-z]+)\"", Lane::Cold);
    assert_eq!(
        extract.bound,
        RegexSites {
            extract: 1,
            matching: 0,
            fixed: 0,
        }
    );
    // A union holds its heaviest branch, kind by kind; a join holds both
    // sides. The patterns are no literal, which the optimizer would
    // rewrite into `contains`.
    let source = format!("read_parquet('{}')", parquet_file());
    let one = format!("SELECT regexp_matches(message, '[ab]+c') AS \"m\" FROM {source}");
    let two = format!(
        "SELECT regexp_matches(message, '[cd]+e') AND regexp_matches(message, '[ef]+g') AS \"m\" \
         FROM {source}"
    );
    for optimize in [false, true] {
        assert_eq!(
            plan_calls(&format!("{one} UNION ALL {two}"), optimize),
            RegexSites {
                extract: 0,
                matching: 2,
                fixed: 0,
            }
        );
        assert_eq!(
            plan_calls(
                &format!("SELECT * FROM ({one}) AS l, ({two}) AS r WHERE l.m <> r.m"),
                optimize
            ),
            RegexSites {
                extract: 0,
                matching: 3,
                fixed: 0,
            }
        );
    }
}

/// Every function the bundled `DuckDB` registers under a regex name, or
/// as an alias of one, and its `~` operator: the calls whose pattern
/// compiles an RE2 program.
#[test]
fn every_regex_function_duckdb_registers_counts() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let mut statement = conn
        .prepare(
            "SELECT DISTINCT function_name FROM duckdb_functions() \
             WHERE function_name ILIKE '%regex%' OR alias_of ILIKE '%regex%' \
             OR function_name = '~' OR alias_of = '~'",
        )
        .expect("the catalog query prepares");
    let names: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .expect("the catalog query runs")
        .collect::<Result<_, _>>()
        .expect("every name reads");
    assert!(
        names.iter().any(|name| name == "regexp_full_match") && names.iter().any(|n| n == "~"),
        "the catalog lists the regex functions: {names:?}"
    );
    let missing: Vec<&String> = names
        .iter()
        .filter(|name| !emitter::is_re2_function(name))
        .collect();
    assert!(missing.is_empty(), "uncounted regex functions: {missing:?}");
    // `LIKE`, `ILIKE` and `GLOB` share the tilde spelling and compile no RE2.
    for operator in ["~~", "!~~", "~~*", "!~~*", "~~~"] {
        assert!(
            !emitter::is_re2_function(operator),
            "{operator} is no regex"
        );
    }
}

/// One scalar macro the bundled `DuckDB` defines.
struct Macro {
    schema: String,
    name: String,
    params: Vec<String>,
    body: String,
}

fn scalar_macros() -> Vec<Macro> {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let mut statement = conn
        .prepare(
            "SELECT schema_name, function_name, parameters::VARCHAR, macro_definition \
             FROM duckdb_functions() \
             WHERE function_type = 'macro' AND macro_definition IS NOT NULL",
        )
        .expect("the catalog query prepares");
    statement
        .query_map([], |row| {
            let params: String = row.get(2)?;
            Ok(Macro {
                schema: row.get::<_, String>(0)?.to_ascii_lowercase(),
                name: row.get::<_, String>(1)?.to_ascii_lowercase(),
                params: params
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|param| param.trim().trim_matches('"').to_ascii_lowercase())
                    .filter(|param| !param.is_empty())
                    .collect(),
                body: row.get(3)?,
            })
        })
        .expect("the catalog query runs")
        .collect::<Result<_, _>>()
        .expect("every macro reads")
}

/// The words of a macro body that decide where an argument is copied.
#[derive(Debug, PartialEq, Eq)]
enum Lexeme {
    /// A bare or quoted name, folded to lower case.
    Name(String),
    Dot,
    Open,
    Close,
    Comma,
    Other,
}

fn lex(body: &str) -> Vec<Lexeme> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    // A quoted run ending at the next unescaped `quote`, from just past
    // the opening one; returns the text and the index past the close.
    let quoted = |mut i: usize, quote: char| {
        let mut text = String::new();
        while i < chars.len() {
            if chars[i] == quote {
                if chars.get(i + 1) == Some(&quote) {
                    text.push(quote);
                    i += 2;
                    continue;
                }
                return (text, i + 1);
            }
            text.push(chars[i]);
            i += 1;
        }
        (text, i)
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                i = quoted(i + 1, '\'').1;
                out.push(Lexeme::Other);
            }
            '"' => {
                let (name, next) = quoted(i + 1, '"');
                out.push(Lexeme::Name(name.to_ascii_lowercase()));
                i = next;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                out.push(Lexeme::Name(name.to_ascii_lowercase()));
            }
            c => {
                out.extend(match c {
                    '.' => Some(Lexeme::Dot),
                    '(' | '[' => Some(Lexeme::Open),
                    ')' | ']' => Some(Lexeme::Close),
                    ',' => Some(Lexeme::Comma),
                    c if c.is_whitespace() => None,
                    _ => Some(Lexeme::Other),
                });
                i += 1;
            }
        }
    }
    out
}

/// How many copies of its argument at `position` the macro `name` makes,
/// the most over the overloads taking `arity` arguments, or over every
/// overload when `arity` is `None`: one when none takes one there.
fn macro_copies(
    macros: &[Macro],
    schema: Option<&str>,
    name: &str,
    arity: Option<usize>,
    position: usize,
    depth: usize,
) -> u64 {
    assert!(depth < 16, "{name} nests macros past any built-in's depth");
    macros
        .iter()
        .filter(|m| m.name == name && schema.is_none_or(|s| m.schema == s))
        .filter(|m| arity.is_none_or(|arity| m.params.len() == arity))
        .filter_map(|m| {
            let param = m.params.get(position)?;
            Some(references(macros, m, param, depth + 1))
        })
        .max()
        .unwrap_or(1)
}

/// The arguments the call opening at `open` passes.
fn arity(lexemes: &[Lexeme], open: usize) -> usize {
    let mut nesting = 0_usize;
    let mut commas = 0;
    for lexeme in &lexemes[open + 1..] {
        match lexeme {
            Lexeme::Open => nesting += 1,
            Lexeme::Close if nesting == 0 => break,
            Lexeme::Close => nesting -= 1,
            Lexeme::Comma if nesting == 0 => commas += 1,
            _ => {}
        }
    }
    if lexemes.get(open + 1) == Some(&Lexeme::Close) {
        0
    } else {
        commas + 1
    }
}

/// The copies `m`'s body makes of `param`: each unqualified reference,
/// times what every macro call around it copies of the argument it sits
/// in.
fn references(macros: &[Macro], m: &Macro, param: &str, depth: usize) -> u64 {
    let lexemes = lex(&m.body);
    let is_reference = |k: usize| {
        lexemes[k] == Lexeme::Name(param.to_owned())
            && (k == 0 || lexemes[k - 1] != Lexeme::Dot)
            && lexemes.get(k + 1) != Some(&Lexeme::Open)
    };
    (0..lexemes.len())
        .filter(|&k| is_reference(k))
        .map(|k| {
            let mut copies = 1;
            let (mut nesting, mut argument) = (0_usize, 0_usize);
            for j in (0..k).rev() {
                match &lexemes[j] {
                    Lexeme::Close => nesting += 1,
                    Lexeme::Open if nesting > 0 => nesting -= 1,
                    Lexeme::Open => {
                        if let Some(Lexeme::Name(callee)) = j.checked_sub(1).map(|i| &lexemes[i]) {
                            let schema = match j
                                .checked_sub(3)
                                .map(|i| (&lexemes[i], &lexemes[i + 1]))
                            {
                                Some((Lexeme::Name(schema), Lexeme::Dot)) => Some(schema.as_str()),
                                _ => None,
                            };
                            let arity = Some(arity(&lexemes, j));
                            copies *= macro_copies(macros, schema, callee, arity, argument, depth);
                        }
                        argument = 0;
                    }
                    Lexeme::Comma if nesting == 0 => argument += 1,
                    _ => {}
                }
            }
            copies
        })
        .sum()
}

/// The emitter's macro table against the macros the bundled `DuckDB`
/// defines: no argument position copies more than the table says. The
/// control pins two copies the bound plan shows directly.
#[test]
fn the_macro_table_covers_every_copy_a_builtin_macro_makes() {
    let macros = scalar_macros();
    assert_eq!(macro_copies(&macros, None, "nullif", None, 0, 0), 2);
    assert_eq!(macro_copies(&macros, None, "map_to_pg_oid", None, 0, 0), 19);
    let mut short = Vec::new();
    for m in &macros {
        for (position, param) in m.params.iter().enumerate() {
            let copies = references(&macros, m, param, 0);
            let table = emitter::macro_copies(&m.name, position);
            if table < copies {
                short.push(format!(
                    "{}.{}({}) copies argument {position} {copies} times, the table {table}",
                    m.schema,
                    m.name,
                    m.params.join(", ")
                ));
            }
        }
    }
    assert!(short.is_empty(), "{short:#?}");
}
