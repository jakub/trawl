// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every committed pin-aware seed emits SQL that real `DuckDB` binds and
//! plans.
//!
//! That is the whole claim of this file, and it is narrow on purpose. The
//! `parse_emit` fuzz target proves emission does not PANIC over arbitrary
//! text and arbitrary pin maps; it never asks whether the string it
//! produced is SQL. A target that emits `SELECT WHERE FROM` all day would
//! stay green. This fixture closes that hole for the seeds someone chose
//! deliberately: for each one it builds a parquet file carrying the columns
//! the query binds, emits under the seed's own pins, and hands the result
//! to `Connection::prepare`, which runs `DuckDB`'s parser and binder
//! without executing anything.
//!
//! This is NOT [`duckdb_probe`](../duckdb_probe.rs)'s job and does not
//! belong in that file. The probe establishes GROUND TRUTH, meaning what a
//! cast domain actually contains and what a live mirror must reproduce, by
//! executing statements and reading values back. Here nothing is executed
//! and no value is read: the oracle is "does the binder accept it", so the
//! two files fail for disjoint reasons and share no fixtures.
//!
//! # The pin map is decoded, never hardcoded
//!
//! A seed is `QUERY \0 SELECTOR` and its pins come out of
//! [`trawl_core::fuzz_input::derive_field_types`], the same decoder the
//! fuzz target runs. Writing the pin maps out in Rust here would be a
//! second source of truth for what a seed MEANS, and the first time a
//! selector byte changed, this file would keep asserting against the old
//! reading while reporting green.
//!
//! # What a seed may reference
//!
//! The fixture parquet gets one column per PINNED field plus the declared
//! envelope, and nothing else (see [`fixture_columns`]). So a seed may
//! reference an unpinned name only when the PIPELINE produces that name.
//! `| rename status as code` mints `code`, and materialising a `code`
//! column would collide with the rename's own output. A future seed that
//! reads an unpinned name out of the source instead makes this test red
//! with `Referenced column ... not found`, which is the correct outcome:
//! the seeds directory is a shared on-disk contract and the constraint
//! belongs somewhere that enforces it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use trawl_core::context::EvalContext;
use trawl_core::emitter;
use trawl_core::fuzz_input;
use trawl_core::schema::{self, CanonicalType, FieldTypes};

/// The committed pin-aware seed corpus, reached across the crate boundary.
///
/// Canonicalised so a failure names an absolute location rather than a
/// relative path whose meaning depends on the reader's cwd.
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

/// Every `.case` file, sorted by path.
///
/// Sorted because `read_dir` yields whatever order the filesystem feels
/// like, and an assertion that names "the third seed" has to mean the same
/// file on every host or a CI failure cannot be reproduced locally.
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

/// A connection configured the way every conforming connection is
/// ([`trawl_core::conform::SESSION_TIME_ZONE_SQL`]).
///
/// The session zone matters even here, where nothing executes: a
/// `TIMESTAMPTZ` conform expression binds against the session's zone, so a
/// host running in `Europe/Warsaw` must not plan a different statement
/// than one running in UTC.
fn conn() -> duckdb::Connection {
    let conn = duckdb::Connection::open_in_memory().expect("an in-memory DuckDB should open");
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .expect("SET TimeZone='UTC' should succeed");
    conn
}

/// Quote a column name as a `DuckDB` identifier.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A plausible one-row value for a column, as a SQL literal.
///
/// Plausible rather than arbitrary: the planner is allowed to look at the
/// data, and a corpus of NULLs invites a constant-folding shortcut that
/// would let a genuinely unplannable expression through.
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

/// The text a VARCHAR column carries: the envelope slots get something
/// shaped like what ingest writes there, everything else a placeholder.
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

/// The columns a seed's fixture parquet must carry, in file order.
///
/// Three rules, and the reasoning behind each:
///
/// 1. Every DECLARED ENVELOPE field, at its envelope type. A bare text
///    search binds `message` and `_raw`; the default ordering binds
///    `_time`. Those columns exist on every file trawl writes, so leaving
///    them out would fail prepares for a reason that has nothing to do
///    with the emitter.
/// 2. Every field in the DERIVED PIN MAP, at
///    [`CanonicalType::as_duckdb`] for its pin. A pinned field is by
///    definition a catalog field, which is to say a real source column.
///    `as_duckdb` is deliberately not injective, since SEVERITY is a
///    BIGINT on disk, so a SEVERITY pin lands a BIGINT column here,
///    exactly as compaction would write it.
/// 3. A pin WINS over the envelope type for the same name, and the column
///    appears ONCE. The severity seed pins `_severity` (to SEVERITY, which
///    is what the envelope declares anyway), and emitting the name twice
///    would put two columns of one name in the parquet.
///
/// Referenced-but-unpinned names get NO column. See the module docs: they
/// are pipeline-invented, and inventing a source column of that name would
/// collide with the stage that produces it.
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

/// Write a one-row parquet with exactly `columns`, then prove by
/// `DESCRIBE` that it came back with the names and physical types asked
/// for.
///
/// The read-back is not ceremony. If the fixture were silently wrong, say
/// a column missing or a `CAST` collapsing two types into one, every
/// prepare downstream would fail with a message about the emitted SQL, and
/// the file would look like it had found an emitter bug.
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
        path.display()
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

/// `DESCRIBE` a parquet file: `(column_name, column_type)` in file order.
fn describe(conn: &duckdb::Connection, path: &Path) -> Vec<(String, String)> {
    let sql = format!("DESCRIBE SELECT * FROM read_parquet('{}')", path.display());
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

/// Replay one seed end to end and return the pins it decoded plus whether
/// this seed reached the `raw_free_sql` prepare.
fn check_seed(conn: &duckdb::Connection, scratch: &Path, seed: &Path) -> (FieldTypes, bool) {
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

    let fixture = scratch.join(format!(
        "{}.parquet",
        seed.file_stem()
            .expect("a .case file has a stem")
            .to_string_lossy()
    ));
    write_fixture(conn, &fixture, &fixture_columns(&pins));

    let source = fixture.display().to_string();
    let emitted = emitter::emit_with_pins(&query, &source, &pins, EvalContext::capture())
        .unwrap_or_else(|err| panic!("seed {label} failed to emit: {err}\n  pins {pins:?}"));

    assert!(
        emitted.rust_stages.is_empty(),
        "seed {label} splits at `extract kv` and leaves {} stage(s) outside SQL; PREPARE would \
         then cover only the SQL prefix while this test reported green over the whole query",
        emitted.rust_stages.len()
    );

    prepare_ok(conn, &emitted.sql, emitted.params.len(), &label, "sql");
    let reached_raw_free = emitted.raw_free_sql.is_some();
    if let Some(raw_free) = &emitted.raw_free_sql {
        // The one emitted SQL string nothing else in the suite executes:
        // the executor only reaches it when a source turns out to have no
        // `_raw` column, which no other test sets up.
        prepare_ok(conn, raw_free, emitted.params.len(), &label, "raw_free_sql");
    }

    (pins, reached_raw_free)
}

/// Prepare one emitted statement and check its placeholder count.
///
/// The `?` placeholders are handed to `DuckDB` exactly as emitted.
/// Nothing here rewrites or inlines them, because the placeholder count IS
/// half the claim. An emitter that dropped a parameter would still produce
/// preparable SQL; it would just bind the wrong values at runtime.
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

/// The main event: every committed seed's emitted SQL prepares, and the
/// seeds between them reach every canonical pin.
#[test]
fn every_committed_seed_emits_sql_duckdb_can_prepare() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let conn = conn();
    let seeds = seed_files();

    // A renamed or emptied directory must be red, never a quiet pass over
    // zero files. Seven is what is committed today — six pin seeds plus the
    // bare-text-search one that reaches the `raw_free_sql` prepare. A FLOOR,
    // not an equality: an eighth seed is a legitimate addition and must not
    // redden this.
    assert!(
        seeds.len() >= 7,
        "expected at least the seven committed .case seeds under {}, found {}",
        seeds_dir().display(),
        seeds.len()
    );

    let mut covered: BTreeSet<CanonicalType> = BTreeSet::new();
    let mut raw_free_reached = 0usize;
    for seed in &seeds {
        let (pins, reached_raw_free) = check_seed(&conn, scratch.path(), seed);
        covered.extend(pins.iter().map(|(_, ty)| ty));
        if reached_raw_free {
            raw_free_reached += 1;
        }
    }

    // A FLOOR, not an equality: a second bare-text-search seed must not
    // redden this. `raw_free_sql` is `Some` only when a bare text search
    // binds `_raw`, and it is the one emitted SQL string nothing else in
    // the suite executes (see `check_seed`). Without a seed that carries a
    // bare term, that branch goes unprepared and this file would report
    // green while never having run it. Keep at least one seed with a bare
    // term under crates/trawl-core/fuzz/seeds/parse_emit/.
    assert!(
        raw_free_reached >= 1,
        "no committed seed produced Some(raw_free_sql); a seed with a bare text search (which \
         binds `_raw`) must exist under crates/trawl-core/fuzz/seeds/parse_emit/ to keep that \
         prepare exercised"
    );

    // Acceptance criterion 3, and the designed drift guard for
    // `CanonicalType::ALL`: the vocabulary is iterated, never re-listed
    // here, so a seventh variant widens what this assertion demands
    // without anyone editing this file.
    let all: BTreeSet<CanonicalType> = CanonicalType::ALL.into_iter().collect();
    let missing: Vec<CanonicalType> = all.difference(&covered).copied().collect();
    assert!(
        missing.is_empty(),
        "no committed seed pins {missing:?}; a new CanonicalType needs a new seed under \
         crates/trawl-core/fuzz/seeds/parse_emit/ that pins a field to it, or the fuzzer and \
         this fixture both stop exercising the type"
    );
    assert_eq!(
        covered, all,
        "the seeds decoded a pin outside CanonicalType::ALL, which the selector cannot produce"
    );
}

/// CONTROL. If this ever goes green, every other assertion in this file is
/// worthless.
///
/// It proves `DuckDB`'s binder resolves column names at PREPARE time. The
/// whole design leans on that: nothing here executes a statement, so if
/// binding were deferred to execution, a seed emitting `SELECT
/// nonsense_column` would prepare happily and this file would be asserting
/// that the emitter produces well-formed *text*.
#[test]
fn control_prepare_rejects_a_missing_column() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let conn = conn();
    let fixture = scratch.path().join("control.parquet");
    write_fixture(&conn, &fixture, &fixture_columns(&FieldTypes::new()));

    let sql = format!(
        "SELECT definitely_missing_column FROM read_parquet('{}')",
        fixture.display()
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

/// CONTROL. Green here would mean this file cannot tell "your SQL is fine"
/// from "the file you pointed at is not there".
///
/// Every seed's prepare names a fixture path. If a missing source prepared
/// cleanly, a broken fixture builder would look exactly like a passing
/// suite.
#[test]
fn control_prepare_rejects_a_missing_source_file() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let absent = scratch.path().join("no-such-file.parquet");
    let sql = format!("SELECT * FROM read_parquet('{}')", absent.display());
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

/// CONTROL. Name resolution alone is a weak oracle: the emitter's whole
/// pin-aware job is choosing an expression that TYPE-CHECKS against the
/// column it reads.
///
/// The pair is the point. `abs` over the BIGINT `_severity` prepares;
/// `abs` over the TIMESTAMP `_time` must not, and both name real columns
/// of the same file. A green failure half would mean the binder resolves
/// names but defers types, and every claim this file makes about
/// conform expressions would shrink to "the column exists".
#[test]
fn control_prepare_rejects_a_type_invalid_expression() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let conn = conn();
    let fixture = scratch.path().join("types.parquet");
    write_fixture(&conn, &fixture, &fixture_columns(&FieldTypes::new()));
    let source = format!("read_parquet('{}')", fixture.display());

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
