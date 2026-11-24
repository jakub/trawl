// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every committed pin-aware seed emits SQL that real `DuckDB` binds and
//! plans.
//!
//! `Connection::prepare` runs `DuckDB`'s parser and binder without executing
//! anything.
//!
//! A seed's pins come out of [`trawl_core::fuzz_input::derive_field_types`],
//! the same decoder the fuzz target runs. Writing the pin maps out in Rust
//! here would be a second source of truth for what a seed MEANS.
//!
//! The fixture parquet gets one column per PINNED field plus the declared
//! envelope, and nothing else (see [`fixture_columns`]). So a seed may
//! reference an unpinned name only when the PIPELINE produces that name.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use trawl_core::context::EvalContext;
use trawl_core::emitter;
use trawl_core::fuzz_input;
use trawl_core::schema::{self, CanonicalType, FieldTypes};

fn seeds_dir() -> PathBuf {
    let expected =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../trawl-core/fuzz/seeds/parse_emit");
    expected.canonicalize().unwrap_or_else(|err| {
        panic!(
            "the pin-aware seed corpus must live at \
             crates/trawl-core/fuzz/seeds/parse_emit (resolved here as {}): {err}",
            expected.display()
        )
    })
}

fn seed_files() -> Vec<PathBuf> {
    let dir = seeds_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("cannot read the seed corpus at {}: {err}", dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|err| panic!("cannot read an entry of {}: {err}", dir.display()))
                .path()
        })
        .filter(|path| path.extension().is_some_and(|ext| ext == "case"))
        .collect();
    files.sort();
    files
}

/// A `TIMESTAMPTZ` conform expression binds against the session's zone, so a
/// host running in `Europe/Warsaw` must not plan a different statement
/// than one running in UTC.
fn conn() -> duckdb::Connection {
    let conn = duckdb::Connection::open_in_memory().expect("an in-memory DuckDB should open");
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .expect("SET TimeZone='UTC' should succeed");
    conn
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn sql_path(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}

/// Plausible rather than arbitrary: a corpus of NULLs invites a
/// constant-folding shortcut that would let a genuinely unplannable
/// expression through.
fn sample_literal(name: &str, ty: CanonicalType) -> String {
    match ty {
        CanonicalType::Boolean => "TRUE".to_owned(),
        // 17 is a real ladder point (`error`) rather than a bare 42, so a
        // SEVERITY column holds something its own conform rung admits.
        CanonicalType::Severity => "17".to_owned(),
        CanonicalType::BigInt => "42".to_owned(),
        CanonicalType::Double => "1.5".to_owned(),
        CanonicalType::Timestamp => "TIMESTAMP '2026-01-01 00:00:00'".to_owned(),
        CanonicalType::Varchar => format!("'{}'", varchar_sample(name)),
    }
}

fn varchar_sample(name: &str) -> &'static str {
    match name {
        schema::RAW => "{\"service\":\"nginx\",\"status\":\"200\"}",
        schema::REPAIRS => "[]",
        schema::ENV => "prod",
        schema::SERVICE => "nginx",
        schema::HOST => "host-1",
        schema::MESSAGE => "a sample message",
        schema::PRODUCER => "http",
        _ => "sample",
    }
}

fn fixture_columns(pins: &FieldTypes) -> Vec<(String, CanonicalType)> {
    let mut columns: Vec<(String, CanonicalType)> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();

    for (name, envelope_ty) in schema::ENVELOPE_TYPES {
        seen.insert(name);
        columns.push(((*name).to_owned(), pins.get(name).unwrap_or(*envelope_ty)));
    }
    for (name, ty) in pins.iter() {
        if seen.insert(name) {
            columns.push((name.to_owned(), ty));
        }
    }
    columns
}

/// If the fixture were silently wrong, say a column missing or a `CAST`
/// collapsing two types into one, every prepare downstream would fail with a
/// message about the emitted SQL.
fn write_fixture(conn: &duckdb::Connection, path: &Path, columns: &[(String, CanonicalType)]) {
    let projection = columns
        .iter()
        .map(|(name, ty)| {
            format!(
                "CAST({} AS {}) AS {}",
                sample_literal(name, *ty),
                ty.as_duckdb(),
                quote_ident(name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let copy = format!(
        "COPY (SELECT {projection}) TO '{}' (FORMAT PARQUET)",
        sql_path(path)
    );
    conn.execute_batch(&copy)
        .unwrap_or_else(|err| panic!("the fixture write failed: {err}\n  {copy}"));

    let expected: Vec<(String, String)> = columns
        .iter()
        .map(|(name, ty)| (name.clone(), ty.as_duckdb().to_owned()))
        .collect();
    assert_eq!(
        describe(conn, path),
        expected,
        "the fixture at {} does not have the schema this test asked for",
        path.display()
    );
}

fn describe(conn: &duckdb::Connection, path: &Path) -> Vec<(String, String)> {
    let sql = format!("DESCRIBE SELECT * FROM read_parquet('{}')", sql_path(path));
    let mut statement = conn
        .prepare(&sql)
        .unwrap_or_else(|err| panic!("DESCRIBE failed to prepare: {err}\n  {sql}"));
    statement
        .query_map(duckdb::params![], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap_or_else(|err| panic!("DESCRIBE failed to run: {err}\n  {sql}"))
        .map(|row| row.expect("a DESCRIBE row should decode"))
        .collect()
}

fn check_seed(conn: &duckdb::Connection, scratch: &Path, seed: &Path) -> FieldTypes {
    let label = seed.display().to_string();
    let text = std::fs::read_to_string(seed)
        .unwrap_or_else(|err| panic!("seed {label} is not readable UTF-8: {err}"));

    let case = fuzz_input::decode_case(&text);
    let query = trawl_core::parser::parse(case.query).unwrap_or_else(|errors| {
        let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
        panic!(
            "seed {label} does not parse, so it can never reach the emitter: {}\n  query {:?}",
            rendered.join("; "),
            case.query
        )
    });
    let pins = fuzz_input::derive_field_types(&query, case.selector);

    let stem = seed
        .file_stem()
        .expect("a .case file has a stem")
        .to_string_lossy()
        .into_owned();
    let fixture = scratch.join(format!("{stem}.parquet"));
    write_fixture(conn, &fixture, &fixture_columns(&pins));

    let source = fixture.display().to_string();
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        &pins,
        &emitter::InputColumns::unprobed(),
        EvalContext::capture(),
    )
    .unwrap_or_else(|err| panic!("seed {label} failed to emit: {err}\n  pins {pins:?}"));

    assert!(
        emitted.rust_stages.is_empty(),
        "seed {label} splits at `extract kv` and leaves {} stage(s) outside SQL; PREPARE would \
         then cover only the SQL prefix while this test reported green over the whole query",
        emitted.rust_stages.len()
    );

    prepare_ok(conn, &emitted.sql, emitted.params.len(), &label, "sql");
    pins
}

/// The `?` placeholders are handed to `DuckDB` exactly as emitted. An emitter
/// that dropped a parameter would still produce preparable SQL; it would
/// just bind the wrong values at runtime.
fn prepare_ok(conn: &duckdb::Connection, sql: &str, params: usize, label: &str, lane: &str) {
    let statement = conn.prepare(sql).unwrap_or_else(|err| {
        panic!("seed {label} emitted {lane} DuckDB cannot prepare: {err}\n  {sql}")
    });
    assert_eq!(
        statement.parameter_count(),
        params,
        "seed {label}: {lane} has {} placeholder(s) but the emitter handed over {params} \
         parameter(s)\n  {sql}",
        statement.parameter_count()
    );
}

#[test]
fn every_committed_seed_emits_sql_duckdb_can_prepare() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let conn = conn();
    let seeds = seed_files();

    // A renamed or emptied directory must be red, never a quiet pass over
    // zero files. A FLOOR, not an equality: a ninth seed is a legitimate
    // addition and must not redden this.
    assert!(
        seeds.len() >= 8,
        "expected at least the eight committed .case seeds under {}, found {}",
        seeds_dir().display(),
        seeds.len()
    );

    let mut covered: BTreeSet<CanonicalType> = BTreeSet::new();
    for seed in &seeds {
        let pins = check_seed(&conn, scratch.path(), seed);
        covered.extend(pins.iter().map(|(_, ty)| ty));
    }

    // The vocabulary is iterated, never re-listed here, so adding an entry
    // to `ALL` widens what this demands without anyone editing this file.
    let all: BTreeSet<CanonicalType> = CanonicalType::ALL.into_iter().collect();
    let missing: Vec<CanonicalType> = all.difference(&covered).copied().collect();
    assert!(
        missing.is_empty(),
        "no committed seed pins {missing:?}; a new entry in CanonicalType::ALL needs a new seed \
         under crates/trawl-core/fuzz/seeds/parse_emit/ that pins a field to it, or the fuzzer \
         and this fixture both stop exercising the type"
    );
}

#[test]
fn control_prepare_rejects_a_missing_column() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let conn = conn();
    let fixture = scratch.path().join("control.parquet");
    write_fixture(&conn, &fixture, &fixture_columns(&FieldTypes::new()));

    let sql = format!(
        "SELECT definitely_missing_column FROM read_parquet('{}')",
        sql_path(&fixture)
    );
    let err = conn.prepare(&sql).err().unwrap_or_else(|| {
        panic!(
            "DuckDB prepared a SELECT over a column that does not exist, so PREPARE no longer \
             binds columns and this whole file proves nothing\n  {sql}"
        )
    });
    let err = err.to_string();
    assert!(
        err.contains("definitely_missing_column"),
        "the binder refused for some reason other than the missing column: {err}"
    );
}

#[test]
fn control_prepare_rejects_a_missing_source_file() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let absent = scratch.path().join("no-such-file.parquet");
    let sql = format!("SELECT * FROM read_parquet('{}')", sql_path(&absent));
    let err = conn().prepare(&sql).err().unwrap_or_else(|| {
        panic!(
            "DuckDB prepared a read of a file that does not exist, so a fixture that never got \
             written would still look like a passing seed\n  {sql}"
        )
    });
    let err = err.to_string();
    assert!(
        err.contains("no-such-file.parquet"),
        "the refusal did not name the absent file: {err}"
    );
}

#[test]
fn control_prepare_rejects_a_type_invalid_expression() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let conn = conn();
    let fixture = scratch.path().join("types.parquet");
    write_fixture(&conn, &fixture, &fixture_columns(&FieldTypes::new()));
    let source = format!("read_parquet('{}')", sql_path(&fixture));

    let valid = format!("SELECT abs(\"_severity\") FROM {source}");
    conn.prepare(&valid).unwrap_or_else(|err| {
        panic!("abs() over the BIGINT _severity column should prepare: {err}\n  {valid}")
    });

    let invalid = format!("SELECT abs(\"_time\") FROM {source}");
    let err = conn.prepare(&invalid).err().unwrap_or_else(|| {
        panic!(
            "DuckDB prepared abs() over a TIMESTAMP column, so PREPARE checks names but not \
             types and this file cannot see a mistyped conform expression\n  {invalid}"
        )
    });
    let err = err.to_string();
    assert!(
        err.contains("abs"),
        "the refusal did not name the mistyped function: {err}"
    );
}
