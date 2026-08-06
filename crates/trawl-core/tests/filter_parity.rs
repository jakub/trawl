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
use trawl_core::eval::eval_expr;
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
/// (`level` is no longer a physical field — it aliases the numeric
/// `severity` column via band predicates, generated separately.)
const FIELDS: &[&str] = &["service", "status", "host", "path", "severity"];

/// String-typed fields (excludes numeric `status`/`severity`).
/// Used when the filter value is a string to avoid `DuckDB` conversion errors.
const STRING_FIELDS: &[&str] = &["service", "host", "path"];

/// Severity tokens exercised through the `level` alias.
const LEVEL_TOKENS: &[&str] = &["trace", "debug", "info", "notice", "warn", "error", "fatal"];

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
    match rng.range(7) {
        0 => random_field_eq(rng),
        1 => random_field_compare(rng),
        2 => random_field_list(rng),
        3 => random_text_search(rng),
        4 => random_quoted_search(rng),
        5 => random_field_glob(rng),
        6 => random_level_filter(rng),
        _ => unreachable!(),
    }
}

/// Generate a `level` alias filter: eq / ordered / ne / list of tokens.
fn random_level_filter(rng: &mut Rng) -> String {
    let tok = rng.pick(LEVEL_TOKENS);
    match rng.range(4) {
        0 => format!("level={tok}"),
        1 => {
            let op = rng.pick(&[">", ">=", "<", "<="]);
            format!("level{op}{tok}")
        }
        2 => format!("level!={tok}"),
        3 => {
            let tok2 = rng.pick(LEVEL_TOKENS);
            format!("level={tok},{tok2}")
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

    // Include message ~80% of the time. Set to null ~20% to exercise
    // NULL semantics in both CompiledFilter and DuckDB SQL
    // (NULL ILIKE/NOT ILIKE → NULL → excluded from results).
    // We use explicit null instead of omitting the key, because DuckDB's
    // read_json_auto infers schema from the data — a missing column
    // causes binder errors rather than NULL comparison semantics.
    if rng.range(5) != 0 {
        event.insert("message".to_string(), Value::String(random_message(rng)));
    } else {
        event.insert("message".to_string(), Value::Null);
    }

    // `_raw` present ~70% of the time (bare search covers message OR _raw);
    // null otherwise to exercise the COALESCE semantics.
    if rng.range(10) < 7 {
        event.insert("_raw".to_string(), Value::String(random_message(rng)));
    } else {
        event.insert("_raw".to_string(), Value::Null);
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
            }
        })
        .collect()
}

/// Execute emitted SQL against `DuckDB` and return whether any rows match.
fn sql_matches(conn: &Connection, emitted: &EmittedQuery) -> bool {
    let count_sql = format!("SELECT count(*)::BIGINT FROM ({}) AS _sub", emitted.sql);

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
        let event_value = Value::Object(event.clone());
        writeln!(tmp, "{event_value}").unwrap();
        tmp.flush().unwrap();
        let tmp_path = tmp.path().to_str().expect("temp path is valid UTF-8");

        // Emit SQL.
        let Ok(emitted) = emitter::emit(&query, tmp_path) else {
            skipped += 1;
            continue;
        };

        // DuckDB SQL result.
        let sql_result = sql_matches(&conn, &emitted);

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
    let emitted = emitter::emit(&query, tmp.path().to_str().unwrap()).expect("emit succeeds");
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
        "severity".into(),
        severity.map_or(Value::Null, |n| Value::Number(n.into())),
    );
    m
}

/// `level` band predicates agree between SQL and the in-memory filter for
/// every severity number and NULL.
#[test]
fn level_band_parity_exhaustive() {
    let conn = Connection::open_in_memory().unwrap();
    let dsls = [
        "level=error",
        "level>=warn",
        "level>warn",
        "level<info",
        "level<=info",
        "level!=info",
        "level=error,fatal",
        "level=trace,notice",
    ];
    for dsl in dsls {
        for sev in (1..=24).map(Some).chain([None]) {
            let event = envelope_event(sev, "hello", None);
            assert_parity(&conn, dsl, &event);
        }
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

/// Assert the streaming `where` evaluator and SQL agree for one
/// (dsl, event) pair. The DSL must carry exactly one `where` stage.
fn assert_where_parity(conn: &Connection, dsl: &str, event: &Map<String, Value>) {
    let query = parser::parse(dsl).expect("dsl parses");
    let where_stage = query
        .pipeline
        .iter()
        .find_map(|s| match &s.node {
            PipeStage::Where(w) => Some(w),
            _ => None,
        })
        .expect("dsl has a where stage");
    let eval_result = eval_expr(&where_stage.condition, event).is_truthy();

    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    tmp.flush().unwrap();
    let emitted = emitter::emit(&query, tmp.path().to_str().unwrap()).expect("emit succeeds");
    let sql_result = sql_matches(conn, &emitted);

    assert_eq!(
        eval_result, sql_result,
        "where-stage parity mismatch
dsl: {dsl:?}
event: {event:?}
sql: {}",
        emitted.sql
    );
}

/// The pipeline `where level …` stage means the same thing to the SQL
/// emitter and to the streaming evaluator, for every severity number and
/// NULL. `level` is an alias, not a column: the evaluator has to mirror
/// the band predicate or SSE filters out everything batch returns.
#[test]
fn where_level_band_parity_exhaustive() {
    let conn = Connection::open_in_memory().unwrap();
    let dsls = [
        "* | where level == \"error\"",
        "* | where level != \"info\"",
        "* | where level >= \"warn\"",
        "* | where level > \"warn\"",
        "* | where level < \"info\"",
        "* | where level <= \"info\"",
        "* | where level == \"notice\"",
        "* | where level == \"error\" and status == 500",
        "* | where not (level == \"error\")",
    ];
    for dsl in dsls {
        for sev in (1..=24).map(Some).chain([None]) {
            let mut event = envelope_event(sev, "hello", None);
            event.insert("status".into(), Value::Number(500.into()));
            assert_where_parity(&conn, dsl, &event);
        }
    }
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
    let count_sql = format!("SELECT count(*)::BIGINT FROM ({}) AS _sub", emitted.sql);
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
fn assert_pinned_parity(conn: &Connection, dsl: &str, event: &Map<String, Value>, ft: &FieldTypes) {
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
    let emitted = emitter::emit_with_pins(&query, &source, ft).expect("emit succeeds");
    let sql_result = sql_matches_strict(conn, &emitted);

    assert_eq!(
        filter_result, sql_result,
        "pinned parity mismatch\ndsl: {dsl:?}\nevent: {event:?}\nsql: {}\nparams: {:?}",
        emitted.sql, emitted.params
    );
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
        // Values inside DuckDB's TRY_CAST(… AS DOUBLE) domain but outside
        // `str::parse::<f64>` — batch counts them as numbers, so the live
        // matcher must too (whitespace, `_` separators). 'nan' additionally
        // orders ABOVE every literal in DuckDB's total DOUBLE ordering
        // where Rust's operators answer false.
        Value::String(" 200".into()),
        Value::String("200_000".into()),
        Value::String("nan".into()),
        Value::String("inf".into()),
        Value::String("-inf".into()),
        Value::String("+5".into()),
        Value::String("1e3".into()),
        // Outside both domains: still UNKNOWN on both sides.
        Value::String("0x10".into()),
        Value::Null,
    ];
    let dsls = [
        // eq/ne/IN, numeric and non-numeric literals
        "status=200",
        "status!=200",
        "status=200,301",
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
/// The column is written the way compaction writes it — `TRY_CAST` of the
/// wire text to the pin — so a value with no timestamp reading is NULL on
/// disk and UNKNOWN in memory, including under `NOT`.
#[test]
fn pinned_timestamp_pattern_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = pinned(&[("_time", CanonicalType::Timestamp)]);
    let values = [
        // The canonical wire form ingest writes.
        Value::String("2026-01-15T09:00:00.000000Z".into()),
        Value::String("2026-01-15T09:00:00.123456Z".into()),
        Value::String("2026-01-15T22:00:00.000000Z".into()),
        // Shapes a custom TIMESTAMP-pinned field can carry.
        Value::String("2026-01-15 09:00:00".into()),
        Value::String("2026-01-15T09:00:00+05:30".into()),
        Value::String("2026-01-15".into()),
        // No timestamp reading → NULL column / UNKNOWN matcher.
        Value::String("yesterday-ish".into()),
        Value::Null,
    ];
    let dsls = [
        // separator-anchored, both ways round
        "_time=/T09:/",
        "_time=/ 09:/",
        // zone suffix
        "_time=/Z$/",
        // fractional seconds
        r"_time=/\.000000Z$/",
        r"_time=/\.123456Z$/",
        // date prefix (glob and regex)
        "_time=2026-01-15*",
        "_time=2026-01-15T09*",
        "_time=/^2026-01-15/",
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
            _ => "NULL".to_owned(),
        };
        conn.execute_batch(&format!(
            "COPY (SELECT TRY_CAST({wire} AS TIMESTAMP) AS _time, 'hello' AS message) \
             TO '{path}' (FORMAT PARQUET)"
        ))
        .expect("write timestamp parquet");

        for dsl in dsls {
            let query = parser::parse(dsl).expect("dsl parses");
            let filter = CompiledFilter::compile(&query.search, &ft).expect("filter compiles");
            let filter_result = filter.matches(&event);
            let emitted = emitter::emit_with_pins(&query, &path, &ft).expect("emit succeeds");
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
