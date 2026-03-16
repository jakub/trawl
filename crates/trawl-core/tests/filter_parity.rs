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

use trawl_core::emitter::{self, EmittedQuery, SqlValue};
use trawl_core::filter::CompiledFilter;
use trawl_core::parser;

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
const FIELDS: &[&str] = &["service", "level", "status", "host", "path"];

/// String-typed fields (excludes `status` which is numeric in events).
/// Used when the filter value is a string to avoid `DuckDB` conversion errors.
const STRING_FIELDS: &[&str] = &["service", "level", "host", "path"];

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
    match rng.range(6) {
        0 => random_field_eq(rng),
        1 => random_field_compare(rng),
        2 => random_field_list(rng),
        3 => random_text_search(rng),
        4 => random_quoted_search(rng),
        5 => random_field_glob(rng),
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

        // In-memory filter result.
        let filter = CompiledFilter::compile(&query.search);
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
