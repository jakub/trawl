// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The correlation builders under hostile input (ADR-0058).
//!
//! For any value without NUL, the composed text parses back to the query
//! that was built, and the parsed query has the shape the builder
//! promises: one quoted text search for `sightings`, quoted exact matches
//! for `context`, the fixed stage list for both and for `compare`. The
//! assertions read the PARSED text, not the built AST, so they hold
//! independently of `Composed::verify`.
//!
//! The last tests run the composed `compare` and `sightings` statements in
//! `DuckDB` over a small corpus, so the stage list is not only parseable
//! but executes to the counts, rates and statuses it names.

use std::io::Write as _;

use duckdb::Connection;
use proptest::prelude::*;
use serde_json::{Value, json};

use trawl_core::ast::{FilterOp, FilterValue, PipeStage, Query, SearchToken};
use trawl_core::context::EvalContext;
use trawl_core::correlate::{
    BuildError, COMPARE_COLUMNS, MAX_LITERAL_BYTES, Window, compare_query, contains_ci,
    context_query, follow_ups, sightings_query,
};
use trawl_core::emitter::{self, SqlValue};
use trawl_core::format::format_query;
use trawl_core::parser::parse;
use trawl_core::parser::suggest::quote_dsl_field;
use trawl_core::schema::catalog_key;

const HOUR_US: i64 = 3_600_000_000;
/// 2026-10-24T10:00:00Z.
const T0: i64 = 1_792_836_000_000_000;

// ── Generators ────────────────────────────────────────────────────────

/// A piece of hostile text: the DSL's syntax characters, keywords,
/// wildcards, newlines and non-ASCII, or any one character but NUL.
fn hostile_piece() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => prop::sample::select(vec![
            "\"", "\\", "\\\"", "|", "| stats count()", "#", "# comment", "//", "*", "?", "%",
            "_", "[", "]", "(", ")", ",", "`", "=", "!=", "\n", "\r", "\t", " ", "ä", "ÄB", "İ",
            "ς", "OR", "or", "NOT", "last=1h", "earliest=\"x\"", "/re/", "${x}", "\u{202e}",
        ])
        .prop_map(str::to_owned),
        1 => any::<char>()
            .prop_filter("no NUL", |c| *c != '\0')
            .prop_map(String::from),
        1 => "[a-z0-9.]{1,4}",
    ]
}

/// A string without NUL, rich in DSL syntax.
fn hostile() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => prop::collection::vec(hostile_piece(), 0..24).prop_map(|parts| parts.concat()),
        1 => any::<String>().prop_map(|s| s.replace('\0', "")),
    ]
}

/// A field name: a plain one, a keyword, or hostile text.
fn group_name() -> impl Strategy<Value = String> {
    prop_oneof![
        2 => "[a-z_][a-z0-9_]{0,6}",
        1 => prop::sample::select(vec![
            "last", "OR", "not", "null", "@timestamp", "k8s.pod", "status", "compare_side",
        ])
        .prop_map(str::to_owned),
        2 => hostile(),
    ]
}

// ── Parsed-shape helpers ──────────────────────────────────────────────

fn reparse(text: &str) -> Query {
    parse(text).unwrap_or_else(|e| panic!("composed text does not parse: {e:?}\n{text}"))
}

fn stage_names(query: &Query) -> Vec<String> {
    query.pipeline.iter().map(|s| s.node.to_string()).collect()
}

/// The pipeline alone as canonical text: two parses of different text
/// carry different spans, so stages compare by what they say.
fn pipeline_text(query: &Query) -> String {
    let search = parse("").expect("the empty query parses").search;
    format_query(&Query {
        search,
        pipeline: query.pipeline.clone(),
    })
}

fn single_group(query: &Query) -> Vec<SearchToken> {
    let [group] = query.search.groups.as_slice() else {
        panic!("expected one search group: {:?}", query.search.groups);
    };
    group.iter().map(|t| t.node.clone()).collect()
}

fn no_time_clause(query: &Query) {
    assert!(query.time_clause().is_none());
    assert!(query.search.hoisted.is_empty());
}

// ── Properties ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The literal stays one quoted search token, and the pipeline is the
    /// one `eventstats` stage, whatever the literal holds.
    #[test]
    fn sightings_literal_never_changes_the_pipeline(literal in hostile()) {
        let built = sightings_query(&literal);
        if literal.is_empty() {
            prop_assert_eq!(built, Err(BuildError::EmptyLiteral));
            return Ok(());
        }
        if literal.len() > MAX_LITERAL_BYTES {
            prop_assert_eq!(built, Err(BuildError::LiteralTooLong { bytes: literal.len() }));
            return Ok(());
        }
        let built = built.expect("a literal within bounds builds");
        prop_assert!(built.verify().is_ok());
        let parsed = reparse(built.text());
        no_time_clause(&parsed);
        prop_assert_eq!(
            single_group(&parsed),
            vec![SearchToken::QuotedSearch(trawl_core::ast::QuotedSearch { phrase: literal.clone() })]
        );
        let reference = reparse(sightings_query("x").unwrap().text());
        prop_assert_eq!(stage_names(&parsed), vec!["eventstats".to_owned()]);
        prop_assert_eq!(pipeline_text(&parsed), pipeline_text(&reference));

        for proposal in follow_ups(&literal, Some(&literal)) {
            let parsed = reparse(proposal.text());
            no_time_clause(&parsed);
            let tokens = single_group(&parsed);
            prop_assert!(matches!(
                &tokens[0],
                SearchToken::QuotedSearch(q) if q.phrase == literal
            ));
            match tokens.as_slice() {
                [_] => prop_assert_eq!(stage_names(&parsed), vec!["timechart".to_owned()]),
                [_, SearchToken::FieldFilter(f)] => {
                    prop_assert_eq!(f.op, FilterOp::Eq);
                    prop_assert_eq!(&f.value, &FilterValue::Literal(literal.clone()));
                    prop_assert!(parsed.pipeline.is_empty());
                }
                other => prop_assert!(false, "unexpected follow-up tokens {:?}", other),
            }
        }
    }

    /// Env and host values stay quoted exact matches (never a glob, a
    /// regex or a second token) and add no pipe stage.
    #[test]
    fn context_values_stay_exact_matches(
        env in proptest::option::of(hostile()),
        host in proptest::option::of(hostile()),
    ) {
        let built = context_query(env.as_deref(), host.as_deref());
        if env.is_none() && host.is_none() {
            prop_assert_eq!(built, Err(BuildError::EmptyScope));
            return Ok(());
        }
        let built = built.expect("a NUL-free scope builds");
        let parsed = reparse(built.text());
        no_time_clause(&parsed);
        prop_assert!(parsed.pipeline.is_empty());
        let expected: Vec<SearchToken> = [("env", env), ("host", host)]
            .into_iter()
            .filter_map(|(field, value)| {
                value.map(|value| SearchToken::FieldFilter(trawl_core::ast::FieldFilter {
                    field: field.to_owned(),
                    op: FilterOp::Eq,
                    value: FilterValue::Literal(value),
                }))
            })
            .collect();
        prop_assert_eq!(single_group(&parsed), expected);
    }

    /// A group name either is refused by name or reaches the `stats by`
    /// and the sort exactly, with the stage list unchanged.
    #[test]
    fn compare_group_names_round_trip_or_are_refused(
        names in prop::collection::vec(group_name(), 0..3),
    ) {
        let built = compare_query(
            "service=auth",
            Window { start_us: T0, end_us: T0 + HOUR_US },
            Window { start_us: T0 - HOUR_US, end_us: T0 },
            &names,
            1,
        );
        let refused = names.iter().enumerate().any(|(i, name)| {
            quote_dsl_field(name).is_none()
                || COMPARE_COLUMNS.contains(&catalog_key(name).as_str())
                || names[..i].iter().any(|prior| catalog_key(prior) == catalog_key(name))
        });
        if refused {
            prop_assert!(matches!(built, Err(BuildError::Field { .. })), "{:?}", built);
            return Ok(());
        }
        let built = built.expect("valid names build");
        let parsed = reparse(built.text());
        no_time_clause(&parsed);
        prop_assert_eq!(
            stage_names(&parsed),
            ["let", "where", "let", "stats", "where", "let", "sort"].map(str::to_owned).to_vec()
        );
        let PipeStage::Stats(stats) = &parsed.pipeline[3].node else { unreachable!() };
        prop_assert_eq!(&stats.group_by, &names);
        let PipeStage::Sort(sort) = &parsed.pipeline[6].node else { unreachable!() };
        let tail: Vec<&str> = sort.fields[3..].iter().map(|f| f.field.as_str()).collect();
        prop_assert_eq!(tail, names.iter().map(String::as_str).collect::<Vec<_>>());
    }

    /// A reserved column name in any ASCII case is refused: the DSL reads
    /// `Compare_Side` as the column `compare_side`.
    #[test]
    fn compare_refuses_reserved_names_in_any_ascii_case(
        (name, cased) in prop::sample::select(COMPARE_COLUMNS.to_vec()).prop_flat_map(|name| {
            let cased = prop::collection::vec(any::<bool>(), name.len()).prop_map(move |flips| {
                name.chars()
                    .zip(flips)
                    .map(|(c, up)| if up { c.to_ascii_uppercase() } else { c })
                    .collect::<String>()
            });
            (Just(name), cased)
        }),
    ) {
        let built = compare_query(
            "",
            Window { start_us: T0, end_us: T0 + HOUR_US },
            Window { start_us: T0 - HOUR_US, end_us: T0 },
            std::slice::from_ref(&cased),
            1,
        );
        prop_assert_eq!(
            built,
            Err(BuildError::Field {
                name: cased,
                reason: "is a column the compare statement assigns",
            }),
            "{}",
            name
        );
    }

    /// Any filter text is either refused or becomes the search stage it
    /// parses to, followed by the fixed stage list.
    #[test]
    fn compare_filter_never_adds_a_stage(filter in hostile()) {
        let built = compare_query(
            &filter,
            Window { start_us: T0, end_us: T0 + HOUR_US },
            Window { start_us: T0 - HOUR_US, end_us: T0 },
            &[],
            1,
        );
        let Ok(built) = built else {
            prop_assert!(
                matches!(
                    built,
                    Err(
                        BuildError::FilterParse { .. }
                            | BuildError::FilterShape { .. }
                            | BuildError::Reparse
                    )
                ),
                "{:?}", built
            );
            return Ok(());
        };
        let own = parse(&filter).expect("an accepted filter parses");
        prop_assert!(own.pipeline.is_empty());
        let parsed = reparse(built.text());
        no_time_clause(&parsed);
        prop_assert_eq!(
            stage_names(&parsed),
            ["let", "where", "let", "stats", "where", "let", "sort"].map(str::to_owned).to_vec()
        );
        let search_text = format_query(&Query { search: own.search, pipeline: Vec::new() });
        let rebuilt = format_query(&Query { search: parsed.search, pipeline: Vec::new() });
        prop_assert_eq!(search_text, rebuilt);
    }

    /// `contains_ci` agrees with folding both sides and searching, and a
    /// string always contains itself in any case.
    #[test]
    fn contains_ci_is_reflexive_under_case(text in hostile()) {
        prop_assert!(contains_ci(&text, &text));
        prop_assert!(contains_ci(&text.to_uppercase(), &text.to_uppercase()));
        let wrapped = format!("<{text}>");
        prop_assert!(contains_ci(&wrapped, &text));
    }
}

#[test]
fn compare_keeps_the_callers_spelling_of_a_mixed_case_group() {
    let names = vec!["Host".to_owned(), "SERVICE".to_owned()];
    let built = compare_query(
        "",
        Window {
            start_us: T0,
            end_us: T0 + HOUR_US,
        },
        Window {
            start_us: T0 - HOUR_US,
            end_us: T0,
        },
        &names,
        1,
    )
    .expect("mixed-case names that fold apart build");
    assert!(
        built.text().contains("by Host, SERVICE"),
        "{}",
        built.text()
    );
    let parsed = reparse(built.text());
    let PipeStage::Stats(stats) = &parsed.pipeline[3].node else {
        panic!("stage 3 is stats");
    };
    assert_eq!(stats.group_by, names);
}

// ── Execution ─────────────────────────────────────────────────────────

fn anchor() -> EvalContext {
    EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-10-24T12:00:00Z")
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc),
    )
}

/// Run `dsl` over `events` in `DuckDB`, each output row as a JSON object.
fn run(dsl: &str, events: &[Value]) -> Vec<serde_json::Map<String, Value>> {
    let mut file = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .expect("tempfile");
    for event in events {
        writeln!(file, "{event}").expect("write fixture");
    }
    file.flush().expect("flush fixture");
    let path = file.path().to_str().expect("UTF-8 path");
    let emitted = emitter::emit(&parse(dsl).expect("parses"), path, anchor())
        .unwrap_or_else(|e| panic!("{dsl} does not emit: {e:?}"));
    assert!(emitted.rust_stages.is_empty());
    let conn = Connection::open_in_memory().expect("duckdb");
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .expect("session zone");
    let params: Vec<Box<dyn duckdb::ToSql>> = emitted
        .params
        .iter()
        .map(|v| -> Box<dyn duckdb::ToSql> {
            match v {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
                SqlValue::Timestamp(at) => Box::new(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    at.and_utc().timestamp_micros(),
                )),
            }
        })
        .collect();
    let refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let sql = format!(
        "SELECT CAST(to_json(t) AS VARCHAR) FROM ({}) t",
        emitted.sql
    );
    let mut stmt = conn.prepare(&sql).unwrap_or_else(|e| panic!("{e}\n{sql}"));
    stmt.query_map(refs.as_slice(), |row| row.get::<_, String>(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .map(
            |text| match serde_json::from_str(&text).expect("JSON row") {
                Value::Object(map) => map,
                other => panic!("not an object: {other}"),
            },
        )
        .collect()
}

fn event(time: &str, service: &str, host: &str, message: &str) -> Value {
    json!({ "_time": time, "service": service, "host": host, "message": message })
}

fn number(row: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
    row.get(key).and_then(Value::as_f64)
}

#[test]
fn compare_executes_to_counts_rates_and_statuses() {
    let events = [
        // host a: 3 in the foreground, 1 in the baseline.
        event("2026-10-24T10:05:00Z", "auth", "a", "fail"),
        event("2026-10-24T10:06:00Z", "auth", "a", "fail"),
        event("2026-10-24T10:59:59.999999Z", "auth", "a", "fail"),
        event("2026-10-24T09:10:00Z", "auth", "a", "fail"),
        // host b: new in the foreground; the window start is inclusive.
        event("2026-10-24T10:00:00Z", "auth", "b", "fail"),
        event("2026-10-24T10:30:00Z", "auth", "b", "fail"),
        // host c: vanished; the baseline end is exclusive of the fg start.
        event("2026-10-24T09:00:00Z", "auth", "c", "fail"),
        event("2026-10-24T09:59:59Z", "auth", "c", "fail"),
        // Outside both windows, and outside the filter.
        event("2026-10-24T08:59:59Z", "auth", "d", "fail"),
        event("2026-10-24T11:00:00Z", "auth", "d", "fail"),
        event("2026-10-24T10:10:00Z", "web", "e", "fail"),
    ];
    let composed = compare_query(
        "service=auth",
        Window {
            start_us: T0,
            end_us: T0 + HOUR_US,
        },
        Window {
            start_us: T0 - HOUR_US,
            end_us: T0,
        },
        &["host".to_owned()],
        1,
    )
    .expect("builds");
    let rows = run(composed.text(), &events);
    let summary: Vec<(String, f64, f64, String)> = rows
        .iter()
        .map(|row| {
            (
                row["host"].as_str().expect("host").to_owned(),
                number(row, "foreground_count").expect("fg"),
                number(row, "baseline_count").expect("bl"),
                row["status"].as_str().expect("status").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("a".to_owned(), 3.0, 1.0, "both".to_owned()),
            ("b".to_owned(), 2.0, 0.0, "new_in_foreground".to_owned()),
            ("c".to_owned(), 0.0, 2.0, "vanished".to_owned()),
        ],
        "{rows:?}"
    );
    let close = |got: Option<f64>, want: f64| {
        let got = got.expect("a number");
        assert!((got - want).abs() < 1e-12, "{got} != {want}");
    };
    close(number(&rows[0], "foreground_rate"), 3.0 / 60.0);
    close(number(&rows[0], "baseline_rate"), 1.0 / 60.0);
    close(number(&rows[0], "abs_rate_change"), 2.0 / 60.0);
    close(number(&rows[0], "relative_change"), 2.0);
    assert_eq!(rows[1]["relative_change"], Value::Null);
    close(number(&rows[2], "rate_change"), -2.0 / 60.0);
    close(number(&rows[2], "relative_change"), -1.0);

    // A minimum count of 3 keeps only the group that reaches it on one side.
    let composed = compare_query(
        "service=auth",
        Window {
            start_us: T0,
            end_us: T0 + HOUR_US,
        },
        Window {
            start_us: T0 - HOUR_US,
            end_us: T0,
        },
        &["host".to_owned()],
        3,
    )
    .expect("builds");
    let hosts: Vec<Value> = run(composed.text(), &events)
        .into_iter()
        .map(|row| row["host"].clone())
        .collect();
    assert_eq!(hosts, [json!("a")]);
}

fn seen(time: &str, service: &str, env: &str, message: &str, raw: &str) -> Value {
    json!({ "_time": time, "service": service, "env": env, "message": message, "_raw": raw })
}

#[test]
fn sightings_counts_every_match_per_service_and_env() {
    let events = [
        seen(
            "2026-10-24T10:00:00Z",
            "auth",
            "prod",
            "login from 10.0.4.17",
            "-",
        ),
        seen(
            "2026-10-24T10:05:00Z",
            "auth",
            "prod",
            "LOGIN FROM 10.0.4.17",
            "-",
        ),
        seen(
            "2026-10-24T09:00:00Z",
            "edge",
            "prod",
            "peer 10.0.4.17 \"quoted\"",
            "-",
        ),
        // `10.0.4.170` contains the literal; `10.0.4.1` does not.
        seen(
            "2026-10-24T09:30:00Z",
            "edge",
            "prod",
            "peer 10.0.4.170",
            "-",
        ),
        seen("2026-10-24T09:40:00Z", "edge", "prod", "peer 10.0.4.1", "-"),
        // A match in `_raw` alone is in the population.
        seen(
            "2026-10-24T08:00:00Z",
            "edge",
            "stage",
            "peer",
            "src=10.0.4.17",
        ),
    ];
    let composed = sightings_query("10.0.4.17").expect("builds");
    let rows = run(composed.text(), &events);
    let text = |row: &serde_json::Map<String, Value>, key: &str| {
        row[key].as_str().expect("a string").to_owned()
    };
    let mut groups: Vec<(String, String, f64, String, String)> = rows
        .iter()
        .map(|row| {
            (
                text(row, "service"),
                text(row, "env"),
                number(row, "sighting_count").expect("count"),
                text(row, "sighting_first"),
                text(row, "sighting_last"),
            )
        })
        .collect();
    groups.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    groups.dedup();
    let group = |service: &str, env: &str, count: f64, first: &str, last: &str| {
        (
            service.to_owned(),
            env.to_owned(),
            count,
            format!("2026-10-24 {first}"),
            format!("2026-10-24 {last}"),
        )
    };
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert_eq!(
        groups,
        [
            group("auth", "prod", 2.0, "10:00:00", "10:05:00"),
            group("edge", "prod", 2.0, "09:00:00", "09:30:00"),
            group("edge", "stage", 1.0, "08:00:00", "08:00:00"),
        ],
        "{rows:?}"
    );

    // A literal carrying a quote matches it literally.
    let rows = run(
        sightings_query("10.0.4.17 \"quoted\"")
            .expect("builds")
            .text(),
        &events,
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
}

/// `sightings` attribution folds case as the composed statement's `ILIKE`
/// does, on the characters where Rust's Unicode lowercase mapping and
/// `DuckDB`'s differ: `contains_ci` attributes a row exactly when `DuckDB`
/// counts it, in `message` and in `_raw`.
#[test]
fn sightings_attribution_folds_as_ilike_does() {
    let pairs = [
        ("İ", "i"),
        ("i", "İ"),
        ("İ", "İ"),
        ("i\u{307}", "İ"),
        ("im", "xİMx"),
        ("ı", "I"),
        ("ɤ", "Ɤ"),
        ("Ɤ", "ɤ"),
        ("ɤ", "ɤ"),
    ];
    let mut counted = 0;
    for (literal, text) in pairs {
        let dsl = sightings_query(literal).expect("builds");
        for (message, raw) in [(text, "-"), ("-", text)] {
            let rows = run(
                dsl.text(),
                &[seen("2026-10-24T10:00:00Z", "auth", "prod", message, raw)],
            );
            let attributed = contains_ci(message, literal) || contains_ci(raw, literal);
            assert_eq!(
                attributed,
                !rows.is_empty(),
                "{literal:?} over message {message:?}, _raw {raw:?}: DuckDB counted {rows:?}"
            );
            counted += usize::from(attributed);
        }
    }
    // Some pairs match and some do not, so agreement is not vacuous.
    assert!(counted > 0 && counted < 2 * pairs.len(), "{counted}");
}
