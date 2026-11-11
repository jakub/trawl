// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every call a query writes is checked once, before either lane runs it,
//! and every lane refuses a bad call with the same sentence (ADR-0001,
//! Amendment 2026-11-11).
//!
//! The emitter's SQL-position refusals are the reference: their exact
//! `to_string()` and `hint()` are pinned here as a baseline, so the shared
//! call check that the validate door and the stream compiler adopt has to
//! say what emit already says.

use trawl_core::context::EvalContext;
use trawl_core::emitter::{EmitError, emit, validate_pipeline};
use trawl_core::parser;
use trawl_core::parser::suggest::KNOWN_FUNCTIONS;

const SOURCE: &str = "/data/*.parquet";

/// The operator hint every spelling of `match(` carries.
const MATCH_HINT: &str = "matches is an operator, not a function: where message matches /re/";

/// One bad call and the refusal emit gives it today.
struct Refusal {
    query: &'static str,
    display: &'static str,
    hint: Option<&'static str>,
}

/// Emit's refusal of each single-defect call, in a SQL position: a `where`
/// condition, a `let`/`eval` value, and an aggregation head or operand.
///
/// `case(x)` once had two sentences: the arity table said "argument(s)"
/// where emit's SQL-position guard said "arguments". The table now says
/// what the guard says, in every position.
const BASELINE: &[Refusal] = &[
    // unknown name, with a suggestion
    Refusal {
        query: "* | where countt(message) > 0",
        display: "unknown function: countt (did you mean 'count'?)",
        hint: Some("did you mean 'count'?"),
    },
    Refusal {
        query: "* | let x = countt(message)",
        display: "unknown function: countt (did you mean 'count'?)",
        hint: Some("did you mean 'count'?"),
    },
    Refusal {
        query: "* | let x = lowerr(message)",
        display: "unknown function: lowerr (did you mean 'lower'?)",
        hint: Some("did you mean 'lower'?"),
    },
    Refusal {
        query: "* | stats countt(message)",
        display: "unknown function: countt (did you mean 'count'?)",
        hint: Some("did you mean 'count'?"),
    },
    // unknown name, no suggestion
    Refusal {
        query: "* | where nosuchfn(message)",
        display: "unknown function: nosuchfn",
        hint: None,
    },
    Refusal {
        query: "* | let x = nosuchfn(message)",
        display: "unknown function: nosuchfn",
        hint: None,
    },
    Refusal {
        query: "* | eval x = nosuchfn(message)",
        display: "unknown function: nosuchfn",
        hint: None,
    },
    Refusal {
        query: "* | stats nosuchfn(message)",
        display: "unknown function: nosuchfn",
        hint: None,
    },
    Refusal {
        query: "* | stats count(nosuchfn(message))",
        display: "unknown function: nosuchfn",
        hint: None,
    },
    // `match(`, in either case, with the operator hint
    Refusal {
        query: r#"* | where match(message, "x")"#,
        display: "unknown function: match (matches is an operator, not a function: where \
                  message matches /re/)",
        hint: Some(MATCH_HINT),
    },
    Refusal {
        query: r#"* | where MATCH(message, "x")"#,
        display: "unknown function: MATCH (matches is an operator, not a function: where \
                  message matches /re/)",
        hint: Some(MATCH_HINT),
    },
    Refusal {
        query: r#"* | let x = match(message, "x")"#,
        display: "unknown function: match (matches is an operator, not a function: where \
                  message matches /re/)",
        hint: Some(MATCH_HINT),
    },
    Refusal {
        query: "* | stats match(message)",
        display: "unknown function: match (matches is an operator, not a function: where \
                  message matches /re/)",
        hint: Some(MATCH_HINT),
    },
    // argument count
    Refusal {
        query: "* | let x = length()",
        display: "invalid aggregation: length() requires exactly one argument",
        hint: None,
    },
    Refusal {
        query: "* | where length() > 0",
        display: "invalid aggregation: length() requires exactly one argument",
        hint: None,
    },
    Refusal {
        query: "* | stats length()",
        display: "invalid aggregation: length() requires exactly one argument",
        hint: None,
    },
    Refusal {
        query: "* | let x = lower(a, b)",
        display: "invalid aggregation: lower() requires exactly one argument",
        hint: None,
    },
    Refusal {
        query: "* | let x = case(x)",
        display: "invalid aggregation: case() requires at least 2 arguments",
        hint: None,
    },
    Refusal {
        query: "* | stats case(x)",
        display: "invalid aggregation: case() requires at least 2 arguments",
        hint: None,
    },
    // integer-literal position
    Refusal {
        query: "* | let x = round(1, message)",
        display: "invalid aggregation: round() argument 2 must be an integer literal",
        hint: None,
    },
    Refusal {
        query: "* | let x = round(12.3, -1)",
        display: "invalid aggregation: round() argument 2 must be an integer literal",
        hint: None,
    },
    Refusal {
        query: r#"* | let x = split(a, ",", i)"#,
        display: "invalid aggregation: split() argument 3 must be an integer literal",
        hint: None,
    },
    // unit and dialect
    Refusal {
        query: r#"* | let x = date_part("nanosecond", _time)"#,
        display: "unsupported operation: date_part() unit \"nanosecond\" is not in the allowed \
                  set: year, quarter, month, week, day, hour, minute, second, dow, doy, epoch",
        hint: None,
    },
    Refusal {
        query: r#"* | eval x = date_part("nanosecond", _time)"#,
        display: "unsupported operation: date_part() unit \"nanosecond\" is not in the allowed \
                  set: year, quarter, month, week, day, hour, minute, second, dow, doy, epoch",
        hint: None,
    },
    Refusal {
        query: "* | let x = date_part(u, _time)",
        display: "unsupported operation: date_part() argument 1 must be a string literal unit \
                  name, not an expression",
        hint: None,
    },
    Refusal {
        query: r#"* | let x = sev(level, "klingon")"#,
        display: "unsupported operation: sev() dialect \"klingon\" is not in the allowed set: \
                  otel, syslog",
        hint: None,
    },
    Refusal {
        query: "* | let x = sev(level, d)",
        display: "unsupported operation: sev() argument 2 must be a string literal dialect \
                  name, not an expression",
        hint: None,
    },
    // format
    Refusal {
        query: r#"* | let x = strftime(_time, "%Q")"#,
        display: "strftime(): invalid format string \"%Q\"",
        hint: None,
    },
];

fn emit_refusal(query: &str) -> EmitError {
    let ast = parser::parse(query).unwrap_or_else(|e| panic!("{query}: parse failed: {e:?}"));
    match emit(&ast, SOURCE, EvalContext::capture()) {
        Ok(_) => panic!("{query}: emit accepted a bad call"),
        Err(err) => err,
    }
}

#[test]
fn emit_refuses_each_bad_call_with_the_baseline_sentence() {
    for case in BASELINE {
        let err = emit_refusal(case.query);
        assert_eq!(err.to_string(), case.display, "{}", case.query);
        assert_eq!(err.hint().as_deref(), case.hint, "{}", case.query);
    }
}

/// At least one valid call per `KNOWN_FUNCTIONS` entry, keyed by name.
///
/// The fixtures cover optional arguments, unit and dialect names in any
/// case, dynamic format expressions, aliases, calls nested in calls and
/// under `not`, `in` and comparisons, and fields named like functions
/// (`count`, `lower`), which are leaves and never calls.
const VALID_CALLS: &[(&str, &[&str])] = &[
    // aggregates
    (
        "count",
        &[
            "* | stats count()",
            "* | stats count(message) as n by host",
            "* | timechart span=1h count()",
            "* | where count > 3 | stats count() as total by upper",
        ],
    ),
    (
        "avg",
        &[
            "* | stats avg(dur) by service",
            "* | eventstats avg(dur) as mean",
        ],
    ),
    ("sum", &["* | stats sum(bytes) by count"]),
    ("min", &["* | stats min(dur)"]),
    ("max", &["* | stats max(dur) as worst by host"]),
    ("dc", &["* | stats dc(host)"]),
    ("distinct_count", &["* | stats distinct_count(host)"]),
    ("p50", &["* | stats p50(dur)"]),
    ("p90", &["* | stats p90(dur)"]),
    ("p95", &["* | timechart span=5m p95(dur) by service"]),
    ("p99", &["* | pivot p99(dur) on status by host"]),
    ("first", &["* | stats first(message) by host"]),
    ("last", &["* | stats last(message)"]),
    ("values", &["* | stats values(host)"]),
    ("list", &["* | stats list(host)"]),
    ("median", &["* | stats median(dur)"]),
    ("stddev", &["* | stats stddev(dur)"]),
    // scalars
    (
        "lower",
        &[
            "* | let x = lower(message)",
            "* | let y = lower(lower)",
            r#"* | where lower(host) in ("a", "b")"#,
            "* | let l = lower(lower) | stats count() by lower",
        ],
    ),
    ("upper", &["* | let x = upper(trim(message))"]),
    ("length", &["* | where length(message) > 0"]),
    ("len", &["* | let n = len(message) + 1"]),
    (
        "coalesce",
        &[
            "* | let x = coalesce(a)",
            r#"* | let x = coalesce(a, b, "none")"#,
        ],
    ),
    ("if", &[r#"* | let x = if(status >= 500, "error", "ok")"#]),
    ("replace", &[r#"* | let x = replace(message, "a", "b")"#]),
    (
        "substr",
        &[
            "* | let x = substr(message, 1)",
            "* | let x = substr(message, 1, 3)",
        ],
    ),
    ("trim", &["* | let x = trim(message)"]),
    ("ltrim", &["* | let x = ltrim(message)"]),
    ("rtrim", &["* | let x = rtrim(message)"]),
    ("isnull", &["* | where isnull(user)"]),
    ("isnotnull", &["* | where not isnotnull(user)"]),
    ("abs", &["* | let x = abs(-delta)"]),
    ("ceil", &["* | let x = ceil(dur)"]),
    ("ceiling", &["* | let x = ceiling(dur)"]),
    ("floor", &["* | eval x = floor(dur)"]),
    (
        "round",
        &[
            "* | let x = round(dur)",
            "* | let x = round(dur, 2)",
            "* | let x = round(12.3, 0)",
        ],
    ),
    ("now", &["* | let t = now()"]),
    ("typeof", &["* | let t = typeof(status)"]),
    ("tonumber", &["* | let n = tonumber(status)"]),
    ("tostring", &["* | let s = tostring(status)"]),
    (
        "sev",
        &[
            "* | let s = sev(level)",
            r#"* | let s = sev(level, "syslog")"#,
            r#"* | let s = sev(level, "OTEL")"#,
        ],
    ),
    // string
    ("contains", &[r#"* | where contains(message, "timeout")"#]),
    ("startswith", &[r#"* | where startswith(path, "/api")"#]),
    ("endswith", &[r#"* | where endswith(path, ".json")"#]),
    ("split", &[r#"* | let x = split(path, "/", 1)"#]),
    (
        "concat",
        &[
            "* | let x = concat(host)",
            r#"* | let x = concat(host, ":", port)"#,
        ],
    ),
    // date/time
    (
        "date_part",
        &[
            r#"* | let d = date_part("dow", _time)"#,
            r#"* | let d = date_part("EPOCH", _time)"#,
        ],
    ),
    (
        "date_trunc",
        &[
            r#"* | let d = date_trunc("hour", _time)"#,
            r#"* | let d = date_trunc("Day", _time)"#,
        ],
    ),
    (
        "date_diff",
        &[r#"* | let d = date_diff("second", start, _time)"#],
    ),
    (
        "strftime",
        &[
            r#"* | let s = strftime(_time, "%Y-%m-%d %H:%M:%S")"#,
            "* | let s = strftime(_time, fmt)",
            r#"* | let s = strftime(_time, concat("%Y", "-%m"))"#,
        ],
    ),
    (
        "strptime",
        &[
            r#"* | let t = strptime(ts, "%Y-%m-%d")"#,
            "* | let t = strptime(ts, fmt)",
        ],
    ),
    // conditional
    (
        "case",
        &[
            r#"* | let x = case(status >= 500, "error", "ok")"#,
            r#"* | let x = case(status >= 500, "error", status >= 400, "warn")"#,
        ],
    ),
    // json
    ("json", &[r#"* | let x = json(payload, "$.a")"#]),
    (
        "json_extract",
        &[r#"* | let x = json_extract(payload, "$.a")"#],
    ),
    (
        "json_extract_string",
        &[r#"* | let x = json_extract_string(payload, "$.a")"#],
    ),
    ("json_valid", &["* | where json_valid(payload)"]),
    ("json_keys", &["* | let k = json_keys(payload)"]),
    (
        "json_array_length",
        &["* | let n = json_array_length(payload)"],
    ),
];

#[test]
fn every_known_function_has_a_valid_call() {
    for &name in KNOWN_FUNCTIONS {
        assert!(
            VALID_CALLS.iter().any(|(fixture, _)| *fixture == name),
            "KNOWN_FUNCTIONS entry {name:?} has no valid-call fixture"
        );
    }
    for (name, queries) in VALID_CALLS {
        assert!(
            KNOWN_FUNCTIONS.contains(name),
            "fixture {name:?} names no KNOWN_FUNCTIONS entry"
        );
        assert!(!queries.is_empty(), "fixture {name:?} has no query");
        for query in *queries {
            assert!(
                query.contains(&format!("{name}(")),
                "{query}: does not call {name}"
            );
            let ast =
                parser::parse(query).unwrap_or_else(|e| panic!("{query}: parse failed: {e:?}"));
            if let Err(err) = validate_pipeline(&ast.pipeline) {
                panic!("{query}: validate_pipeline refused: {err}");
            }
            if let Err(err) = emit(&ast, SOURCE, EvalContext::capture()) {
                panic!("{query}: emit refused: {err}");
            }
        }
    }
}

/// Bad calls in the stages after `extract kv`, which run in Rust behind
/// the SQL.
///
/// This pins the gap #435 closes: `validate_pipeline` answers `valid` for
/// each of these today, and the run refuses them only when the kv tail
/// compiles. The shared call check makes them refuse at the door.
const KV_TAIL_GAP: &[&str] = &[
    "* | extract kv | let r = nosuchfn(a)",
    r#"* | extract kv | let r = split(a, ",", i)"#,
    "* | extract kv | where length() > 0",
];

#[test]
fn validate_accepts_bad_calls_after_extract_kv() {
    for query in KV_TAIL_GAP {
        let ast = parser::parse(query).unwrap_or_else(|e| panic!("{query}: parse failed: {e:?}"));
        assert!(
            validate_pipeline(&ast.pipeline).is_ok(),
            "{query}: validate_pipeline already refuses"
        );
    }
}
