// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Property tests: [`CompiledFilter::matches`] must agree with `DuckDB` SQL
//! for every `(search_stage, event)` pair.
//!
//! Generates random DSL search strings and JSON events, then verifies
//! that the in-memory filter and the emitted SQL produce identical
//! match/no-match results.

use std::io::Write;

use duckdb::Connection;
use serde_json::{Map, Value};

use trawl_core::ast::PipeStage;
use trawl_core::emitter::{self, EmittedQuery, SqlValue};
use trawl_core::filter::CompiledFilter;
use trawl_core::parser;
use trawl_core::schema::{CanonicalType, FieldTypes};

// ── Deterministic RNG (splitmix64) ────────────────────────────────────

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn range(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n
    }

    fn bool(&mut self) -> bool {
        self.next_u64().is_multiple_of(2)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len())]
    }
}

// ── Vocabulary ────────────────────────────────────────────────────────

/// Fields referenced in generated search terms.
/// All are present in every generated event to avoid `DuckDB` binder errors.
const FIELDS: &[&str] = &["service", "status", "host", "path", "severity"];

/// String-typed fields (excludes numeric `status`/`severity`).
/// Used when the filter value is a string to avoid `DuckDB` conversion errors.
const STRING_FIELDS: &[&str] = &["service", "host", "path"];

const STRING_VALS: &[&str] = &[
    "nginx", "apache", "postgres", "redis", "error", "warn", "info", "debug", "web-1", "web-2",
    "db-01",
];

const PATH_VALS: &[&str] = &["/api/users", "/api/health", "/web/index"];

const INT_VALS: &[i64] = &[200, 301, 404, 500, 0, 42, 100];

const MESSAGE_WORDS: &[&str] = &[
    "error",
    "warning",
    "connection",
    "refused",
    "timeout",
    "success",
    "started",
    "stopped",
    "failed",
    "retry",
];

// ── DSL generation ────────────────────────────────────────────────────

fn random_dsl(rng: &mut Rng) -> String {
    let num_groups = if rng.range(4) == 0 { 2 } else { 1 };
    let mut groups = Vec::new();

    for _ in 0..num_groups {
        let num_tokens = rng.range(3) + 1;
        let mut tokens = Vec::new();
        for _ in 0..num_tokens {
            tokens.push(random_token(rng));
        }
        groups.push(tokens.join(" "));
    }

    groups.join(" OR ")
}

fn random_token(rng: &mut Rng) -> String {
    match rng.range(9) {
        0 => random_field_eq(rng),
        1 => random_field_compare(rng),
        2 => random_field_list(rng),
        3 => random_text_search(rng),
        4 => random_quoted_search(rng),
        5 => random_field_glob(rng),
        6 => format!("NOT {}", rng.pick(MESSAGE_WORDS)),
        7 => {
            let w1 = rng.pick(MESSAGE_WORDS);
            let w2 = rng.pick(MESSAGE_WORDS);
            format!(r#"NOT "{w1} {w2}""#)
        }
        8 => {
            let included = rng.pick(MESSAGE_WORDS);
            let excluded = rng.pick(MESSAGE_WORDS);
            format!("{included} OR NOT {excluded}")
        }
        _ => unreachable!(),
    }
}

fn random_field_eq(rng: &mut Rng) -> String {
    // Keep filter value type consistent with event field type to avoid
    // DuckDB conversion errors that abort the entire query.
    if rng.bool() {
        // String value on string field only.
        let field = rng.pick(STRING_FIELDS);
        let value = rng.pick(STRING_VALS);
        format!("{field}={value}")
    } else {
        // Numeric value on numeric field only.
        let value = rng.pick(INT_VALS);
        format!("status={value}")
    }
}

fn random_field_compare(rng: &mut Rng) -> String {
    // Always compare on status (numeric field) with positive ints
    // to avoid type conversion ambiguity.
    let op = rng.pick(&[">", ">=", "<", "<=", "!="]);
    let value = rng.pick(INT_VALS).unsigned_abs();
    format!("status{op}{value}")
}

fn random_field_list(rng: &mut Rng) -> String {
    let count = rng.range(3) + 2;
    if rng.bool() {
        // String values on string field only.
        let field = rng.pick(STRING_FIELDS);
        let values: Vec<String> = (0..count)
            .map(|_| (*rng.pick(STRING_VALS)).to_string())
            .collect();
        format!("{}={}", field, values.join(","))
    } else {
        // Numeric values on status field only.
        let values: Vec<String> = (0..count).map(|_| rng.pick(INT_VALS).to_string()).collect();
        format!("status={}", values.join(","))
    }
}

fn random_text_search(rng: &mut Rng) -> String {
    let word = rng.pick(MESSAGE_WORDS);
    if rng.range(4) == 0 {
        format!("-{word}")
    } else {
        word.to_string()
    }
}

fn random_quoted_search(rng: &mut Rng) -> String {
    let w1 = rng.pick(MESSAGE_WORDS);
    let w2 = rng.pick(MESSAGE_WORDS);
    format!(r#""{w1} {w2}""#)
}

fn random_field_glob(rng: &mut Rng) -> String {
    // GLOB is a string operation — only target VARCHAR fields to avoid
    // DuckDB conversion errors on BIGINT columns.
    let field = rng.pick(STRING_FIELDS);
    let base = rng.pick(STRING_VALS);
    // Append `*` so the parser auto-detects as glob.
    format!("{field}={base}*")
}

// ── Event generation ──────────────────────────────────────────────────

fn random_event(rng: &mut Rng) -> Map<String, Value> {
    let mut event = Map::new();

    // Exercise carried, null, and absent text columns. The SQL harness adds a
    // schema-only sibling row, so an omitted key binds as a NULL cell rather
    // than turning the parity check into a binder-error test.
    match rng.range(6) {
        0 => {}
        1 => {
            event.insert("message".to_string(), Value::Null);
        }
        _ => {
            event.insert("message".to_string(), Value::String(random_message(rng)));
        }
    }

    match rng.range(6) {
        0 => {}
        1 => {
            event.insert("_raw".to_string(), Value::Null);
        }
        _ => {
            event.insert("_raw".to_string(), Value::String(random_message(rng)));
        }
    }

    // Always include all filterable fields to avoid DuckDB binder errors.
    for &field in FIELDS {
        event.insert(field.to_string(), random_event_value(rng, field));
    }

    event
}

fn random_message(rng: &mut Rng) -> String {
    let count = rng.range(4) + 1;
    let words: Vec<&str> = (0..count).map(|_| *rng.pick(MESSAGE_WORDS)).collect();
    words.join(" ")
}

fn random_event_value(rng: &mut Rng, field: &str) -> Value {
    match field {
        "severity" => {
            // OTel SeverityNumber 1-24, or null ~20% of the time to
            // exercise the `!=` NULL-inclusion semantics.
            if rng.range(5) == 0 {
                Value::Null
            } else {
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                Value::Number(((rng.range(24) + 1) as i64).into())
            }
        }
        "status" => {
            // Status is typically numeric — always use a number to avoid
            // DuckDB conversion errors on comparison operators.
            Value::Number((*rng.pick(INT_VALS)).into())
        }
        "path" => Value::String((*rng.pick(PATH_VALS)).to_string()),
        _ => Value::String((*rng.pick(STRING_VALS)).to_string()),
    }
}

// ── SQL execution ─────────────────────────────────────────────────────

fn bind_params(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
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
        .collect()
}

/// Execute emitted SQL against `DuckDB` and return whether any rows match.
fn sql_matches_where(conn: &Connection, emitted: &EmittedQuery, row_filter: &str) -> bool {
    let count_sql = format!(
        "SELECT count(*)::BIGINT FROM ({}) AS _sub WHERE {row_filter}",
        emitted.sql
    );

    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

    match conn.query_row(&count_sql, param_refs.as_slice(), |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(count) => count > 0,
        Err(e) => {
            let msg = e.to_string();
            // Type conversion failures and missing columns are semantically
            // "no match" — equivalent to CompiledFilter returning false.
            if msg.contains("not found")
                || msg.contains("Binder Error")
                || msg.contains("Conversion Error")
            {
                false
            } else {
                panic!(
                    "unexpected DuckDB error: {e}\nsql: {count_sql}\nparams: {:?}",
                    emitted.params
                );
            }
        }
    }
}

fn sql_matches(conn: &Connection, emitted: &EmittedQuery) -> bool {
    sql_matches_where(conn, emitted, "TRUE")
}

fn schema_witness_event() -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("message".into(), Value::String("schema witness".into()));
    event.insert("_raw".into(), Value::String("schema witness".into()));
    event.insert("service".into(), Value::String("schema-witness".into()));
    event.insert("status".into(), Value::from(-1));
    event.insert("host".into(), Value::String("schema-witness".into()));
    event.insert("path".into(), Value::String("/schema-witness".into()));
    event.insert("severity".into(), Value::from(1));
    event.insert("_parity_target".into(), Value::Bool(false));
    event
}

fn non_text_schema_witness_event() -> Map<String, Value> {
    let mut event = schema_witness_event();
    event.insert("message".into(), Value::from(123));
    event.insert("_raw".into(), Value::Bool(false));
    event.insert("service".into(), Value::String("non-text-witness".into()));
    event
}

// ── Property test ─────────────────────────────────────────────────────

#[test]
fn filter_matches_sql_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let mut rng = Rng::new(0xDEAD_BEEF);
    let mut passed = 0u32;
    let mut skipped = 0u32;

    for i in 0..1000 {
        let dsl = random_dsl(&mut rng);
        let event = random_event(&mut rng);

        // Parse DSL — skip invalid combinations.
        let Ok(query) = parser::parse(&dsl) else {
            skipped += 1;
            continue;
        };

        // In-memory filter result — a compile error mirrors an emit error.
        let Ok(filter) = CompiledFilter::compile(&query.search, &FieldTypes::new()) else {
            skipped += 1;
            continue;
        };
        let filter_result = filter.matches(&event);

        // Write event as ndjson (suffix required for emitter dispatch).
        let mut tmp = tempfile::Builder::new()
            .suffix(".ndjson")
            .tempfile()
            .unwrap();
        let mut target = event.clone();
        target.insert("_parity_target".into(), Value::Bool(true));
        let event_value = Value::Object(target);
        writeln!(tmp, "{event_value}").unwrap();
        writeln!(tmp, "{}", Value::Object(schema_witness_event())).unwrap();
        tmp.flush().unwrap();
        let tmp_path = tmp.path().to_str().expect("temp path is valid UTF-8");

        // Emit SQL.
        let Ok(emitted) = emitter::emit(
            &query,
            tmp_path,
            trawl_core::context::EvalContext::capture(),
        ) else {
            skipped += 1;
            continue;
        };

        // DuckDB SQL result.
        let sql_result = sql_matches_where(&conn, &emitted, r#""_parity_target" = TRUE"#);

        assert_eq!(
            filter_result, sql_result,
            "PARITY MISMATCH (iteration {i})\n\
             dsl: {dsl:?}\n\
             event: {event:?}\n\
             filter: {filter_result}\n\
             sql: {sql_result}\n\
             emitted: {}\n\
             params: {:?}",
            emitted.sql, emitted.params
        );

        passed += 1;
    }

    eprintln!("filter parity: {passed} passed, {skipped} skipped");
    assert!(
        passed >= 800,
        "too many skipped iterations: {skipped} skipped, {passed} passed"
    );
}

// ── Deterministic parity cases (ADR-0009) ─────────────────────────────

/// Assert filter and SQL agree for one (dsl, event) pair.
fn assert_parity(conn: &Connection, dsl: &str, event: &Map<String, Value>) {
    let query = parser::parse(dsl).expect("dsl parses");
    let filter =
        CompiledFilter::compile(&query.search, &FieldTypes::new()).expect("filter compiles");
    let filter_result = filter.matches(event);

    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    tmp.flush().unwrap();
    let emitted = emitter::emit(
        &query,
        tmp.path().to_str().unwrap(),
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches(conn, &emitted);

    assert_eq!(
        filter_result, sql_result,
        "parity mismatch
dsl: {dsl:?}
event: {:?}
sql: {}",
        event, emitted.sql
    );
}

/// Assert text-search parity against a sparse target row while a second row
/// supplies `read_json_auto` with both text columns. The outer target filter
/// prevents the schema witness from affecting the answer under test.
fn assert_sparse_text_parity(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    expected: bool,
) {
    let query = parser::parse(dsl).expect("dsl parses");
    let filter =
        CompiledFilter::compile(&query.search, &FieldTypes::new()).expect("filter compiles");
    let filter_result = filter.matches(event);

    let mut target = event.clone();
    target.insert("_parity_target".into(), Value::Bool(true));
    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(target)).unwrap();
    writeln!(tmp, "{}", Value::Object(schema_witness_event())).unwrap();
    // Force `read_json_auto` to infer heterogeneous text columns as JSON. The
    // target row can then prove both genuine JSON strings and non-string cells.
    writeln!(tmp, "{}", Value::Object(non_text_schema_witness_event())).unwrap();
    tmp.flush().unwrap();

    let emitted = emitter::emit(
        &query,
        tmp.path().to_str().unwrap(),
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches_where(conn, &emitted, r#""_parity_target" = TRUE"#);
    assert_eq!(
        filter_result, sql_result,
        "sparse text parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\nsql: {}",
        emitted.sql
    );
    assert_eq!(
        filter_result, expected,
        "wrong outcome for {dsl:?} on {event:?}"
    );
}

fn envelope_event(severity: Option<i64>, message: &str, raw: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("_time".into(), Value::String("2026-01-01T12:00:00Z".into()));
    m.insert("service".into(), Value::String("nginx".into()));
    m.insert("host".into(), Value::String("web-1".into()));
    m.insert("message".into(), Value::String(message.into()));
    m.insert(
        "_raw".into(),
        raw.map_or(Value::Null, |r| Value::String(r.into())),
    );
    m.insert(
        "_severity".into(),
        severity.map_or(Value::Null, |n| Value::Number(n.into())),
    );
    m
}

/// Assert the live filter and the pin-aware SQL agree for one
/// `_severity` search filter over a stored ladder number, and return the
/// agreed answer.
fn assert_severity_parity(conn: &Connection, dsl: &str, stored: Option<i64>) -> bool {
    let ft = pinned(&[("_severity", CanonicalType::Severity)]);
    let query = parser::parse(dsl).expect("dsl parses");
    let filter = CompiledFilter::compile(&query.search, &ft).expect("filter compiles");

    let mut event = Map::new();
    event.insert("message".into(), Value::String("hello".into()));
    if let Some(n) = stored {
        event.insert("_severity".into(), Value::from(n));
    }
    let filter_result = filter.matches(&event);

    let tmp = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let stored_sql = stored.map_or_else(
        || "CAST(NULL AS BIGINT)".to_owned(),
        |n| format!("CAST({n} AS BIGINT)"),
    );
    conn.execute_batch(&format!(
        "COPY (SELECT {stored_sql} AS \"_severity\", 'hello' AS message) \
         TO '{source}' (FORMAT PARQUET)"
    ))
    .expect("write typed parquet");

    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        &ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches_strict(conn, &emitted);
    assert_eq!(
        filter_result, sql_result,
        "severity parity mismatch\ndsl: {dsl:?}\nstored: {stored:?}\nsql: {}\nparams: {:?}",
        emitted.sql, emitted.params
    );
    filter_result
}

/// `_severity` band predicates agree between SQL and the in-memory
/// filter for every severity number and NULL (ADR-0013: the vocabulary
/// rides the SEVERITY pin, so both lanes bind it from one rule table).
#[test]
fn severity_band_parity_exhaustive() {
    let conn = Connection::open_in_memory().unwrap();
    let dsls = [
        "_severity=error",
        "_severity>=warn",
        "_severity>warn",
        "_severity<info",
        "_severity<=info",
        "_severity!=info",
        "_severity=error,fatal",
        "_severity=trace,notice",
        // Issue #82: the whole list collapses into ONE membership test
        // over ladder points, so a list naming the same band twice, or a
        // band beside a point inside it, must still be exactly that set.
        "_severity=warn,17",
        "_severity=error,err,error2",
        "_severity=warn,99",
        "_severity!=warn,error",
        // Review finding A: out-of-ladder points collapse to ONE
        // representative in the render, because a SEVERITY subject is
        // 1-24 or NULL. These cells run over every stored number AND the
        // NULL — which is exactly where a naive constant-FALSE collapse
        // would diverge from the live matcher (NULL must stay UNKNOWN).
        "_severity=99,101,250",
        "_severity=-5,99",
        "_severity=error,99,101,250",
        "_severity!=99,101,250",
        "_severity=error2",
        "_severity=17",
        "_severity=warn*",
        "_severity=/^fatal/",
    ];
    for dsl in dsls {
        for sev in (1..=24).map(Some).chain([None]) {
            assert_severity_parity(&conn, dsl, sev);
        }
    }
}

/// Issue #91: search-stage `f!=a,b` is `f!=a AND f!=b`, including
/// scalar `!=`'s NULL widening, in both lanes.
#[test]
fn search_stage_ne_over_a_list_is_compositional_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    for sev in (1..=24).map(Some).chain([None]) {
        let matched = assert_severity_parity(&conn, "_severity!=warn,error", sev);
        let expected = sev.is_none_or(|n| !(13..=20).contains(&n));
        assert_eq!(
            matched, expected,
            "WARN and ERROR bands are excluded; outside/NULL matches (stored {sev:?})"
        );
    }

    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    for (event, expected) in [
        (status_event(&Value::from("200")), false),
        (status_event(&Value::from("0200")), false),
        (status_event(&Value::from("200.0")), false),
        (status_event(&Value::from("301")), false),
        (status_event(&Value::from("404")), true),
        (absent_status_event(), true),
    ] {
        assert_eq!(
            assert_pinned_parity(&conn, "status!=200,301", &event, &ft),
            expected,
            "event {event:?}"
        );
    }
}

/// A bare term matching only `_raw` content returns the event; negation
/// respects `_raw` too.
#[test]
fn bare_search_raw_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let cases: &[(&str, Option<i64>, &str, Option<&str>)] = &[
        // term only in _raw
        ("connection", None, "clean text", Some("connection refused")),
        // term only in message
        ("connection", None, "connection ok", Some("other")),
        // term in neither
        ("connection", None, "clean", Some("other")),
        // _raw null
        ("connection", None, "clean", None),
        ("connection", None, "connection", None),
        // negated: term in _raw only → excluded
        ("-connection", None, "clean", Some("connection refused")),
        // negated: term nowhere → included
        ("-connection", None, "clean", Some("other")),
        // negated with null _raw → included when message clean
        ("-connection", None, "clean", None),
        // quoted phrase in _raw only
        (
            "\"connection refused\"",
            None,
            "clean",
            Some("xx connection refused yy"),
        ),
    ];
    for &(dsl, sev, message, raw) in cases {
        let event = envelope_event(sev, message, raw);
        assert_parity(&conn, dsl, &event);
    }
}

/// ADR-0015: bare/quoted containment is the two-valued exception inside the
/// otherwise three-valued search evaluator. Generate every query spelling
/// over events whose text columns are carried, null, or absent, including an
/// OR composition where leaf totality is observable under NOT.
#[test]
fn sparse_text_containment_is_total_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let cases = [
        serde_json::json!({}),
        serde_json::json!({"message": null}),
        serde_json::json!({"_raw": null}),
        serde_json::json!({"message": "clean"}),
        serde_json::json!({"_raw": "clean"}),
        serde_json::json!({"message": "needle multi word alpha", "_raw": null}),
        serde_json::json!({"message": null, "_raw": "needle multi word beta"}),
        serde_json::json!({"message": "clean", "_raw": "also clean"}),
        serde_json::json!({"message": "beta", "_raw": "clean"}),
        serde_json::json!({"message": 123, "_raw": false}),
        serde_json::json!({"message": true, "_raw": 123}),
        serde_json::json!({"message": "123 true", "_raw": false}),
        serde_json::json!({"message": true, "_raw": "123 true"}),
    ];

    for value in cases {
        let Value::Object(event) = value else {
            unreachable!("fixtures are objects")
        };
        let contains = |needle: &str| {
            ["message", "_raw"].iter().any(|field| {
                event
                    .get(*field)
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.to_ascii_lowercase().contains(needle))
            })
        };
        for (dsl, expected) in [
            ("needle", contains("needle")),
            ("-needle", !contains("needle")),
            ("NOT needle", !contains("needle")),
            (r#"NOT "multi word""#, !contains("multi word")),
            ("alpha OR NOT beta", contains("alpha") || !contains("beta")),
            ("123", contains("123")),
            ("NOT 123", !contains("123")),
            ("true", contains("true")),
            ("NOT true", !contains("true")),
        ] {
            assert_sparse_text_parity(&conn, dsl, &event, expected);
        }
    }
}

/// The pipeline `where _severity …` stage means the same thing to the SQL
/// emitter and to the streaming evaluator, for every severity number and
/// NULL — the same rule table the search stage binds, with the STRICT
/// null policy (ADR-0011 slice A′).
#[test]
fn where_severity_band_parity_exhaustive() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[
        ("_severity", CanonicalType::Severity),
        ("status", CanonicalType::BigInt),
    ]);
    let dsls = [
        "* | where _severity == \"error\"",
        "* | where _severity != \"info\"",
        "* | where _severity >= \"warn\"",
        "* | where _severity > \"warn\"",
        "* | where _severity < \"info\"",
        "* | where _severity <= \"info\"",
        "* | where _severity == \"notice\"",
        "* | where _severity == \"error2\"",
        "* | where _severity == 17",
        "* | where _severity == \"error\" and status == 500",
        "* | where not (_severity == \"error\")",
        "* | where _severity in (\"warn\", \"error\")",
        // Review finding A, in the STRICT lane where `!=` really negates:
        // an all-out-of-ladder set must answer FALSE for every stored
        // number and UNKNOWN for the absent one, and its negation must be
        // TRUE / UNKNOWN respectively — which is what keeps the one
        // representative (rather than dropping the points) necessary.
        "* | where _severity in (99, 101, 250)",
        "* | where _severity in (\"error\", 99, 101, 250)",
        "* | where _severity != 99",
        "* | where not (_severity in (99, 101))",
        // ADJACENT REPRESENTATIVE EXTENSION, the subtlest collapse case:
        // the out-of-ladder 0 sits beside the TRACE band, so the render
        // widens to `BETWEEN 0 AND 4` rather than emitting 0 separately.
        // That is only sound because no stored severity is ever 0 — and
        // the NEGATION is where an unsound widening would show, since it
        // would start excluding a row the exact per-point semantics keep.
        // Exhaustive over every stored 1-24 and the absent column, both
        // lanes.
        "* | where _severity in (0, \"trace\")",
        "* | where not (_severity in (0, \"trace\"))",
        "* | where not (_severity in (0, 25))",
    ];
    for dsl in dsls {
        for sev in (1..=24).map(Some).chain([None]) {
            let mut event = envelope_event(sev, "hello", None);
            event.insert("status".into(), Value::Number(500.into()));
            assert_where_parity_over_severity(&conn, dsl, &event, &ft, sev);
        }
    }
}

/// The `| where` half of [`assert_severity_parity`]: both lanes over a
/// stored `_severity` BIGINT column, three-valued answer compared
/// directly so UNKNOWN is observed rather than inferred from "no row".
fn assert_where_parity_over_severity(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    stored: Option<i64>,
) {
    let (query, condition) = where_condition(dsl);
    let scope = trawl_core::pin_scope::PinScope::root(ft);
    let eval_result = eval_truth(&trawl_core::eval::eval_expr_with_pins(
        &condition,
        &trawl_core::row::from_json(event),
        &scope,
    ));

    let tmp = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let stored_sql = stored.map_or_else(
        || "CAST(NULL AS BIGINT)".to_owned(),
        |n| format!("CAST({n} AS BIGINT)"),
    );
    conn.execute_batch(&format!(
        "COPY (SELECT {stored_sql} AS \"_severity\", CAST(500 AS BIGINT) AS status, \
         'hello' AS message) TO '{source}' (FORMAT PARQUET)"
    ))
    .expect("write typed parquet");

    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches_strict(conn, &emitted);
    assert_eq!(
        eval_result == Some(true),
        sql_result,
        "where severity parity mismatch\ndsl: {dsl:?}\nstored: {stored:?}\nsql: {}",
        emitted.sql
    );
}

/// `last=` windows evaluate against `_time` identically in SQL and the
/// in-memory filter (times chosen far from the boundary — no race).
#[test]
fn time_filter_parity_on_time_column() {
    let conn = Connection::open_in_memory().unwrap();
    for (age_secs, dsl) in [(10, "last=1h"), (10 * 3600, "last=1h"), (30, "last=2m")] {
        let ts = (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let mut event = envelope_event(Some(9), "hello", None);
        event.insert("_time".into(), Value::String(ts));
        assert_parity(&conn, dsl, &event);
    }
}

// ── Pinned parity (ADR-0011 slice A) ──────────────────────────────────

/// Execute emitted SQL strictly: ANY `DuckDB` error fails the test. The
/// pinned rules exist precisely so a comparison against a pinned column
/// can never throw — an unexpected error here is a broken rule, never a
/// "no match".
fn sql_matches_strict(conn: &Connection, emitted: &EmittedQuery) -> bool {
    sql_matches_strict_row(conn, emitted, "TRUE")
}

/// [`sql_matches_strict`] restricted to the rows a source carries beside
/// the one under test — the filter names the target row, never the
/// predicate under test.
fn sql_matches_strict_row(conn: &Connection, emitted: &EmittedQuery, row_filter: &str) -> bool {
    let count_sql = format!(
        "SELECT count(*)::BIGINT FROM ({}) AS _sub WHERE {row_filter}",
        emitted.sql
    );
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let count: i64 = conn
        .query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or_else(|e| {
            panic!(
                "unexpected DuckDB error under pins: {e}\nsql: {count_sql}\nparams: {:?}",
                emitted.params
            )
        });
    count > 0
}

fn pinned(entries: &[(&str, CanonicalType)]) -> FieldTypes {
    let mut ft = FieldTypes::new();
    for (field, ty) in entries {
        ft.insert(field, *ty);
    }
    ft
}

/// Assert filter and pin-aware SQL agree for one (dsl, event, pins)
/// triple, over a source whose physical column type equals the pin.
///
/// Non-null events ride ndjson (`read_json` infers VARCHAR for JSON
/// strings, BIGINT for JSON ints — already the pin's physical type). An
/// all-null event would infer a JSON column instead — off the write-time
/// invariant every real cold file satisfies — so the null case goes
/// through a parquet COPY harness that types the column explicitly.
///
/// An ABSENT key takes that same parquet path, because in batch it IS the
/// null case: a file whose rows never carried the field still reads the
/// pinned column as NULL. Absent is the dominant shape on the event bus
/// (the canonicalizer only fills envelope fields), so the two must answer
/// identically or `/query` and `/stream` disagree on every field-less
/// event.
///
/// Returns the answer the two engines agreed on, because agreement is not
/// always the whole property: the DOUBLE comparison space ADR-0011 ruling
/// #6 replaced collapsed every id above 2^53 in the SQL and in the matcher
/// IDENTICALLY, so a parity assertion alone watched both sides agree on
/// the wrong row. A caller that knows what the answer must BE says so.
fn assert_pinned_parity(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
) -> bool {
    let query = parser::parse(dsl).expect("dsl parses");
    let filter = CompiledFilter::compile(&query.search, ft).expect("filter compiles");
    let filter_result = filter.matches(event);

    // Keep the temp files alive for the duration of the SQL run.
    let _guard: Box<dyn std::any::Any>;
    let source = if matches!(event.get("status"), None | Some(&Value::Null)) {
        let tmp = tempfile::Builder::new()
            .suffix(".parquet")
            .tempfile()
            .unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        let ty = match ft.get("status") {
            Some(t) => t.as_duckdb(),
            None => "VARCHAR",
        };
        conn.execute_batch(&format!(
            "COPY (SELECT CAST(NULL AS {ty}) AS status, 'hello' AS message) \
             TO '{path}' (FORMAT PARQUET)"
        ))
        .expect("write typed-null parquet");
        _guard = Box::new(tmp);
        path
    } else {
        let mut tmp = tempfile::Builder::new()
            .suffix(".ndjson")
            .tempfile()
            .unwrap();
        writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
        tmp.flush().unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        _guard = Box::new(tmp);
        path
    };
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches_strict(conn, &emitted);

    assert_eq!(
        filter_result, sql_result,
        "pinned parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\nsql: {}\nparams: {:?}",
        emitted.sql, emitted.params
    );
    filter_result
}

/// Assert filter and pin-aware SQL agree for one (dsl, event, pins)
/// triple over a source whose `status` column is written by an explicit
/// SQL expression.
///
/// The wire shape and the stored shape are NOT the same thing once a pin
/// exists: a wire `200` and a wire `"200"` both conform to the DOUBLE
/// `200.0`, and no `read_json` inference reproduces that. So the stored
/// value is spelled out — exactly the column compaction wrote — while the
/// matcher still sees the wire event.
///
/// Returns the agreed answer, for the same reason
/// [`assert_pinned_parity`] does.
fn assert_pinned_parity_over_column(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    stored_sql: &str,
) -> bool {
    let query = parser::parse(dsl).expect("dsl parses");
    let filter = CompiledFilter::compile(&query.search, ft).expect("filter compiles");
    let filter_result = filter.matches(event);

    let tmp = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    conn.execute_batch(&format!(
        "COPY (SELECT {stored_sql} AS status, 'hello' AS message) \
         TO '{source}' (FORMAT PARQUET)"
    ))
    .expect("write typed parquet");

    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches_strict(conn, &emitted);

    assert_eq!(
        filter_result, sql_result,
        "pinned parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\nstored: {stored_sql}\nsql: {}\nparams: {:?}",
        emitted.sql, emitted.params
    );
    filter_result
}

fn status_event(value: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("status".into(), value.clone());
    m.insert("message".into(), Value::String("hello".into()));
    m
}

/// The field-less event: `status` never reaches the bus at all.
fn absent_status_event() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("message".into(), Value::String("hello".into()));
    m
}

/// The ADR-0013 deterministic matrix: a SEVERITY-pinned `status` over
/// stored ladder numbers × the whole token vocabulary × every operator
/// class, with the EXPECTED answer stated so a matching pair of wrong
/// lanes cannot pass.
///
/// `status` rather than `_severity` on purpose: the harness's one pinned
/// column is what makes the stored value spellable, and the rule table
/// binds by the PIN, never by the name.
#[test]
fn pinned_severity_matrix_parity() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    let ft = pinned(&[("status", CanonicalType::Severity)]);

    // (dsl, stored severity number, expected match)
    let cases: &[(&str, i64, bool)] = &[
        // A band token is the whole band, on both ends.
        ("status=error", 17, true),
        ("status=error", 20, true),
        ("status=error", 16, false),
        ("status=error", 21, false),
        // The aliases the ADR-0009 token table carries.
        ("status=err", 18, true),
        ("status=WARN", 13, true),
        ("status=warning", 16, true),
        // An OTel exact short name is ONE number.
        ("status=error2", 18, true),
        ("status=error2", 17, false),
        ("status=warn4", 16, true),
        // An integer literal is that number.
        ("status=17", 17, true),
        ("status=17", 18, false),
        // Ordered comparisons take the token's own number.
        ("status>=warn", 13, true),
        ("status>=warn", 12, false),
        ("status>error", 18, true),
        ("status<info", 8, true),
        ("status<=fatal", 21, true),
        // `!=` is the band's complement.
        ("status!=error", 9, true),
        ("status!=error", 17, false),
        // An IN list ORs the bands.
        ("status=warn,error", 14, true),
        ("status=warn,error", 21, false),
        // Glob and regex match the CANONICAL token text, so a band's
        // prefix glob is exactly its four rungs.
        ("status=warn*", 13, true),
        ("status=warn*", 16, true),
        ("status=warn*", 17, false),
        ("status=/^error2$/", 18, true),
        ("status=/^error2$/", 17, false),
    ];

    for (dsl, stored, expected) in cases {
        let event = status_event(&Value::from(*stored));
        let got = assert_pinned_parity_over_column(
            &conn,
            dsl,
            &event,
            &ft,
            &format!("CAST({stored} AS BIGINT)"),
        );
        assert_eq!(got, *expected, "{dsl} over stored {stored}");
    }

    // An out-of-ladder stored value conforms to NULL, so every comparison
    // over it is UNKNOWN — and `!=` widens in the search stage alone.
    for (dsl, expected) in [
        ("status=error", false),
        ("status>=warn", false),
        ("status!=error", true),
    ] {
        let event = status_event(&Value::from(99));
        let got = assert_pinned_parity_over_column(&conn, dsl, &event, &ft, "CAST(NULL AS BIGINT)");
        assert_eq!(got, expected, "{dsl} over an out-of-ladder value");
    }

    // An unknown token is an ERROR in BOTH lanes, with the same sentence.
    let query = parser::parse("status=spicy").expect("dsl parses");
    let filter_err = CompiledFilter::compile(&query.search, &ft).expect_err("filter refuses");
    let emit_err = emitter::emit_with_pins(
        &query,
        "/x/*.parquet",
        &ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect_err("emit refuses");
    assert_eq!(filter_err.to_string(), emit_err.to_string());
    assert!(
        filter_err.to_string().contains("unknown severity value"),
        "{filter_err}"
    );
}

/// The slice-A deterministic matrix: a VARCHAR-pinned `status` over
/// string-stored values (the physical column `read_json` infers is
/// VARCHAR, matching the pin) × every operator class × numeric and
/// non-numeric literals. Unexpected `DuckDB` errors fail the test.
#[test]
fn pinned_varchar_matrix_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    let values = [
        Value::String("200".into()),
        Value::String("404".into()),
        Value::String("accepted".into()),
        Value::String("0".into()),
        Value::String("1.5".into()),
        // Values inside DuckDB's DECIMAL(38,6) cast domain but outside
        // `str::parse` — batch counts them as numbers, so the live matcher
        // must too (whitespace, `_` separators, leading zeros).
        Value::String(" 200".into()),
        Value::String("200_000".into()),
        Value::String("0404".into()),
        Value::String("+5".into()),
        Value::String("1e3".into()),
        // Numbers to Rust's parser with NO reading in the comparison
        // space: 'nan' and 'inf' used to order above every literal in
        // DuckDB's total DOUBLE ordering, and now match nothing at all —
        // on both sides (ADR-0011 ruling #6).
        Value::String("nan".into()),
        Value::String("inf".into()),
        Value::String("-inf".into()),
        // Past the space's magnitude, and past 2^53 inside it.
        Value::String("1e40".into()),
        Value::String("9007199254740993".into()),
        // Outside every domain: still UNKNOWN on both sides.
        Value::String("0x10".into()),
        Value::Null,
    ];
    let dsls = [
        // eq/ne/IN, numeric and non-numeric literals
        "status=200",
        "status!=200",
        "status=200,301",
        "status!=200,301",
        "status=200,accepted",
        "status=accepted",
        "status!=accepted",
        // ordered, numeric literal (TRY_CAST DOUBLE rule)
        "status>400",
        "status>=400",
        "status<400",
        "status<=400",
        "status>=0",
        "status>1",
        "status<2",
        // ordered, non-numeric literal (lexical rule, unchanged)
        "status>accepted",
        "status<accepted",
        // numeric literals with no reading in the comparison space: the
        // numeric RULE still applies (never the lexical one), and its
        // right-hand cast is NULL, so every row is UNKNOWN on both sides.
        "status>nan",
        "status<inf",
        "status>=1e40",
        "status=nan",
        "status!=nan",
        // past 2^53, where a DOUBLE space equated neighbours
        "status=9007199254740993",
        "status!=9007199254740993",
        "status>9007199254740992",
        // glob / regex (unchanged under the VARCHAR pin)
        "status=2*",
        "status=/2.*/",
        // NOT over each class: a TRY_CAST miss and a NULL column are
        // UNKNOWN, and `NOT UNKNOWN` is UNKNOWN — never a live match the
        // batch query would drop.
        "NOT status>=400",
        "NOT status<400",
        "NOT status=200",
        "NOT status!=200",
        "NOT status=200,301",
        "NOT status=accepted",
        "NOT status>accepted",
        "NOT status=2*",
        "NOT status=/2.*/",
    ];
    let events = values
        .iter()
        .map(status_event)
        .chain(std::iter::once(absent_status_event()));
    for event in events {
        for dsl in dsls {
            assert_pinned_parity(&conn, dsl, &event, &ft);
        }
    }
}

/// BIGINT-pinned control: glob/regex against integer-stored values (the
/// physical column is BIGINT, matching the pin) go through
/// `CAST(col AS VARCHAR)` on the SQL side and the matcher's
/// stringification in memory — and must agree.
#[test]
fn pinned_bigint_pattern_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::BigInt)]);
    let dsls = [
        "status=4*",
        "status=2*",
        "status=/4.*/",
        "status=/^40.$/",
        // non-pattern ops stay native under a typed pin
        "status=404",
        "status>=400",
        "status!=200",
        "NOT status=4*",
        "NOT status>=400",
        "NOT status!=200",
    ];
    let events = [200i64, 404, 0, 4]
        .into_iter()
        .map(|v| status_event(&Value::Number(v.into())))
        // ... and the field-less event, a NULL BIGINT column in batch.
        .chain(std::iter::once(absent_status_event()));
    for event in events {
        for dsl in dsls {
            assert_pinned_parity(&conn, dsl, &event, &ft);
        }
    }
}

/// DOUBLE-pinned patterns: the column is DOUBLE whatever the wire number
/// looked like, so the pattern text is `DuckDB`'s DOUBLE rendering
/// (`200.0`, `1e-07`, `1.2345678901234568e+17`) on BOTH sides — a matcher
/// that stringified the wire value would answer `status=/^200$/` TRUE
/// where the batch query answers FALSE.
///
/// Each case pairs the wire event the matcher sees with the stored column
/// compaction wrote for it: a wire `200`, a wire `200.0` and a wire
/// `"200"` all conform to the same DOUBLE (the conform guard round-trips
/// `TRY_CAST(text AS DOUBLE)`), while text with no numeric reading
/// conforms to NULL.
#[test]
fn pinned_double_pattern_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Double)]);
    let patterns = [
        "status=2*",
        "status=200*",
        "status=/^200$/",
        r"status=/^200\.0$/",
        r"status=/^-?[0-9]+\.0$/",
        "status=/e[+-]/",
        "status=/^0.0$/",
        r"status=/^-0\.0$/",
        "NOT status=/^200$/",
        r"NOT status=/^200\.0$/",
        "NOT status=2*",
    ];
    // Non-pattern ops stay native under a typed pin, unchanged.
    let natives = [
        "status=200",
        "status>=400",
        "status!=200",
        "NOT status>=400",
    ];
    // (wire value the matcher sees, the DOUBLE column compaction stored)
    let cases = [
        (Value::from(200), "CAST(200 AS DOUBLE)"),
        (Value::from(200.0), "CAST(200 AS DOUBLE)"),
        (Value::from("200"), "CAST(200 AS DOUBLE)"),
        (Value::from(0), "CAST(0 AS DOUBLE)"),
        // Negative zero keeps its sign in DuckDB's rendering. Spelled as
        // a product because the SQL literal `-0.0` constant-folds to
        // positive zero — the stored value would not be the one under
        // test.
        (Value::from(-0.0), "CAST(0.0 AS DOUBLE) * -1"),
        (Value::from(-3), "CAST(-3 AS DOUBLE)"),
        (Value::from(1.5), "CAST(1.5 AS DOUBLE)"),
        (Value::from(404), "CAST(404 AS DOUBLE)"),
        // Sci-notation shapes: the exponent is signed and two-digit wide
        // in DuckDB, unsigned and unpadded in Rust's own rendering.
        (Value::from(1e-7), "CAST(1e-7 AS DOUBLE)"),
        (
            Value::from(123_456_789_012_345_680i64),
            "CAST(123456789012345680 AS DOUBLE)",
        ),
    ];
    for (wire, stored) in cases {
        let event = status_event(&wire);
        for dsl in patterns.iter().chain(natives.iter()) {
            assert_pinned_parity_over_column(&conn, dsl, &event, &ft, stored);
        }
    }
    // A wire value the pin NULLs out: no numeric reading, so the pattern
    // target is a NULL column in batch and `None` in memory — UNKNOWN on
    // both sides. Patterns only: whether a non-conforming wire value
    // should compare as its own text or as the NULL the corpus stores is
    // the wire-vs-conformed question every typed pin already has (a
    // string under a BIGINT pin included), not the pattern rule.
    let unreadable = status_event(&Value::from("accepted"));
    for dsl in patterns {
        assert_pinned_parity_over_column(&conn, dsl, &unreadable, &ft, "CAST(NULL AS DOUBLE)");
    }
    // Explicit null and the field-less event go through the shared
    // typed-null harness.
    for event in [status_event(&Value::Null), absent_status_event()] {
        for dsl in patterns.iter().chain(natives.iter()) {
            assert_pinned_parity(&conn, dsl, &event, &ft);
        }
    }
}

/// The IEEE specials under a DOUBLE pin: a live filter compares them in
/// `DuckDB`'s TOTAL order, not Rust's.
///
/// Every NaN equals every other NaN whatever its sign and outranks `inf`,
/// so `metric=nan` MATCHES a stored NaN — the answer the matcher's own
/// IEEE `==` used to get wrong while the batch query returned the row.
/// The expected answers are stated, not merely agreed on: two lanes
/// sharing one wrong comparator would pass a bare parity assertion.
///
/// The column is spelled out rather than inferred, because a wire `"nan"`
/// and a wire `"-nan"` both conform to a DOUBLE the round-trip guard
/// keeps (probed), and no `read_json` inference reproduces that.
#[test]
fn pinned_double_special_value_parity() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    let ft = pinned(&[("status", CanonicalType::Double)]);

    // (wire value, the stored DOUBLE, `status=nan`, `status>1.5`)
    let cases = [
        (Value::from("nan"), "CAST('nan' AS DOUBLE)", true, true),
        (Value::from("-nan"), "CAST('-nan' AS DOUBLE)", true, true),
        (Value::from("inf"), "CAST('inf' AS DOUBLE)", false, true),
        (Value::from("-inf"), "CAST('-inf' AS DOUBLE)", false, false),
        (Value::from(1.5), "CAST(1.5 AS DOUBLE)", false, false),
        (Value::from(2.5), "CAST(2.5 AS DOUBLE)", false, true),
        // Negative zero, spelled as a product: the SQL literal `-0.0`
        // constant-folds to positive zero.
        (Value::from(-0.0), "CAST(0.0 AS DOUBLE) * -1", false, false),
        (Value::from(0), "CAST(0 AS DOUBLE)", false, false),
    ];
    // Every operator class against a NaN literal, plus the ordered
    // comparisons that place NaN in the order at all.
    let dsls = [
        "status=nan",
        "status=-nan",
        "status!=nan",
        "status<nan",
        "status>nan",
        "status>=nan",
        "status<=nan",
        "NOT status=nan",
        "status>1.5",
        "status<1.5",
        "status=inf",
        "status=0",
        "status=-0.0",
        "status=nan,1.5",
        "status!=nan,1.5",
    ];
    for (wire, stored, equals_nan, above_one_and_a_half) in cases {
        let event = status_event(&wire);
        for dsl in dsls {
            let agreed = assert_pinned_parity_over_column(&conn, dsl, &event, &ft, stored);
            match dsl {
                "status=nan" => assert_eq!(agreed, equals_nan, "{dsl} over {wire:?}"),
                "status>1.5" => assert_eq!(agreed, above_one_and_a_half, "{dsl} over {wire:?}"),
                _ => {}
            }
        }
    }

    // A NULL column is UNKNOWN, not false: nothing matches it — except
    // the search stage's `!=`, which is the one TOTAL comparison (it
    // widens with `OR col IS NULL`, ADR-0011), and `NOT status=nan`,
    // which does NOT, because `NOT (NULL)` is NULL.
    for event in [status_event(&Value::Null), absent_status_event()] {
        for dsl in dsls {
            let widened = dsl.contains("!=");
            assert_eq!(
                assert_pinned_parity(&conn, dsl, &event, &ft),
                widened,
                "{dsl} over a NULL column"
            );
        }
    }
}

/// A mixed-case DSL reference names ONE column on both sides: `DuckDB`
/// folds `"Status"` onto the physical `status`, and ingest folds every
/// incoming field name, so the matcher must read the same folded key.
///
/// Absent-key `!=` is a MATCH (`OR col IS NULL`), so a matcher that reads
/// `Status` verbatim sees a NULL column on EVERY event and fires on all of
/// them while the batch query returns only the real non-matching rows —
/// which is why this runs against `DuckDB` rather than asserting a shape.
#[test]
fn mixed_case_field_reference_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    let dsls = [
        "Status=200",
        "Status!=200",
        "STATUS!=200",
        "Status=200,301",
        "Status>=400",
        "Status=2*",
        "Status=/2.*/",
        "NOT Status!=200",
    ];
    let values = [
        Value::String("200".into()),
        Value::String("404".into()),
        Value::String("accepted".into()),
        Value::Null,
    ];
    let events = values
        .iter()
        .map(status_event)
        .chain(std::iter::once(absent_status_event()));
    for event in events {
        for dsl in dsls {
            assert_pinned_parity(&conn, dsl, &event, &ft);
        }
    }
}

/// Patterns over a TIMESTAMP-pinned column: batch renders the canonical
/// RFC 3339 microsecond text through `strftime`, the live matcher renders
/// the same text from the wire value, so a pattern anchored on the
/// separator, the zone suffix or the fraction means ONE thing.
///
/// A value with no timestamp reading is NULL on disk and UNKNOWN in
/// memory, including under `NOT`.
///
/// The parquet is written through [`trawl_core::conform::guarded_cast`]
/// itself — what compaction and the hot `REPLACE` both emit — so this is
/// end-to-end evidence for ADR-0011 ruling #1 rather than a re-statement
/// of the mirror: a zone-bearing value is stored as the UTC instant, and
/// `…T09:00:00+05:30` therefore matches `/T03:30/` on BOTH sides and
/// `/T09:/` on neither. That rung parses through `TIMESTAMPTZ`, so the
/// writing session is pinned to UTC exactly as every conforming
/// connection is.
///
/// The value list is the shapes ADR-0011 ruling #4 turned up by
/// execution — `epoch`, a trailing zone NAME, hour-24 rollover, and the
/// seconds-less `T09:00+00:00` that used to fire live while batch stored
/// NULL — beside the ordinary ones. The exhaustive text matrix lives in
/// `trawl-engine/tests/duckdb_probe.rs`; this test proves the same
/// agreement survives the whole pipeline (pin → emit → parquet → GLOB).
#[test]
fn pinned_timestamp_pattern_parity() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    let ft = pinned(&[("_time", CanonicalType::Timestamp)]);
    let values = [
        // The canonical wire form ingest writes.
        Value::String("2026-01-15T09:00:00.000000Z".into()),
        Value::String("2026-01-15T09:00:00.123456Z".into()),
        Value::String("2026-01-15T22:00:00.000000Z".into()),
        // Shapes a custom TIMESTAMP-pinned field can carry.
        Value::String("2026-01-15 09:00:00".into()),
        Value::String("2026-01-15".into()),
        // Zone-bearing: the offset is APPLIED, so the stored hour moves.
        Value::String("2026-01-15T09:00:00+05:30".into()),
        Value::String("2026-01-15T09:00:00-08:00".into()),
        Value::String("2026-01-15T09:00:00+0530".into()),
        Value::String("2026-01-15T09:00:00+02".into()),
        // The ruling #4 shapes.
        Value::String("epoch".into()),
        Value::String("2026-01-15 09:00:00 UTC".into()),
        Value::String("2026-01-15 24:00:00".into()),
        Value::String("2026-01-15T09:00+00:00".into()),
        // No timestamp reading → NULL column / UNKNOWN matcher.
        Value::String("yesterday-ish".into()),
        Value::String("1737000000".into()),
        Value::Null,
    ];
    let dsls = [
        // separator-anchored, both ways round
        "_time=/T09:/",
        "_time=/ 09:/",
        // the hour an applied offset moves the value to
        "_time=/T03:30/",
        "_time=/T17:00/",
        "_time=/T07:00/",
        // zone suffix
        "_time=/Z$/",
        // fractional seconds
        r"_time=/\.000000Z$/",
        r"_time=/\.123456Z$/",
        // date prefix (glob and regex), incl. the day hour-24 rolls into
        "_time=2026-01-15*",
        "_time=2026-01-16*",
        "_time=2026-01-15T09*",
        "_time=/^2026-01-15/",
        // the keyword instant's own date
        "_time=1970-01-01*",
        // NOT over each shape: UNKNOWN must not invert into a live match
        "NOT _time=/T09:/",
        "NOT _time=/Z$/",
        "NOT _time=2026-01-15T09*",
    ];

    for value in &values {
        let mut event = Map::new();
        event.insert("_time".into(), value.clone());
        event.insert("message".into(), Value::String("hello".into()));

        let tmp = tempfile::Builder::new()
            .suffix(".parquet")
            .tempfile()
            .unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        let wire = match value {
            Value::String(s) => format!("'{s}'"),
            _ => "CAST(NULL AS VARCHAR)".to_owned(),
        };
        let stored = trawl_core::conform::guarded_cast(&wire, CanonicalType::Timestamp);
        conn.execute_batch(&format!(
            "COPY (SELECT {stored} AS _time, 'hello' AS message) TO '{path}' (FORMAT PARQUET)"
        ))
        .expect("write timestamp parquet");

        for dsl in dsls {
            let query = parser::parse(dsl).expect("dsl parses");
            let filter = CompiledFilter::compile(&query.search, &ft).expect("filter compiles");
            let filter_result = filter.matches(&event);
            let emitted = emitter::emit_with_pins(
                &query,
                &path,
                &ft,
                trawl_core::context::EvalContext::capture(),
            )
            .expect("emit succeeds");
            let sql_result = sql_matches_strict(&conn, &emitted);
            assert_eq!(
                filter_result, sql_result,
                "timestamp pattern parity mismatch\ndsl: {dsl:?}\nvalue: {value:?}\nsql: {}",
                emitted.sql
            );
        }
    }
}

/// Hot+cold union with BOTH pin sets: the cold branch reads one event,
/// the hot branch another (conformed via the REPLACE list), and the
/// pin-aware comparison must agree with the in-memory filter over the
/// pair — SQL matches iff the filter matches either event.
#[test]
fn pinned_hot_cold_union_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);

    let envelope = |status: &str| {
        let mut m = Map::new();
        m.insert("_time".into(), Value::String("2026-01-01T12:00:00Z".into()));
        m.insert(
            "_ingested".into(),
            Value::String("2026-01-01T12:00:01Z".into()),
        );
        m.insert("status".into(), Value::String(status.into()));
        m.insert("message".into(), Value::String("hello".into()));
        m
    };

    let cases = [
        ("status=200", "200", "404"),
        ("status=200", "404", "200"),
        ("status=200", "404", "500"),
        ("status>=400", "500", "accepted"),
        ("status>=400", "accepted", "200"),
        ("status!=200", "200", "200"),
        ("status=200,301", "301", "accepted"),
    ];
    for (dsl, cold_status, hot_status) in cases {
        let cold = envelope(cold_status);
        let hot = envelope(hot_status);

        let mut cold_tmp = tempfile::Builder::new()
            .suffix(".ndjson")
            .tempfile()
            .unwrap();
        writeln!(cold_tmp, "{}", Value::Object(cold.clone())).unwrap();
        cold_tmp.flush().unwrap();
        let mut hot_tmp = tempfile::Builder::new()
            .suffix(".ndjson")
            .tempfile()
            .unwrap();
        writeln!(hot_tmp, "{}", Value::Object(hot.clone())).unwrap();
        hot_tmp.flush().unwrap();

        let query = parser::parse(dsl).expect("dsl parses");
        // hot_pins = pins ∩ hot keys; `status` is observed in the hot
        // snapshot, so both sets carry it here.
        let emitted = emitter::emit_with_hot_source(
            &query,
            cold_tmp.path().to_str().unwrap(),
            hot_tmp.path().to_str().unwrap(),
            &ft,
            &ft,
            trawl_core::context::EvalContext::capture(),
        )
        .expect("emit succeeds");
        let sql_result = sql_matches_strict(&conn, &emitted);

        let filter = CompiledFilter::compile(&query.search, &ft).expect("filter compiles");
        let filter_result = filter.matches(&cold) || filter.matches(&hot);

        assert_eq!(
            filter_result, sql_result,
            "hot+cold pinned parity mismatch\ndsl: {dsl:?}\ncold: {cold_status} hot: {hot_status}\nsql: {}",
            emitted.sql
        );
    }
}

/// Write `event` (plus any `siblings`) as a hot-buffer snapshot and emit
/// the hot-only query over it — the lane where the emitter conforms the
/// very bytes the matcher reads, so no stored column has to be spelled out
/// by hand.
///
/// The returned temp file must outlive the SQL run: it IS the source.
fn emit_hot_only_over(
    query: &trawl_core::ast::Query,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    siblings: &[Map<String, Value>],
) -> (tempfile::NamedTempFile, EmittedQuery) {
    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    for sibling in siblings {
        writeln!(tmp, "{}", Value::Object(sibling.clone())).unwrap();
    }
    tmp.flush().unwrap();

    // hot_pins = pins ∩ the snapshot's keys: the REPLACE list must never
    // name a column the snapshot does not carry — and the key set is the
    // SNAPSHOT's, not one row's, exactly as `HotBuffer::snapshot` computes
    // it. A row that skipped a field still reads the conformed column its
    // neighbours put there.
    let mut hot_pins = FieldTypes::new();
    for (field, ty) in ft.iter() {
        if event.contains_key(field) || siblings.iter().any(|s| s.contains_key(field)) {
            hot_pins.insert(field, ty);
        }
    }
    let emitted = emitter::emit_hot_only(
        query,
        tmp.path().to_str().unwrap(),
        &hot_pins,
        ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    (tmp, emitted)
}

/// Assert filter and hot-only SQL agree for one (dsl, event, pins) triple.
///
/// The hot-only lane is the SSE filter's own corpus: the same event the
/// matcher sees, read back through the `REPLACE` conformance
/// (`TRY_CAST(col AS <pin>)`) the executor applies on a cold start. So the
/// wire value is not spelled out here the way
/// [`assert_pinned_parity_over_column`] spells the stored column — the
/// point is precisely that the emitter conforms the SAME bytes the matcher
/// reads, and the two must still answer alike.
fn assert_hot_only_parity(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
) {
    assert_hot_only_parity_beside(conn, dsl, event, ft, &[]);
}

/// [`assert_hot_only_parity`] with SIBLING events sharing the snapshot.
///
/// The siblings are not incidental: `read_json` types each column from the
/// whole file, so a sibling decides how the event under test is SPELLED
/// once it is read back (a fractional `status` widens the column to DOUBLE
/// and stores `200` as `"200.0"`). Only the target row is asked about —
/// it is the one whose `message` is `hello`, and the row filter sits
/// outside the emitted predicate.
fn assert_hot_only_parity_beside(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    siblings: &[Map<String, Value>],
) {
    let query = parser::parse(dsl).expect("dsl parses");
    let filter = CompiledFilter::compile(&query.search, ft).expect("filter compiles");
    let filter_result = filter.matches(event);

    let (_snapshot, emitted) = emit_hot_only_over(&query, event, ft, siblings);
    let sql_result = sql_matches_strict_row(conn, &emitted, "message = 'hello'");

    assert_eq!(
        filter_result, sql_result,
        "hot-only pinned parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\nsiblings: {siblings:?}\nsql: {}\nparams: {:?}",
        emitted.sql, emitted.params
    );
}

/// The equality-class DSLs a VARCHAR-pinned numeric field is queried with.
const VARCHAR_EQ_DSLS: [&str; 10] = [
    "status=200",
    "status!=200",
    "status=200.0",
    "status!=200.0",
    "status=200,301",
    "status=200,accepted",
    "status=accepted",
    "NOT status=200",
    "NOT status!=200",
    "NOT status=200,301",
];

/// A VARCHAR pin does NOT mean the stored text is the wire text.
/// `read_json` types each column from the whole batch, so one fractional
/// sibling makes a wire `200` read back as `"200.0"` — and compaction's
/// `TRY_CAST(col AS VARCHAR)` writes exactly that spelling to parquet, so
/// the widening is durable, not hot-only.
///
/// Exact-text equality is therefore unmirrorable: the live matcher only
/// ever sees the wire `200`. Both sides carry the value's numeric reading
/// beside its text, and this is the execution evidence — the same batch
/// the reviewer's probe used, answered identically on both sides.
#[test]
fn pinned_varchar_eq_survives_read_json_widening() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    // The sibling that widens `status` to DOUBLE for the whole snapshot.
    let mut sibling = status_event(&Value::from(200.5));
    sibling.insert("message".into(), Value::from("sibling"));
    for wire in [
        Value::from(200),
        Value::from(200.0),
        Value::from(200.5),
        Value::from(404),
        Value::from("200"),
        Value::from("accepted"),
        Value::Null,
    ] {
        let mut event = status_event(&wire);
        // The REPLACE list always names both envelope timestamps.
        event.insert("_time".into(), Value::from("2026-01-01T12:00:00Z"));
        event.insert("_ingested".into(), Value::from("2026-01-01T12:00:01Z"));
        let mut sibling = sibling.clone();
        sibling.insert("_time".into(), Value::from("2026-01-01T12:00:00Z"));
        sibling.insert("_ingested".into(), Value::from("2026-01-01T12:00:01Z"));
        for dsl in VARCHAR_EQ_DSLS {
            assert_hot_only_parity_beside(&conn, dsl, &event, &ft, &[sibling.clone()]);
        }
    }
}

/// The cold half of the same divergence: the spellings compaction actually
/// writes for a wire number under a VARCHAR pin, each spelled out as the
/// stored column while the matcher still reads the wire event.
#[test]
fn pinned_varchar_eq_over_conformed_number_spellings() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    // (wire value, the text conformance stored for it)
    let cases = [
        // DOUBLE inference: the reported case.
        (Value::from(200), "'200.0'"),
        (Value::from(200.0), "'200.0'"),
        // BIGINT inference: same value, plain spelling.
        (Value::from(200), "'200'"),
        // Exponent renderings DuckDB produces for extreme magnitudes.
        (Value::from(1e20), "'1e+20'"),
        (Value::from(1e-7), "'1e-07'"),
        // Text the wire carried verbatim, numeric and not.
        (Value::from("200"), "'200'"),
        (Value::from("accepted"), "'accepted'"),
    ];
    for (wire, stored) in cases {
        let event = status_event(&wire);
        for dsl in VARCHAR_EQ_DSLS {
            assert_pinned_parity_over_column(&conn, dsl, &event, &ft, stored);
        }
    }
}

/// Ids a `DOUBLE` comparison space cannot tell apart (ADR-0011 ruling #6).
///
/// Consecutive above 2^53, where the gap between representable doubles is
/// 2 (and 256 up at 2^60): every pair here collapses onto one double.
const COLLIDING_IDS: [&str; 5] = [
    "1737000000123456788",
    "1737000000123456789",
    "1737000000123456790",
    "9007199254740992",
    "9007199254740993",
];

/// Numeric comparison against a VARCHAR-pinned field is EXACT past 2^53 —
/// asserted against the ANSWER, not merely against agreement.
///
/// This is the one place a parity assertion could not have done the job.
/// The old `TRY_CAST(col AS DOUBLE)` arm bound the literal as an `f64` and
/// compared in double space; the live matcher mirrored it faithfully, so
/// both engines returned `1737000000123456788` and `…790` for
/// `status=1737000000123456789` and both suppressed `9007199254740992`
/// for `status!=9007199254740993`. Perfect parity, wrong rows. Snowflake
/// ids and nanosecond epochs land in VARCHAR-pinned fields exactly like
/// this, and the resulting query is silently, plausibly wrong.
///
/// So each case states the truth an operator would expect — the value is
/// the id or it isn't — and the helper still asserts the two engines agree
/// on the way there. Both wire shapes are covered: the string a text field
/// carries, and the JSON integer whose conformed column is that same text.
#[test]
fn pinned_varchar_numeric_comparison_is_exact_above_2_pow_53() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    for stored in COLLIDING_IDS {
        let value: i128 = stored.parse().unwrap();
        let event = status_event(&Value::String(stored.into()));
        for probe in COLLIDING_IDS {
            let literal: i128 = probe.parse().unwrap();
            for (dsl, expected) in [
                (format!("status={probe}"), value == literal),
                (format!("status!={probe}"), value != literal),
                (format!("status>{probe}"), value > literal),
                (format!("status>={probe}"), value >= literal),
                (format!("status<{probe}"), value < literal),
                (format!("status<={probe}"), value <= literal),
            ] {
                assert_eq!(
                    assert_pinned_parity(&conn, &dsl, &event, &ft),
                    expected,
                    "both engines agreed on the wrong answer: stored {stored}, {dsl}"
                );
            }
        }
    }

    // The wire JSON integer, whose conformed column is the same text: the
    // matcher reads a `serde_json` number where the query reads a string,
    // and both must land on the same exact reading.
    for stored in COLLIDING_IDS {
        let value: i128 = stored.parse().unwrap();
        let event = status_event(&Value::from(i64::try_from(value).unwrap()));
        for probe in COLLIDING_IDS {
            let expected = value == probe.parse::<i128>().unwrap();
            assert_eq!(
                assert_pinned_parity_over_column(
                    &conn,
                    &format!("status={probe}"),
                    &event,
                    &ft,
                    &format!("'{stored}'"),
                ),
                expected,
                "wire integer {stored} vs literal {probe}"
            );
        }
    }
}

/// COMPARISONS under a typed pin, over the hot-only lane — the same
/// event read by the matcher and by the query, which is the strictest
/// form of the batch/live contract.
///
/// The gap this closes: the pattern rules conformed the wire value and
/// the comparison rules did not, so every value the round-trip guard
/// nulls out answered one way live and another in batch. A wire `1.5`
/// under a BIGINT pin is NULL in both batch lanes — `duration>1` is
/// UNKNOWN there and was TRUE here — and `"TRUE"` under a BOOLEAN pin
/// made `NOT flag=true` fire on a stream while `/query` returned nothing.
///
/// Literals a typed column cannot be compared against at all (a word
/// against a numeric pin) are deliberately absent: `DuckDB` answers those
/// with a conversion ERROR, so there is no row set to agree with.
#[test]
#[allow(clippy::too_many_lines)] // one matrix, kept in one place to stay readable
fn pinned_typed_comparison_parity() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    let cases: [(CanonicalType, &[&str], &[Value]); 4] = [
        (
            CanonicalType::BigInt,
            &[
                "status>1",
                "status>=1",
                "status<9",
                "status<=9",
                "status=2",
                "status!=2",
                "status=404",
                "status!=404",
                "status>1.5",
                "status=2,404",
                "NOT status>1",
                "NOT status!=2",
            ],
            &[
                // Values the round-trip guard nulls out: the cast would
                // round them, or read them as something it cannot write
                // back.
                Value::from(1.5),
                Value::from("1.5"),
                Value::from(2.5),
                Value::from("accepted"),
                Value::from(true),
                Value::Null,
                // …and values that DO conform, spelled unlike the wire.
                Value::from("0404"),
                Value::from(" 200"),
                Value::from("2"),
                Value::from(404),
                Value::from(2),
                // Integral above 2^53, where the deleted `f64` cast rung
                // read nothing.
                Value::from("9007199254740993.0"),
            ],
        ),
        (
            CanonicalType::Boolean,
            &[
                "status=true",
                "status!=true",
                "status=false",
                "status=1",
                "status=yes",
                "status>0",
                "NOT status=true",
            ],
            &[
                // The cast's vocabulary is wide and the guard is narrow,
                // so only the two rendered spellings conform.
                Value::from("TRUE"),
                Value::from("1"),
                Value::from("no"),
                Value::from("accepted"),
                Value::from(1),
                Value::from(200),
                Value::Null,
                Value::from(true),
                Value::from(false),
                Value::from("true"),
                Value::from("false"),
            ],
        ),
        (
            CanonicalType::Double,
            &[
                "status>1",
                "status>=200",
                "status=1.5",
                "status!=1.5",
                "status<0",
                "NOT status>1",
            ],
            &[
                Value::from(1.5),
                Value::from("1.5"),
                Value::from(" 200"),
                Value::from("200"),
                Value::from("accepted"),
                Value::from(true),
                Value::from(404),
                Value::Null,
            ],
        ),
        (
            CanonicalType::Timestamp,
            &[
                r#"status>"2026-01-15T00:00:00Z""#,
                r#"status<"2026-01-16T00:00:00Z""#,
                r#"status="2026-01-15T09:00:00Z""#,
                r#"status!="2026-01-15T09:00:00Z""#,
                r#"status>="2026-01-15 09:00:00""#,
                r#"NOT status>"2026-01-15T00:00:00Z""#,
            ],
            &[
                Value::from("2026-01-15T09:00:00Z"),
                Value::from("2026-01-15T09:00:00+05:30"),
                Value::from("2026-01-15 09:00:00"),
                Value::from("2026-01-15"),
                Value::from("infinity"),
                Value::from("-infinity"),
                Value::from("accepted"),
                Value::from(1_735_689_600),
                Value::Null,
            ],
        ),
    ];
    // A string sibling fixes the snapshot's inference at VARCHAR/JSON, so
    // the conform reads the text the wire carried rather than whatever
    // `read_json` would have parsed a lone value into — the conform is
    // text-first, but the reader in front of it is not.
    let mut sibling = status_event(&Value::from("sentinel"));
    sibling.insert("message".into(), Value::from("sibling"));
    sibling.insert("_time".into(), Value::from("2026-01-01T12:00:00Z"));
    sibling.insert("_ingested".into(), Value::from("2026-01-01T12:00:01Z"));

    for (pin, dsls, values) in cases {
        let ft = pinned(&[("status", pin)]);
        for wire in values {
            let mut event = status_event(wire);
            // The REPLACE list always names both envelope timestamps.
            event.insert("_time".into(), Value::from("2026-01-01T12:00:00Z"));
            event.insert("_ingested".into(), Value::from("2026-01-01T12:00:01Z"));
            for dsl in dsls {
                assert_hot_only_parity_beside(&conn, dsl, &event, &ft, &[sibling.clone()]);
            }
        }
    }
}

/// Patterns over the hot-only lane, where the matcher and the query read
/// the SAME event — the strictest form of the batch/live contract, and the
/// one that catches a pattern text taken from the wire instead of from the
/// conformed value.
///
/// Every case here diverged before the typed pattern texts landed: the
/// matcher globbed `accepted` / `0404` / `TRUE` / `1.50` while the query
/// globbed the conformed `NULL` / `404` / `true` / `1.5`, so a live tail
/// fired on events the equivalent `/api/v1/query` dropped.
#[test]
fn pinned_hot_only_pattern_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let patterns = [
        "status=4*",
        "status=0*",
        "status=a*",
        "status=t*",
        "status=T*",
        "status=1*",
        "status=/acc.*/",
        "status=/^404$/",
        "status=/^true$/",
        r"status=/^1\.5$/",
        "status=/^2$/",
        "NOT status=4*",
        "NOT status=a*",
        "NOT status=/^true$/",
    ];
    // (pin, wire value the matcher AND the query both read)
    let cases = [
        // The reported BIGINT divergences: a non-numeric text conforms to
        // NULL, a leading-zero text conforms to a differently-spelled
        // integer, and the control (a wire integer) must keep matching.
        (CanonicalType::BigInt, Value::from("accepted")),
        (CanonicalType::BigInt, Value::from("0404")),
        (CanonicalType::BigInt, Value::from(404)),
        (CanonicalType::BigInt, Value::from("1.5")),
        (CanonicalType::BigInt, Value::from(2.5)),
        (CanonicalType::BigInt, Value::from(true)),
        (CanonicalType::BigInt, Value::Null),
        // BOOLEAN: the vocabulary is case-insensitive, the rendering is not.
        (CanonicalType::Boolean, Value::from("TRUE")),
        (CanonicalType::Boolean, Value::from(true)),
        (CanonicalType::Boolean, Value::from(false)),
        (CanonicalType::Boolean, Value::from("no")),
        (CanonicalType::Boolean, Value::from("1")),
        (CanonicalType::Boolean, Value::from("accepted")),
        (CanonicalType::Boolean, Value::Null),
        // DOUBLE, already typed-text: a trailing zero in the wire text is
        // not in DuckDB's rendering.
        (CanonicalType::Double, Value::from("1.50")),
        (CanonicalType::Double, Value::from(404)),
        // VARCHAR and unpinned keep the wire text on both sides.
        (CanonicalType::Varchar, Value::from("0404")),
        (CanonicalType::Varchar, Value::from("accepted")),
    ];
    for (pin, wire) in cases {
        let ft = pinned(&[("status", pin)]);
        let mut event = status_event(&wire);
        // The REPLACE list always names both envelope timestamps.
        event.insert("_time".into(), Value::from("2026-01-01T12:00:00Z"));
        event.insert("_ingested".into(), Value::from("2026-01-01T12:00:01Z"));
        for dsl in patterns {
            assert_hot_only_parity(&conn, dsl, &event, &ft);
        }
    }
}

// ── Pinned pipeline where/let parity (ADR-0011 slice A′) ──────────────

/// Extract the sole `| where` condition of `dsl`.
fn where_condition(
    dsl: &str,
) -> (
    trawl_core::ast::Query,
    trawl_core::ast::Spanned<trawl_core::ast::Expr>,
) {
    let query = parser::parse(dsl).expect("dsl parses");
    let condition = query
        .pipeline
        .iter()
        .find_map(|s| match &s.node {
            PipeStage::Where(w) => Some(w.condition.clone()),
            _ => None,
        })
        .expect("dsl has a where stage");
    (query, condition)
}

/// An `EvalValue` boolean answer as SQL truth.
fn eval_truth(v: &trawl_core::eval::EvalValue) -> Option<bool> {
    match v {
        trawl_core::eval::EvalValue::Bool(b) => Some(*b),
        trawl_core::eval::EvalValue::Null => None,
        other => panic!("a comparison must answer Bool/Null, got {other:?}"),
    }
}

/// Assert the streaming `where` evaluator and SQL agree on one
/// `| where` (dsl, event, pins) triple, and return the three-valued eval
/// answer so callers can pin what it must BE (agreement alone would let
/// both lanes be wrong together — the ruling-#6 lesson).
///
/// Pins are threaded through BOTH lanes — [`trawl_core::pin_scope::PinScope::root`]
/// for the evaluator, [`emitter::emit_with_pins`] for the SQL — so an
/// EMPTY catalog is the pin-blind lane (byte-identical to what
/// [`emitter::emit`] emits), and a populated one is slice A′.
///
/// Same source strategy as [`assert_pinned_parity`]: ndjson when the wire
/// shape already infers the pin's physical type, a typed-NULL parquet for
/// the null/absent case. A wire shape that does NOT infer its pin belongs
/// on the hot-only lane ([`assert_where_parity_hot_only`]), where the
/// emitter conforms the bytes itself.
fn assert_where_parity(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
) -> Option<bool> {
    let (query, condition) = where_condition(dsl);
    let scope = trawl_core::pin_scope::PinScope::root(ft);
    let eval_result = eval_truth(&trawl_core::eval::eval_expr_with_pins(
        &condition,
        &trawl_core::row::from_json(event),
        &scope,
    ));

    let _guard: Box<dyn std::any::Any>;
    let source = if matches!(event.get("status"), None | Some(&Value::Null)) {
        let tmp = tempfile::Builder::new()
            .suffix(".parquet")
            .tempfile()
            .unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        let ty = match ft.get("status") {
            Some(t) => t.as_duckdb(),
            None => "VARCHAR",
        };
        conn.execute_batch(&format!(
            "COPY (SELECT CAST(NULL AS {ty}) AS status, 'hello' AS message) \
             TO '{path}' (FORMAT PARQUET)"
        ))
        .expect("write typed-null parquet");
        _guard = Box::new(tmp);
        path
    } else {
        let mut tmp = tempfile::Builder::new()
            .suffix(".ndjson")
            .tempfile()
            .unwrap();
        writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
        tmp.flush().unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();
        _guard = Box::new(tmp);
        path
    };
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches_strict(conn, &emitted);

    assert_eq!(
        eval_result == Some(true),
        sql_result,
        "pinned where parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\neval: {eval_result:?}\nsql: {}\nparams: {:?}",
        emitted.sql,
        emitted.params
    );
    eval_result
}

/// [`assert_where_parity`] over an explicitly-stored column: the
/// wire shape the evaluator sees and the conformed column batch reads are
/// spelled out separately, exactly as compaction separates them.
fn assert_where_parity_over_column(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    stored_sql: &str,
) -> Option<bool> {
    let (query, condition) = where_condition(dsl);
    let scope = trawl_core::pin_scope::PinScope::root(ft);
    let eval_result = eval_truth(&trawl_core::eval::eval_expr_with_pins(
        &condition,
        &trawl_core::row::from_json(event),
        &scope,
    ));

    let tmp = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    conn.execute_batch(&format!(
        "COPY (SELECT {stored_sql} AS status, 'hello' AS message) \
         TO '{source}' (FORMAT PARQUET)"
    ))
    .expect("write typed parquet");

    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let sql_result = sql_matches_strict(conn, &emitted);

    assert_eq!(
        eval_result == Some(true),
        sql_result,
        "pinned where parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\nstored: {stored_sql}\nsql: {}\nparams: {:?}",
        emitted.sql,
        emitted.params
    );
    eval_result
}

/// Assert the pinned `| let x = <cmp>` SELECT-list value agrees between the
/// lanes and IS `expected` — TRUE/FALSE/NULL as a stored value, not a row
/// filter, so UNKNOWN is directly observable in batch too.
fn assert_pinned_let_parity(
    conn: &Connection,
    expr_dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    expected: Option<bool>,
) {
    let dsl = format!("* | let x = {expr_dsl}");
    let query = parser::parse(&dsl).expect("dsl parses");
    let assignment = query
        .pipeline
        .iter()
        .find_map(|s| match &s.node {
            PipeStage::Let(l) => Some(l.assignments[0].1.clone()),
            _ => None,
        })
        .expect("dsl has a let stage");
    let scope = trawl_core::pin_scope::PinScope::root(ft);
    let eval_result = eval_truth(&trawl_core::eval::eval_expr_with_pins(
        &assignment,
        &trawl_core::row::from_json(event),
        &scope,
    ));
    assert_eq!(
        eval_result, expected,
        "pinned let eval answer\nexpr: {expr_dsl:?}\nevent: {event:?}"
    );

    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    tmp.flush().unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");

    let sql_result = let_value(conn, &emitted, "TRUE");
    assert_eq!(
        sql_result, expected,
        "pinned let batch answer\nexpr: {expr_dsl:?}\nevent: {event:?}\nsql: {}",
        emitted.sql
    );
}

/// The value the `| let x = …` column holds in batch for the row named by
/// `row_filter` — TRUE / FALSE / NULL, so UNKNOWN is directly observable
/// (a row filter would collapse it into "no row").
fn let_value(conn: &Connection, emitted: &EmittedQuery, row_filter: &str) -> Option<bool> {
    let select_sql = format!(
        "SELECT \"x\" FROM ({}) AS _sub WHERE {row_filter}",
        emitted.sql
    );
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    conn.query_row(&select_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or_else(|e| {
            panic!(
                "pinned let must not error: {e}\nsql: {select_sql}\nparams: {:?}",
                emitted.params
            )
        })
}

/// The slice-A′ deterministic matrix: pin-aware `| where` over the same
/// value × operator grid the search stage runs, strict SQL (an error is a
/// broken rule, never a "no match").
#[test]
fn pinned_where_varchar_matrix_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    let values = [
        Value::String("200".into()),
        Value::String("404".into()),
        Value::String("accepted".into()),
        Value::String("1.5".into()),
        Value::String("0404".into()),
        Value::String("200_000".into()),
        Value::String("nan".into()),
        Value::String("1e40".into()),
        Value::String("9007199254740993".into()),
        Value::String("0x10".into()),
        Value::Null,
    ];
    let dsls = [
        "* | where status == 200",
        "* | where status != 200",
        "* | where status > 400",
        "* | where status >= 400",
        "* | where status < 400",
        "* | where status <= 400",
        "* | where 400 < status",
        "* | where status == \"accepted\"",
        "* | where status != \"accepted\"",
        "* | where status in (200, 301)",
        "* | where status in (200, \"accepted\")",
        "* | where status > \"nan\"",
        "* | where status >= \"1e40\"",
        "* | where status == 9007199254740993",
        "* | where status > 9007199254740992",
        "* | where not (status > 400)",
        "* | where not (status == 200)",
        "* | where not (status in (200, 301))",
        "* | where status matches \"^2\"",
        "* | where status like \"2%\"",
        "* | where not (status matches \"^2\")",
    ];
    let events = values
        .iter()
        .map(status_event)
        .chain(std::iter::once(absent_status_event()));
    for event in events {
        for dsl in dsls {
            assert_where_parity(&conn, dsl, &event, &ft);
        }
    }
}

/// Expected values pinned, not just agreement (the ruling-#6 lesson):
/// the headline slice-A′ answers over a VARCHAR pin.
#[test]
fn pinned_where_expected_answers() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);

    // 'accepted' has no reading: UNKNOWN, and NOT does not invert it.
    let accepted = status_event(&Value::from("accepted"));
    assert_eq!(
        assert_where_parity(&conn, "* | where status > 400", &accepted, &ft),
        None
    );
    assert_eq!(
        assert_where_parity(&conn, "* | where not (status > 400)", &accepted, &ft),
        None
    );

    // A readable value answers, and NOT genuinely inverts.
    let s404 = status_event(&Value::from("404"));
    assert_eq!(
        assert_where_parity(&conn, "* | where status > 400", &s404, &ft),
        Some(true)
    );
    assert_eq!(
        assert_where_parity(&conn, "* | where not (status > 400)", &s404, &ft),
        Some(false)
    );

    // Strict `!=` NULL policy: absent/null is UNKNOWN (the search stage's
    // widening does NOT apply in the pipeline).
    assert_eq!(
        assert_where_parity(
            &conn,
            "* | where status != 200",
            &absent_status_event(),
            &ft
        ),
        None
    );
    assert_eq!(
        assert_where_parity(
            &conn,
            "* | where status != 200",
            &status_event(&Value::Null),
            &ft
        ),
        None
    );

    // The stored text of a wire 200 that shared a batch with a fraction is
    // "200.0" — the numeric arm still answers `== 200` TRUE.
    assert_eq!(
        assert_where_parity_over_column(
            &conn,
            "* | where status == 200",
            &status_event(&Value::from(200)),
            &ft,
            "'200.0'"
        ),
        Some(true)
    );

    // Exact above 2^53: neighbours stay distinct in the DECIMAL space.
    assert_eq!(
        assert_where_parity(
            &conn,
            "* | where status > 9007199254740992",
            &status_event(&Value::from("9007199254740993")),
            &ft
        ),
        Some(true)
    );
    assert_eq!(
        assert_where_parity(
            &conn,
            "* | where status == 9007199254740993",
            &status_event(&Value::from("9007199254740992")),
            &ft
        ),
        Some(false)
    );

    // A literal with no reading (`nan`, `1e40`) makes every row UNKNOWN.
    for dsl in ["* | where status > \"nan\"", "* | where status >= \"1e40\""] {
        assert_eq!(
            assert_where_parity(&conn, dsl, &status_event(&Value::from("200")), &ft),
            None,
            "{dsl}"
        );
    }
}

/// TIMESTAMP pin: the conform is zone-aware, so an offset-bearing wire
/// value compares (and pattern-matches) at its UTC instant, while a query
/// literal parses wall-clock (offset ignored) — both probed in slice A.
#[test]
fn pinned_where_timestamp_offset_instant() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Timestamp)]);
    // Wire value carries +05:30; the corpus holds the 03:30 UTC instant.
    let event = status_event(&Value::from("2026-01-15T09:00:00+05:30"));
    let stored = "TIMESTAMP '2026-01-15 03:30:00'";

    assert_eq!(
        assert_where_parity_over_column(
            &conn,
            "* | where status == \"2026-01-15T03:30:00\"",
            &event,
            &ft,
            stored
        ),
        Some(true)
    );
    assert_eq!(
        assert_where_parity_over_column(
            &conn,
            "* | where status == \"2026-01-15T09:00:00\"",
            &event,
            &ft,
            stored
        ),
        Some(false)
    );
    assert_eq!(
        assert_where_parity_over_column(
            &conn,
            "* | where status like \"2026-01-15T03:30%\"",
            &event,
            &ft,
            stored
        ),
        Some(true)
    );
    assert_eq!(
        assert_where_parity_over_column(
            &conn,
            "* | where status matches \"T09:\"",
            &event,
            &ft,
            stored
        ),
        Some(false)
    );
}

/// The pinned comparison as a SELECT-list value: `| let` stores
/// TRUE/FALSE/NULL, three-valued in batch exactly as in memory.
#[test]
fn pinned_let_matrix_with_expected_values() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("status", CanonicalType::Varchar)]);
    let cases: &[(&str, &str, Option<bool>)] = &[
        ("status >= 400", r#"{"status": "404"}"#, Some(true)),
        ("status >= 400", r#"{"status": "200"}"#, Some(false)),
        ("status >= 400", r#"{"status": "accepted"}"#, None),
        ("status == 200", r#"{"status": "200"}"#, Some(true)),
        ("status == 200", r#"{"status": "accepted"}"#, Some(false)),
        ("status != 200", r#"{"status": "accepted"}"#, Some(true)),
        ("status in (200, 301)", r#"{"status": "301"}"#, Some(true)),
        (
            "status in (200, 301)",
            r#"{"status": "accepted"}"#,
            Some(false),
        ),
        ("status > \"nan\"", r#"{"status": "200"}"#, None),
    ];
    for (expr, event_json, expected) in cases {
        let event: Map<String, Value> = serde_json::from_str(event_json).unwrap();
        assert_pinned_let_parity(&conn, expr, &event, &ft, *expected);
    }
}

// ── Generative pinned where/let parity (ADR-0011 slice A′) ────────────

/// [`assert_where_parity`] over the HOT-ONLY lane: the evaluator and the
/// query read the SAME bytes, the emitter conforming them exactly as
/// compaction will.
///
/// This is the lane every wire shape can take. The cold lane needs the
/// stored column to already BE the pin's physical type, which a wire value
/// only infers when it happens to look like its pin (a JSON string under a
/// VARCHAR pin); here the `REPLACE` conformance derives it, so a wire
/// `"1.5"` under a BIGINT pin is the NULL both batch lanes hold.
///
/// Only the target row is asked about — the one whose `message` is
/// `hello` — and that filter sits outside the predicate under test.
fn assert_where_parity_hot_only(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    siblings: &[Map<String, Value>],
) -> Option<bool> {
    let (query, condition) = where_condition(dsl);
    let scope = trawl_core::pin_scope::PinScope::root(ft);
    let eval_result = eval_truth(&trawl_core::eval::eval_expr_with_pins(
        &condition,
        &trawl_core::row::from_json(event),
        &scope,
    ));

    let (_snapshot, emitted) = emit_hot_only_over(&query, event, ft, siblings);
    let sql_result = sql_matches_strict_row(conn, &emitted, "message = 'hello'");

    assert_eq!(
        eval_result == Some(true),
        sql_result,
        "hot-only where parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\nsiblings: {siblings:?}\neval: {eval_result:?}\nsql: {}\nparams: {:?}",
        emitted.sql,
        emitted.params
    );
    eval_result
}

/// The `| let` mirror of [`assert_where_parity_hot_only`]: the same
/// comparison as a STORED value, so the lanes are compared three-valued —
/// FALSE and UNKNOWN are one row set to a `where`, two values here.
fn assert_let_parity_hot_only(
    conn: &Connection,
    expr_dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    siblings: &[Map<String, Value>],
) -> Option<bool> {
    let dsl = format!("* | let x = {expr_dsl}");
    let query = parser::parse(&dsl).expect("dsl parses");
    let assignment = query
        .pipeline
        .iter()
        .find_map(|s| match &s.node {
            PipeStage::Let(l) => Some(l.assignments[0].1.clone()),
            _ => None,
        })
        .expect("dsl has a let stage");
    let scope = trawl_core::pin_scope::PinScope::root(ft);
    let eval_result = eval_truth(&trawl_core::eval::eval_expr_with_pins(
        &assignment,
        &trawl_core::row::from_json(event),
        &scope,
    ));

    let (_snapshot, emitted) = emit_hot_only_over(&query, event, ft, siblings);
    let sql_result = let_value(conn, &emitted, "message = 'hello'");

    assert_eq!(
        eval_result, sql_result,
        "hot-only let parity mismatch\nexpr: {expr_dsl:?}\nevent: {event:?}\nsiblings: {siblings:?}\nsql: {}\nparams: {:?}",
        emitted.sql, emitted.params
    );
    eval_result
}

/// The pins a generated case draws from — the whole `CanonicalType`
/// vocabulary, because a repin can land on any of them.
const GENERATIVE_PINS: [CanonicalType; 6] = [
    CanonicalType::BigInt,
    CanonicalType::Boolean,
    CanonicalType::Double,
    CanonicalType::Timestamp,
    CanonicalType::Varchar,
    CanonicalType::Severity,
];

/// Wire values a generated event can carry under `pin`: shapes the
/// round-trip guard KEEPS (spelled unlike the wire, so the two lanes must
/// each derive the stored reading), shapes it NULLS, and JSON null.
fn wire_pool(pin: CanonicalType) -> Vec<Value> {
    match pin {
        CanonicalType::BigInt => vec![
            Value::from(404),
            Value::from("0404"),
            Value::from(" 200"),
            Value::from("1.5"),
            Value::from(2.5),
            Value::from("accepted"),
            Value::from(true),
            Value::from("9007199254740993"),
            Value::Null,
        ],
        CanonicalType::Boolean => vec![
            Value::from(true),
            Value::from(false),
            Value::from("true"),
            Value::from("TRUE"),
            Value::from("1"),
            Value::from("no"),
            Value::from("accepted"),
            Value::from(1),
            Value::Null,
        ],
        CanonicalType::Double => vec![
            Value::from(1.5),
            Value::from("1.50"),
            Value::from("200"),
            Value::from(404),
            Value::from("accepted"),
            Value::from(true),
            // Both NaN spellings survive the pin's round-trip guard, so
            // both are values the corpus really holds — and they compare
            // in DuckDB's TOTAL order, not Rust's IEEE one.
            Value::from("nan"),
            Value::from("-nan"),
            Value::Null,
        ],
        CanonicalType::Timestamp => vec![
            Value::from("2026-01-15T09:00:00Z"),
            Value::from("2026-01-15T09:00:00+05:30"),
            Value::from("2026-01-15 09:00:00"),
            Value::from("2026-01-15"),
            Value::from("infinity"),
            Value::from("accepted"),
            Value::Null,
        ],
        CanonicalType::Varchar => vec![
            Value::from("200"),
            Value::from("0404"),
            Value::from("200.0"),
            Value::from("accepted"),
            Value::from("1.5"),
            Value::from("nan"),
            Value::from("1e40"),
            Value::from("0x10"),
            Value::from("9007199254740993"),
            Value::Null,
        ],
        // The SEVERITY pin: the ladder's own numbers, the spellings the
        // guarded BIGINT cast keeps, and the shapes it nulls (out of
        // ladder, fractional, a word).
        CanonicalType::Severity => vec![
            Value::from(17),
            Value::from(13),
            Value::from("17"),
            Value::from("017"),
            Value::from(0),
            Value::from(25),
            Value::from(1.5),
            Value::from("error"),
            Value::Null,
        ],
    }
}

/// Literals a generated comparison against `pin` can carry.
///
/// Type-appropriate on purpose, exactly as the deterministic matrix is: a
/// word against a numeric pin is a `DuckDB` conversion ERROR, so there is
/// no row set for the evaluator to agree with. The VARCHAR pin is the
/// exception — its rules are the ones that take a literal of either shape.
fn literal_pool(pin: CanonicalType) -> &'static [&'static str] {
    match pin {
        CanonicalType::BigInt => &["1", "2", "404", "1.5", "-1", "9007199254740992"],
        CanonicalType::Boolean => &["true", "false"],
        // The NaN literal is QUOTED: a bare `nan` in an expression
        // position is a FIELD reference, and the quoted spelling binds
        // through the same content coercion the search stage applies.
        CanonicalType::Double => &["0", "1.5", "-1.5", "200", "404", "\"nan\""],
        CanonicalType::Timestamp => &[
            "\"2026-01-15T00:00:00Z\"",
            "\"2026-01-15T09:00:00Z\"",
            "\"2026-01-16 00:00:00\"",
        ],
        CanonicalType::Varchar => &[
            "200",
            "400",
            "1.5",
            "9007199254740993",
            // The parser hands a negative literal over as a unary
            // negation, not a signed literal — a shape the pinned door
            // has to fold, and the one the pools originally missed.
            "-400",
            "-1.5",
            "\"-400\"",
            "\"200\"",
            "\"accepted\"",
            "\"nan\"",
        ],
        // The SEVERITY pin takes the ladder's numbers AND its token
        // vocabulary: band tokens, exact OTel short names, and integers.
        CanonicalType::Severity => &["17", "13", "0", "\"error\"", "\"warn\"", "\"error2\""],
    }
}

/// Literals a generated `in (…)` list draws from — [`literal_pool`],
/// except that the BIGINT pin drops its fractional literal.
///
/// `DuckDB` widens a MIXED integer/fraction `IN` list to DOUBLE, and
/// DOUBLE is blind above 2^53: `status IN (1.5, 9007199254740992)` matches
/// a stored `9007199254740993` in batch and nowhere else. That is the
/// ruling-#6 pathology in the LITERAL list, and it is not slice A′'s — the
/// search stage emits the same list from `status=1.5,9007199254740992` and
/// diverges identically, so this generator stays off a shipped rule
/// instead of re-litigating it here.
fn in_list_pool(pin: CanonicalType) -> &'static [&'static str] {
    match pin {
        CanonicalType::BigInt => &["1", "2", "404", "-1", "9007199254740992"],
        other => literal_pool(other),
    }
}

/// Patterns are text-shaped whatever the pin is: each typed pin renders
/// its CONFORMED value's canonical text on both sides.
const GENERATIVE_REGEXES: [&str; 6] = ["^2", "4", "acc", "true", "^40", "0"];
const GENERATIVE_GLOBS: [&str; 5] = ["2%", "%0%", "true", "acc%", "%.%"];

const GENERATIVE_ORDERED_OPS: [&str; 6] = ["==", "!=", ">", ">=", "<", "<="];

/// One `status`-vs-literal comparison under `pin`.
fn random_comparison(rng: &mut Rng, pin: CanonicalType) -> String {
    let lits = literal_pool(pin);
    match rng.range(6) {
        0 | 1 => format!(
            "status {} {}",
            rng.pick(&GENERATIVE_ORDERED_OPS),
            rng.pick(lits)
        ),
        // Literal on the LEFT: the rule table has to bind the same way
        // whichever side the field sits on.
        2 => format!(
            "{} {} status",
            rng.pick(lits),
            rng.pick(&GENERATIVE_ORDERED_OPS)
        ),
        3 => {
            let list = in_list_pool(pin);
            format!("status in ({}, {})", rng.pick(list), rng.pick(list))
        }
        4 => format!("status matches \"{}\"", rng.pick(&GENERATIVE_REGEXES)),
        _ => format!("status like \"{}\"", rng.pick(&GENERATIVE_GLOBS)),
    }
}

/// A comparison, sometimes wrapped in the connectives whose three-valued
/// behaviour is the whole point: `NOT (UNKNOWN)` is UNKNOWN, not a match.
fn random_condition(rng: &mut Rng, pin: CanonicalType) -> String {
    let base = random_comparison(rng, pin);
    match rng.range(6) {
        0 => format!("not ({base})"),
        1 => format!("({base}) and message == \"hello\""),
        2 => format!("({base}) or message == \"nope\""),
        3 => format!("not (({base}) and message == \"hello\")"),
        _ => base,
    }
}

/// Generative batch/live parity for the pinned pipeline: random DSL ×
/// random events × random pins, both stages, both lanes.
///
/// The deterministic matrices above pin what each answer must BE; this one
/// covers the combinations nobody enumerated — a `NOT` over a reversed
/// comparison whose literal has no reading, an `in` list against a value
/// the guard nulled, a pattern over a pin whose canonical text is not the
/// wire text — and it is the test that would have caught slice A′ shipping
/// pin-blind for any one operator, pin or connective.
///
/// `| where` is checked as a row set and `| let` as a stored value, so
/// UNKNOWN is observed directly rather than through "no row": a lane that
/// collapsed UNKNOWN to FALSE would still pass the `where` half alone.
#[test]
fn pinned_where_let_parity_generative() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    let mut rng = Rng::new(0x5A11_CEA5_0000_0066);

    // Every snapshot carries a string sibling, exactly as
    // `pinned_typed_comparison_parity` does: the conform is text-first but
    // the READER in front of it is not, and a lone value lets `read_json`
    // retype the column before the conform sees it (a single-row snapshot
    // holding `"infinity"` infers DATE and stores 1900-01-01). That is a
    // snapshot-inference property, not a comparison rule; pinning the
    // inference at VARCHAR/JSON keeps this test on its own subject.
    let mut sibling = status_event(&Value::from("sentinel"));
    sibling.insert("message".into(), Value::from("sibling"));
    sibling.insert("_time".into(), Value::from("2026-01-01T12:00:00Z"));
    sibling.insert("_ingested".into(), Value::from("2026-01-01T12:00:01Z"));

    let mut seen: std::collections::BTreeSet<(String, Option<bool>)> =
        std::collections::BTreeSet::new();
    let mut cold_lane = 0u32;

    for _ in 0..400 {
        let pin = *rng.pick(&GENERATIVE_PINS);
        let ft = pinned(&[("status", pin)]);
        let pool = wire_pool(pin);
        let wire = rng.pick(&pool).clone();
        let absent = rng.range(8) == 0;

        let mut event = if absent {
            absent_status_event()
        } else {
            status_event(&wire)
        };
        // The REPLACE list always names both envelope timestamps.
        event.insert("_time".into(), Value::from("2026-01-01T12:00:00Z"));
        event.insert("_ingested".into(), Value::from("2026-01-01T12:00:01Z"));

        // The sibling also keeps the column present for the absent case:
        // a snapshot in which NO row carries `status` has no such column
        // for either lane to read (the executor's missing-column policy
        // answers that, not the comparison rules), while a sibling makes
        // it the NULL a real corpus holds for a row that skipped it.
        let siblings = std::slice::from_ref(&sibling);

        let condition = random_condition(&mut rng, pin);
        let answer = assert_where_parity_hot_only(
            &conn,
            &format!("* | where {condition}"),
            &event,
            &ft,
            siblings,
        );
        assert_let_parity_hot_only(&conn, &condition, &event, &ft, siblings);
        seen.insert((format!("{pin:?}"), answer));

        // The cold lane too, wherever the wire shape already infers the
        // pin's physical type: a VARCHAR pin over a JSON string, and the
        // null/absent case the typed-NULL parquet covers for every pin.
        let cold_ready =
            absent || wire.is_null() || (pin == CanonicalType::Varchar && wire.is_string());
        if cold_ready {
            cold_lane += 1;
            assert_where_parity(&conn, &format!("* | where {condition}"), &event, &ft);
        }
    }

    // A generative test that stops generating is a green test with no
    // evidence: every pin must have been driven to all three answers, and
    // the cold lane must have run at all.
    for pin in GENERATIVE_PINS {
        for answer in [Some(true), Some(false), None] {
            assert!(
                seen.contains(&(format!("{pin:?}"), answer)),
                "generator never produced {answer:?} under the {pin:?} pin"
            );
        }
    }
    assert!(cold_lane > 0, "the cold lane never ran");
}
