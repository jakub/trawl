// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine-assumption probe (ADR-0009, ADR-0011): every claim the conform
//! path makes about `DuckDB` is executed here against the bundled engine,
//! never assumed — the cast domains, the round-trip guard, the
//! `read_json` inference classes a hot snapshot can produce, and the
//! agreement between the two conform lanes and their live mirrors.
//!
//! # The probe matrix is the contract
//!
//! Where a live mirror in [`trawl_core::compare`] claims to reproduce a
//! `DuckDB` reading, the pair runs here side by side over a matrix of
//! inputs, and a divergence the matrix does not name is a bug in the
//! mirror, not something to work around at the call site. Both engines
//! answer the same user question: the batch query reads the conformed
//! column off disk while the live tail reads the wire JSON, so a mirror
//! that is merely close makes a stream fire on events the equivalent
//! query drops (or drop events it returns) with nothing in the request
//! to explain it.
//!
//! When a new divergence turns up: add the input to the matrix, then fix
//! the mirror against what the engine actually does. Every divergence
//! that survives is a deliberate, one-directional residual with its cost
//! written down and its own test asserting it stays one-directional (see
//! [`the_timestamp_mirror_residuals_are_one_directional`]) — the mirror
//! may under-read what `DuckDB` reads, never the reverse.
//!
//! Ground truth comes from execution, not from a parser's documentation:
//! the timestamp divergences ADR-0011 ruling #4 settled (`epoch`, a
//! trailing ` UTC`, hour-24 rollover, `T09:00+00:00`) were all in rules
//! that had been reasoned out rather than run.

use std::io::Write as _;

use trawl_core::conform::{decimal_reading, guarded_cast, untyped_text};
use trawl_core::schema::CanonicalType;

fn hot_reader(path: &std::path::Path) -> String {
    format!(
        "read_json('{}', format='newline_delimited', records=true, \
         auto_detect=true, field_appearance_threshold=0)",
        path.display()
    )
}

/// A connection configured the way every conforming connection is
/// ([`trawl_core::conform::SESSION_TIME_ZONE_SQL`]).
fn conn() -> duckdb::Connection {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    conn
}

/// The conform both lanes emit: one text form, one guard. Mirrors
/// `trawl_server::ingest::compaction::conform_expr`, whose `DESCRIBE`d
/// `dtype` decides only whether a column is already its pin, never how
/// the column is read.
fn conform(quoted: &str, pin: CanonicalType) -> String {
    guarded_cast(&untyped_text(quoted), pin)
}

/// The emitted negated-list predicate against a real VARCHAR parquet
/// column: every element must use the dual text/DECIMAL equality rule,
/// and the search-stage NULL widening must survive their AND
/// composition.
#[test]
fn varchar_negated_list_executes_as_pin_aware_not_in_with_null_widening() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("status.parquet");
    let conn = conn();
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES ('200'), ('0200'), ('200.0'), ('301'), ('404'), \
         (CAST(NULL AS VARCHAR))) AS t(status)) TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();

    let query = trawl_core::parser::parse("status!=200,301").unwrap();
    let mut pins = trawl_core::schema::FieldTypes::new();
    pins.insert("status", CanonicalType::Varchar);
    let emitted = trawl_core::emitter::emit_with_pins(
        &query,
        &file.display().to_string(),
        &pins,
        trawl_core::context::EvalContext::capture(),
    )
    .unwrap();
    let params: Vec<Box<dyn duckdb::ToSql>> = emitted
        .params
        .iter()
        .map(|value| -> Box<dyn duckdb::ToSql> {
            match value {
                trawl_core::emitter::SqlValue::String(value) => Box::new(value.clone()),
                trawl_core::emitter::SqlValue::Int(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Float(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Bool(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Timestamp(value) => {
                    Box::new(duckdb::types::Value::Timestamp(
                        duckdb::types::TimeUnit::Microsecond,
                        value.and_utc().timestamp_micros(),
                    ))
                }
            }
        })
        .collect();
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let sql = format!(
        "SELECT status FROM ({}) AS matched ORDER BY status NULLS LAST",
        emitted.sql
    );
    let mut statement = conn.prepare(&sql).unwrap();
    let rows: Vec<Option<String>> = statement
        .query_map(param_refs.as_slice(), |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert_eq!(rows, vec![Some("404".to_owned()), None], "{sql}");
}

#[test]
fn untyped_varchar_conform_expression_is_unquoted_across_inference_classes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hot.ndjson");
    let mut f = std::fs::File::create(&file).unwrap();
    // s: all strings -> VARCHAR; m: mixed -> JSON; b: all ints -> BIGINT.
    writeln!(f, r#"{{"s":"n/a","m":5,"b":410}}"#).unwrap();
    writeln!(f, r#"{{"s":"ok","m":"n/a","b":420}}"#).unwrap();
    f.sync_all().unwrap();

    let conn = duckdb::Connection::open_in_memory().unwrap();
    // Confirm the fixture really lands in the three classes above.
    let mut stmt = conn
        .prepare(&format!("DESCRIBE SELECT * FROM {}", hot_reader(&file)))
        .unwrap();
    let types: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        types,
        vec![
            ("s".into(), "VARCHAR".into()),
            ("m".into(), "JSON".into()),
            ("b".into(), "BIGINT".into()),
        ],
        "read_json inference classes changed under us"
    );

    let sql = format!(
        "SELECT json_extract_string(to_json(s), '$'), \
                json_extract_string(to_json(m), '$'), \
                json_extract_string(to_json(b), '$') \
         FROM {} ORDER BY b",
        hot_reader(&file)
    );
    let mut stmt = conn.prepare(&sql).unwrap();
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        vec![
            ("n/a".into(), "5".into(), "410".into()),
            ("ok".into(), "n/a".into(), "420".into()),
        ],
        "strings must land UNQUOTED (n/a, not \"n/a\") in every class"
    );
}

/// The pin ladder and the conform step guard every typed cast with a
/// round-trip check (ADR-0009): `TRY_CAST` to a numeric type rounds
/// rather than fails — `1.5 → 2` counts as a "success" — so a bare
/// `count(TRY_CAST(...))` would score a fractional batch ≥90% BIGINT and
/// silently round every value on write, with no `field_conflicts` row.
/// This pins both halves across the inference classes the WAL can produce
/// (VARCHAR, JSON-mixed, HUGEINT numeric):
///
/// 1. the premise: the unguarded cast really does round (a `DuckDB` bump
///    that makes it fail instead would let the guard be simplified), and
/// 2. the guard as both lanes emit it
///    ([`trawl_core::conform::guarded_cast`]): BIGINT compares the cast
///    against the value's canonical text re-parsed as `DECIMAL(38,6)` —
///    exact across the whole BIGINT range, where a DOUBLE-space comparison
///    is blind above 2^53 (both sides collapse to one double, so
///    `1735689600123456710.7` conforms BIGINT silently); representation
///    drift is tolerated (`4.0 → 4`, `"042" → 42`), as is a fraction
///    below DECIMAL(38,6)'s half-microstep (`4.0000001 → 4`, the
///    documented residual tolerance). DOUBLE stays in DOUBLE space
///    (`u64::MAX` → DOUBLE with precision loss, which is what pinning
///    DOUBLE means); BOOLEAN compares strict text (`true`/`false` only,
///    so `1` never conforms to a BOOLEAN pin).
#[test]
#[allow(clippy::too_many_lines)] // one probe per comparison-space decision, kept together
fn typed_casts_round_so_the_conform_guard_must_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let conn = conn();

    let describe_type = |sql: &str| -> String {
        let mut stmt = conn.prepare(&format!("DESCRIBE {sql}")).unwrap();
        stmt.query_row([], |row| row.get(1)).unwrap()
    };

    // --- inference classes: mixed values infer JSON, u64-range HUGEINT ---
    let mixed = dir.path().join("mixed.ndjson");
    {
        let mut f = std::fs::File::create(&mixed).unwrap();
        writeln!(f, r#"{{"m":1.5,"u":18446744073709551615}}"#).unwrap();
        writeln!(f, r#"{{"m":"n/a","u":9007199254740993}}"#).unwrap();
        f.sync_all().unwrap();
    }
    let reader = hot_reader(&mixed);
    assert_eq!(
        describe_type(&format!("SELECT m FROM {reader}")).as_str(),
        "JSON",
        "mixed values must infer JSON (the class the ladder exists for)"
    );
    assert_eq!(
        describe_type(&format!("SELECT u FROM {reader}")).as_str(),
        "HUGEINT",
        "u64-range integers must infer HUGEINT"
    );

    // --- the premise: TRY_CAST rounds, it does not fail ---
    let one = |sql: &str| -> Option<i64> { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
    assert_eq!(
        one("SELECT TRY_CAST(m AS BIGINT) FROM (SELECT m FROM read_json('MIXED', format='newline_delimited', records=true, auto_detect=true, field_appearance_threshold=0)) WHERE json_extract_string(m,'$') = '1.5'"
            .replace("MIXED", &mixed.display().to_string())
            .as_str()),
        Some(2),
        "TRY_CAST(JSON 1.5 AS BIGINT) rounds to 2 — the silent-loss premise"
    );
    assert_eq!(
        one("SELECT TRY_CAST('1.5' AS BIGINT)"),
        Some(2),
        "TRY_CAST(VARCHAR '1.5' AS BIGINT) rounds too"
    );
    assert_eq!(
        one("SELECT TRY_CAST(1.5::DOUBLE AS BIGINT)"),
        Some(2),
        "TRY_CAST(DOUBLE 1.5 AS BIGINT) rounds too"
    );
    assert_eq!(
        one(&format!(
            "SELECT TRY_CAST(m AS BIGINT) FROM {reader} WHERE json_extract_string(m,'$') = '1.5'"
        )),
        Some(2),
        "the JSON class rounds through the real reader as well"
    );

    // --- the guard, exactly as the shared builder emits it ---
    let guard_bigint_json = conform("m", CanonicalType::BigInt);
    let rows: Vec<(String, Option<i64>)> = {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT json_extract_string(m, '$'), {guard_bigint_json} FROM {reader} \
                 ORDER BY 1"
            ))
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        rows,
        vec![("1.5".into(), None), ("n/a".into(), None)],
        "the guarded BIGINT cast must NULL a fractional value instead of rounding"
    );

    // VARCHAR class: strings that survive vs strings that round.
    let varchar_cases: Vec<(String, Option<i64>)> = {
        let guard = conform("v", CanonicalType::BigInt);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT v, {guard} \
                 FROM (VALUES ('42'), ('042'), ('1.5'), ('n/a')) t(v) ORDER BY v"
            ))
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        varchar_cases,
        vec![
            ("042".into(), Some(42)), // representation drift tolerated (numeric space)
            ("1.5".into(), None),     // rounding refused
            ("42".into(), Some(42)),
            ("n/a".into(), None),
        ],
        "VARCHAR class: numeric-space round-trip"
    );

    // HUGEINT class: u64::MAX must fail the BIGINT guard but still conform
    // to DOUBLE — the DOUBLE rung deliberately tolerates the precision
    // loss, since a u64-range batch pins DOUBLE.
    let (bigint, double): (Option<i64>, Option<f64>) = conn
        .query_row(
            &format!(
                "SELECT {}, {} FROM {reader} \
                 WHERE CAST(u AS VARCHAR) = '18446744073709551615'",
                conform("u", CanonicalType::BigInt),
                conform("u", CanonicalType::Double),
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (bigint, double),
        (None, Some(18_446_744_073_709_551_615.0)),
        "u64::MAX: the BIGINT cast NULLs, the DOUBLE rung keeps it lossily"
    );

    // BOOLEAN: TRY_CAST(true AS BIGINT) = 1, so an unguarded count scores
    // a boolean batch as BIGINT and the BOOLEAN rung is unreachable. Under
    // the guard booleans fail BIGINT and pass only the strict-text rung.
    let bools = dir.path().join("bools.ndjson");
    {
        let mut f = std::fs::File::create(&bools).unwrap();
        for i in 0..19 {
            writeln!(
                f,
                r#"{{"b":{}}}"#,
                if i % 2 == 0 { "true" } else { "false" }
            )
            .unwrap();
        }
        writeln!(f, r#"{{"b":"n/a"}}"#).unwrap();
        f.sync_all().unwrap();
    }
    let breader = hot_reader(&bools);
    assert_eq!(
        describe_type(&format!("SELECT b FROM {breader}")).as_str(),
        "JSON"
    );
    let (raw_bi, ok_bi, ok_bool): (i64, i64, i64) = conn
        .query_row(
            &format!(
                "SELECT count(TRY_CAST(b AS BIGINT))::BIGINT, \
                        count({})::BIGINT, count({})::BIGINT FROM {breader}",
                conform("b", CanonicalType::BigInt),
                conform("b", CanonicalType::Boolean),
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        raw_bi, 19,
        "premise: TRY_CAST(bool AS BIGINT) succeeds (1/0) — the old counting \
         scored a boolean batch ≥90% BIGINT, making the Boolean rung unreachable"
    );
    assert_eq!((ok_bi, ok_bool), (0, 19), "guarded: booleans pin BOOLEAN");

    // TIMESTAMP: the rung reads the format drift a wire timestamp carries
    // (RFC 3339 `T`/`Z`, DuckDB's own space-separated rendering, a bare
    // date) and NULLs a text no parser reads. Zone handling has its own
    // probe (`timestamp_conform_applies_the_offset...`).
    let ts_cases: Vec<(String, Option<String>)> = {
        let guard = conform("v", CanonicalType::Timestamp);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT v, CAST({guard} AS VARCHAR) \
                 FROM (VALUES ('2024-01-15T09:00:00Z'), ('2024-01-15 09:00:00'), \
                              ('2024-01-15'), ('yesterday-ish')) t(v) ORDER BY v"
            ))
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        ts_cases,
        vec![
            ("2024-01-15".into(), Some("2024-01-15 00:00:00".into())),
            (
                "2024-01-15 09:00:00".into(),
                Some("2024-01-15 09:00:00".into())
            ),
            (
                "2024-01-15T09:00:00Z".into(),
                Some("2024-01-15 09:00:00".into())
            ),
            ("yesterday-ish".into(), None),
        ],
        "TIMESTAMP conform is format-tolerant, parse failures still NULL"
    );
}

/// Why the BIGINT rung compares in `DECIMAL(38,6)` space and not DOUBLE:
/// above 2^53 both sides of a DOUBLE comparison collapse to the same
/// double, so a nanosecond-epoch-magnitude fractional value
/// (`1735689600123456710.7`) would "round-trip" and conform BIGINT as
/// `...711` — the silent-rounding failure mode the guard exists to refuse,
/// moved up the number line. This pins the boundary classes: 2^53 ± 1 must
/// stay exact accepts, the >2^53 fractional must fail, a >2^53 integer must
/// still pass (DECIMAL is exact where DOUBLE space cannot distinguish),
/// `u64::MAX` still NULLs the cast, and the residual
/// tolerance — a fraction below DECIMAL(38,6)'s half-microstep quantizes
/// away (`4.0000001 → 4` accepted, `4.5` refused) — is deliberate and
/// documented, not an accident a bump may silently change.
#[test]
fn bigint_round_trip_is_exact_in_decimal_space_beyond_2_pow_53() {
    let conn = conn();

    // The DOUBLE-space blindness the DECIMAL rung avoids, as the premise.
    let blind: bool = conn
        .query_row(
            "SELECT TRY_CAST('1735689600123456710.7' AS DOUBLE) = \
             TRY_CAST('1735689600123456710.7' AS BIGINT)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        blind,
        "premise: DOUBLE-space comparison is blind above 2^53 — if a DuckDB \
         bump changes this, the DECIMAL space is defence, not necessity"
    );

    let cases: Vec<(String, Option<i64>)> = {
        let guard = conform("v", CanonicalType::BigInt);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT v, {guard} \
                 FROM (VALUES ('9007199254740991'), ('9007199254740992'), \
                              ('9007199254740993'), ('1735689600123456710.7'), \
                              ('1735689600123456710'), ('18446744073709551615'), \
                              ('9223372036854775807'), ('-9223372036854775808'), \
                              ('4.0000001'), ('4.5')) t(v)"
            ))
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        cases,
        vec![
            ("9007199254740991".into(), Some(9_007_199_254_740_991)),
            ("9007199254740992".into(), Some(9_007_199_254_740_992)),
            ("9007199254740993".into(), Some(9_007_199_254_740_993)),
            ("1735689600123456710.7".into(), None),
            (
                "1735689600123456710".into(),
                Some(1_735_689_600_123_456_710)
            ),
            ("18446744073709551615".into(), None),
            ("9223372036854775807".into(), Some(i64::MAX)),
            ("-9223372036854775808".into(), Some(i64::MIN)),
            ("4.0000001".into(), Some(4)),
            ("4.5".into(), None),
        ],
        "DECIMAL(38,6) round-trip: exact across the BIGINT range, refusing \
         rounding above the half-microstep"
    );

    // The DOUBLE rung deliberately keeps DOUBLE space: the >2^53 fractional
    // the BIGINT rung refuses still passes DOUBLE (it pins DOUBLE with
    // DOUBLE's precision, which is what pinning DOUBLE means).
    let db_ok: bool = conn
        .query_row(
            "SELECT TRY_CAST('1735689600123456710.7' AS DOUBLE) = \
             TRY_CAST('1735689600123456710.7' AS DOUBLE)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(db_ok, "the value stays representable on the DOUBLE rung");
}

/// `DuckDB` identifiers are case-INSENSITIVE, but only over ASCII — a
/// `REPLACE` list naming two ASCII case-variants is a hard parse error,
/// not a degraded read. This pins both halves of the equivalence the
/// ingest canonicalizer's field-name fold is built on
/// (`envelope::fold_field_names`, ADR-0009): what `DuckDB` collapses
/// (ASCII case) and what it must NOT (folding `CAFÉ` onto `café` would
/// merge two real columns — the fold maps `CAFÉ` to `cafÉ` instead).
#[test]
fn replace_list_identifier_folding_is_ascii_only() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hot.ndjson");
    let mut f = std::fs::File::create(&file).unwrap();
    writeln!(f, r#"{{"status":"ok","café":"a","Ωx":"b"}}"#).unwrap();
    f.sync_all().unwrap();

    let conn = duckdb::Connection::open_in_memory().unwrap();
    let replace = |entries: &str| {
        conn.prepare(&format!(
            "SELECT * REPLACE ({entries}) FROM {}",
            hot_reader(&file)
        ))
        .err()
        .map(|e| e.to_string())
    };

    // ASCII case-variants are ONE column: naming both is a parse error.
    let err = replace(r#"to_json("Status") AS "Status", to_json("status") AS "status""#)
        .expect("two ASCII case-variants must be rejected");
    assert!(
        err.contains("Duplicate entry"),
        "expected a duplicate-entry parse error, got: {err}"
    );
    // ...and either spelling alone binds the column.
    assert_eq!(replace(r#"to_json("STATUS") AS "STATUS""#), None);

    // Non-ASCII case is NOT folded — these stay distinct identifiers, so
    // the emitter must not fold them either.
    for (name, variant) in [("café", "CAFÉ"), ("Ωx", "ωx")] {
        let err = replace(&format!(r#"to_json("{variant}") AS "{variant}""#))
            .unwrap_or_else(|| panic!("{variant} unexpectedly bound {name}"));
        assert!(
            err.contains("not found"),
            "expected {variant} to be a distinct identifier from {name}, got: {err}"
        );
    }
}

/// A snapshot carrying BOTH spellings of one field is not two nameable
/// columns: `read_json` renames the collided key (`duration` + `Duration_1`),
/// and a `REPLACE` naming either spelling binds the FIRST column. So a pin
/// applied there conforms the WRONG column — retyping one service's values
/// while the pinned field's own data sits untouched in `Duration_1`. This is
/// the execution evidence for folding field names at the one ingest door
/// every producer enters (ADR-0009) rather than trying to pin either
/// spelling once both have reached a snapshot.
#[test]
fn case_collided_json_keys_are_renamed_and_replace_binds_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hot.ndjson");
    let mut f = std::fs::File::create(&file).unwrap();
    writeln!(f, r#"{{"duration":410,"x":1}}"#).unwrap();
    writeln!(f, r#"{{"Duration":"slow","x":2}}"#).unwrap();
    f.sync_all().unwrap();

    let conn = duckdb::Connection::open_in_memory().unwrap();
    let describe = |sql: &str| -> Vec<(String, String)> {
        let mut stmt = conn.prepare(&format!("DESCRIBE {sql}")).unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };

    // The second spelling is not its own name: it comes back suffixed.
    let plain = describe(&format!("SELECT * FROM {}", hot_reader(&file)));
    assert_eq!(
        plain,
        vec![
            ("duration".to_owned(), "BIGINT".to_owned()),
            ("x".to_owned(), "BIGINT".to_owned()),
            ("Duration_1".to_owned(), "VARCHAR".to_owned()),
        ],
        "expected read_json to rename the case-collided key"
    );

    // Naming the pinned spelling rewrites the OTHER service's column, and
    // leaves the pinned field's real values (`Duration_1`) alone.
    let replaced = describe(&format!(
        "SELECT * REPLACE (json_extract_string(to_json(\"Duration\"), '$') AS \"Duration\") \
         FROM {}",
        hot_reader(&file)
    ));
    assert_eq!(
        replaced,
        vec![
            ("Duration".to_owned(), "VARCHAR".to_owned()),
            ("x".to_owned(), "BIGINT".to_owned()),
            ("Duration_1".to_owned(), "VARCHAR".to_owned()),
        ],
        "expected the REPLACE to bind (and retype) the first column, not the \
         pinned spelling's own column"
    );
}

/// Two engine facts behind folding every producer's field names at the
/// one ingest door (`envelope::canonicalize`, ADR-0009):
///
/// 1. an UNCONFORMED hot column can throw the WHOLE composite source — the
///    union binds types for every column, so a query whose DSL never names
///    the field fails too; and
/// 2. `UNION ALL BY NAME` matches column names case-INSENSITIVELY, and a
///    `VARCHAR` hot column unions with every cold scalar type.
///
/// Together: two spellings of one identifier in a hot snapshot leave an
/// unnameable `_1` twin that can fail every query, which is why field
/// names are folded to one spelling BEFORE they can reach the buffer —
/// and why a `VARCHAR` conform under the surviving spelling is safe.
#[test]
fn unconformed_hot_column_throws_the_union_and_varchar_conform_saves_it() {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    let cold = dir.path().join("cold.parquet");
    let mut f = std::fs::File::create(&hot).unwrap();
    // Mixed values -> JSON, under the OTHER spelling of every cold column.
    writeln!(
        f,
        r#"{{"service":"svc","Duration":"1.5s","Started":1700000000}}"#
    )
    .unwrap();
    writeln!(f, r#"{{"service":"svc","Duration":42,"Started":"x"}}"#).unwrap();
    f.sync_all().unwrap();

    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT 'svc' AS service, '1.5s' AS duration, \
                      CAST('2024-01-15 09:00:00' AS TIMESTAMP) AS started) \
         TO '{}' (FORMAT PARQUET)",
        cold.display()
    ))
    .unwrap();

    let union = |replace: &str| {
        format!(
            "SELECT * FROM read_parquet('{}') UNION ALL BY NAME SELECT * {replace} FROM {}",
            cold.display(),
            hot_reader(&hot)
        )
    };
    let count = |sql: &str| -> Result<usize, duckdb::Error> {
        // The shape every trawl query has: a filter naming one field, `*` for
        // the projection. Nothing here mentions the collided field — the
        // union binds it anyway.
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM ({sql}) WHERE service = 'svc' LIMIT 5"
        ))?;
        let mut rows = stmt.query([])?;
        let mut n = 0;
        while rows.next()?.is_some() {
            n += 1;
        }
        Ok(n)
    };

    // Unconformed: cold VARCHAR x hot JSON is irreconcilable, and it takes
    // the unrelated query down with it.
    let err = count(&union("")).expect_err("an unconformed hot column must throw the union");
    assert!(
        err.to_string().contains("Malformed JSON"),
        "expected the JSON-vs-VARCHAR union conflict, got: {err}"
    );

    // Conformed to VARCHAR under the hot spelling: BY NAME folds the case,
    // so both columns line up and every pair promotes instead of throwing.
    let replace = "REPLACE (json_extract_string(to_json(\"Duration\"), '$') AS \"Duration\", \
                   json_extract_string(to_json(\"Started\"), '$') AS \"Started\")";
    assert_eq!(
        count(&union(replace)).expect("a VARCHAR hot column must union with any cold type"),
        3,
        "the cold row and both hot rows must survive one execution"
    );
    assert_eq!(
        {
            let mut stmt = conn
                .prepare(&format!("DESCRIBE {}", union(replace)))
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
        },
        vec![
            ("service".to_owned(), "VARCHAR".to_owned()),
            ("duration".to_owned(), "VARCHAR".to_owned()),
            ("started".to_owned(), "VARCHAR".to_owned()),
        ],
        "BY NAME must fold ASCII case (no separate `Duration` column) and \
         promote the cold TIMESTAMP to VARCHAR rather than throwing"
    );
}

// ── pin-aware comparison rules, execution-evidenced (ADR-0011) ──
//
// One probe per emission rule, against real VARCHAR and BIGINT columns.
// The pin-blind shapes are probed beside them, so a DuckDB bump that
// alters the implicit-cast outcome surfaces here, not in production.

/// A VARCHAR column seeded with mixed numeric-looking and word values —
/// the shape a VARCHAR pin guarantees on disk.
fn varchar_status_conn() -> duckdb::Connection {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t AS SELECT unnest(['200', '404', '500', 'accepted', '1.5']) AS v",
    )
    .unwrap();
    conn
}

fn count(conn: &duckdb::Connection, sql: &str) -> Result<i64, duckdb::Error> {
    conn.query_row(sql, [], |row| row.get(0))
}

/// Why a VARCHAR pin needs its own rule: pin-blind emission binds an
/// INTEGER parameter against the VARCHAR column, and the outcome is an
/// error either way — `=`/IN make `DuckDB` cast the COLUMN to INT64 and
/// the first word value throws a Conversion error; ordered comparisons
/// refuse to bind at all (Binder: "Cannot compare values of type VARCHAR
/// and type BIGINT"). Pinned here with bound parameters (exactly what the
/// emitter produces) so a `DuckDB` bump that changes the implicit-cast
/// outcome surfaces.
#[test]
fn pin_blind_int_comparison_on_varchar_column_errors() {
    let conn = varchar_status_conn();
    let eq: Result<i64, _> = conn.query_row(
        "SELECT count(*)::BIGINT FROM t WHERE v = ?",
        [200i64],
        |row| row.get(0),
    );
    let err = eq.expect_err("Int param equality over 'accepted' must throw");
    assert!(
        trawl_engine::executor::is_conversion_error(&err),
        "= binds by casting the column: expected Conversion class, got {err}"
    );

    let in_list: Result<i64, _> = conn.query_row(
        "SELECT count(*)::BIGINT FROM t WHERE v IN (?, ?)",
        [200i64, 301i64],
        |row| row.get(0),
    );
    let err = in_list.expect_err("Int param IN over 'accepted' must throw");
    assert!(trawl_engine::executor::is_conversion_error(&err), "{err}");

    let ordered: Result<i64, _> = conn.query_row(
        "SELECT count(*)::BIGINT FROM t WHERE v >= ?",
        [400i64],
        |row| row.get(0),
    );
    let err = ordered.expect_err("Int param ordered comparison must refuse to bind");
    assert!(
        err.to_string().contains("Binder Error"),
        ">= refuses VARCHAR-vs-BIGINT outright: {err}"
    );
}

/// Rule: VARCHAR pin + `=`/`!=`/IN binds text — matches exactly the
/// stored string, never errors, and does not equate numeric variants
/// ('200' != '200.0').
#[test]
fn varchar_text_equality_matches_exact_string_only() {
    let conn = varchar_status_conn();
    assert_eq!(
        count(&conn, "SELECT count(*)::BIGINT FROM t WHERE v = '200'").unwrap(),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE v IN ('200', '301', 'accepted')"
        )
        .unwrap(),
        2
    );
    // != with the OR-IS-NULL policy over a column with no NULLs: everything else.
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE (v != '200' OR v IS NULL)"
        )
        .unwrap(),
        4
    );
    // Text equality is exact: no numeric equivalence.
    assert_eq!(
        count(&conn, "SELECT count(*)::BIGINT FROM t WHERE v = '200.0'").unwrap(),
        0
    );
}

/// Rule: VARCHAR pin + ordered numeric literal → both sides read through
/// [`decimal_reading`]. Numeric-looking strings order numerically,
/// values outside the space are NULL (excluded), and nothing throws.
///
/// The comparison is built from the shared expression rather than a
/// hand-written cast, so a change to the space fails here instead of
/// silently leaving this probe testing an expression nothing emits.
#[test]
fn varchar_ordered_rung_compares_in_the_decimal_space() {
    let conn = varchar_status_conn();
    let ordered = |op: &str, literal: &str| {
        count(
            &conn,
            &format!(
                "SELECT count(*)::BIGINT FROM t WHERE {} {op} {}",
                decimal_reading("v"),
                decimal_reading(&format!("'{literal}'"))
            ),
        )
        .unwrap()
    };
    assert_eq!(
        ordered(">=", "400"),
        2, // '404', '500'; 'accepted' NULLs out, '200'/'1.5' below
    );
    assert_eq!(
        ordered(">", "1"),
        4, // '1.5', '200', '404', '500' — the fraction survives the space
    );
    // The reading of 'accepted' is NULL: excluded from BOTH sides of the
    // comparison, never an error.
    assert_eq!(ordered("<", "1000"), 4);
    // A LITERAL the space cannot read needs no special case: its own cast
    // is NULL, so every row is UNKNOWN and none match.
    for literal in ["nan", "inf", "1e40"] {
        assert_eq!(ordered(">", literal), 0, "{literal}");
        assert_eq!(ordered("<", literal), 0, "{literal}");
    }
}

/// The DOUBLE cast's DOMAIN, run on both engines side by side:
/// `TRY_CAST(v AS DOUBLE)` in `DuckDB` against `compare::try_cast_double`
/// in the live matcher — the reading behind the DOUBLE pin's pattern text
/// and the `tonumber()` scalar. `str::parse::<f64>` is NOT that domain —
/// `DuckDB` trims ASCII whitespace and honours `_` digit separators — and
/// every disagreement costs the stream a row the batch query returns.
#[test]
fn try_cast_double_domain_matches_the_live_mirror() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let inputs = [
        // Whitespace: trimmed both ends, ASCII only (\x0b included, which
        // Rust's is_ascii_whitespace excludes; U+00A0 is not whitespace).
        " 200",
        "200 ",
        "\t200\n",
        "\r200\r",
        "\x0b200\x0c",
        "  200  ",
        "\u{a0}200",
        "\u{2000}200",
        "2 00",
        " ",
        "",
        // `_` digit separators: only between ASCII digits.
        "200_000",
        "1_000.5",
        "1_0",
        "-1_0",
        "+1_0",
        "1.0_0",
        "1e1_0",
        " 1_0 ",
        "1_0.0_1e1_0",
        "_200",
        "200_",
        "1__0",
        "1._5",
        "1.5_",
        "1_.5",
        "1_e3",
        "1e_3",
        "_",
        // Shapes the two engines already agree on.
        "200",
        "1.5",
        "-0.5",
        "+5",
        "1.",
        ".5",
        "-.5",
        "1e3",
        "1E3",
        "1e+3",
        "1e-3",
        "00200",
        "1e400",
        "-1e400",
        "1e-400",
        "nan",
        "-NAN",
        "+nan",
        "inf",
        "-inf",
        "infinity",
        "0x10",
        "0b101",
        "1,000",
        "1d",
        "true",
        "1.5e2.5",
        "1-",
        "accepted",
    ];
    for input in inputs {
        let sql: Option<f64> = conn
            .query_row("SELECT TRY_CAST(? AS DOUBLE)", [input], |row| row.get(0))
            .unwrap();
        let live = trawl_core::compare::try_cast_double(input);
        // NaN != NaN, so compare bit patterns, not values.
        assert_eq!(
            sql.map(f64::to_bits).is_some(),
            live.map(f64::to_bits).is_some(),
            "cast domain disagrees for {input:?}: sql={sql:?} live={live:?}"
        );
        if let (Some(sql), Some(live)) = (sql, live) {
            assert!(
                sql.to_bits() == live.to_bits() || (sql.is_nan() && live.is_nan()),
                "cast value disagrees for {input:?}: sql={sql:?} live={live:?}"
            );
        }
    }
}

/// The texts [`decimal_comparison_space_domain_matches_the_live_mirror`]
/// runs through both engines — hoisted so the matrix can grow without the
/// test body growing with it.
const DECIMAL_DOMAIN_INPUTS: &[&str] = &[
    // Whitespace: trimmed both ends, ASCII only.
    " 200",
    "200 ",
    "\t200\n",
    "\x0b200\x0c",
    "  200  ",
    "\u{a0}200",
    "2 00",
    " ",
    "",
    // `_` digit separators, only between ASCII digits.
    "200_000",
    "1_000.5",
    "1e1_0",
    "_200",
    "200_",
    "1__0",
    "1_.5",
    // Spelling drift that denotes the same number.
    "200",
    "200.0",
    "200.000000",
    "0404",
    "+5",
    "1.",
    ".5",
    "-0",
    "-0.0",
    "1e3",
    "1E3",
    "1e-3",
    "00200",
    // Exact where a DOUBLE reading collapses.
    "1737000000123456788",
    "1737000000123456789",
    "1737000000123456790",
    "9007199254740992",
    "9007199254740993",
    "9223372036854775807",
    "-9223372036854775808",
    // The scale boundary: rounded half away from zero at 10^-6.
    "0.0000001",
    "0.00000049",
    "0.0000005",
    "0.0000015",
    "-0.0000005",
    "4.0000001",
    "4.0000005",
    // The magnitude boundary: below 10^32 reads, at it does not.
    "1e31",
    "99999999999999999999999999999999.999999",
    "-99999999999999999999999999999999.999999",
    "1e32",
    "1e40",
    "1e400",
    "1e-400",
    // No reading at all — the DOUBLE domain read the first three.
    "nan",
    "NaN",
    "inf",
    "-inf",
    "infinity",
    "0x10",
    "0b101",
    "1,000",
    "accepted",
    "1d",
    "true",
    // A scan whitespace cut short is FORGIVEN in three states and no
    // others, and a `.` right after exponent digits terminates the
    // same way. `'- '` is a realistic missing-value token from a
    // fixed-width log format, and a mirror that refuses it reports
    // `status!=0` as a live match on a row the batch query drops.
    "-",
    "- ",
    "-\t",
    "- x",
    "+ ",
    "+\n",
    "1e",
    "1e ",
    "1e\t",
    "1E ",
    "1e+ ",
    "1e- ",
    "1e+x",
    "1.e",
    "1.e ",
    "1e0.",
    "1e0. ",
    "1e0.5",
    "1e0..",
    "1e0.0",
    "1e5.",
    "1e-5.",
    "1e-.",
    "1e-. ",
    "1.5.",
    "1..",
    ".",
    ". ",
    "-.",
    "-. ",
    "e ",
    "-e ",
    "-- ",
    "-+ ",
    "1_ ",
    "1e0 5",
    // A negative exponent whose shift drops every mantissa digit
    // rounds on the LEADING significant digit; the same value spelled
    // without an exponent does not, and neither does a positive one.
    // The discriminator is the SPELLING, not the value.
    "5e-7",
    "5e-8",
    "5e-9",
    "5e-30",
    "4e-8",
    "6e-8",
    "1e-8",
    "45e-9",
    "54e-9",
    "50e-9",
    "500e-10",
    "1.5e-8",
    "5.5e-8",
    "0.5e-8",
    "0.05e-7",
    "0.00000005",
    "0.000000005",
    "0.00000000000000000005",
    "0.0000005e-1",
    "0.00000005e-1",
    "0.00000005e+1",
    "0.000000005e+1",
    "0.00000000005e2",
    // …and one whose shift lands INSIDE the mantissa, where the
    // boundary digit decides as everywhere else.
    "15e-7",
    "14e-7",
    "1.5e-6",
    "1.4e-6",
    "1000005e-8",
    "1000005e-9",
    "1000005e-13",
    "123456789.987654321e-3",
    "0.9999995e-1",
    "9999995e-7",
    // The exponent CEILING: the type's integer digits raised by the
    // mantissa's own excessive decimals, refusing magnitudes the type
    // would otherwise hold.
    "0.5e32",
    "0.05e32",
    "0.01e32",
    "0.01e33",
    "0.001e33",
    "0.001e34",
    "0.000000005e35",
    "0.000000005e36",
    "0.000000005e39",
    "0.000000005e40",
    "0e40",
    "0.1e33",
    "5e31",
    "9.9e31",
];

/// The comparison space's DOMAIN, run on both engines side by side:
/// [`decimal_reading`] in `DuckDB` against `compare::decimal_micros` in
/// the live matcher. It is `DuckDB`'s cast domain, not Rust's number
/// parser — whitespace is trimmed, `_` separates digits, `'0404'` is 404 —
/// and every disagreement is a row the stream and the batch query answer
/// differently.
///
/// The engine side is read back as TEXT: `DECIMAL(38,6)` renders with
/// exactly six fractional digits, which is the scaled integer the mirror
/// carries with the point put back.
#[test]
fn decimal_comparison_space_domain_matches_the_live_mirror() {
    let conn = conn();
    for input in DECIMAL_DOMAIN_INPUTS.iter().copied() {
        let sql: Option<String> = conn
            .query_row(
                &format!("SELECT CAST({} AS VARCHAR)", decimal_reading("?")),
                [input],
                |row| row.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::decimal_micros(input).map(render_micros);
        assert_eq!(
            sql, live,
            "comparison space disagrees for {input:?}: sql={sql:?} live={live:?}"
        );
    }
}

/// `DECIMAL(38,6)`'s own rendering, rebuilt from the scaled integer the
/// live mirror carries — six fractional digits, sign on the whole value.
fn render_micros(micros: i128) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let magnitude = micros.unsigned_abs();
    format!(
        "{sign}{}.{:06}",
        magnitude / 1_000_000,
        magnitude % 1_000_000
    )
}

/// The emitter binds the query literal as a STRING and casts it with the
/// same expression it casts the column with, so this pins what a bound
/// parameter does inside that cast: `DuckDB` keeps it VARCHAR and the
/// `TRY_CAST` degrades to NULL. It must not resolve the parameter's type
/// from the cast target — that would make an unreadable literal a
/// conversion ERROR at execution time instead of an UNKNOWN row, turning
/// `status=nan` from "matches the text `nan`" into a failed query.
#[test]
fn a_bound_literal_casts_as_text_and_nulls_instead_of_throwing() {
    let conn = conn();
    for (literal, expected) in [
        ("200", Some("200.000000".to_owned())),
        (
            "1737000000123456789",
            Some("1737000000123456789.000000".to_owned()),
        ),
        ("nan", None),
        ("1e40", None),
        ("accepted", None),
    ] {
        let sql: Option<String> = conn
            .query_row(
                &format!("SELECT CAST({} AS VARCHAR)", decimal_reading("?")),
                [literal],
                |row| row.get(0),
            )
            .expect("a bound literal must never throw inside the cast");
        assert_eq!(sql, expected, "{literal:?}");
    }
}

/// The VARCHAR-pinned equality and `!=` shapes the emitter builds, run
/// over ids that differ by one.
///
/// In DOUBLE space every id above 2^53 collapses onto its neighbours, so
/// `id=1737000000123456789` returns three distinct stored ids and
/// `id!=9007199254740993` silently suppresses `9007199254740992`. Both
/// engines collapse identically, so parity testing cannot see it — only
/// execution against stored data can. The DOUBLE half stays as the
/// premise: it is what makes the DECIMAL assertions mean something.
#[test]
fn the_decimal_comparison_space_separates_ids_a_double_equates() {
    let conn = conn();
    conn.execute_batch(
        "CREATE TABLE t AS SELECT unnest([\
         '1737000000123456788', '1737000000123456789', '1737000000123456790', \
         '9007199254740992', '9007199254740993', '200', '200.0', 'accepted']) AS v",
    )
    .unwrap();

    // The two shapes the emitter builds, with the numeric arm in each
    // space. `!=` carries its OR-IS-NULL policy and is asked which rows it
    // EXCLUDES, because a suppressed row is invisible rather than wrong.
    let equality = |space: &dyn Fn(&str) -> String, literal: &str| {
        format!(
            "(v = '{literal}' OR COALESCE({} = {}, FALSE))",
            space("v"),
            space(&format!("'{literal}'"))
        )
    };
    let excluded_by_inequality = |space: &dyn Fn(&str) -> String, literal: &str| {
        format!(
            "NOT ((v != '{literal}' AND COALESCE({} != {}, TRUE)) OR v IS NULL)",
            space("v"),
            space(&format!("'{literal}'"))
        )
    };
    let rows = |predicate: &str| -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("SELECT v FROM t WHERE {predicate} ORDER BY v"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let double = |expr: &str| format!("TRY_CAST({expr} AS DOUBLE)");

    assert_eq!(
        rows(&equality(&double, "1737000000123456789")).len(),
        3,
        "the premise: DOUBLE space equates the two neighbours"
    );
    assert_eq!(
        rows(&equality(&decimal_reading, "1737000000123456789")),
        ["1737000000123456789"],
        "the comparison space must match exactly the stored id"
    );
    // The rule the numeric arm exists for still holds: one number, two
    // spellings, both matched.
    assert_eq!(
        rows(&equality(&decimal_reading, "200")),
        ["200", "200.0"],
        "a number's other spelling must still meet its literal"
    );

    assert_eq!(
        rows(&excluded_by_inequality(&double, "9007199254740993")),
        ["9007199254740992", "9007199254740993"],
        "the premise: DOUBLE space suppresses a genuinely different id"
    );
    assert_eq!(
        rows(&excluded_by_inequality(
            &decimal_reading,
            "9007199254740993"
        )),
        ["9007199254740993"],
        "the comparison space must exclude exactly the named id"
    );
}

/// Why the comparison space is never a per-literal BIGINT domain:
/// `TRY_CAST('1.5' AS BIGINT)` ROUNDS to 2, so an integer space would make
/// `dur>1` and `dur>1.5` disagree about the same stored value.
/// `DECIMAL(38,6)` keeps 1.5 as 1.5 — and unlike DOUBLE it also keeps
/// every `i64` distinct. (The rounding premise itself is also pinned by
/// the conform-guard probes above.)
#[test]
fn try_cast_bigint_rounds_where_the_comparison_space_preserves() {
    let conn = conn();
    let (as_bigint, as_decimal): (i64, String) = conn
        .query_row(
            &format!(
                "SELECT TRY_CAST('1.5' AS BIGINT), CAST({} AS VARCHAR)",
                decimal_reading("'1.5'")
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(as_bigint, 2, "TRY_CAST to BIGINT rounds");
    assert_eq!(as_decimal, "1.500000", "the comparison space preserves");
}

/// GLOB and `regexp_matches` both refuse a numeric column outright
/// (Binder: no `~~~(INTEGER, UNKNOWN)` / `regexp_matches(INTEGER,
/// UNKNOWN)` overload), which is why the pattern rules cast a typed pin
/// to text rather than matching the column directly.
#[test]
fn pin_blind_patterns_on_bigint_column_refuse_to_bind() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t AS SELECT unnest([200, 404, 500]) AS v")
        .unwrap();
    for sql in [
        "SELECT count(*)::BIGINT FROM t WHERE v GLOB ?",
        "SELECT count(*)::BIGINT FROM t WHERE regexp_matches(v, ?)",
    ] {
        let outcome: Result<i64, _> = conn.query_row(sql, ["4*"], |row| row.get(0));
        let err = outcome.expect_err("patterns must not bind against BIGINT");
        assert!(err.to_string().contains("Binder Error"), "{sql}: {err}");
    }
}

/// Rule: typed pins glob/regex via `CAST(col AS VARCHAR)` — both match
/// the text form of BIGINT values, explicitly.
#[test]
fn cast_text_patterns_match_bigint_text_form() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t AS SELECT unnest([200, 404, 500]) AS v")
        .unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE CAST(v AS VARCHAR) GLOB '4*'"
        )
        .unwrap(),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE regexp_matches(CAST(v AS VARCHAR), '^[45]0[04]$')"
        )
        .unwrap(),
        2, // 404 and 500; 200 starts with neither 4 nor 5
    );
}

/// The wire texts [`bigint_pattern_text_is_the_cast_reading_on_both_engines`]
/// runs through both engines — hoisted so the matrix can grow without the
/// test body growing with it.
const BIGINT_PATTERN_INPUTS: &[&str] = &[
    // Plain integers, incl. the leading-zero case the wire text and the
    // stored value spell differently.
    "404",
    "0404",
    "00200",
    "+5",
    "-0",
    "9223372036854775807",
    "-9223372036854775808",
    // Whitespace and `_` separators, as for the DOUBLE domain.
    " 404 ",
    "\t404\n",
    "404_000",
    "1_0",
    "1_0.5",
    "1e1_0",
    // Fractional / exponent texts ROUND, half AWAY FROM ZERO.
    "1.5",
    "1.4",
    "2.5",
    "3.5",
    "-1.5",
    "-2.5",
    "0.5",
    "-0.5",
    ".5",
    "1.",
    "0.0",
    "1e3",
    "1.9e2",
    "1e18",
    "1.5e18",
    "1.0000000000000001",
    // Radix prefixes bind on the raw text: no sign, no whitespace.
    "0x10",
    "0X10",
    "0b101",
    "0B101",
    "-0x10",
    " 0x10 ",
    "0xzz",
    "0x+10",
    "0x1.5",
    "0o17",
    "010",
    // No reading at all.
    "accepted",
    "",
    "true",
    "nan",
    "inf",
    "1,000",
    "4 04",
    "\u{a0}404",
    "1e",
    "e1",
    "-",
    // Out of BIGINT range: NULL, never a saturated approximation.
    "9223372036854775808",
    "-9223372036854775809",
    "1e19",
    "1e400",
    // Integral values above 2^53 written as a fraction or an
    // exponent. Read through `f64` they answer nothing where both
    // batch lanes hold the integer — including the value the pin
    // ladder's own text-first test blesses
    // (`compaction::pin_ladder_beyond_2_pow_53_*`).
    "1.7356896001234568e+18",
    "1735689600123456800.0",
    "9007199254740993.0",
    "9007199254740993",
    "9007199254740992.0",
    "9223372036854775807.0",
    "9223372036854775808.0",
    "-9223372036854775808.0",
    "1735689600123456710.7",
    // …and the lax terminators, which reach the BIGINT reading
    // through its DECIMAL guard.
    "- ",
    "1e ",
    "1e0.",
    "5e-8",
];

/// The BIGINT pin's pattern text is the CONFORMED integer's rendering, and
/// the live mirror (`compare::conformed_bigint`) must read the same value
/// out of the same wire text — stringifying the wire text instead answers
/// `status=0*` TRUE for a stored 404 and `status=a*` TRUE for a value the
/// column stores as NULL.
///
/// Both sides are the GUARDED reading, not the bare cast: the value a
/// pinned column holds is what the guard admitted, so a text the cast
/// would round (`'1.5'`) or respell (`'0x10'`) has no pattern text at all.
#[test]
fn bigint_pattern_text_is_the_cast_reading_on_both_engines() {
    let conn = conn();
    let guard = conform("v", CanonicalType::BigInt);
    for input in BIGINT_PATTERN_INPUTS.iter().copied() {
        let sql: Option<String> = conn
            .query_row(
                &format!("SELECT CAST({guard} AS VARCHAR) FROM (SELECT ? AS v) t"),
                [input],
                |r| r.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::conformed_bigint(input).map(|i| i.to_string());
        assert_eq!(live, sql, "BIGINT pattern text disagrees for {input:?}");
    }

    // The guard's residuals, on both engines: a text the cast rounds has
    // no reading, and neither does one the DECIMAL space cannot re-read.
    for input in ["9223372036854775807.4", "1.5", "0x10"] {
        let sql: Option<String> = conn
            .query_row(
                &format!("SELECT CAST({guard} AS VARCHAR) FROM (SELECT ? AS v) t"),
                [input],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sql, None, "{input:?} must not conform");
        assert_eq!(trawl_core::compare::conformed_bigint(input), None);
    }

    // The whole point: a stored 404 renders `404`, so the wire text's own
    // leading zero matches on neither side.
    let matched: bool = conn
        .query_row(
            &format!("SELECT CAST({guard} AS VARCHAR) GLOB '0*' FROM (SELECT '0404' AS v) t"),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!matched, "the stored BIGINT is 404, so `0*` misses");
}

/// The BOOLEAN pin's pattern text is `true`/`false` and nothing else: the
/// CAST takes a wide case-insensitive vocabulary, and the round-trip guard
/// keeps only the two spellings `DuckDB` writes back. A matcher reading
/// the CAST instead of the CONFORM would answer `flag=/^true$/` TRUE for a
/// wire `"TRUE"`/`"yes"`/`1` the column stores as NULL.
#[test]
fn boolean_pattern_text_is_the_cast_reading_on_both_engines() {
    let conn = conn();
    let guard = conform("v", CanonicalType::Boolean);
    let texts = [
        "true",
        "TRUE",
        "True",
        "tRuE",
        "t",
        "T",
        "yes",
        "yEs",
        "Y",
        "y",
        "1",
        "false",
        "FALSE",
        "f",
        "F",
        "no",
        "nO",
        "N",
        "n",
        "0", // Outside the vocabulary: NULL.
        "on",
        "off",
        "",
        " true ",
        "\ttrue\n",
        "accepted",
        "2",
        "-1",
        "1.0",
        "01",
        "+1",
        "true1",
        "\u{a0}true",
    ];
    for input in texts {
        let sql: Option<String> = conn
            .query_row(
                &format!("SELECT CAST({guard} AS VARCHAR) FROM (SELECT ? AS v) t"),
                [input],
                |r| r.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::conformed_boolean(input).map(|b| b.to_string());
        assert_eq!(live, sql, "BOOLEAN pattern text disagrees for {input:?}");
    }

    // The premise the guard exists for: the CAST reads far more than the
    // conform keeps, so reading the cast would invent values the corpus
    // does not hold.
    let cast_reads: i64 = conn
        .query_row(
            "SELECT count(TRY_CAST(v AS BOOLEAN))::BIGINT \
             FROM (VALUES ('TRUE'), ('t'), ('yes'), ('1'), ('no')) t(v)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cast_reads, 5, "premise: the CAST's vocabulary is wide");

    // A numeric wire value has NO boolean reading once the guard applies:
    // the cast answers `true` for `'200'` in a JSON-inferred column, and
    // `'true'` is not the text `'200'`.
    let numeric: Option<String> = conn
        .query_row(
            &format!("SELECT CAST({guard} AS VARCHAR) FROM (SELECT '200' AS v) t"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(numeric, None, "a numeric text must not conform to BOOLEAN");
}

/// The DOUBLE pin's pattern text is `DuckDB`'s DOUBLE rendering, and the
/// live mirror (`compare::canonical_double_text`) must produce the same
/// string for the same stored value — the wire number's own
/// stringification does NOT (`200` vs `200.0`, `1e-7` vs `1e-07`,
/// `123456789012345680` vs `1.2345678901234568e+17`), so a matcher that
/// stringified the wire value would answer `dur=/^200$/` TRUE where the
/// batch query answers FALSE.
#[test]
fn double_pattern_text_is_duckdb_rendering_on_both_engines() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    // Values are BOUND, never spelled as SQL literals: `-0.0` written as a
    // literal is constant-folded to positive zero before anything renders
    // it, which would hide the fact that a stored `-0.0` keeps its sign.
    let inputs = [
        200.0,
        0.0,
        -0.0,
        -3.0,
        1.5,
        1e-7,
        1e16,
        1.234_567_890_123_456_8e17,
        1e100,
        1e-300,
        0.0001,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        // A NaN renders with its SIGN, and a DOUBLE-pinned column can
        // hold either spelling (`a_rendered_nan_keeps_its_sign`), so the
        // pattern text has to carry the sign too.
        -f64::NAN,
    ];
    for input in inputs {
        let sql: String = conn
            .query_row(
                "SELECT CAST(CAST(? AS DOUBLE) AS VARCHAR)",
                [input],
                |row| row.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::canonical_double_text(input);
        assert_eq!(live, sql, "canonical pattern text disagrees for {input}");
    }

    // The whole point: an anchored pattern means the same thing on both
    // sides of the same value — and `^200$` matches NEITHER, because the
    // stored double renders `200.0`.
    let matched: bool = conn
        .query_row(
            "SELECT regexp_matches(CAST(CAST(200 AS DOUBLE) AS VARCHAR), '^200$')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!matched, "a DOUBLE 200 renders 200.0, so ^200$ misses");
    assert_eq!(trawl_core::compare::canonical_double_text(200.0), "200.0");
}

/// Why a TIMESTAMP pin needs its own pattern text: `DuckDB`'s plain CAST
/// rendering is space-separated, zoneless and fraction-trimmed — a form
/// no live event carries. Globbing that would make `_time=/T09:/` match
/// live and miss in batch, on every install (`_time` is seeded TIMESTAMP).
#[test]
fn timestamp_cast_text_form_is_space_separated() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let rendered: String = conn
        .query_row(
            "SELECT CAST(CAST('2026-01-15T09:00:00Z' AS TIMESTAMP) AS VARCHAR)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rendered, "2026-01-15 09:00:00");
}

/// Every text shape whose TIMESTAMP reading the two engines must agree
/// on — the matrix the module doc's contract refers to.
///
/// Grouped by the rule each row exercises, and deliberately including the
/// shapes nobody would write on purpose: `epoch`, a trailing ` UTC`,
/// hour-24 rollover and `T09:00+00:00` were all unpredicted divergences.
/// Add rather than replace.
const TIMESTAMP_TEXT_MATRIX: &[&str] = &[
    // The wire form ingest canonicalizes _time into, and its variants.
    "2026-01-15T09:00:00.000000Z",
    "2026-01-15T09:00:00Z",
    "2026-01-15 09:00:00",
    "2026-01-15T09:00",
    "  2026-01-15 09:00:00  ",
    "\t2026-01-15T09:00:00Z\n",
    "2026-01-15T09:00:00Z   ",
    "2026-01-15  09:00:00",
    "2026-01-15T 09:00:00",
    "2026-01-15\t09:00:00",
    "2026-01-15\n09:00:00",
    "2026-01-15 T09:00:00",
    // Offsets, in every spelling and both directions.
    "2026-01-15T09:00:00+05:30",
    "2026-01-15T09:00:00-08:00",
    "2026-01-15T09:00:00+02",
    "2026-01-15T09:00:00-02",
    "2026-01-15T09:00:00+0530",
    "2026-01-15T09:00:00-0800",
    "2026-01-15T09:00:00-0000",
    "2026-01-15T09:00:00-00:00",
    "2026-01-15T09:00:00+00",
    "2026-01-15T09:00:00+05:30:15",
    "2026-01-15T09:00:00-00:00:01",
    "2026-01-15T09:00:00+99:99",
    "2026-01-15T09:00:00+24:00",
    "2026-01-15T09:00:00-99:00",
    "2026-01-15T09:00:00+14:00",
    "2026-01-15T09:00:00-12:00",
    "2026-01-15T09:00:00+05:30 ",
    "2026-01-15T09:00:00.123456+05:30",
    "2026-01-15T09:00:00.123456+0530",
    "2026-01-15 9:0:0.5+05:30",
    "2026-01-15T09:00:00.-08:00",
    // Malformed offsets: components are exactly two digits, `Z` is
    // uppercase, nothing follows the zone.
    "2026-01-15t09:00:00z",
    "2026-01-15T09:00:00z",
    "2026-01-15T09:00:00ZZ",
    "2026-01-15T09:00:00 +05:30",
    "2026-01-15T09:00:00+5:30",
    "2026-01-15T09:00:00+5",
    "2026-01-15T09:00:00+053",
    "2026-01-15T09:00:00+05:3",
    "2026-01-15T09:00:00+053015",
    "2026-01-15T09:00:00+0530:15",
    "2026-01-15T09:00:00+100:00",
    "2026-01-15T09:00:00+005:30",
    "2026-01-15T09:00:00+999",
    "2026-01-15T09:00:00+05:30:1",
    "2026-01-15T09:00:00+05:30:155",
    "2026-01-15T09:00:00+",
    "2026-01-15T09:00:00-",
    "2026-01-15T09:00:00.123-",
    "2026-01-15T09:00:00,123Z",
    // Fractional seconds: padded, truncated, empty, absurd.
    "2026-01-15T09:00:00.123Z",
    "2026-01-15T09:00:00.1234567",
    "2026-01-15T09:00:00.0000000000Z",
    "2026-01-15T09:00:00.123456789Z",
    "2026-01-15T09:00:00.9999999Z",
    "2026-01-15T09:00:00.000000999Z",
    "2026-01-15T09:00:00.1234567890123456789012345Z",
    "2026-01-15T09:00:00.5Z",
    "2026-01-15T09:00:00.0Z",
    "2026-01-15T09:00:00.Z",
    "2026-01-15T09:00:00.",
    // Dates: slash form, unpadded and over-padded components, junk.
    "2026-01-15",
    "  2026-01-15",
    "2026/01/15",
    "2026/01/15 09:00:00",
    "2026/01/15T09:00:00",
    "2026-1-5",
    "2026-1-5 9:0:0",
    "02026-01-15",
    "2026-01-15T009:00:00Z",
    "2026-01-15T0009:00:00Z",
    "2026-011-15",
    "2026-01-015",
    "2026-01/15",
    "2026/01-15",
    "2026.01.15",
    "20260115",
    "2026-02-30",
    "2026-13-01",
    "+2026-01-15",
    "2026-01-15T09:000:00Z",
    "2026-01-15T09:00:000Z",
    // A date/time separator commits the text to carrying a time.
    "2026-01-15T",
    "2026-01-15 ",
    "2026-01-15\t",
    "2026-01-15\n",
    "2026-01-15T09",
    "2026-01-15 09",
    "2026-01-15Z",
    "2026-01-15+05:30",
    "2026-01-15TT09:00:00",
    // A seconds-less time must end the text: the false-positive
    // direction, where a lax mirror fires and batch NULLs.
    "2026-01-15T09:00Z",
    "2026-01-15T09:00+05:30",
    "2026-01-15T09:00 UTC",
    "2026-01-15T09:00 ",
    "2026-01-15T09:00\t",
    "2026-01-15T09:00.5",
    "2026-01-15T09:00.",
    "2026-01-15 9:0Z",
    "2026-01-15T24:00Z",
    "2026-01-15T24:00 ",
    // Hour 24 rolls the date over, and only from an exact midnight.
    "2026-01-15 24:00:00",
    "2026-01-15T24:00:00",
    "2026-01-15T24:00:00Z",
    "2026-01-15 24:00",
    "2026-01-15 24:00:00.000000",
    "2026-01-15 24:00:00 UTC",
    "2026-01-15 24:00:00+05:30",
    "2026-12-31 24:00:00",
    "2026-01-15 24:00:01",
    "2026-01-15 24:01:00",
    "2026-01-15T24:00:00.000001",
    "2026-01-15 25:00:00",
    "2026-01-15 23:59:60",
    "2026-01-15T09:60:00Z",
    "2026-01-15T09:00:60Z",
    // Keyword instants — including the two that render as words.
    "epoch",
    "EpOcH",
    " epoch ",
    "epoch+1",
    "infinity",
    "INFINITY",
    "Infinity",
    "inf",
    "INF",
    " infinity ",
    "-infinity",
    "-inf",
    "+infinity",
    "now",
    "today",
    "tomorrow",
    "yesterday",
    // Zone NAMES that mean UTC for all time, and the near-misses.
    "2026-01-15 09:00:00 UTC",
    "2026-01-15T09:00:00 UTC",
    "2026-01-15 09:00:00 uTc",
    "2026-01-15 09:00:00 GMT",
    "2026-01-15 09:00:00 gmt",
    "2026-01-15 09:00:00 Zulu",
    "2026-01-15 09:00:00 zulu",
    "2026-01-15 09:00:00 UCT",
    "2026-01-15 09:00:00 Universal",
    "2026-01-15 09:00:00 Greenwich",
    "2026-01-15 09:00:00 GMT0",
    "2026-01-15 09:00:00 GMT+0",
    "2026-01-15 09:00:00 GMT-0",
    "2026-01-15 09:00:00 Etc/UTC",
    "2026-01-15 09:00:00 Etc/GMT",
    "2026-01-15 09:00:00 Etc/GMT+0",
    "2026-01-15 09:00:00 Etc/GMT-0",
    "2026-01-15 09:00:00 Etc/GMT0",
    "2026-01-15 09:00:00 Etc/Greenwich",
    "2026-01-15 09:00:00 Etc/UCT",
    "2026-01-15 09:00:00 Etc/Universal",
    "2026-01-15 09:00:00 Etc/Zulu",
    "2026-01-15 09:00:00 etc/utc",
    "2026-01-15 09:00:00 UTC ",
    "2026-01-15 09:00:00 UTC  ",
    "2026-01-15 09:00:00 UTC\t",
    "2026-01-15T09:00:00.123456789 UTC",
    "2026-01-15 09:00:00UTC",
    "2026-01-15 09:00:00  UTC",
    "2026-01-15 09:00:00\tUTC",
    "2026-01-15 09:00:00\nUTC",
    "2026-01-15 09:00:00 Z",
    "2026-01-15 09:00:00 UT",
    "2026-01-15 09:00:00 GMT+2",
    "2026-01-15 09:00:00 Narnia/Cair_Paravel",
    // Years outside four digits, and outside DuckDB's own range.
    "0001-01-01 00:00:00",
    "1-01-01",
    "0-01-01",
    "0000-01-01 00:00:00",
    "-0001-01-01 00:00:00",
    "-0100-01-01 00:00:00",
    "10000-01-01 00:00:00",
    "100000-01-01 00:00:00",
    "9999-12-31 24:00:00",
    "9999-12-31T23:59:59.999999Z",
    "1969-12-31T23:59:59Z",
    "300000-01-01 00:00:00",
    "-290308-01-01 00:00:00",
    // Not timestamps at all — epoch numerals included.
    "yesterday-ish",
    "",
    "accepted",
    "0404",
    "1737000000",
    "1737000000123",
    "\u{a0}2026-01-15T09:00:00Z",
    // The keyword instants: the FULL spellings tolerate trailing
    // whitespace, the `inf` abbreviations do NOT, and the leading `-` is
    // consumed before the keyword is read (so `-epoch` is epoch).
    "inf",
    " inf",
    "\tinf",
    "inf ",
    "inf\t",
    "-inf",
    "-inf ",
    "INF\n",
    "Inf",
    "infinity ",
    "infinity\t",
    " infinity ",
    "-infinity ",
    "epoch ",
    "epoch\t",
    "-epoch",
    "-epoch ",
    "-EPOCH",
    "- epoch",
    "--epoch",
    "+inf",
    "+infinity",
    "epochx",
    "infx",
    // Trailing whitespace after a ZONELESS time: a space is where a zone
    // name would start and closes the time, any other whitespace is NULL.
    "2026-01-15T09:00:00 ",
    "2026-01-15T09:00:00  ",
    "2026-01-15T09:00:00 \t",
    "2026-01-15T09:00:00\t",
    "2026-01-15T09:00:00\n",
    "2026-01-15T09:00:00\r",
    "2026-01-15T09:00:00\x0b",
    "2026-01-15T09:00:00\x0c",
    "2026-01-15 09:00:00\t",
    "2026-01-15T09:00:00.123\t",
    "2026-01-15T09:00:00Z\t",
    "2026-01-15T09:00:00+05:30\t",
    "2026-01-15T09:00:00 UTC\t",
];

/// The rule: a TIMESTAMP pin globs/regexes against ONE canonical text —
/// `strftime(<the conform>, TIMESTAMP_PATTERN_SQL_FORMAT)` in SQL,
/// `compare::canonical_timestamp_text` in the live matcher — so both
/// engines answer the same string for the same wire value.
///
/// The conform is [`trawl_core::conform::guarded_cast`] itself, not a
/// hand-written cast: the mirror owes its answer to what the corpus
/// DURABLY holds, so a change to the rung has to break this test rather
/// than quietly retire it. That rung parses through `TIMESTAMPTZ`, so the
/// session must be pinned to UTC ([`conn`]) or the expectations move with
/// `/etc/localtime`.
///
/// Every row of [`TIMESTAMP_TEXT_MATRIX`] runs through both engines. A
/// mismatch is a MIRROR bug — see the module doc.
#[test]
fn timestamp_pattern_text_is_rfc3339_micros_on_both_engines() {
    let conn = conn();
    let fmt = trawl_core::compare::TIMESTAMP_PATTERN_SQL_FORMAT;
    let conform = conform("v", CanonicalType::Timestamp);
    let residuals = timestamp_mirror_residuals();

    for input in TIMESTAMP_TEXT_MATRIX {
        let sql: Option<String> = conn
            .query_row(
                &format!("SELECT strftime({conform}, ?) FROM (SELECT ? AS v) t"),
                [fmt, *input],
                |row| row.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::canonical_timestamp_text(input);
        assert!(
            !residuals.contains(input),
            "{input:?} is in the matrix AND in the residual list — pick one"
        );
        assert_eq!(live, sql, "canonical pattern text disagrees for {input:?}");
    }

    // The whole point: an anchored pattern means the same thing on both
    // sides of the same value.
    let matched: bool = conn
        .query_row(
            &format!(
                "SELECT strftime({conform}, '{fmt}') GLOB '*T09:*' \
                 FROM (SELECT '2026-01-15T09:00:00.000000Z' AS v) t"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(matched, "batch matches the separator-anchored pattern");

    // A non-timestamp value is NULL, not the empty string: GLOB over it is
    // NULL (UNKNOWN), which is what the live matcher's `None` mirrors.
    let unknown: Option<bool> = conn
        .query_row(
            &format!(
                "SELECT strftime({conform}, '{fmt}') GLOB '2026*' \
                 FROM (SELECT 'yesterday-ish' AS v) t"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unknown, None);
}

/// A bound LITERAL meets a typed column through the COLUMN's cast, and
/// that cast is not always the conform's — which is why
/// `compare::literal_timestamp` exists beside `conformed_timestamp`.
///
/// Every row here is what the live matcher's `pin_literal` claims
/// (`trawl_core::pin_match`), executed: a BOOLEAN column takes the cast's WIDE
/// vocabulary from a string literal (`flag=TRUE` and `flag=yes` match a
/// stored `true`, where the same texts STORED conform to NULL) and casts
/// ITSELF to meet a number; a TIMESTAMP column casts the literal
/// wall-clock, so an offset spelled in the literal is IGNORED where the
/// same offset in a stored value is applied; and the pairs `DuckDB`
/// refuses outright raise a conversion or binder ERROR, which returns no
/// rows at all.
#[test]
fn a_typed_columns_literal_takes_the_columns_own_cast() {
    let conn = conn();
    conn.execute_batch(
        "CREATE TABLE t(b BIGINT, d DOUBLE, f BOOLEAN, ts TIMESTAMP);
         INSERT INTO t VALUES (404, 200.5, true, TIMESTAMP '2026-01-15 09:00:00');",
    )
    .unwrap();
    let text_param = |sql: &str, param: &str| -> Result<bool, duckdb::Error> {
        conn.query_row(sql, [param], |row| row.get(0))
    };

    // BOOLEAN: the wide cast vocabulary, on the LITERAL side only.
    for (literal, expected) in [("true", true), ("TRUE", true), ("yes", true), ("1", true)] {
        assert_eq!(
            text_param("SELECT f = ? FROM t", literal).unwrap(),
            expected,
            "BOOLEAN literal {literal:?}"
        );
        assert_eq!(
            trawl_core::compare::try_cast_boolean(literal),
            Some(expected),
            "the mirror reads the literal the same way"
        );
    }
    // …and the same texts STORED conform to NULL under the guard, which
    // is the asymmetry the mirror encodes.
    for stored in ["TRUE", "yes", "1"] {
        assert_eq!(trawl_core::compare::conformed_boolean(stored), None);
    }
    // BOOLEAN meeting a NUMBER: the column casts itself.
    assert!(
        conn.query_row("SELECT f = ? FROM t", [1i64], |row| row.get::<_, bool>(0))
            .unwrap()
    );
    assert!(
        !conn
            .query_row("SELECT f = ? FROM t", [0i64], |row| row.get::<_, bool>(0))
            .unwrap()
    );
    assert!(
        conn.query_row("SELECT f > ? FROM t", [0i64], |row| row.get::<_, bool>(0))
            .unwrap()
    );
    assert!(
        conn.query_row("SELECT f = ? FROM t", [1.0f64], |row| row.get::<_, bool>(0))
            .unwrap()
    );

    // BIGINT meeting a fractional literal: promoted to DOUBLE, so the
    // mirror's `as f64` promotion is DuckDB's own.
    assert!(
        conn.query_row("SELECT b > ? FROM t", [1.5f64], |row| row.get::<_, bool>(0))
            .unwrap()
    );

    // TIMESTAMP: the literal is cast WALL-CLOCK — an offset in it is
    // ignored, where the same offset in a stored value shifts the instant.
    assert!(
        text_param("SELECT ts = ? FROM t", "2026-01-15T09:00:00+05:30").unwrap(),
        "a literal's offset is ignored"
    );
    assert!(text_param("SELECT ts = ? FROM t", "2026-01-15T09:00:00Z").unwrap());
    assert!(text_param("SELECT ts > ? FROM t", "2026-01-15").unwrap());

    // The comparisons DuckDB refuses: no row set to agree with, which the
    // mirror answers with UNKNOWN.
    for (sql, param) in [
        ("SELECT b = ? FROM t", "accepted"),
        ("SELECT d = ? FROM t", "accepted"),
        ("SELECT f = ? FROM t", "accepted"),
        ("SELECT ts = ? FROM t", "accepted"),
    ] {
        assert!(
            text_param(sql, param).is_err(),
            "{sql} [{param}] must be an error"
        );
    }
    for sql in ["SELECT ts = ? FROM t", "SELECT ts > ? FROM t"] {
        assert!(
            conn.query_row(sql, [1i64], |row| row.get::<_, bool>(0))
                .is_err(),
            "{sql} with a number must be an error"
        );
    }
}

/// The literal's wall-clock cast reads the SAME syntax the conform does
/// and differs only in what it does with a zone — run over the whole
/// timestamp matrix, so a syntax rule cannot drift between the two.
#[test]
fn the_literal_timestamp_cast_differs_from_the_conform_only_in_the_zone() {
    let conn = conn();
    let fmt = trawl_core::compare::TIMESTAMP_PATTERN_SQL_FORMAT;
    let residuals = timestamp_mirror_residuals();
    for input in TIMESTAMP_TEXT_MATRIX {
        if residuals.contains(input) {
            continue;
        }
        let sql: Option<String> = conn
            .query_row(
                "SELECT strftime(TRY_CAST(? AS TIMESTAMP), ?)",
                [*input, fmt],
                |row| row.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::literal_timestamp(input)
            .map(trawl_core::compare::Instant::pattern_text);
        assert_eq!(live, sql, "literal timestamp cast disagrees for {input:?}");
    }
    // The one syntax rule that IS different: the wall-clock cast resolves
    // exactly one zone NAME, where the conform's ICU-backed cast takes
    // every name `pg_timezone_names()` lists.
    for name in ["UTC", "utc"] {
        assert!(
            trawl_core::compare::literal_timestamp(&format!("2026-01-15 09:00:00 {name}"))
                .is_some()
        );
    }
    for name in [
        "GMT",
        "Etc/UTC",
        "Zulu",
        "Universal",
        "Greenwich",
        "Asia/Kolkata",
    ] {
        let text = format!("2026-01-15 09:00:00 {name}");
        let sql: Option<String> = conn
            .query_row(
                "SELECT strftime(TRY_CAST(? AS TIMESTAMP), ?)",
                [text.as_str(), fmt],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sql, None, "the wall-clock cast must not resolve {name}");
        assert_eq!(
            trawl_core::compare::literal_timestamp(&text),
            None,
            "{name}"
        );
    }
}

/// The shapes the live mirror deliberately does NOT read, pinned as
/// EXPECTED divergences so they stay a known cost.
///
/// Both classes are one-directional: `DuckDB` has a reading, the mirror
/// answers `None`, so a live tail under-matches a batch query and can
/// never invent a match it does not have.
///
/// 1. **zone names beyond the definitionally-UTC set.** `DuckDB` links ICU
///    and resolves all 638 of `pg_timezone_names()` — with DST rules and
///    pre-1970 local mean time, which is why an offset TABLE cannot stand
///    in for it (`Africa/Abidjan` is +00:00 today and +00:16:08 in 1800).
///    Mirroring it means a tz database inside `trawl-core`, which compiles
///    to wasm for the SPA, and two tzdata versions drifting apart would be
///    a SILENT divergence in place of this loud one.
/// 2. **years outside chrono's calendar** (`NaiveDate` spans
///    `-262143-01-01` to `+262142-12-31` in this build) where `DuckDB`'s
///    microsecond range reaches ±~290 000.
///
/// This test also proves the first residual is drawn where the mirror
/// claims: every name in its table really is UTC to `DuckDB`, at two
/// instants a century apart.
#[test]
fn the_timestamp_mirror_residuals_are_one_directional() {
    let conn = conn();
    let fmt = trawl_core::compare::TIMESTAMP_PATTERN_SQL_FORMAT;
    let conform = conform("v", CanonicalType::Timestamp);

    for input in timestamp_mirror_residuals() {
        let sql: Option<String> = conn
            .query_row(
                &format!("SELECT strftime({conform}, ?) FROM (SELECT ? AS v) t"),
                [fmt, input],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            sql.is_some(),
            "{input:?} is not a residual: DuckDB NULLs it"
        );
        assert_eq!(
            trawl_core::compare::canonical_timestamp_text(input),
            None,
            "{input:?} left the residual list — move it into the matrix"
        );
    }

    // The residual line is drawn at names that are UTC for ALL time, so
    // the mirror's table has to hold at an instant far from today's rules.
    for name in [
        "Etc/GMT",
        "Etc/GMT+0",
        "Etc/GMT-0",
        "Etc/GMT0",
        "Etc/Greenwich",
        "Etc/UCT",
        "Etc/UTC",
        "Etc/Universal",
        "Etc/Zulu",
        "GMT",
        "GMT+0",
        "GMT-0",
        "GMT0",
        "Greenwich",
        "UCT",
        "UTC",
        "Universal",
        "Zulu",
    ] {
        for (wall, expected) in [
            ("2026-07-15 09:00:00", "2026-07-15T09:00:00.000000Z"),
            ("1890-01-15 09:00:00", "1890-01-15T09:00:00.000000Z"),
        ] {
            let text = format!("{wall} {name}");
            let sql: Option<String> = conn
                .query_row(
                    &format!("SELECT strftime({conform}, ?) FROM (SELECT ? AS v) t"),
                    [fmt, text.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(sql.as_deref(), Some(expected), "{text:?} is not UTC");
            assert_eq!(
                trawl_core::compare::canonical_timestamp_text(&text).as_deref(),
                Some(expected),
                "the mirror lost {text:?}"
            );
        }
    }
}

/// The inputs [`the_timestamp_mirror_residuals_are_one_directional`] owns,
/// kept out of [`TIMESTAMP_TEXT_MATRIX`] because the two engines are
/// EXPECTED to disagree on them.
fn timestamp_mirror_residuals() -> Vec<&'static str> {
    vec![
        "2026-01-15 09:00:00 America/New_York",
        "2026-01-15 09:00:00 Asia/Kolkata",
        "2026-01-15 09:00:00 EST",
        "2026-01-15 09:00:00 PST8PDT",
        "2026-01-15 09:00:00 US/Eastern",
        "2026-01-15 09:00:00 Etc/GMT+5",
        "2026-01-15 09:00:00 UTC+2",
        "1800-01-01 00:00:00 Africa/Abidjan",
        "262144-01-01 00:00:00",
        "294247-01-01 00:00:00",
    ]
}

/// The rules over `read_json` columns — the hot-only fallback's untyped
/// source (no REPLACE conformance). A VARCHAR-inferred column behaves
/// like the parquet case: text eq exact, TRY_CAST-DOUBLE orders and NULLs
/// words. A BIGINT-inferred column under the VARCHAR pin's text-eq form
/// implicit-casts the literal to the column side and still matches.
#[test]
fn pinned_rules_hold_over_read_json_columns() {
    let dir = tempfile::tempdir().unwrap();

    // VARCHAR inference (strings, incl. a word).
    let strings = dir.path().join("strings.ndjson");
    {
        let mut f = std::fs::File::create(&strings).unwrap();
        for v in ["200", "404", "accepted"] {
            writeln!(f, "{{\"v\": \"{v}\"}}").unwrap();
        }
    }
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let reader = hot_reader(&strings);
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*)::BIGINT FROM {reader} WHERE v = '200'")
        )
        .unwrap(),
        1
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*)::BIGINT FROM {reader} WHERE TRY_CAST(v AS DOUBLE) >= 400")
        )
        .unwrap(),
        1
    );

    // BIGINT inference (a hot event sent numbers for a VARCHAR-pinned
    // field before conformance): the text literal implicit-casts onto the
    // BIGINT column and matches the numeric value.
    let ints = dir.path().join("ints.ndjson");
    {
        let mut f = std::fs::File::create(&ints).unwrap();
        for v in [200, 404] {
            writeln!(f, "{{\"v\": {v}}}").unwrap();
        }
    }
    let reader = hot_reader(&ints);
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*)::BIGINT FROM {reader} WHERE v = '200'")
        )
        .unwrap(),
        1
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*)::BIGINT FROM {reader} WHERE TRY_CAST(v AS DOUBLE) >= 400")
        )
        .unwrap(),
        1
    );
}

/// The two conform lanes must read one value out of one event: whatever
/// the hot branch shows a query is what compaction will durably store, or
/// a result flips minutes later when the compactor runs (ADR-0011).
///
/// The matrix is the shapes that can disagree — a boolean spelling the
/// cast reads but the rendering does not round-trip, a fraction the cast
/// rounds, a leading-zero integer, the whitespace and `_` widenings, a
/// numeric that a JSON-inferred column casts to `true` — read three ways
/// over the same texts:
///
/// 1. through a snapshot where the field holds only strings (VARCHAR),
/// 2. through one where a JSON number shares the field (JSON), and
/// 3. through the live mirror (`compare::conformed_*`).
///
/// All three must agree, and the probe carries its own premise for why
/// that is not automatic: a bare cast answers differently in the two
/// inference classes, so an event's reading would depend on what happened
/// to share its buffer.
#[test]
#[allow(clippy::too_many_lines)] // one matrix, kept in one place to stay readable
fn hot_and_compaction_conform_agree_across_inference_classes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hot.ndjson");
    // `s` holds only strings (VARCHAR); `m` holds the same texts beside a
    // JSON number (JSON). `n` is the number itself, for the BOOLEAN case.
    let texts = ["TRUE", "true", "1.5", "0404", " 200", "200_000", "nan"];
    {
        let mut f = std::fs::File::create(&file).unwrap();
        for (idx, text) in texts.iter().enumerate() {
            writeln!(f, r#"{{"i":{idx},"s":"{text}","m":"{text}","n":200}}"#).unwrap();
        }
        writeln!(f, r#"{{"i":99,"s":"x","m":200,"n":200}}"#).unwrap();
        f.sync_all().unwrap();
    }
    let reader = hot_reader(&file);
    let conn = conn();

    // The inference classes this probe exists to tell apart.
    let types: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(&format!("DESCRIBE SELECT s, m, n FROM {reader}"))
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        types,
        vec![
            ("s".into(), "VARCHAR".into()),
            ("m".into(), "JSON".into()),
            ("n".into(), "BIGINT".into()),
        ],
        "read_json inference classes changed under us"
    );

    let read = |expr: &str| -> Vec<Option<String>> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT CAST(({expr}) AS VARCHAR) FROM {reader} WHERE i < 90 ORDER BY i"
            ))
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };

    for (pin, expected) in [
        (
            CanonicalType::BigInt,
            vec![
                None,
                None,
                None,
                Some("404"),
                Some("200"),
                Some("200000"),
                None,
            ],
        ),
        (
            CanonicalType::Boolean,
            vec![None, Some("true"), None, None, None, None, None],
        ),
        (
            CanonicalType::Double,
            vec![
                None,
                None,
                Some("1.5"),
                Some("404.0"),
                Some("200.0"),
                Some("200000.0"),
                Some("nan"),
            ],
        ),
    ] {
        let expected: Vec<Option<String>> =
            expected.into_iter().map(|v| v.map(str::to_owned)).collect();
        // The premise: a bare cast reads the two inference classes
        // differently.
        if pin == CanonicalType::BigInt {
            assert_ne!(
                read(&format!("TRY_CAST(m AS {})", pin.as_duckdb())),
                read(&format!("TRY_CAST(s AS {})", pin.as_duckdb())),
                "premise: the bare cast is inference-dependent"
            );
        }
        for (class, expr) in [("VARCHAR", conform("s", pin)), ("JSON", conform("m", pin))] {
            assert_eq!(read(&expr), expected, "{class} conform for {pin:?}");
        }
        // The live mirror answers the same for every one of those texts.
        let live: Vec<Option<String>> = texts
            .iter()
            .map(|t| match pin {
                CanonicalType::BigInt => {
                    trawl_core::compare::conformed_bigint(t).map(|i| i.to_string())
                }
                CanonicalType::Boolean => {
                    trawl_core::compare::conformed_boolean(t).map(|b| b.to_string())
                }
                CanonicalType::Double => trawl_core::compare::try_cast_double(t)
                    .map(trawl_core::compare::canonical_double_text),
                _ => unreachable!("only typed pins"),
            })
            .collect();
        assert_eq!(live, expected, "live mirror for {pin:?}");
    }

    // A JSON NUMBER under a BOOLEAN pin: the bare cast reads it as `true`
    // in the JSON class and NULL in the VARCHAR class — the sharpest form
    // of the state-dependence — while both conform lanes answer NULL.
    let numeric_row = |expr: &str| -> Option<String> {
        conn.query_row(
            &format!("SELECT CAST(({expr}) AS VARCHAR) FROM {reader} WHERE i = 99"),
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        numeric_row("TRY_CAST(m AS BOOLEAN)").as_deref(),
        Some("true"),
        "premise: a JSON numeric casts to BOOLEAN true"
    );
    for expr in [
        conform("m", CanonicalType::Boolean),
        conform("n", CanonicalType::Boolean),
    ] {
        assert_eq!(
            numeric_row(&expr),
            None,
            "a number is not a boolean: {expr}"
        );
    }
}

/// The TIMESTAMP rung is ZONE-AWARE: an offset in the text is applied, a
/// zoneless text reads as UTC — the semantics ingest already ratified for
/// `_time` (ADR-0008), now the same in every lane that conforms.
///
/// Both halves are pinned, because both are easy to get wrong: the plain
/// TIMESTAMP cast IGNORES an offset (storing `09:00` for
/// `09:00:00+05:30`, where `read_json`'s own inference stores `03:30`),
/// and the `TIMESTAMPTZ` parse that fixes it reads the SESSION zone — which
/// the bundled `DuckDB` links ICU for and defaults to the HOST zone. Under
/// a hostile session the same conform writes a different instant, which is
/// what `conform::SESSION_TIME_ZONE_SQL` exists to prevent.
#[test]
fn timestamp_conform_applies_the_offset_under_the_pinned_session() {
    let conn = conn();
    let guard = conform("v", CanonicalType::Timestamp);
    let read = |conn: &duckdb::Connection, text: &str| -> Option<String> {
        conn.query_row(
            &format!("SELECT CAST(({guard}) AS VARCHAR) FROM (SELECT ? AS v) t"),
            [text],
            |row| row.get(0),
        )
        .unwrap()
    };

    // The premise: the plain cast drops the offset instead of applying it.
    let wall: Option<String> = conn
        .query_row(
            "SELECT CAST(TRY_CAST('2026-01-15T09:00:00+05:30' AS TIMESTAMP) AS VARCHAR)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        wall.as_deref(),
        Some("2026-01-15 09:00:00"),
        "premise: TRY_CAST(text AS TIMESTAMP) is a wall-clock parse"
    );

    for (text, expected) in [
        // Offsets are applied, in both directions and in the basic form.
        ("2026-01-15T09:00:00+05:30", "2026-01-15 03:30:00"),
        ("2026-01-15T09:00:00-08:00", "2026-01-15 17:00:00"),
        ("2026-01-15T09:00:00+02", "2026-01-15 07:00:00"),
        // Zoneless is UTC — the wall clock, unmoved.
        ("2026-01-15T09:00:00Z", "2026-01-15 09:00:00"),
        ("2026-01-15 09:00:00", "2026-01-15 09:00:00"),
        ("2026/01/15 09:00:00", "2026-01-15 09:00:00"),
        ("2026-01-15", "2026-01-15 00:00:00"),
        ("2026-01-15T09:00:00.1234567", "2026-01-15 09:00:00.123456"),
    ] {
        assert_eq!(read(&conn, text).as_deref(), Some(expected), "{text:?}");
    }
    // No reading at all — epoch numerals included, which are not
    // timestamps to DuckDB.
    for text in ["yesterday-ish", "", "1737000000", "2026-01-15T"] {
        assert_eq!(read(&conn, text), None, "{text:?}");
    }

    // The session pin is load-bearing, not decoration: without it the
    // same expression writes a different instant on a non-UTC host.
    let hostile = duckdb::Connection::open_in_memory().unwrap();
    hostile
        .execute_batch("SET TimeZone='Asia/Kolkata'")
        .unwrap();
    assert_eq!(
        read(&hostile, "2026-01-15T09:00:00+05:30").as_deref(),
        Some("2026-01-15 09:00:00"),
        "an unpinned session renders the instant in ITS zone"
    );
    hostile
        .execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    assert_eq!(
        read(&hostile, "2026-01-15T09:00:00+05:30").as_deref(),
        Some("2026-01-15 03:30:00"),
        "the pin restores UTC on a connection that had drifted"
    );
}

/// `Executor::configure` runs on every connection the pool makes, clones
/// included, and that is load-bearing rather than defensive: `try_clone()`
/// shares the DATABASE, not the session, so a clone starts from the
/// process default — which the bundled ICU build takes from the HOST — and
/// would read a zoneless text in `/etc/localtime`'s zone.
///
/// Written host-independently: the assertion is that a clone reports the
/// default a FRESH connection reports, whatever the parent was set to, and
/// that at least one of the two zones exercised really did differ from
/// that default, so the test cannot pass by the host happening to match.
#[test]
fn a_cloned_connection_starts_from_the_process_default_not_the_parent() {
    let zone_of = |conn: &duckdb::Connection| -> String {
        conn.query_row("SELECT current_setting('TimeZone')", [], |row| row.get(0))
            .unwrap()
    };
    let default = zone_of(&duckdb::Connection::open_in_memory().unwrap());
    assert!(
        !default.is_empty(),
        "the bundled build links ICU, so a session always has a zone"
    );

    let parent = duckdb::Connection::open_in_memory().unwrap();
    let mut differed_from_the_default = false;
    for zone in ["Asia/Kolkata", "America/Phoenix"] {
        parent
            .execute_batch(&format!("SET TimeZone='{zone}'"))
            .unwrap();
        assert_eq!(zone_of(&parent), zone, "the parent's own SET must hold");
        let clone = parent.try_clone().unwrap();
        assert_eq!(
            zone_of(&clone),
            default,
            "a clone must not inherit the parent's {zone}"
        );
        differed_from_the_default |= zone != default;
    }
    assert!(
        differed_from_the_default,
        "neither probe zone differed from the host default — the test proved nothing"
    );

    // The pin is what makes the answer independent of the host, and it
    // has to be applied to the clone itself.
    let clone = parent.try_clone().unwrap();
    clone
        .execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    assert_eq!(zone_of(&clone), "UTC");
}

/// A `_time` conform can never produce NULL on a standing file, zone-aware
/// rung or not: a NULL partition key sorts first and falls outside every
/// `last=Xh` filter, so the row would survive the rewrite yet become
/// permanently unqueryable by time (ADR-0008).
///
/// This is the composition `guard_partition_key` writes — the conform
/// wrapped in `COALESCE(…, <the file's own partition instant>)` — proved
/// total over every shape the rung can refuse.
#[test]
fn time_conform_never_nulls_the_partition_key() {
    let conn = conn();
    let guarded = conform("v", CanonicalType::Timestamp);
    let fallback = "TIMESTAMP '2026-01-15 07:00:00.000000'";
    for text in [
        "2026-01-15T09:00:00.000000Z",
        "2026-01-15T09:00:00+05:30",
        "2026-01-15",
        "yesterday-ish",
        "1737000000",
        "",
    ] {
        let value: Option<String> = conn
            .query_row(
                &format!(
                    "SELECT CAST(COALESCE({guarded}, {fallback}) AS VARCHAR) \
                     FROM (SELECT ? AS v) t"
                ),
                [text],
                |row| row.get(0),
            )
            .unwrap();
        assert!(value.is_some(), "partition key NULLed for {text:?}");
    }
    // And a genuinely NULL input still takes the fallback.
    let value: Option<String> = conn
        .query_row(
            &format!(
                "SELECT CAST(COALESCE({guarded}, {fallback}) AS VARCHAR) \
                 FROM (SELECT CAST(NULL AS VARCHAR) AS v) t"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value.as_deref(), Some("2026-01-15 07:00:00"));
}

/// The two text forms a DOUBLE can take are NOT the same string —
/// `CAST(v AS VARCHAR)` writes `1e+20` and `1.7356896001234568e+18` where
/// `json_extract_string(to_json(v), '$')` writes `100000000000000000000.0`
/// and `1735689600123456800.0` — which is why the lanes cannot each pick
/// their own.
///
/// Under the TYPED pins it would not have mattered: both renderings are
/// shortest-round-trip and every rung's guarded cast reads them alike, so
/// the conformed value is the same either way. Under the VARCHAR pin there
/// IS no cast — [`guarded_cast`] is the identity — so the text form is the
/// stored value itself, and a per-lane spelling is a per-lane corpus: a hot
/// `note=/^1e/` would match and the same query stop matching minutes
/// later, when the compactor rewrites the value it had already shown.
#[test]
fn the_varchar_pin_stores_the_text_form_so_both_lanes_must_share_one() {
    let conn = conn();
    let doubles = [
        "1735689600123456710.7",
        "1e20",
        "1e-7",
        "9007199254740993.0",
        "200.0",
        "1e17",
        "12345678901234567.0",
        "-0.0",
        "0.5",
    ];
    let values = doubles
        .iter()
        .map(|d| format!("(CAST({d} AS DOUBLE))"))
        .collect::<Vec<_>>()
        .join(", ");
    let read = |expr: &str| -> Vec<Option<String>> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT CAST(({expr}) AS VARCHAR) FROM (VALUES {values}) t(v)"
            ))
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    // The premise: the spellings really are different.
    let cast_text = "TRY_CAST(v AS VARCHAR)";
    assert_ne!(
        read(cast_text),
        read(&untyped_text("v")),
        "premise: the two text renderings of a DOUBLE differ"
    );
    // Under the VARCHAR pin that difference IS the stored value.
    assert_ne!(
        read(&guarded_cast(cast_text, CanonicalType::Varchar)),
        read(&conform("v", CanonicalType::Varchar)),
        "the VARCHAR pin has no guard to absorb a per-lane spelling"
    );
    assert_eq!(
        read(&conform("v", CanonicalType::Varchar)),
        read(&untyped_text("v")),
        "the VARCHAR conform is the shared text form, verbatim"
    );
    // Under the typed pins the guard absorbs the spelling, which is why
    // the divergence is invisible there.
    for pin in [
        CanonicalType::BigInt,
        CanonicalType::Double,
        CanonicalType::Boolean,
        CanonicalType::Timestamp,
    ] {
        assert_eq!(
            read(&guarded_cast(cast_text, pin)),
            read(&conform("v", pin)),
            "the guard must absorb the spelling under {pin:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The pinned comparison shapes in the pipeline positions: SELECT-list
// values (`| let`) and the LIKE/ILIKE pattern operators (ADR-0011).
// ---------------------------------------------------------------------------

/// The VARCHAR-pin comparison shapes as SELECT-LIST values: `| let x =
/// status >= 400` stores TRUE/FALSE/NULL per row, three-valued exactly as
/// the where-position shapes filter — UNKNOWN is a stored NULL, not an
/// error and not FALSE.
#[test]
fn pinned_comparison_shapes_as_select_list_values() {
    let conn = varchar_status_conn();
    let read = |expr: &str, row_filter: &str| -> Option<bool> {
        conn.query_row(
            &format!("SELECT {expr} FROM t WHERE v = '{row_filter}'"),
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    // NumericOnText (`status >= 400`): the DECIMAL space on both sides.
    let ordered = format!("{} >= {}", decimal_reading("v"), decimal_reading("'400'"));
    assert_eq!(read(&ordered, "404"), Some(true));
    assert_eq!(read(&ordered, "200"), Some(false));
    assert_eq!(read(&ordered, "accepted"), None, "no reading = stored NULL");

    // TextOrNumeric (`status == 200`): text arm OR COALESCEd reading.
    let eq = format!(
        "(v = '200' OR COALESCE({} = {}, FALSE))",
        decimal_reading("v"),
        decimal_reading("'200'")
    );
    assert_eq!(read(&eq, "200"), Some(true));
    assert_eq!(read(&eq, "404"), Some(false));
    assert_eq!(
        read(&eq, "accepted"),
        Some(false),
        "COALESCE keeps it total"
    );

    // The Strict `!=` complement — total over non-NULL values, and NULL
    // over a NULL column (no `OR v IS NULL` widening in the pipeline).
    let ne = format!(
        "(v != '200' AND COALESCE({} != {}, TRUE))",
        decimal_reading("v"),
        decimal_reading("'200'")
    );
    assert_eq!(read(&ne, "accepted"), Some(true));
    assert_eq!(read(&ne, "200"), Some(false));
    let over_null: Option<bool> = conn
        .query_row(
            &format!("SELECT {ne} FROM (SELECT CAST(NULL AS VARCHAR) AS v)"),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        over_null, None,
        "a NULL column stays UNKNOWN under strict !="
    );
}

/// Why `| let x = status > 400` needs the pin: a pin-blind SELECT-list
/// comparison binds an INTEGER against the VARCHAR column and refuses to
/// bind, erroring the whole query. The pinned rules answer three-valued
/// instead (above).
#[test]
fn pin_blind_select_list_comparison_on_varchar_column_errors() {
    let conn = varchar_status_conn();
    let outcome: Result<Option<bool>, _> =
        conn.query_row("SELECT (v > ?) FROM t WHERE v = '404'", [400i64], |row| {
            row.get(0)
        });
    let err = outcome.expect_err("VARCHAR-vs-BIGINT must refuse to bind in a SELECT list");
    assert!(
        err.to_string().contains("Binder Error"),
        "expected a binder refusal, got {err}"
    );
}

/// The where-position pattern targets under LIKE/ILIKE — the operators
/// the pipeline lane adds beyond the search stage's GLOB/regexp: a typed
/// pin's canonical text form takes LIKE patterns exactly as it takes
/// globs.
#[test]
fn pattern_targets_take_like_and_ilike() {
    let conn = conn();
    conn.execute_batch(
        "CREATE TABLE p AS SELECT 404::BIGINT AS b, 200.5::DOUBLE AS d, TRUE AS f, \
         TIMESTAMP '2026-01-15 03:30:00' AS ts",
    )
    .unwrap();
    let matched = |clause: &str| -> i64 {
        conn.query_row(
            &format!("SELECT count(*)::BIGINT FROM p WHERE {clause}"),
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    assert_eq!(matched("CAST(b AS VARCHAR) LIKE '4%'"), 1);
    assert_eq!(matched("CAST(b AS VARCHAR) LIKE '2%'"), 0);
    assert_eq!(matched("CAST(d AS VARCHAR) LIKE '200._'"), 1);
    assert_eq!(matched("CAST(f AS VARCHAR) ILIKE 'TRU%'"), 1);
    // The TIMESTAMP target is the strftime RFC 3339 text, so a LIKE can
    // anchor on the `T` separator the wire form carries.
    assert_eq!(
        matched("strftime(ts, '%Y-%m-%dT%H:%M:%S.%fZ') LIKE '2026-01-15T03:30%'"),
        1
    );
    assert_eq!(
        matched("strftime(ts, '%Y-%m-%dT%H:%M:%S.%fZ') LIKE '2026-01-15 03:30%'"),
        0,
        "the space-separated CAST rendering is NOT the pattern text"
    );
}

// ---------------------------------------------------------------------------
// The repin rewrite's engine assumptions (ADR-0011).
// ---------------------------------------------------------------------------

/// The repin rewrite's target-column expression
/// ([`trawl_core::conform::resurrection_expr`]): the stored value's guarded
/// reading first, and where the column is NULL — a prior conform shelved the
/// value — the `_raw` re-extraction's guarded reading. Executed per ladder
/// target over a conflict-shaped corpus.
#[test]
fn resurrection_recovers_shelved_values_per_ladder_target() {
    use trawl_core::conform::{RepinTarget, resurrection_expr};

    let conn = conn();
    // A BIGINT-pinned column after a lossy conform: `404` survived, the
    // rest were nulled and live only in `_raw` (one of them not even JSON —
    // a client-supplied verbatim `_raw` is honoured, not validated).
    conn.execute_batch(
        "CREATE TABLE t (v BIGINT, _raw VARCHAR); \
         INSERT INTO t VALUES \
         (404, '{\"status\":404}'), \
         (NULL, '{\"status\":\"accepted\"}'), \
         (NULL, '{\"status\":\"200\"}'), \
         (NULL, 'not json')",
    )
    .unwrap();

    // Repin BIGINT -> VARCHAR: the stored value keeps its canonical text,
    // the shelved values come back as their wire text, non-JSON `_raw`
    // resurrects nothing.
    let expr = resurrection_expr(
        "v",
        "\"_raw\"",
        "status",
        RepinTarget::otel(CanonicalType::Varchar),
    );
    let got: Vec<Option<String>> = conn
        .prepare(&format!("SELECT {expr} FROM t"))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        got,
        vec![
            Some("404".to_owned()),
            Some("accepted".to_owned()),
            Some("200".to_owned()),
            None,
        ]
    );

    // Repin BIGINT -> DOUBLE: the numeric raw value reads, the enum text
    // has no reading (NULL, counted as a projected null).
    let expr = resurrection_expr(
        "v",
        "\"_raw\"",
        "status",
        RepinTarget::otel(CanonicalType::Double),
    );
    let got: Vec<Option<f64>> = conn
        .prepare(&format!("SELECT {expr} FROM t"))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(got, vec![Some(404.0), None, Some(200.0), None]);

    // The guarded rungs stay guarded through resurrection: a raw `"1.5"`
    // must NOT round into a BIGINT 2.
    conn.execute_batch(
        "CREATE TABLE g (v VARCHAR, _raw VARCHAR); \
         INSERT INTO g VALUES \
         (NULL, '{\"dur\":\"1.5\"}'), \
         (NULL, '{\"dur\":\"0404\"}'), \
         ('7', '{\"dur\":7}')",
    )
    .unwrap();
    let expr = resurrection_expr(
        "v",
        "\"_raw\"",
        "dur",
        RepinTarget::otel(CanonicalType::BigInt),
    );
    let got: Vec<Option<i64>> = conn
        .prepare(&format!("SELECT {expr} FROM g"))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        got,
        vec![None, Some(404), Some(7)],
        "the round-trip guard holds through the `_raw` arm ('1.5' refused, \
         '0404' absorbed as representation drift)"
    );
}

/// The `_raw` lookup is an EXACT key lookup in JSON Pointer form — never a
/// `JSONPath` parse — so a client key carrying `.`/`"`/`'`/`[`/`$`/`/`/`~`
/// resolves as itself. The case-variant fallback recovers a mixed-case
/// original, because `_raw` is captured before the fold (or carries a
/// client's own spelling); the exact spelling wins when both exist.
#[test]
fn raw_extraction_is_exact_key_lookup_and_survives_hostile_keys() {
    use trawl_core::conform::{RepinTarget, resurrection_expr};

    let conn = conn();
    let read = |field: &str, raw: &str| -> Option<String> {
        let expr = resurrection_expr("v", "r", field, RepinTarget::otel(CanonicalType::Varchar));
        conn.query_row(
            &format!(
                "SELECT {expr} FROM (SELECT CAST(NULL AS VARCHAR) AS v, '{}' AS r)",
                raw.replace('\'', "''")
            ),
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    for (field, raw, want) in [
        ("a.b", r#"{"a.b":"dot","a":{"b":"nested"}}"#, "dot"),
        (r#"a"b"#, r#"{"a\"b":"quote"}"#, "quote"),
        ("a'b", r#"{"a'b":"tick"}"#, "tick"),
        ("a[0]", r#"{"a[0]":"bracket","a":["indexed"]}"#, "bracket"),
        ("$weird", r#"{"$weird":"dollar"}"#, "dollar"),
        ("/slash", r#"{"/slash":"slash"}"#, "slash"),
        ("~tilde", r#"{"~tilde":"tilde"}"#, "tilde"),
    ] {
        assert_eq!(
            read(field, raw).as_deref(),
            Some(want),
            "field {field:?} must resolve as an exact key"
        );
    }

    // Case-variant fallback: the catalog key is folded, `_raw` holds the
    // client's original spelling.
    assert_eq!(
        read("dur", r#"{"Dur":"9"}"#).as_deref(),
        Some("9"),
        "a case-variant original spelling is recovered best-effort"
    );
    assert_eq!(
        read("dur", r#"{"dur":"exact","Dur":"variant"}"#).as_deref(),
        Some("exact"),
        "the exact spelling wins over a variant"
    );
    // And the fallback is harmless where `_raw` is valid JSON but not an
    // object at all.
    assert_eq!(read("dur", "[1,2]"), None);
    assert_eq!(read("dur", "42"), None);
}

/// The `json_valid` guard must hold under VECTORIZED execution: a batch
/// mixing valid and invalid `_raw` rows must answer NULL for the invalid
/// ones without erroring the whole scan (a `CASE` that eagerly evaluated
/// its THEN arm over every row would throw on the cast).
#[test]
fn raw_guard_is_vector_safe_over_mixed_validity() {
    use std::fmt::Write as _;

    use trawl_core::conform::{RepinTarget, resurrection_expr};

    let conn = conn();
    conn.execute_batch("CREATE TABLE m (v VARCHAR, r VARCHAR)")
        .unwrap();
    let mut insert = String::from("INSERT INTO m VALUES ");
    for i in 0..2048 {
        if i > 0 {
            insert.push(',');
        }
        if i % 3 == 0 {
            write!(insert, "(NULL, 'garbage {i}')").unwrap();
        } else {
            write!(insert, "(NULL, '{{\"k\":\"{i}\"}}')").unwrap();
        }
    }
    conn.execute_batch(&insert).unwrap();

    let expr = resurrection_expr("v", "r", "k", RepinTarget::otel(CanonicalType::Varchar));
    let (rows, recovered): (i64, i64) = conn
        .query_row(
            &format!("SELECT count(*)::BIGINT, count({expr})::BIGINT FROM m"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("mixed-validity batch must not error");
    assert_eq!(rows, 2048);
    assert_eq!(
        recovered,
        2048 - 683,
        "every valid-JSON row recovers its value"
    );
}

/// What `read_parquet(union_by_name=true)` actually does over each mixed
/// scalar ladder pair: it PROMOTES silently — it does not error. Executed
/// over every ordered pair of the five canonical types.
///
/// This kills a load-bearing assumption the repin design could otherwise
/// lean on: a half-swapped corpus (one env repinned, another not) would NOT
/// fail loudly — it would answer queries with silently promoted values
/// (`BIGINT ∪ VARCHAR` reads VARCHAR, `BIGINT ∪ BOOLEAN` reads the booleans
/// as 0/1). The cutover therefore may not rely on union errors for
/// atomicity: the exclusion primitives (all query permits held across the
/// swap and the pin flip) are the ONLY thing keeping a mixed-type corpus
/// unobservable, and this probe is why they are mandatory.
#[test]
fn mixed_ladder_pair_unions_promote_rather_than_error() {
    let all = [
        CanonicalType::BigInt,
        CanonicalType::Double,
        CanonicalType::Boolean,
        CanonicalType::Timestamp,
        CanonicalType::Varchar,
    ];
    let lit = |t: CanonicalType| match t {
        CanonicalType::BigInt => "42::BIGINT",
        CanonicalType::Double => "1.5::DOUBLE",
        CanonicalType::Boolean => "TRUE",
        CanonicalType::Timestamp => "TIMESTAMP '2026-01-15 10:00:00'",
        CanonicalType::Varchar => "'text'",
        // Not in `all`: SEVERITY is physically BIGINT, so it can never be
        // one half of a MIXED pair.
        CanonicalType::Severity => unreachable!("physically BIGINT"),
    };
    // The promoted type per unordered pair, probed by execution: VARCHAR
    // absorbs everything, TIMESTAMP absorbs the remaining scalars, DOUBLE
    // absorbs BIGINT and BOOLEAN, BIGINT absorbs BOOLEAN.
    let promoted = |a: CanonicalType, b: CanonicalType| -> &'static str {
        let has = |t| a == t || b == t;
        if has(CanonicalType::Varchar) {
            "VARCHAR"
        } else if has(CanonicalType::Timestamp) {
            "TIMESTAMP"
        } else if has(CanonicalType::Double) {
            "DOUBLE"
        } else {
            "BIGINT"
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let conn = conn();
    for a in all {
        for b in all {
            if a == b {
                continue;
            }
            let d = dir.path().join(format!("{a:?}_{b:?}"));
            std::fs::create_dir_all(&d).unwrap();
            conn.execute_batch(&format!(
                "COPY (SELECT {} AS v) TO '{}/a.parquet' (FORMAT PARQUET); \
                 COPY (SELECT {} AS v) TO '{}/b.parquet' (FORMAT PARQUET)",
                lit(a),
                d.display(),
                lit(b),
                d.display()
            ))
            .unwrap();
            let got: String = conn
                .query_row(
                    &format!(
                        "SELECT typeof(v) FROM read_parquet('{}/*.parquet', \
                         union_by_name=true) LIMIT 1",
                        d.display()
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_else(|e| panic!("{a:?} ∪ {b:?} must promote, not error: {e}"));
            assert_eq!(
                got,
                promoted(a, b),
                "{a:?} ∪ {b:?} promotes to the wider scalar"
            );
        }
    }
}

/// `DuckDB`'s recursive glob descends into dot-directories: a shadow
/// generation staged INSIDE the data root — however it is named — would be
/// unioned into every fallback-glob query as duplicate rows. This is why
/// the repin build stages as a SIBLING of the data root (the epoch
/// set-aside pattern), which no data-root glob or walk can reach.
#[test]
fn recursive_glob_descends_into_dot_directories() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("data/prod/2026-01-01/10");
    let shadow = dir.path().join("data/.repin-next/prod/2026-01-01/10");
    let sibling = dir.path().join("data.repin-next/prod/2026-01-01/10");
    for d in [&live, &shadow, &sibling] {
        std::fs::create_dir_all(d).unwrap();
    }
    let conn = conn();
    for p in [
        live.join("svc.parquet"),
        shadow.join("svc.parquet"),
        sibling.join("svc.parquet"),
    ] {
        conn.execute_batch(&format!(
            "COPY (SELECT 1 AS x) TO '{}' (FORMAT PARQUET)",
            p.display()
        ))
        .unwrap();
    }
    let count: i64 = conn
        .query_row(
            &format!(
                "SELECT count(*)::BIGINT FROM read_parquet('{}/data/**/*.parquet', \
                 union_by_name=true)",
                dir.path().display()
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 2,
        "the glob reads the in-root dot-dir (2 rows) but never the sibling \
         — staging must live beside the root, not inside it"
    );
}

/// The misfit-sample capture shape (ADR-0011): what compaction runs, per
/// conflicted column, in the same phase as the conflict tally — the only
/// moment the value the conform is about to null is still in the table.
///
/// Four claims, all executed rather than reasoned about, because the Rust
/// that reads the result depends on every one of them:
///
/// - `FILTER (WHERE …)` composes with `list(DISTINCT …)`, so the misfit
///   predicate (a non-NULL source value whose guarded cast reads NULL) can
///   be expressed once instead of smuggled into a `CASE` whose NULL arm
///   would then rely on `list`'s own NULL handling;
/// - `DISTINCT` collapses a repeated misfit, so one sender looping the same
///   bad value spends one sample slot, not five;
/// - `array_slice(l, 1, 5)` is 1-based and inclusive, and is a no-op below
///   the cap — the count bound;
/// - `to_json(list)::VARCHAR` is a JSON array of strings for every value a
///   client can send, so the transport out of `DuckDB` is one `VARCHAR`
///   column parsed by `serde_json`, not a `LIST` binding.
///
/// And the shape a caller must handle: a filter matching nothing aggregates
/// to SQL NULL, and `to_json` of NULL is NULL — never `[]` and never the
/// JSON literal `null`, so the column binds as `Option<String>`.
#[test]
fn misfit_sample_capture_is_distinct_null_free_and_capped() {
    let conn = conn();
    // Ten rows: six distinct misfits under a BIGINT pin, one of them
    // repeated, plus a SQL NULL and two values that convert cleanly.
    conn.execute_batch(
        "CREATE TABLE b AS SELECT * FROM (VALUES \
         ('a'), ('b'), ('c'), ('d'), ('e'), ('f'), ('a'), (NULL), ('1'), ('2')) t(v)",
    )
    .unwrap();

    let text = untyped_text("v");
    let cast = conform("v", CanonicalType::BigInt);
    let sql = format!(
        "SELECT to_json(array_slice(list(DISTINCT {text}) \
         FILTER (WHERE v IS NOT NULL AND {cast} IS NULL), 1, 5))::VARCHAR FROM b"
    );
    let rendered: String = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
    let samples: Vec<String> = serde_json::from_str(&rendered).expect("a JSON array of strings");
    assert_eq!(samples.len(), 5, "array_slice caps the set: {samples:?}");
    let mut sorted = samples.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        5,
        "DISTINCT already deduplicated: {samples:?}"
    );
    assert!(
        samples
            .iter()
            .all(|s| ["a", "b", "c", "d", "e", "f"].contains(&s.as_str())),
        "only the nulled values are sampled — never a NULL row, never a \
         value that converts: {samples:?}"
    );

    // Below the cap the slice is the identity, and the JSON is still an array.
    let sql = format!(
        "SELECT to_json(array_slice(list(DISTINCT {text}) \
         FILTER (WHERE v IS NOT NULL AND {cast} IS NULL), 1, 5))::VARCHAR \
         FROM b WHERE v IN ('a', 'b')"
    );
    let rendered: String = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
    let mut samples: Vec<String> = serde_json::from_str(&rendered).unwrap();
    samples.sort();
    assert_eq!(samples, vec!["a".to_owned(), "b".to_owned()]);

    // No misfit at all: the aggregate is SQL NULL, and so is the rendering.
    let sql = format!(
        "SELECT to_json(array_slice(list(DISTINCT {text}) \
         FILTER (WHERE v IS NOT NULL AND {cast} IS NULL), 1, 5))::VARCHAR \
         FROM b WHERE v IN ('1', '2')"
    );
    let rendered: Option<String> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
    assert_eq!(
        rendered, None,
        "an empty capture is SQL NULL, not an empty array — the caller \
         binds Option<String> and reads no samples"
    );
}

/// A misfit value carries whatever bytes a client sent, so the sample is
/// truncated before it is aggregated — but `DuckDB`'s `left()` counts
/// CHARACTERS, not bytes, so it bounds accumulation only, never the wire
/// size. The byte cap the store promises is therefore applied in Rust, on
/// a `char` boundary; this probe is why it cannot be pushed into the SQL.
#[test]
fn left_truncates_by_character_not_by_byte() {
    let conn = conn();
    // Four-byte astral characters: 8 chars, 32 bytes.
    let value = "\u{1F600}".repeat(8);
    let mut stmt = conn
        .prepare("SELECT left(?, 8), length(left(?, 8))")
        .unwrap();
    let (kept, chars): (String, i64) = stmt
        .query_row(duckdb::params![&value, &value], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(chars, 8, "left() counts characters");
    assert_eq!(
        kept.len(),
        32,
        "…so an 8-'character' truncation is 32 BYTES: a byte cap must be \
         applied in Rust"
    );
}

// ---------------------------------------------------------------------------
// The SEVERITY canonical type (ADR-0013)
// ---------------------------------------------------------------------------

/// The severity reading matrix: the cases where the two engines have
/// their OWN opinions about a value, shared by every probe that pairs a
/// reading against the Rust kernel (the conform rung in either dialect,
/// and the dialect-ambiguity classifier).
const SEVERITY_READING_CASES: &[&str] = &[
    "error",
    "ERR",
    " error ",
    "error2",
    "ERROR2",
    "warn",
    "notice",
    "0",
    "1",
    "7",
    "8",
    "24",
    "25",
    "0404",
    "007",
    "+17",
    "-1",
    "1.5",
    "17.0",
    "1_2",
    "1e1",
    "0x10",
    "9223372036854775807",
    "9223372036854775808",
    "99999999999999999999999999",
    "",
    "   ",
    "\t 17 \r\n",
    "\u{a0}error",
    "gold",
    "nan",
    "inf",
    // `DuckDB`'s `lower()` is UNICODE and the kernel's fold is ASCII:
    // `lower('İ')` is `i`, so an ungated token CASE would read `İNFO`
    // as 9 in SQL and as nothing in Rust. Both dotted/dotless Turkish i,
    // a full-width digit (which `TRY_CAST` also refuses), and a
    // Kelvin sign that folds to `k`.
    "İNFO",
    "info\u{307}",
    "ı",
    "İ",
    "\u{212a}",
    "ＩＮＦＯ",
    "１７",
    "ERROR",
    "Warning",
    // The trim set is the Unicode `White_Space` property on BOTH
    // engines (`DuckDB` matches a multibyte character set by
    // character, which is what licenses the wide set): a padded
    // token has a reading at ingest, and narrowing the set here
    // would drop it silently.
    "\u{a0}error\u{a0}",
    "\u{2003}error",
    "\u{3000}error\u{3000}",
    "\u{85}error",
    "\u{202f}\u{2009}error\t",
    "\u{a0}17\u{a0}",
    "er\u{a0}ror",
];

/// The reading matrix (ADR-0013 ruling 9): `severity_reading_sql`
/// answers what `severity::reading` answers, case by case, in BOTH
/// dialects — the whole basis for one kernel serving ingest, `sev()` and
/// the conform rung.
///
/// The cases the probe exists for are the ones where the two engines have
/// their OWN opinions: `TRY_CAST` reads `'1e1'` as 10 and `'0x10'` as 16
/// where `str::parse` reads neither (so the SQL guards with a digits-only
/// `regexp_full_match` first), `'+17'` is a sign both must accept,
/// `trim(s, chars)` must trim the same six ASCII characters `trim_matches`
/// does and no Unicode space, and an integer past `i64` must overflow to
/// NULL on both sides rather than saturating on one.
#[test]
fn severity_reading_sql_matches_the_rust_kernel_in_both_dialects() {
    use trawl_core::severity::{self, Dialect};

    let conn = conn();
    let cases: &[&str] = SEVERITY_READING_CASES;
    for dialect in [Dialect::Otel, Dialect::Syslog] {
        for text in cases {
            let escaped = text.replace('\'', "''");
            let kernel = severity::reading_text(text, dialect).map(i64::from);
            // BOTH shapes — the repeated one the conform rung emits and
            // the bind-once one `sev()` emits — answer the kernel. They
            // share an arm generator, and this is what proves the two
            // wrappers around it are the same reading.
            for build in [
                trawl_core::conform::severity_reading_sql,
                trawl_core::conform::severity_reading_sql_bind_once,
            ] {
                let sql = format!("SELECT {}", build(&format!("'{escaped}'"), dialect));
                let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
                assert_eq!(
                    engine, kernel,
                    "{dialect:?} reading disagreed on {text:?}\n{sql}"
                );
            }
        }
        // A NULL input has no reading on either side.
        let sql = format!(
            "SELECT {}",
            trawl_core::conform::severity_reading_sql("CAST(NULL AS VARCHAR)", dialect)
        );
        let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        assert_eq!(engine, None, "{dialect:?} read a NULL as a severity");
    }
}

/// The generated arms, the result TYPE, and the bind-once shape's reason
/// for existing — the half of the reading contract that is not the
/// input matrix.
#[test]
fn severity_reading_sql_generates_its_arms_and_types_as_bigint() {
    use trawl_core::severity::{self, Dialect};

    let conn = conn();
    // Every token spelling and every exact short name, through the SQL:
    // the arms are generated from the kernel's tables, so a table edit
    // that misses the generator fails here.
    for (token, number) in severity::token_entries() {
        for spelling in [token.to_owned(), token.to_uppercase()] {
            let sql = format!(
                "SELECT {}",
                trawl_core::conform::severity_reading_sql(&format!("'{spelling}'"), Dialect::Otel)
            );
            let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
            assert_eq!(engine, Some(i64::from(number)), "token {spelling}");
        }
    }
    for n in 1..=24u8 {
        let name = severity::otel_name(n).unwrap();
        let sql = format!(
            "SELECT {}",
            trawl_core::conform::severity_reading_sql(&format!("'{name}'"), Dialect::Otel)
        );
        let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        assert_eq!(engine, Some(i64::from(n)), "exact name {name}");
    }

    // The reading types as the physical BIGINT a SEVERITY column holds —
    // an INTEGER-typed conform would disagree with the parquet side and
    // throw the hot+cold union.
    for build in [
        trawl_core::conform::severity_reading_sql,
        trawl_core::conform::severity_reading_sql_bind_once,
    ] {
        let sql = format!("DESCRIBE SELECT {} AS s", build("'error'", Dialect::Otel));
        let ty: String = conn.query_row(&sql, [], |row| row.get(1)).unwrap();
        assert_eq!(ty, CanonicalType::Severity.as_duckdb());
    }

    // The bind-once shape exists so a subject carrying a BOUND PARAMETER
    // is pushed once and read once: `DuckDB` binds `?` positionally, so
    // the repeated shape would need four copies of one value — and
    // `to_json(?)` types nothing, which is why the parameter arrives
    // pre-cast (`emitter::expr::typed_literal`).
    let subject = trawl_core::conform::untyped_text("CAST(? AS VARCHAR)");
    let sql = format!(
        "SELECT {}",
        trawl_core::conform::severity_reading_sql_bind_once(&subject, Dialect::Otel)
    );
    assert_eq!(
        sql.matches('?').count(),
        2,
        "one param, one regex `?`: {sql}"
    );
    let reading: Option<i64> = conn
        .query_row(&sql, duckdb::params![" Error "], |row| row.get(0))
        .unwrap();
    assert_eq!(reading, Some(17));
}

/// The trim set is ONE set in two spellings — `severity::WHITESPACE` in
/// Rust, an RE2 class in the SQL — and this is what makes them one: every
/// character of the const is trimmed by the SQL reader, and a
/// look-alike that is NOT `White_Space` (U+200B ZERO WIDTH SPACE, U+180E,
/// U+FEFF) survives on both sides.
///
/// The wide set is deliberate: a `severity` padded with U+00A0 has a
/// reading, and an ASCII-only trim would drop it with no repair code and
/// nothing in the event to explain the loss.
#[test]
fn the_trim_set_is_the_same_on_both_engines() {
    use trawl_core::severity::{self, Dialect, WHITESPACE};

    let conn = conn();
    for c in WHITESPACE {
        let padded = format!("{c}error{c}");
        let escaped = padded.replace('\'', "''");
        for build in [
            trawl_core::conform::severity_reading_sql,
            trawl_core::conform::severity_reading_sql_bind_once,
        ] {
            let sql = format!("SELECT {}", build(&format!("'{escaped}'"), Dialect::Otel));
            let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
            assert_eq!(
                engine,
                Some(17),
                "SQL did not trim U+{:04X}\n{sql}",
                u32::from(c)
            );
        }
        assert_eq!(
            severity::reading_text(&padded, Dialect::Otel),
            Some(17),
            "Rust did not trim U+{:04X}",
            u32::from(c)
        );
    }

    // Not `White_Space`, so it is part of the value on both engines.
    for c in ['\u{200b}', '\u{180e}', '\u{feff}'] {
        let padded = format!("{c}error");
        let sql = format!(
            "SELECT {}",
            trawl_core::conform::severity_reading_sql(&format!("'{padded}'"), Dialect::Otel)
        );
        let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        assert_eq!(engine, None, "SQL trimmed U+{:04X}", u32::from(c));
        assert_eq!(
            severity::reading_text(&padded, Dialect::Otel),
            None,
            "Rust trimmed U+{:04X}",
            u32::from(c)
        );
    }
}

/// The SEVERITY conform rung is the reading kernel, and its LIVE mirror
/// (`compare::conformed_severity`) reads the same domain — executed, not
/// reasoned. Out-of-ladder numbers (`0`, `25`, `-3`) conform to NULL in
/// both engines, so a stored value always has a token rendering, and a
/// TOKEN (`error`) conforms to its number on both sides (ADR-0013 ruling
/// 10: the pin's lifetime meaning is the full reading).
#[test]
fn severity_conform_rung_bounds_the_ladder_on_both_engines() {
    let conn = conn();
    let cases: [&str; 14] = [
        "1",
        "17",
        "24",
        "0",
        "25",
        "-3",
        "  9  ",
        "017",
        "17.0",
        "1.5",
        "error",
        "",
        "nan",
        "9223372036854775808",
    ];
    for text in cases {
        let escaped = text.replace('\'', "''");
        let sql = format!(
            "SELECT {}",
            guarded_cast(&format!("'{escaped}'"), CanonicalType::Severity)
        );
        let sql_reading: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        let live = trawl_core::compare::conformed_severity(text).map(i64::from);
        assert_eq!(sql_reading, live, "severity conform disagreed on {text:?}");
    }
}

/// The SEVERITY conform rung under an asserted dialect: a repin may
/// declare that a corpus's numerals are syslog PRI, and the rung
/// it gets must be the kernel's syslog reading — not a second expression
/// that agrees on the cases somebody thought of.
///
/// Executed over the whole reading matrix in both dialects, against the
/// Rust kernel, because this is the expression an operator's `--dialect`
/// flag reaches: a drift here rewrites a corpus to numbers no lane will
/// ever read back.
#[test]
fn severity_conform_rung_reads_the_asserted_dialect() {
    use trawl_core::severity::{self, Dialect};

    let conn = conn();
    for dialect in [Dialect::Otel, Dialect::Syslog] {
        for text in SEVERITY_READING_CASES {
            let escaped = text.replace('\'', "''");
            let subject = format!("'{escaped}'");
            // The rung IS the reading, byte for byte — the same identity
            // the OTel rung has, one dialect over.
            assert_eq!(
                trawl_core::conform::guarded_cast_in(&subject, CanonicalType::Severity, dialect),
                trawl_core::conform::severity_reading_sql(&subject, dialect),
                "{dialect:?} rung is not the reading for {text:?}"
            );
            let sql = format!(
                "SELECT {}",
                trawl_core::conform::guarded_cast_in(&subject, CanonicalType::Severity, dialect)
            );
            let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
            assert_eq!(
                engine,
                severity::reading_text(text, dialect).map(i64::from),
                "{dialect:?} conform disagreed on {text:?}\n{sql}"
            );
        }
    }

    // The inversion is the whole point: the same stored numeral conforms
    // to its OTel rung under one assertion and to the syslog rung under
    // the other, so the dialect is data-changing and must be asserted.
    for (numeral, otel, syslog) in [(3i64, 3i64, 17i64), (7, 7, 5), (1, 1, 23)] {
        for (dialect, want) in [(Dialect::Otel, otel), (Dialect::Syslog, syslog)] {
            let sql = format!(
                "SELECT {}",
                trawl_core::conform::guarded_cast_in(
                    &format!("'{numeral}'"),
                    CanonicalType::Severity,
                    dialect
                )
            );
            let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
            assert_eq!(engine, Some(want), "{numeral} under {dialect:?}");
        }
    }

    // Every other pin is dialect-INVARIANT by execution as well as by
    // spelling, which is what lets one dialect ride a pin-generic plan.
    for pin in [
        CanonicalType::BigInt,
        CanonicalType::Double,
        CanonicalType::Timestamp,
        CanonicalType::Boolean,
        CanonicalType::Varchar,
    ] {
        assert_eq!(
            trawl_core::conform::guarded_cast_in("'3'", pin, Dialect::Otel),
            trawl_core::conform::guarded_cast_in("'3'", pin, Dialect::Syslog),
            "{pin:?} read a dialect"
        );
    }
}

/// The dialect-ambiguity classifier is ONE predicate in two engines:
/// `conform::severity_dialect_ambiguous_sql` in SQL,
/// `severity::dialect_ambiguous` in Rust. The repin's force gate fires on
/// the SQL half and the operator reads the Rust half's vocabulary, so a
/// divergence is a refusal (or a silent mistranslation) nobody can explain.
///
/// A one-dialect value must answer FALSE rather than NULL: an unreadable
/// value is visible loss the plan already counts as a projected null, and
/// only a value both ladders claim differently needs a human's assertion.
#[test]
fn dialect_ambiguity_pairs_across_both_engines() {
    let conn = conn();
    let ask = |subject: &str| -> Option<bool> {
        let sql = format!(
            "SELECT {}",
            trawl_core::conform::severity_dialect_ambiguous_sql(subject)
        );
        conn.query_row(&sql, [], |row| row.get(0)).unwrap()
    };

    for text in SEVERITY_READING_CASES {
        let escaped = text.replace('\'', "''");
        assert_eq!(
            ask(&format!("'{escaped}'")),
            Some(trawl_core::severity::dialect_ambiguous(text)),
            "ambiguity disagreed on {text:?}"
        );
    }
    // The overlap is exactly 1-7 on both engines, shoulders included. The
    // whole `-30..=30` sweep is pinned Rust-side (`severity::tests`); here
    // it is every rung of the overlap plus both shoulders, because each
    // case is a PREPARE of a two-reading expression and the sweep would
    // cost minutes to say the same thing.
    for n in [-30i64, -7, -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 17, 24, 25, 30] {
        let text = n.to_string();
        let engine = ask(&format!("'{text}'"));
        assert_eq!(engine, Some((1..=7).contains(&n)), "SQL misclassified {n}");
        assert_eq!(
            engine,
            Some(trawl_core::severity::dialect_ambiguous(&text)),
            "engines disagreed on {n}"
        );
    }
    // A NULL subject is not ambiguous — it is nothing at all.
    assert_eq!(ask("CAST(NULL AS VARCHAR)"), Some(false));
}

/// The canonical token TEXT is one table, rendered by
/// `conform::severity_token_text_sql` in SQL and `severity::otel_name` in
/// Rust. Probed over the whole ladder plus both out-of-range shoulders and
/// NULL, so a glob can never mean one thing live and another in batch.
#[test]
fn severity_token_text_matches_the_rust_mirror_over_the_ladder() {
    let conn = conn();
    for n in 0..=30i64 {
        let sql = format!(
            "SELECT {}",
            trawl_core::conform::severity_token_text_sql(&format!("CAST({n} AS BIGINT)"))
        );
        let rendered: Option<String> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        let expected = u8::try_from(n)
            .ok()
            .and_then(trawl_core::severity::otel_name)
            .map(str::to_owned);
        assert_eq!(rendered, expected, "token text disagreed at {n}");
    }
    let null_sql = format!(
        "SELECT {}",
        trawl_core::conform::severity_token_text_sql("CAST(NULL AS BIGINT)")
    );
    let rendered: Option<String> = conn.query_row(&null_sql, [], |row| row.get(0)).unwrap();
    assert_eq!(rendered, None, "a NULL severity has no token text");
}

/// A SEVERITY-pinned hot column conforms to the same physical BIGINT the
/// parquet side holds, so the hot+cold union types cleanly — the whole
/// reason the pin's physical spelling stays `BIGINT`.
#[test]
fn severity_conform_yields_the_physical_bigint() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hot.ndjson");
    let mut f = std::fs::File::create(&file).unwrap();
    writeln!(f, r#"{{"_severity":17}}"#).unwrap();
    writeln!(f, r#"{{"_severity":"warn"}}"#).unwrap();
    f.sync_all().unwrap();

    let conn = conn();
    let expr = conform("\"_severity\"", CanonicalType::Severity);
    let mut stmt = conn
        .prepare(&format!(
            "DESCRIBE SELECT {expr} AS s FROM {}",
            hot_reader(&file)
        ))
        .unwrap();
    let ty: String = stmt.query_row([], |row| row.get(1)).unwrap();
    assert_eq!(ty, CanonicalType::Severity.as_duckdb());

    let mut stmt = conn
        .prepare(&format!(
            "SELECT {expr} AS s FROM {} ORDER BY s NULLS LAST",
            hot_reader(&file)
        ))
        .unwrap();
    let rows: Vec<Option<i64>> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    // Both conform: the rung is the token-aware reading (ADR-0013 ruling
    // 10), so a stored `warn` is 13 and not a shelved conflict.
    assert_eq!(rows, vec![Some(13), Some(17)]);
}

/// Backticks are LEXING: the name a query spells with them is the
/// identifier in the SQL and the column name `DuckDB` hands back
/// (ADR-0013 ruling 7). Executed rather than assumed — the whole point
/// of the escape is that a name reachable in the DSL is reachable end to
/// end, so `DESCRIBE` is where that claim is settled.
#[test]
fn backticked_names_describe_as_the_names_the_dsl_spells() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data.parquet");
    let conn = conn();
    conn.execute_batch(&format!(
        "COPY (SELECT 'a' AS \"request id\", 500 AS \"http-status\", 'nginx' AS \"where\") \
         TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();
    let source = file.display().to_string();

    let describe = |sql: &str| -> Vec<String> {
        let mut stmt = conn.prepare(&format!("DESCRIBE {sql}")).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };

    for (dsl, expected) in [
        (
            "* | table `request id`, `http-status`",
            vec!["request id", "http-status"],
        ),
        ("* | stats count() by `where`", vec!["where", "count"]),
        (
            "* | rename `http-status` as `status code`",
            vec!["request id", "where", "status code"],
        ),
    ] {
        let query = trawl_core::parser::parse(dsl).unwrap();
        let emitted =
            trawl_core::emitter::emit(&query, &source, trawl_core::context::EvalContext::capture())
                .unwrap();
        assert!(emitted.params.is_empty(), "{dsl} binds no parameters");
        assert_eq!(describe(&emitted.sql), expected, "{dsl}: {}", emitted.sql);
    }
}

/// A stage after PIVOT forces the inlined PIVOT through CTE finalization.
/// The embedded newline is the fixture's point: reindenting the generated
/// SQL must not insert two spaces after it, which would silently filter
/// this row out instead of failing.
#[test]
fn pivot_cte_finalization_preserves_multiline_literal_matching() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data.parquet");
    let conn = conn();
    conn.execute_batch("CREATE TABLE logs(message VARCHAR, status BIGINT)")
        .unwrap();
    conn.execute(
        "INSERT INTO logs VALUES (?, ?)",
        duckdb::params!["a\nb", 200_i64],
    )
    .unwrap();
    conn.execute_batch(&format!(
        "COPY logs TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();

    let dsl = "message=\"a\nb\" | pivot count() on status | sort `200`";
    let query = trawl_core::parser::parse(dsl).unwrap();
    let emitted = trawl_core::emitter::emit(
        &query,
        &file.display().to_string(),
        trawl_core::context::EvalContext::capture(),
    )
    .unwrap();
    assert!(emitted.params.is_empty(), "PIVOT inlines every parameter");

    let matched: i64 = conn
        .query_row(&emitted.sql, [], |row| row.get("200"))
        .unwrap();
    assert_eq!(matched, 1, "the historical multiline value must match");
}

/// An `eventstats` alias naming an incoming column OVERWRITES it, the
/// way `let` does (ADR-0013 ruling 8 documents exactly that, which is
/// why the collision check admits the shape). Executed, because the
/// wildcard is `DuckDB`'s to expand: a plain `SELECT *, … AS status`
/// hands the original column BACK beside the window value, so the alias
/// names must leave the wildcard before the window expressions land.
#[test]
fn eventstats_alias_overwrites_the_incoming_column() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data.parquet");
    let conn = conn();
    conn.execute_batch(&format!(
        "COPY (SELECT 'nginx' AS service, 200 AS status \
         UNION ALL SELECT 'nginx', 500) TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();
    let source = file.display().to_string();

    let query = trawl_core::parser::parse("* | eventstats count() as status by service").unwrap();
    let emitted =
        trawl_core::emitter::emit(&query, &source, trawl_core::context::EvalContext::capture())
            .unwrap();

    let columns: Vec<String> = {
        let mut stmt = conn.prepare(&format!("DESCRIBE {}", emitted.sql)).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        columns,
        vec!["service", "status"],
        "one column per name: {}",
        emitted.sql
    );

    let mut stmt = conn.prepare(&emitted.sql).unwrap();
    let rows: Vec<i64> = stmt
        .query_map([], |r| r.get("status"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, vec![2, 2], "the window value replaced the original");
}

/// The overwrite holds for a CASE-VARIANT alias too, because `DuckDB`
/// binds identifiers case-insensitively while the `COLUMNS` lambda
/// compares plain strings: an unfolded `c NOT IN ('Status')` leaves the
/// incoming `status` in the wildcard and hands back TWO columns of one
/// folded name — exactly the output schema ruling 8 exists to prevent.
#[test]
fn eventstats_case_variant_alias_overwrites_the_incoming_column() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data.parquet");
    let conn = conn();
    conn.execute_batch(&format!(
        "COPY (SELECT 'nginx' AS service, 200 AS status \
         UNION ALL SELECT 'nginx', 500) TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();
    let source = file.display().to_string();

    let query = trawl_core::parser::parse("* | eventstats count() as `Status` by service").unwrap();
    let emitted =
        trawl_core::emitter::emit(&query, &source, trawl_core::context::EvalContext::capture())
            .unwrap();

    let columns: Vec<String> = {
        let mut stmt = conn.prepare(&format!("DESCRIBE {}", emitted.sql)).unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        columns,
        vec!["service", "Status"],
        "one column per folded name: {}",
        emitted.sql
    );
}

/// The overwrite projection every write site shares — `let`, `rename`,
/// `extract` and `eventstats` — compares column names with ASCII case
/// folded on both sides, and the SQL half of that fold has to be the EXACT
/// mirror of `schema::catalog_key`. `lower()` is not: it folds non-ASCII
/// too, where `DuckDB`'s own identifier binding does not — so `lower()`
/// would drop a column named `Ä` from the passthrough for an alias `ä`
/// that never names it. `translate` over the 26 ASCII letters is the
/// mirror, and all three claims run here so a future "simplify this to
/// `lower()`" refactor fails loudly instead of silently losing a column.
#[test]
fn the_columns_exclusion_fold_mirrors_catalog_key() {
    fn fold_sql(expr: &str) -> String {
        format!("translate({expr}, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz')")
    }
    let fold_c = fold_sql("c");
    let conn = conn();
    conn.execute_batch(r#"CREATE TABLE t AS SELECT 'x' AS "Service", 'y' AS "Ä";"#)
        .unwrap();

    // 1. the fold agrees with `catalog_key` on both classes of input
    for name in ["SerVice", "Ä", "ÄX", "host", "_Time", "CAFÉ"] {
        let sql = fold_sql(&format!("'{name}'"));
        let mut stmt = conn.prepare(&format!("SELECT {sql}")).unwrap();
        let folded: String = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(
            folded,
            trawl_core::schema::catalog_key(name),
            "the SQL fold must mirror catalog_key for {name:?}"
        );
    }

    // 2. a case-variant alias REPLACES the input column
    let mut stmt = conn
        .prepare(&format!(
            "DESCRIBE SELECT COLUMNS(c -> {fold_c} NOT IN ('service')), 1 AS \"Service\" FROM t"
        ))
        .unwrap();
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        columns
            .iter()
            .filter(|c| trawl_core::schema::catalog_key(c) == "service")
            .count(),
        1,
        "one `service` column, once: {columns:?}"
    );

    // 3. …and `DuckDB` really does bind identifiers ASCII-insensitively
    // but NOT beyond, which is why the mirror may not use `lower()`.
    assert!(
        conn.prepare(r#"SELECT "SERVICE" FROM t"#).is_ok(),
        "ASCII case is insensitive"
    );
    assert!(
        conn.prepare(r#"SELECT "ä" FROM t"#).is_err(),
        "non-ASCII case is NOT folded by the binder"
    );
}

// ── the numeric / type leaves (ADR-0017 §1, §4) ────────────────────
//
// What `typeof`, `tonumber`, `ceil`, `floor` and `round` answer, taken
// through the shape the batch lane actually executes, which is not the
// shape a hand-written SQL literal has: the emitter pushes one bound
// `SqlValue` per DSL literal (`emitter::emit`), so a DSL `1` reaches
// `DuckDB` as a BIGINT PARAMETER. A bare `typeof(1)` typed into SQL
// answers `INTEGER` and is simply a different question.

/// One scalar expression, executed and read back as (`typeof`, text) —
/// the two facts a value-domain mirror has to reproduce.
///
/// `expr` carries `?` placeholders and appears exactly ONCE (inside a
/// subquery), so the bound parameter list is the caller's rather than
/// silently doubled by a second mention of the same expression.
fn scalar_type_and_text(
    conn: &duckdb::Connection,
    expr: &str,
    params: &[&dyn duckdb::ToSql],
) -> Result<(String, Option<String>), String> {
    let sql = format!("SELECT typeof(x), CAST(x AS VARCHAR) FROM (SELECT ({expr}) AS x) probe");
    conn.query_row(&sql, params, |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|error| error.to_string())
}

/// The type name `DuckDB` gives a value the DSL lane BOUND, per literal
/// shape the emitter can push.
const BOUND_TYPEOF_SPELLINGS: &[&str] = &["BIGINT", "BIGINT", "DOUBLE", "BOOLEAN", "VARCHAR"];

/// The shapes that reach `DuckDB` as SQL text rather than a parameter.
///
/// The last two are recorded, not mirrored: streaming eval spells the
/// NULL type `NULL` (no quotes) and every list `ARRAY`, where `DuckDB`
/// spells a list by its element type. Both stay pinned as unruled
/// divergences in `trawl-core/tests/scalar_parity.rs`.
const LITERAL_TYPEOF_SPELLINGS: &[(&str, &str)] = &[
    ("TIMESTAMP '2026-01-15 10:20:30'", "TIMESTAMP"),
    ("NULL", "\"NULL\""),
    ("[1, 2]", "INTEGER[]"),
];

#[test]
fn typeof_spells_a_bound_dsl_literal_by_its_bound_type() {
    let conn = conn();
    let bound: Vec<Box<dyn duckdb::ToSql>> = vec![
        Box::new(1_i64),
        Box::new(i64::MAX),
        Box::new(1.5_f64),
        Box::new(true),
        Box::new("x".to_string()),
    ];
    for (value, want) in bound.iter().zip(BOUND_TYPEOF_SPELLINGS) {
        let (outer, spelling) =
            scalar_type_and_text(&conn, "typeof(?)", &[value.as_ref()]).unwrap();
        assert_eq!(outer, "VARCHAR", "typeof() itself returns text");
        assert_eq!(spelling.as_deref(), Some(*want));
    }
    for (expr, want) in LITERAL_TYPEOF_SPELLINGS {
        let (_, spelling) = scalar_type_and_text(&conn, &format!("typeof({expr})"), &[]).unwrap();
        assert_eq!(spelling.as_deref(), Some(*want), "typeof({expr})");
    }
}

#[test]
fn a_boolean_casts_to_double_as_one_and_zero() {
    // `tonumber(x)` emits `TRY_CAST(x AS DOUBLE)`, so a boolean argument
    // has a reading rather than NULL, which is what eval mirrors.
    let conn = conn();
    for (value, want) in [(true, "1.0"), (false, "0.0")] {
        let (dtype, text) =
            scalar_type_and_text(&conn, "TRY_CAST(? AS DOUBLE)", &[&value]).unwrap();
        assert_eq!(dtype, "DOUBLE");
        assert_eq!(text.as_deref(), Some(want), "TRY_CAST({value} AS DOUBLE)");
    }
}

/// (function, return type over a BIGINT argument, over a DOUBLE one).
///
/// `round` is the odd one out: it is the only one of the three that keeps
/// an integer argument integral.
const ROUNDING_RETURN_TYPES: &[(&str, &str, &str)] = &[
    ("ceil", "DOUBLE", "DOUBLE"),
    ("floor", "DOUBLE", "DOUBLE"),
    ("round", "BIGINT", "DOUBLE"),
];

/// (bound BIGINT argument, ceil text, floor text, round text).
const ROUNDING_INTEGER_MATRIX: &[(i64, &str, &str, &str)] = &[
    (5, "5.0", "5.0", "5"),
    (-5, "-5.0", "-5.0", "-5"),
    (0, "0.0", "0.0", "0"),
];

/// (bound DOUBLE argument, ceil text, floor text, round text).
///
/// The negative-zero rows are load-bearing: `DuckDB` KEEPS the sign
/// (`ceil(-0.5)` is `-0.0`, not `0.0`), which is also what Rust's
/// `f64::ceil` produces — a mirror that rounded through `i64` could not
/// express it at all. `round` is half-away-from-zero on both engines.
const ROUNDING_DOUBLE_MATRIX: &[(f64, &str, &str, &str)] = &[
    (1.5, "2.0", "1.0", "2.0"),
    (-1.5, "-1.0", "-2.0", "-2.0"),
    (0.0, "0.0", "0.0", "0.0"),
    (-0.0, "-0.0", "-0.0", "-0.0"),
    (2.5, "3.0", "2.0", "3.0"),
    (-2.5, "-2.0", "-3.0", "-3.0"),
    (0.5, "1.0", "0.0", "1.0"),
    (-0.5, "-0.0", "-1.0", "-1.0"),
];

#[test]
fn ceil_floor_and_round_split_their_return_type_on_the_argument_type() {
    let conn = conn();
    for (function, over_integer, over_double) in ROUNDING_RETURN_TYPES {
        let (dtype, _) = scalar_type_and_text(&conn, &format!("{function}(?)"), &[&5_i64]).unwrap();
        assert_eq!(&dtype, over_integer, "{function} over a BIGINT");
        let (dtype, _) =
            scalar_type_and_text(&conn, &format!("{function}(?)"), &[&1.5_f64]).unwrap();
        assert_eq!(&dtype, over_double, "{function} over a DOUBLE");
    }

    for (argument, ceil, floor, round) in ROUNDING_INTEGER_MATRIX {
        for (function, want) in [("ceil", ceil), ("floor", floor), ("round", round)] {
            let (_, text) =
                scalar_type_and_text(&conn, &format!("{function}(?)"), &[argument]).unwrap();
            assert_eq!(text.as_deref(), Some(*want), "{function}({argument})");
        }
    }
    for (argument, ceil, floor, round) in ROUNDING_DOUBLE_MATRIX {
        for (function, want) in [("ceil", ceil), ("floor", floor), ("round", round)] {
            let (_, text) =
                scalar_type_and_text(&conn, &format!("{function}(?)"), &[argument]).unwrap();
            assert_eq!(text.as_deref(), Some(*want), "{function}({argument})");
        }
    }
}

#[test]
fn round_takes_a_precision_only_as_an_inlined_integer() {
    // There is no `round(DOUBLE, BIGINT)` overload, which is exactly why
    // `emitter::functions::literal_int_positions` inlines `round`'s
    // second argument instead of binding it. Probed so a future overload
    // does not quietly make that inlining look optional.
    let conn = conn();
    let error = scalar_type_and_text(&conn, "round(?, ?)", &[&1.5_f64, &1_i64]).unwrap_err();
    assert!(
        error.contains("round(DOUBLE, BIGINT)"),
        "a bound precision must still be a binder error: {error}"
    );
    for (argument, precision, dtype, want) in [
        (1.25_f64, 1, "DOUBLE", "1.3"),
        (-1.25_f64, 1, "DOUBLE", "-1.3"),
        (1.5_f64, 0, "DOUBLE", "2.0"),
    ] {
        let (actual, text) =
            scalar_type_and_text(&conn, &format!("round(?, {precision})"), &[&argument]).unwrap();
        assert_eq!(actual, dtype);
        assert_eq!(
            text.as_deref(),
            Some(want),
            "round({argument}, {precision})"
        );
    }
    // An integer argument keeps its BIGINT shape whatever the precision.
    let (dtype, text) = scalar_type_and_text(&conn, "round(?, 1)", &[&5_i64]).unwrap();
    assert_eq!((dtype.as_str(), text.as_deref()), ("BIGINT", Some("5")));
}

// ── the arithmetic value domain (ADR-0017 §4) ──────────────────────
//
// Three claims eval.rs makes about `+ - * / %`, measured: `/` is true
// division (never truncating), division by zero answers an IEEE special
// where integer `%` by zero answers NULL, and an integer overflow is an
// ERROR — which is what licenses the streaming lane's NULL for it, since
// SSE cannot raise a per-event error and ADR-0017 §5's rule is "eval
// nulls where batch errors". The comparison rows are here for the same
// reason: once `/` can produce NaN, every comparison over one has to
// answer as DuckDB does, and DuckDB orders NaN GREATEST rather than
// leaving it unordered.

/// One scalar expression read back as a DOUBLE — the shape a text
/// comparison cannot express, since `CAST(0/0 AS VARCHAR)` renders the
/// hardware's SIGN bit (`-nan`) while the value is just NaN.
fn scalar_double(
    conn: &duckdb::Connection,
    expr: &str,
    params: &[&dyn duckdb::ToSql],
) -> Result<Option<f64>, String> {
    conn.query_row(&format!("SELECT ({expr})"), params, |row| row.get(0))
        .map_err(|error| error.to_string())
}

/// (dividend, divisor, quotient) — `/` over two BIGINTs.
///
/// The last row is the i64 promotion: both operands go through DOUBLE,
/// so a dividend above 2^53 comes back ROUNDED, and `i64::MIN / -1` —
/// the one integer division that has no i64 answer — is an ordinary
/// value rather than the overflow the `%` of the same pair raises.
const TRUE_DIVISION_MATRIX: &[(i64, i64, f64)] = &[
    (5, 2, 2.5),
    (-5, 2, -2.5),
    (4, 2, 2.0),
    (9_007_199_254_740_993, 1, 9_007_199_254_740_992.0),
    (i64::MIN, -1, 9_223_372_036_854_775_808.0),
];

#[test]
fn integer_division_is_true_division_through_double() {
    let conn = conn();
    for (dividend, divisor, quotient) in TRUE_DIVISION_MATRIX {
        let (dtype, _) = scalar_type_and_text(&conn, "? / ?", &[dividend, divisor]).unwrap();
        assert_eq!(dtype, "DOUBLE", "{dividend} / {divisor}");
        assert_eq!(
            scalar_double(&conn, "? / ?", &[dividend, divisor]).unwrap(),
            Some(*quotient),
            "{dividend} / {divisor}"
        );
    }
}

/// (dividend, divisor, remainder) — `%` over two BIGINTs, both signs.
///
/// Truncated remainder (the sign follows the DIVIDEND), which is Rust's
/// `%` and not a floored modulo.
const INTEGER_REMAINDER_MATRIX: &[(i64, i64, i64)] = &[(5, 2, 1), (-5, 2, -1), (5, -2, 1)];

/// (dividend, divisor, remainder) — `%` where either side is a DOUBLE.
const DOUBLE_REMAINDER_MATRIX: &[(f64, f64, f64)] =
    &[(5.5, 2.0, 1.5), (-5.5, 2.0, -1.5), (5.0, -2.0, 1.0)];

#[test]
fn division_by_zero_is_an_ieee_special_and_integer_modulo_by_zero_is_null() {
    let conn = conn();
    for (dividend, negative) in [(1_i64, false), (-1_i64, true)] {
        let quotient = scalar_double(&conn, "? / ?", &[&dividend, &0_i64])
            .unwrap()
            .unwrap();
        assert!(
            quotient.is_infinite() && quotient.is_sign_negative() == negative,
            "{dividend} / 0 is a signed infinity, got {quotient}"
        );
    }
    assert!(
        scalar_double(&conn, "? / ?", &[&0_i64, &0_i64])
            .unwrap()
            .unwrap()
            .is_nan(),
        "0 / 0 is NaN"
    );

    // `%` splits on the operand types where `/` does not: all-integer is
    // NULL, and any DOUBLE operand takes the IEEE path.
    let (dtype, text) = scalar_type_and_text(&conn, "? % ?", &[&5_i64, &0_i64]).unwrap();
    assert_eq!((dtype.as_str(), text), ("BIGINT", None));
    for params in [
        [&5_i64 as &dyn duckdb::ToSql, &0.0_f64],
        [&5.0_f64 as &dyn duckdb::ToSql, &0_i64],
    ] {
        assert!(
            scalar_double(&conn, "? % ?", &params)
                .unwrap()
                .unwrap()
                .is_nan(),
            "a DOUBLE operand makes `% 0` NaN, not NULL"
        );
    }

    for (dividend, divisor, remainder) in INTEGER_REMAINDER_MATRIX {
        let (dtype, text) = scalar_type_and_text(&conn, "? % ?", &[dividend, divisor]).unwrap();
        assert_eq!(dtype, "BIGINT");
        assert_eq!(text.as_deref(), Some(remainder.to_string().as_str()));
    }
    for (dividend, divisor, remainder) in DOUBLE_REMAINDER_MATRIX {
        assert_eq!(
            scalar_double(&conn, "? % ?", &[dividend, divisor]).unwrap(),
            Some(*remainder),
            "{dividend} % {divisor}"
        );
    }
}

/// (expression, both operands, the substring of the error `DuckDB` raises).
const INTEGER_OVERFLOW_MATRIX: &[(&str, i64, i64, &str)] = &[
    ("? + ?", i64::MAX, 1, "Overflow in addition of INT64"),
    ("? - ?", i64::MIN, 1, "Overflow in subtraction of INT64"),
    ("? * ?", i64::MAX, 2, "Overflow in multiplication of INT64"),
    // `%` is the only remainder that overflows, and it reports itself as
    // a DIVISION overflow.
    ("? % ?", i64::MIN, -1, "Overflow in division of"),
];

#[test]
fn integer_arithmetic_overflow_is_an_error_never_a_wrap() {
    let conn = conn();
    for (expr, lhs, rhs, expected) in INTEGER_OVERFLOW_MATRIX {
        let error = scalar_type_and_text(&conn, expr, &[lhs, rhs]).unwrap_err();
        assert!(
            error.contains(expected),
            "{lhs} {expr} {rhs}: wanted {expected:?}, got {error}"
        );
    }
    // The DOUBLE side does NOT error — it saturates to infinity, so an
    // overflow rule written over `as_f64` operands would be wrong.
    for (expr, rhs) in [("? + ?", f64::MAX), ("? * ?", 2.0)] {
        assert_eq!(
            scalar_double(&conn, expr, &[&f64::MAX, &rhs]).unwrap(),
            Some(f64::INFINITY),
            "{expr} over DOUBLEs saturates"
        );
    }
}

/// (expression, left, right, `DuckDB`'s answer) over the IEEE specials.
///
/// NaN is EQUAL to itself and GREATER than everything else — a total
/// order, not Rust's `partial_cmp` (which answers `None` and would make
/// eval null out where batch returns a row). The two zeros tie, which
/// Rust's `partial_cmp` already gets right.
const DOUBLE_COMPARISON_MATRIX: &[(&str, f64, f64, bool)] = &[
    ("? = ?", f64::NAN, f64::NAN, true),
    ("? != ?", f64::NAN, f64::NAN, false),
    ("? >= ?", f64::NAN, f64::NAN, true),
    ("? > ?", f64::NAN, f64::INFINITY, true),
    ("? < ?", f64::NAN, f64::INFINITY, false),
    ("? > ?", f64::NAN, 1e308, true),
    ("? = ?", -0.0, 0.0, true),
    ("? < ?", -0.0, 0.0, false),
    ("? = ?", f64::INFINITY, f64::INFINITY, true),
    ("? < ?", f64::NEG_INFINITY, -1e308, true),
];

#[test]
fn double_comparison_orders_nan_greatest_and_ties_the_two_zeros() {
    let conn = conn();
    for (expr, lhs, rhs, want) in DOUBLE_COMPARISON_MATRIX {
        let (dtype, text) = scalar_type_and_text(&conn, expr, &[lhs, rhs]).unwrap();
        assert_eq!(dtype, "BOOLEAN");
        assert_eq!(
            text.as_deref(),
            Some(if *want { "true" } else { "false" }),
            "{lhs} {expr} {rhs}"
        );
        // …and the live mirror, side by side with the engine: one owner
        // for this order, read by eval's comparison arms and by the
        // pinned matcher's `apply_f64`.
        let ordering = trawl_core::compare::double_total_cmp(*lhs, *rhs);
        let mirrored = match *expr {
            "? = ?" => ordering.is_eq(),
            "? != ?" => !ordering.is_eq(),
            "? > ?" => ordering.is_gt(),
            "? >= ?" => ordering.is_ge(),
            "? < ?" => ordering.is_lt(),
            other => panic!("the matrix grew an operator the mirror does not read: {other}"),
        };
        assert_eq!(
            mirrored, *want,
            "double_total_cmp disagrees: {lhs} {expr} {rhs}"
        );
    }
    // A NaN of either sign is the same value to the comparison — the
    // rendering keeps the sign bit, the ordering does not.
    let (_, text) = scalar_type_and_text(&conn, "'-nan'::DOUBLE = 'NaN'::DOUBLE", &[]).unwrap();
    assert_eq!(text.as_deref(), Some("true"));

    // …and the same order sorts, with SQL NULL after all of them.
    let mut statement = conn
        .prepare(
            "SELECT CAST(x AS VARCHAR) FROM (VALUES ('-inf'::DOUBLE), ('NaN'::DOUBLE), \
             (1.0::DOUBLE), ('inf'::DOUBLE), (CAST(NULL AS DOUBLE))) t(x) ORDER BY x",
        )
        .unwrap();
    let sorted: Vec<Option<String>> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        sorted,
        vec![
            Some("-inf".to_owned()),
            Some("1.0".to_owned()),
            Some("inf".to_owned()),
            Some("nan".to_owned()),
            None,
        ]
    );
}

// ── the condition domain of `if` / `case` (ADR-0017 §4) ────────────
//
// `DuckDB` reads an `IF`/`CASE WHEN` condition as a BOOLEAN CAST, not as
// truthiness: a string outside the boolean vocabulary is a Conversion
// ERROR, and a TIMESTAMP or a list has no cast at all. A truthiness rule
// (non-empty string is true) would silently take the THEN branch where
// the batch lane refuses the query. These probes pin the whole domain:
// which values are readable, which error, and the order `CASE` reads its
// arms in, since "the whole call is NULL" and "skip this arm" are
// different answers for a multi-arm `case`.

/// The branch `DuckDB` took, or the text of its Conversion error.
fn conditional_branch(
    conn: &duckdb::Connection,
    sql: &str,
    params: &[&dyn duckdb::ToSql],
) -> Result<Option<i64>, String> {
    conn.query_row(sql, params, |row| row.get(0))
        .map_err(|error| error.to_string())
}

/// `IF(cond, 1, 2)` and the `CASE` that must answer identically.
const IF_OVER_BOUND_CONDITION: &str = "SELECT IF(?, 1, 2)";
const CASE_OVER_BOUND_CONDITION: &str = "SELECT CASE WHEN ? THEN 1 ELSE 2 END";

/// (text, the BOOLEAN `DuckDB` casts it to — `None` = Conversion error).
///
/// The same closed, case-insensitive, UNTRIMMED vocabulary
/// `compare::try_cast_boolean` mirrors, which is why this probe asserts
/// the mirror beside the engine rather than beside a second list.
const CONDITION_STRINGS: &[(&str, Option<bool>)] = &[
    ("true", Some(true)),
    ("TRUE", Some(true)),
    ("t", Some(true)),
    ("yes", Some(true)),
    ("y", Some(true)),
    ("1", Some(true)),
    ("false", Some(false)),
    ("f", Some(false)),
    ("no", Some(false)),
    ("n", Some(false)),
    ("0", Some(false)),
    (" true ", None),
    ("nonempty", None),
    ("", None),
    ("2", None),
    ("1.0", None),
    ("on", None),
];

/// (bound BIGINT condition, the branch it takes) — zero is the only
/// false one, and a negative is TRUE.
const CONDITION_INTEGERS: &[(i64, bool)] = &[
    (0, false),
    (1, true),
    (2, true),
    (-1, true),
    (i64::MAX, true),
    (i64::MIN, true),
];

/// (bound DOUBLE condition, the branch it takes) — both zeros are false
/// and NaN is TRUE, so the reading is `!= 0`, not `> 0` and not a cast
/// through an integer.
const CONDITION_DOUBLES: &[(f64, bool)] = &[
    (0.0, false),
    (-0.0, false),
    (1.5, true),
    (-1.5, true),
    (f64::NAN, true),
    (f64::INFINITY, true),
];

#[test]
fn an_if_condition_is_a_boolean_cast_not_truthiness() {
    let conn = conn();
    let branch = |taken: bool| Some(if taken { 1 } else { 2 });

    for (text, reading) in CONDITION_STRINGS {
        let want = reading.map(|value| branch(value).expect("a branch"));
        for sql in [IF_OVER_BOUND_CONDITION, CASE_OVER_BOUND_CONDITION] {
            let answer = conditional_branch(&conn, sql, &[&(*text).to_string()]);
            if let Some(expected) = want {
                assert_eq!(answer.ok().flatten(), Some(expected), "{sql} over {text:?}");
            } else {
                let error = answer.unwrap_err();
                assert!(
                    error.contains(&format!("Could not convert string '{text}' to BOOL")),
                    "{sql} over {text:?}: {error}"
                );
            }
        }
        // The live mirror of that same cast, side by side with it.
        assert_eq!(
            trawl_core::compare::try_cast_boolean(text),
            *reading,
            "try_cast_boolean disagrees with the condition cast for {text:?}"
        );
    }

    for (value, taken) in CONDITION_INTEGERS {
        for sql in [IF_OVER_BOUND_CONDITION, CASE_OVER_BOUND_CONDITION] {
            assert_eq!(
                conditional_branch(&conn, sql, &[value]).unwrap(),
                branch(*taken),
                "{sql} over {value}"
            );
        }
    }
    for (value, taken) in CONDITION_DOUBLES {
        for sql in [IF_OVER_BOUND_CONDITION, CASE_OVER_BOUND_CONDITION] {
            assert_eq!(
                conditional_branch(&conn, sql, &[value]).unwrap(),
                branch(*taken),
                "{sql} over {value}"
            );
        }
    }

    // NULL takes the ELSE branch — it is a false condition, not an
    // unreadable one.
    for sql in [
        "SELECT IF(NULL, 1, 2)",
        "SELECT CASE WHEN NULL THEN 1 ELSE 2 END",
    ] {
        assert_eq!(
            conditional_branch(&conn, sql, &[]).unwrap(),
            Some(2),
            "{sql}"
        );
    }

    // …while a TIMESTAMP or a list has no boolean cast at all.
    for condition in ["TIMESTAMP '2026-01-15 10:20:30'", "[1, 2]"] {
        for shape in [
            format!("SELECT IF({condition}, 1, 2)"),
            format!("SELECT CASE WHEN {condition} THEN 1 ELSE 2 END"),
        ] {
            let error = conditional_branch(&conn, &shape, &[]).unwrap_err();
            assert!(
                error.contains("Unimplemented type for cast"),
                "{shape}: {error}"
            );
        }
    }
}

#[test]
fn a_case_reads_its_arms_in_order_and_stops_at_the_first_true() {
    // The distinction eval has to reproduce: an unreadable condition is
    // not "this arm does not match". It kills the whole call — unless an
    // EARLIER arm already matched, in which case DuckDB never reads it.
    let conn = conn();
    let two_arms = "SELECT CASE WHEN ? THEN 1 WHEN ? THEN 2 ELSE 3 END";

    assert_eq!(
        conditional_branch(&conn, two_arms, &[&true, &"nonempty".to_string()]).unwrap(),
        Some(1),
        "a matched first arm means the second condition is never read"
    );
    for first in [&false as &dyn duckdb::ToSql, &Option::<bool>::None] {
        let error =
            conditional_branch(&conn, two_arms, &[first, &"nonempty".to_string()]).unwrap_err();
        assert!(
            error.contains("Could not convert string 'nonempty' to BOOL"),
            "an unmatched first arm still reads the second: {error}"
        );
    }
    let error = conditional_branch(&conn, two_arms, &[&"nonempty".to_string(), &true]).unwrap_err();
    assert!(
        error.contains("Could not convert string 'nonempty' to BOOL"),
        "an unreadable FIRST arm errors whatever follows it: {error}"
    );
}

/// The stored DOUBLE column a filter really compares against, spelled so
/// every row is a value the SQL parser cannot constant-fold away: both
/// NaN SIGNS, both zeros, both infinities.
const STORED_DOUBLE_ROWS: &[&str] = &[
    "'nan'::DOUBLE",
    "-('nan'::DOUBLE)",
    "'inf'::DOUBLE",
    "'-inf'::DOUBLE",
    "1.5::DOUBLE",
    "0.0::DOUBLE",
    "0.0::DOUBLE * -1",
    "CAST(NULL AS DOUBLE)",
];

/// The same rows as [`STORED_DOUBLE_ROWS`], as (rendering, value) — what
/// the live mirror reads. `None` is the SQL NULL row, which no comparison
/// matches.
const STORED_DOUBLE_ROW_VALUES: &[(&str, Option<f64>)] = &[
    ("nan", Some(f64::NAN)),
    ("-nan", Some(-f64::NAN)),
    ("inf", Some(f64::INFINITY)),
    ("-inf", Some(f64::NEG_INFINITY)),
    ("1.5", Some(1.5)),
    ("0.0", Some(0.0)),
    ("-0.0", Some(-0.0)),
    ("NULL", None),
];

/// (operator, bound literal, the stored rows it returns — rendered).
///
/// The COLUMN shape, which is the one a pinned live filter mirrors: the
/// scalar matrix above compares two bound values, and a constant-folded
/// answer would prove nothing about a column read off parquet. Both
/// answers are the same total order — every NaN equal to every other
/// whatever its sign, NaN above `inf`, the two zeros tied, SQL NULL
/// matching nothing.
const STORED_DOUBLE_COMPARISONS: &[(&str, f64, &[&str])] = &[
    ("=", f64::NAN, &["nan", "-nan"]),
    ("=", -f64::NAN, &["nan", "-nan"]),
    (">=", f64::NAN, &["nan", "-nan"]),
    (">", f64::NAN, &[]),
    ("<", f64::NAN, &["inf", "-inf", "1.5", "0.0", "-0.0"]),
    ("!=", f64::NAN, &["inf", "-inf", "1.5", "0.0", "-0.0"]),
    (">", 1.5, &["nan", "-nan", "inf"]),
    ("<", 1.5, &["-inf", "0.0", "-0.0"]),
    ("=", 0.0, &["0.0", "-0.0"]),
    ("=", -0.0, &["0.0", "-0.0"]),
    ("=", f64::INFINITY, &["inf"]),
];

#[test]
fn a_stored_double_column_compares_in_that_same_total_order() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("metric.parquet");
    let conn = conn();
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES ({})) AS t(metric)) TO '{}' (FORMAT PARQUET)",
        STORED_DOUBLE_ROWS.join("), ("),
        file.display()
    ))
    .unwrap();

    for (op, literal, expected) in STORED_DOUBLE_COMPARISONS {
        let sql = format!(
            "SELECT CAST(metric AS VARCHAR) FROM read_parquet('{}') WHERE metric {op} ?",
            file.display()
        );
        let mut statement = conn.prepare(&sql).unwrap();
        let mut matched: Vec<String> = statement
            .query_map([literal], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        // A filtered scan has no ORDER BY, so the ROW SET is what this
        // case pins, not the order a parquet scan happens to hand back.
        // Sorted rather than set-compared, so multiplicity still counts.
        // The dedicated ordering assertion at the end of this test is the
        // one that pins an ORDER.
        matched.sort();
        let mut want: Vec<String> = expected.iter().map(|text| (*text).to_owned()).collect();
        want.sort();
        assert_eq!(matched, want, "metric {op} {literal}");

        // The mirror decides the same row set from the same order. The
        // stored rows are named by their RENDERING here, so the mirror
        // reads the value each name stands for.
        let mut mirrored: Vec<String> = STORED_DOUBLE_ROW_VALUES
            .iter()
            .filter_map(|(text, value)| {
                let ordering = trawl_core::compare::double_total_cmp((*value)?, *literal);
                let keep = match *op {
                    "=" => ordering.is_eq(),
                    "!=" => !ordering.is_eq(),
                    ">" => ordering.is_gt(),
                    ">=" => ordering.is_ge(),
                    "<" => ordering.is_lt(),
                    other => panic!("unmirrored operator {other}"),
                };
                keep.then(|| (*text).to_owned())
            })
            .collect();
        mirrored.sort();
        assert_eq!(
            mirrored, want,
            "double_total_cmp disagrees over the stored column: metric {op} {literal}"
        );
    }

    // …and the sort agrees with the comparison: the NaNs tie at the top,
    // above `inf`, with SQL NULL after all of them.
    let sql = format!(
        "SELECT CAST(metric AS VARCHAR) FROM read_parquet('{}') ORDER BY metric",
        file.display()
    );
    let mut statement = conn.prepare(&sql).unwrap();
    let sorted: Vec<Option<String>> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        sorted,
        vec![
            Some("-inf".to_owned()),
            Some("0.0".to_owned()),
            Some("-0.0".to_owned()),
            Some("1.5".to_owned()),
            Some("inf".to_owned()),
            Some("nan".to_owned()),
            Some("-nan".to_owned()),
            None,
        ]
    );
}

/// (SQL producing a NaN of a KNOWN sign, the text `DuckDB` renders).
///
/// `DuckDB`'s DOUBLE → VARCHAR cast honours the SIGN BIT, so a NaN has
/// two renderings and a mirror with one is wrong for half of them. Every
/// row here builds its NaN sign-EXPLICITLY — parsed from text, or negated
/// (which flips the bit, probed below) — never from arithmetic: the sign
/// of a computed NaN like `0.0 / 0.0` is the hardware's business and
/// differs across platforms, so pinning one would pin this machine.
const NAN_SIGN_RENDERINGS: &[(&str, &str)] = &[
    ("'nan'::DOUBLE", "nan"),
    ("'NaN'::DOUBLE", "nan"),
    ("'-nan'::DOUBLE", "-nan"),
    ("'-NAN'::DOUBLE", "-nan"),
    ("-('nan'::DOUBLE)", "-nan"),
    ("-('-nan'::DOUBLE)", "nan"),
    ("TRY_CAST('nan' AS DOUBLE)", "nan"),
    ("TRY_CAST('-nan' AS DOUBLE)", "-nan"),
];

#[test]
fn a_rendered_nan_keeps_its_sign() {
    let conn = conn();
    for (expr, want) in NAN_SIGN_RENDERINGS {
        let (dtype, text) = scalar_type_and_text(&conn, expr, &[]).unwrap();
        assert_eq!(dtype, "DOUBLE");
        assert_eq!(text.as_deref(), Some(*want), "{expr}");
    }

    // The same through a BOUND parameter, which is how a computed value
    // reaches the engine from Rust.
    for (value, want) in [
        (f64::NAN, "nan"),
        (-f64::NAN, "-nan"),
        (f64::NAN.copysign(-1.0), "-nan"),
        (f64::NAN.copysign(1.0), "nan"),
    ] {
        let (_, text) = scalar_type_and_text(&conn, "CAST(? AS DOUBLE)", &[&value]).unwrap();
        assert_eq!(
            text.as_deref(),
            Some(want),
            "bound {value} ({:?})",
            value.is_sign_negative()
        );
    }

    // Both spellings survive the DOUBLE pin's round-trip guard, so both
    // are values a conformed column really holds — which is what makes
    // the rendering a live-mirror question rather than a curiosity.
    for text in ["nan", "-nan"] {
        let conformed: Option<String> = conn
            .query_row(
                &format!(
                    "SELECT CAST({} AS VARCHAR) FROM (SELECT ? AS v) probe",
                    conform("v", CanonicalType::Double)
                ),
                [text],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(conformed.as_deref(), Some(text), "the guard keeps {text:?}");
    }
}

// ── the TIMESTAMP value domain (ADR-0017 §1) ───────────────────────
//
// How an instant RENDERS, what each date scalar answers over an
// infinity, and what an infinity looks like coming back through
// duckdb-rs (which the parity harness has to recognise). A bare
// `NaiveDateTime` cannot spell the two values a `DuckDB` TIMESTAMP holds
// beyond the calendar, which is why the mirror carries `compare::Instant`
// instead.

/// (SQL timestamp expression, its `CAST(… AS VARCHAR)`).
///
/// The SCALAR rendering, which is NOT the TIMESTAMP pin's pattern text:
/// that one is RFC 3339 with a `Z` for globbing a stored column
/// (`TIMESTAMP_PATTERN_SQL_FORMAT`), this one is the space-separated form
/// `tostring()` and a projected cell show. Two renderings, two owners; a
/// mirror that reused the pattern text here would print the wrong string
/// for every finite instant.
const TIMESTAMP_CAST_TEXTS: &[(&str, &str)] = &[
    ("TIMESTAMP '2026-01-15 09:00:00'", "2026-01-15 09:00:00"),
    (
        "TIMESTAMP '2026-01-15 09:00:00.123456'",
        "2026-01-15 09:00:00.123456",
    ),
    // Trailing fractional zeros are trimmed, exactly as the live
    // renderer trims them.
    (
        "TIMESTAMP '2026-01-15 09:00:00.100000'",
        "2026-01-15 09:00:00.1",
    ),
    ("TIMESTAMP '0001-01-01 00:00:00'", "0001-01-01 00:00:00"),
    (
        "TIMESTAMP '9999-12-31 23:59:59.999999'",
        "9999-12-31 23:59:59.999999",
    ),
    // The two instants no calendar date can express render as WORDS —
    // and both spellings of the input reach the same value.
    ("'infinity'::TIMESTAMP", "infinity"),
    ("'-infinity'::TIMESTAMP", "-infinity"),
    ("TRY_CAST('inf' AS TIMESTAMP)", "infinity"),
];

#[test]
fn a_timestamp_casts_to_the_text_duckdb_prints() {
    let conn = conn();
    for (expr, want) in TIMESTAMP_CAST_TEXTS {
        let (dtype, text) = scalar_type_and_text(&conn, expr, &[]).unwrap();
        assert_eq!(dtype, "TIMESTAMP", "{expr}");
        assert_eq!(text.as_deref(), Some(*want), "{expr}");

        // The live mirror beside the engine: the instant that text
        // denotes renders back to the same text.
        let instant = trawl_core::compare::literal_timestamp(want)
            .unwrap_or_else(|| panic!("the mirror must read {want:?}"));
        assert_eq!(instant.cast_text(), *want, "cast_text disagrees for {expr}");
    }
}

/// What each date scalar answers for `infinity` and `-infinity`.
///
/// Measured, not reasoned: the three families do three DIFFERENT things,
/// and no rule derived from one of them predicts the others.
///
/// - `date_part` is NULL for EVERY unit, `epoch` included;
/// - `date_trunc` returns the infinity UNCHANGED, for every unit;
/// - `date_diff` is NULL whenever EITHER side is infinite — including
///   both sides, and including two infinities of the same sign;
/// - `strftime` renders the WORD whatever the format asks for.
#[test]
fn the_date_scalars_answer_for_an_infinity() {
    let conn = conn();
    for (infinity, word) in [
        ("'infinity'::TIMESTAMP", "infinity"),
        ("'-infinity'::TIMESTAMP", "-infinity"),
    ] {
        for unit in trawl_core::emitter::DATE_PART_UNITS {
            let (_, text) =
                scalar_type_and_text(&conn, &format!("date_part('{unit}', {infinity})"), &[])
                    .unwrap();
            assert_eq!(text, None, "date_part('{unit}', {infinity}) must be NULL");
        }
        for unit in trawl_core::emitter::DATE_UNITS {
            let (dtype, text) =
                scalar_type_and_text(&conn, &format!("date_trunc('{unit}', {infinity})"), &[])
                    .unwrap();
            assert_eq!(dtype, "TIMESTAMP");
            assert_eq!(
                text.as_deref(),
                Some(word),
                "date_trunc('{unit}', {infinity}) must pass the infinity through"
            );
        }
        for fmt in ["%Y-%m-%d %H:%M:%S", "%Y", "%j", "%f"] {
            let (_, text) =
                scalar_type_and_text(&conn, &format!("strftime({infinity}, '{fmt}')"), &[])
                    .unwrap();
            assert_eq!(text.as_deref(), Some(word), "strftime({infinity}, '{fmt}')");
        }
    }

    let finite = "TIMESTAMP '2026-01-15 09:00:00'";
    for unit in ["year", "day", "second"] {
        for (start, end) in [
            ("'infinity'::TIMESTAMP", finite),
            (finite, "'infinity'::TIMESTAMP"),
            ("'-infinity'::TIMESTAMP", finite),
            ("'infinity'::TIMESTAMP", "'infinity'::TIMESTAMP"),
            ("'infinity'::TIMESTAMP", "'-infinity'::TIMESTAMP"),
        ] {
            let (_, text) =
                scalar_type_and_text(&conn, &format!("date_diff('{unit}', {start}, {end})"), &[])
                    .unwrap();
            assert_eq!(
                text, None,
                "date_diff('{unit}', {start}, {end}) must be NULL"
            );
        }
    }
}

/// The i64 SENTINELS duckdb-rs hands back for the two infinities.
///
/// A result cell arrives as `Value::Timestamp(Microsecond, i64)`, and the
/// infinities are the extremes of that range — note `-infinity` is
/// `-i64::MAX`, NOT `i64::MIN`. The scalar parity harness compares eval
/// against these cells, so it has to know the sentinels by value; a
/// matcher that treated them as ordinary microsecond counts would read
/// them as dates 292 thousand years out.
#[test]
fn an_infinity_timestamp_cell_is_an_i64_sentinel() {
    let conn = conn();
    for (expr, want) in [
        ("'infinity'::TIMESTAMP", i64::MAX),
        ("'-infinity'::TIMESTAMP", -i64::MAX),
    ] {
        let value: duckdb::types::Value = conn
            .query_row(&format!("SELECT {expr}"), [], |row| row.get(0))
            .unwrap();
        let duckdb::types::Value::Timestamp(unit, micros) = value else {
            panic!("{expr} must come back as a TIMESTAMP cell, got {value:?}");
        };
        assert_eq!(unit, duckdb::types::TimeUnit::Microsecond, "{expr}");
        assert_eq!(micros, want, "{expr}");
    }
}

// ── `date_part('epoch', ts)` ───────────────────────────────────────

/// Timestamp texts spanning the epoch reading's whole range: the epoch
/// itself, a pre-1970 instant with a fraction, an ordinary one, the
/// far-future fixture whose rounding this test exists for, and the ends
/// of the range BOTH engines can express.
///
/// `262143-12-31` is deliberately absent: `DuckDB` reads it and the
/// mirror does not (chrono's calendar stops at +262142), which is one of
/// [`trawl_core::compare::literal_timestamp`]'s two documented
/// one-directional residuals — under-reading, never over-reading — and
/// is guarded by `the_timestamp_mirror_residuals_are_one_directional`.
const EPOCH_MATRIX: &[&str] = &[
    "1970-01-01 00:00:00",
    "1969-12-31 23:59:59.5",
    "2026-01-15 10:20:30.123456",
    // The pinned fixture: at this magnitude one f64 ulp spans 32
    // microseconds, so the micro count ROUNDS on the way into the
    // double and the fraction disappears. Summing seconds and a
    // fraction separately gives …799.00003 instead.
    "9999-12-31 23:59:59.000016",
    "9999-12-31 23:59:59.999999",
    "0001-01-01 00:00:00",
    "262142-12-31 23:59:59",
    "-262143-01-01 00:00:00",
];

/// The live mirror under test: the instant's whole MICROSECOND count,
/// divided once ([`trawl_core::compare::Instant::epoch_seconds`], which
/// `date_part('epoch', …)` reads through in both in-memory lanes).
fn epoch_candidate(instant: trawl_core::compare::Instant) -> Option<f64> {
    instant.epoch_seconds()
}

#[test]
fn the_epoch_reading_is_the_micro_count_divided_once() {
    let conn = conn();
    for text in EPOCH_MATRIX {
        let engine: Option<f64> = conn
            .query_row(
                "SELECT date_part('epoch', TRY_CAST(? AS TIMESTAMP))",
                [*text],
                |row| row.get(0),
            )
            .unwrap();
        let engine = engine.unwrap_or_else(|| panic!("DuckDB must read {text:?}"));
        let instant = trawl_core::compare::literal_timestamp(text)
            .unwrap_or_else(|| panic!("the mirror must read {text:?}"));
        let candidate =
            epoch_candidate(instant).unwrap_or_else(|| panic!("{text:?} is a finite instant"));
        // BIT-exact, never a tolerance: an epoch that is merely close is
        // a different value to every comparison downstream of it.
        assert_eq!(
            candidate.to_bits(),
            engine.to_bits(),
            "epoch disagrees for {text:?}: engine {engine}, candidate {candidate}"
        );
    }

    // An infinity has no epoch reading on either side.
    for (sql, instant) in [
        (
            "'infinity'::TIMESTAMP",
            trawl_core::compare::Instant::Infinity,
        ),
        (
            "'-infinity'::TIMESTAMP",
            trawl_core::compare::Instant::NegInfinity,
        ),
    ] {
        let engine: Option<f64> = conn
            .query_row(&format!("SELECT date_part('epoch', {sql})"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(engine, None, "{sql}");
        assert_eq!(epoch_candidate(instant), None, "{sql}");
    }
}

// ── `%f` is SIX-DIGIT MICROSECONDS ─────────────────────────────────
//
// The one strftime/strptime specifier where `DuckDB` and chrono read the
// same letter as different units: `DuckDB`'s bare `%f` is a 6-digit
// microsecond field, chrono's is an UNSCALED NANOSECOND count (9 digits
// out, and a variable-length run read as nanoseconds in). Handing
// chrono's `%f` straight through prints `123456000` live against
// `123456` in batch, and reads `….5` as five NANOseconds where the
// engine reads half a second.

/// (fractional part of the instant, the text `strftime(ts, '%f')` gives).
///
/// Always six digits, zero-padded and zero-FILLED: a `.5` is `500000`,
/// not `5`, so the field is a fraction scaled to microseconds rather
/// than a count of them.
const PERCENT_F_RENDERINGS: &[(&str, &str)] = &[
    ("2026-01-15 09:00:00.123456", "123456"),
    ("2026-01-15 09:00:00.5", "500000"),
    ("2026-01-15 09:00:00.123", "123000"),
    ("2026-01-15 09:00:00.999999", "999999"),
    ("2026-01-15 09:00:00", "000000"),
];

#[test]
fn percent_f_is_six_digit_microseconds() {
    let conn = conn();
    for (text, want) in PERCENT_F_RENDERINGS {
        let rendered: Option<String> = conn
            .query_row(
                "SELECT strftime(TRY_CAST(? AS TIMESTAMP), '%f')",
                [*text],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rendered.as_deref(), Some(*want), "strftime({text:?}, '%f')");
    }

    // An ESCAPED percent is not a specifier: `%%f` is a literal `%` then
    // the letter `f`, so a translation that rewrote `%f` blindly would
    // corrupt it.
    for (fmt, want) in [("%%f", "%f"), ("x%%fy", "x%fy"), ("%%%f", "%123456")] {
        let rendered: Option<String> = conn
            .query_row(
                "SELECT strftime(TIMESTAMP '2026-01-15 09:00:00.123456', ?)",
                [fmt],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rendered.as_deref(), Some(want), "strftime(…, {fmt:?})");
    }
}

/// (input, the instant `strptime(input, '%Y-%m-%d %H:%M:%S.%f')` reads).
///
/// The fraction is VARIABLE length on the way in — `.5` is half a second
/// — which is the half chrono's fixed-width `%6f` cannot reproduce for
/// anything other than exactly six digits.
const PERCENT_F_PARSES: &[(&str, &str)] = &[
    ("2026-01-15 09:00:00.123456", "2026-01-15 09:00:00.123456"),
    ("2026-01-15 09:00:00.500000", "2026-01-15 09:00:00.5"),
    ("2026-01-15 09:00:00.5", "2026-01-15 09:00:00.5"),
    ("2026-01-15 09:00:00.123", "2026-01-15 09:00:00.123"),
];

#[test]
fn percent_f_parses_a_variable_length_fraction() {
    let conn = conn();
    for (input, want) in PERCENT_F_PARSES {
        let parsed: Option<String> = conn
            .query_row(
                "SELECT CAST(TRY_STRPTIME(?, '%Y-%m-%d %H:%M:%S.%f') AS VARCHAR)",
                [*input],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parsed.as_deref(), Some(*want), "strptime({input:?})");
    }
}

// ── `json_extract` returns JSON TEXT ───────────────────────────────
//
// `json_extract` yields JSON, not a decoded scalar: a string keeps its
// QUOTES, a number is its own text, a composite is compact JSON. The
// live mirror renders through `serde_json`, whose text IS `DuckDB`'s for
// every shape but one — a positive exponent, which serde spells `e+300`
// and `DuckDB` spells `e300`.

/// (document, path, the TEXT `json_extract` returns).
///
/// Every class the mirror claims to reproduce: integers, floats, the
/// exponent spellings, strings WITH escapes, booleans, JSON null,
/// arrays, objects, a nested extract, and a missing path (SQL NULL).
const JSON_EXTRACT_TEXTS: &[(&str, &str, Option<&str>)] = &[
    (r#"{"a":1}"#, "$.a", Some("1")),
    (r#"{"a":-1}"#, "$.a", Some("-1")),
    (r#"{"a":1.5}"#, "$.a", Some("1.5")),
    (r#"{"a":1.0}"#, "$.a", Some("1.0")),
    (r#"{"a":0.1}"#, "$.a", Some("0.1")),
    (r#"{"a":-0.0}"#, "$.a", Some("-0.0")),
    (r#"{"a":1e2}"#, "$.a", Some("100.0")),
    // The spelling the mirror normalizes…
    (r#"{"a":1e300}"#, "$.a", Some("1e300")),
    (r#"{"a":-1e300}"#, "$.a", Some("-1e300")),
    (
        r#"{"a":1.7976931348623157e308}"#,
        "$.a",
        Some("1.7976931348623157e308"),
    ),
    // …and the negative exponent that needs no normalizing.
    (r#"{"a":1e-7}"#, "$.a", Some("1e-7")),
    (r#"{"a":5e-324}"#, "$.a", Some("5e-324")),
    // Integers up to the width serde keeps exactly.
    (r#"{"a":9007199254740993}"#, "$.a", Some("9007199254740993")),
    (
        r#"{"a":18446744073709551615}"#,
        "$.a",
        Some("18446744073709551615"),
    ),
    // Strings keep their QUOTES and their escaping.
    (r#"{"a":"x"}"#, "$.a", Some(r#""x""#)),
    (
        r#"{"a":"he said \"hi\""}"#,
        "$.a",
        Some(r#""he said \"hi\"""#),
    ),
    (r#"{"a":"tab\there"}"#, "$.a", Some(r#""tab\there""#)),
    // A string whose CONTENT looks like an exponent must survive the
    // normalization untouched.
    (r#"{"a":"cost e+300"}"#, "$.a", Some(r#""cost e+300""#)),
    (r#"{"a":true}"#, "$.a", Some("true")),
    (r#"{"a":null}"#, "$.a", Some("null")),
    (r#"{"a":[1,2]}"#, "$.a", Some("[1,2]")),
    (
        r#"{"a":{"b":[1,{"c":2}]}}"#,
        "$.a",
        Some(r#"{"b":[1,{"c":2}]}"#),
    ),
    (r#"{"a":{"b":"x"}}"#, "$.a.b", Some(r#""x""#)),
    ("[1,2]", "$", Some("[1,2]")),
    // A missing path is SQL NULL, where a JSON `null` above is a VALUE.
    (r#"{"a":1}"#, "$.missing", None),
];

/// The live mirror — eval's OWN renderer, not a copy of it. A local
/// re-implementation would be a second version of the rule under test,
/// and the naive spelling of it (a blind `replace`) corrupts a string
/// whose CONTENT contains `e+`.
fn json_extract_mirror(doc: &str, path: &str) -> Option<String> {
    let pointer = if path == "$" {
        String::new()
    } else {
        path.trim_start_matches('$').replace('.', "/")
    };
    let value: serde_json::Value = serde_json::from_str(doc).ok()?;
    Some(trawl_core::eval::duckdb_json_text(value.pointer(&pointer)?))
}

#[test]
fn json_extract_returns_the_values_json_text() {
    let conn = conn();
    for (doc, path, want) in JSON_EXTRACT_TEXTS {
        let engine: Option<String> = conn
            .query_row("SELECT json_extract(?, ?)", [*doc, *path], |row| row.get(0))
            .unwrap();
        assert_eq!(engine.as_deref(), *want, "json_extract({doc}, {path})");
        assert_eq!(
            json_extract_mirror(doc, path).as_deref(),
            *want,
            "the mirror disagrees for json_extract({doc}, {path})"
        );
    }
}

/// The residual: `serde_json` re-renders a number from its `f64`, where
/// `DuckDB` renders from the SOURCE spelling.
///
/// Two sub-classes, one cause. An exponent-form source below 1e21 is
/// EXPANDED by `DuckDB` and kept in exponent form by serde; a digit-form
/// source too wide for `u64` keeps its digits in `DuckDB` and collapses
/// to exponent form in serde. Both need the source text, which only
/// `serde_json`'s `arbitrary_precision` feature preserves — rejected,
/// because that flag changes number handling across the whole ingest
/// path.
const JSON_NUMBER_SPELLING_RESIDUALS: &[(&str, &str, &str)] = &[
    // (document, DuckDB's text, the mirror's text)
    (r#"{"a":1e16}"#, "10000000000000000.0", "1e16"),
    (r#"{"a":1e20}"#, "100000000000000000000.0", "1e20"),
    (
        r#"{"a":100000000000000000000}"#,
        "100000000000000000000",
        "1e20",
    ),
    // …and the two that agree either side of the band, so the class is
    // bounded rather than open-ended.
    (r#"{"a":1e15}"#, "1000000000000000.0", "1000000000000000.0"),
    (r#"{"a":1e21}"#, "1e21", "1e21"),
];

#[test]
fn current_json_number_spelling_follows_serdes_f64() {
    let conn = conn();
    for (doc, engine_text, mirror_text) in JSON_NUMBER_SPELLING_RESIDUALS {
        let engine: Option<String> = conn
            .query_row("SELECT json_extract(?, '$.a')", [*doc], |row| row.get(0))
            .unwrap();
        assert_eq!(engine.as_deref(), Some(*engine_text), "{doc}");
        assert_eq!(
            json_extract_mirror(doc, "$.a").as_deref(),
            Some(*mirror_text),
            "{doc}"
        );
    }

    // A number outside `f64`'s range is a step further: serde cannot
    // PARSE the document at all, so the whole call is NULL where the
    // engine answers the number's text.
    let doc = r#"{"a":1e400}"#;
    let engine: Option<String> = conn
        .query_row("SELECT json_extract(?, '$.a')", [doc], |row| row.get(0))
        .unwrap();
    assert_eq!(engine.as_deref(), Some("1e400"));
    assert_eq!(json_extract_mirror(doc, "$.a"), None);
}

// ── the now() anchor's bound TIMESTAMP (ADR-0017 §3) ─────────────────
//
// `now()` does not emit `DuckDB`'s own clock: the statement's instant is
// captured in Rust and BOUND as a microsecond TIMESTAMP under an
// explicit `CAST(? AS TIMESTAMP)` (`emitter::functions`). Three claims
// that design rests on are measured here rather than assumed — what the
// bound parameter's TYPE is, what it RENDERS as, and that the typed
// literal the PIVOT lane inlines instead names the same instant.

/// The anchor as duckdb-rs binds it: microseconds since the epoch, the
/// domain `EvalContext` truncates to at capture.
fn bound_anchor(at: chrono::NaiveDateTime) -> duckdb::types::Value {
    duckdb::types::Value::Timestamp(
        duckdb::types::TimeUnit::Microsecond,
        at.and_utc().timestamp_micros(),
    )
}

/// The two anchor shapes the emitter can produce: a fractional instant
/// and a whole-second one (whose canonical text carries NO fraction).
fn anchor_probe_instants() -> Vec<chrono::NaiveDateTime> {
    ["2026-02-03T04:05:06.789012Z", "2026-02-03T04:05:06Z"]
        .iter()
        .map(|text| {
            chrono::DateTime::parse_from_rfc3339(text)
                .expect("a valid RFC 3339 instant")
                .naive_utc()
        })
        .collect()
}

/// A bound anchor under `CAST(? AS TIMESTAMP)` is a TIMESTAMP — read as
/// `typeof`'s own TEXT, not the driver's `Type` enum, which erases the
/// TIMESTAMP/TIMESTAMPTZ distinction the explicit cast exists to settle.
#[test]
fn a_bound_anchor_is_a_timestamp_not_a_timestamptz() {
    let conn = conn();
    for at in anchor_probe_instants() {
        let value = bound_anchor(at);
        let (dtype, _) = scalar_type_and_text(&conn, "CAST(? AS TIMESTAMP)", &[&value]).unwrap();
        assert_eq!(dtype, "TIMESTAMP", "{at}");
    }
}

/// The bound anchor renders as the canonical text the in-memory lane
/// prints for the same instant — so a `now()` cell computed by the SQL
/// prefix and one computed by the `rust_stages` tail are the same string.
#[test]
fn a_bound_anchor_renders_as_the_canonical_timestamp_text() {
    let conn = conn();
    for at in anchor_probe_instants() {
        let value = bound_anchor(at);
        let (_, text) = scalar_type_and_text(&conn, "CAST(? AS TIMESTAMP)", &[&value]).unwrap();
        assert_eq!(
            text.as_deref(),
            Some(trawl_core::eval::timestamp_to_duckdb_text(&at).as_str()),
            "{at}"
        );
    }
}

/// The PIVOT lane cannot take parameters, so it INLINES the anchor as a
/// typed literal. The literal and the bound form must be
/// indistinguishable — same type, same text — or one query shape would
/// answer `now()` differently from another.
#[test]
fn the_inlined_anchor_literal_matches_the_bound_form() {
    let conn = conn();
    for at in anchor_probe_instants() {
        let value = bound_anchor(at);
        let bound = scalar_type_and_text(&conn, "CAST(? AS TIMESTAMP)", &[&value]).unwrap();

        // The very text `SqlValue::Timestamp`'s Display (and the PIVOT
        // inliner behind it) emits, taken from the emitter rather than
        // rebuilt here — a second spelling is the drift this pins.
        let literal = trawl_core::emitter::SqlValue::Timestamp(at).to_string();
        let inlined = scalar_type_and_text(&conn, &literal, &[]).unwrap();

        assert_eq!(inlined, bound, "{literal} must denote the bound instant");
    }
}

/// A year outside `0..=9999` must still produce a literal `DuckDB` can
/// parse.
///
/// chrono SIGNS such a year — `+10000-01-01` — and `DuckDB`'s timestamp
/// parser accepts a leading `-` but not a leading `+`, so an unhandled
/// `+` makes the inlined PIVOT literal a conversion error while the bound
/// parameter for the same instant is fine: two renderings of one anchor
/// disagreeing at the edge of the domain. Unreachable from a production
/// clock, but `EvalContext::at` is public.
#[test]
fn the_anchor_literal_parses_at_every_year_chrono_can_render() {
    let conn = conn();
    for (year, month, day) in [
        (-1_i32, 1_u32, 1_u32),
        (1, 1, 1),
        (9999, 12, 31),
        (10_000, 1, 1),
        (99_999, 1, 1),
    ] {
        let at = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .expect("a valid date")
            .and_hms_micro_opt(0, 0, 0, 0)
            .expect("a valid time");
        let literal = trawl_core::emitter::SqlValue::Timestamp(at).to_string();
        let inlined = scalar_type_and_text(&conn, &literal, &[])
            .unwrap_or_else(|error| panic!("{literal} must parse: {error}"));
        assert_eq!(inlined.0, "TIMESTAMP", "{literal}");

        // And it must denote the same instant the parameter binds — the
        // whole point of there being ONE rendering.
        let bound = scalar_type_and_text(&conn, "CAST(? AS TIMESTAMP)", &[&bound_anchor(at)])
            .unwrap_or_else(|error| panic!("the bound form of {literal} must run: {error}"));
        assert_eq!(inlined, bound, "{literal} must denote the bound instant");
    }
}

// ---------------------------------------------------------------------------
// The absolute time window is half-open: [earliest, latest)
// ---------------------------------------------------------------------------

/// Bind an emitted query's parameters the way the executor does.
fn bound(emitted: &trawl_core::emitter::EmittedQuery) -> Vec<Box<dyn duckdb::ToSql>> {
    emitted
        .params
        .iter()
        .map(|value| -> Box<dyn duckdb::ToSql> {
            match value {
                trawl_core::emitter::SqlValue::String(value) => Box::new(value.clone()),
                trawl_core::emitter::SqlValue::Int(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Float(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Bool(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Timestamp(value) => {
                    Box::new(duckdb::types::Value::Timestamp(
                        duckdb::types::TimeUnit::Microsecond,
                        value.and_utc().timestamp_micros(),
                    ))
                }
            }
        })
        .collect()
}

/// Run a DSL query's EMITTED sql over `file` and return the matching ids.
/// The WHERE clause is never handwritten here: the point is what the
/// emitter produces, executed.
fn matching_ids(conn: &duckdb::Connection, file: &std::path::Path, dsl: &str) -> Vec<String> {
    let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl} parses: {e:?}"));
    let emitted = trawl_core::emitter::emit_with_pins(
        &query,
        &file.display().to_string(),
        &trawl_core::schema::FieldTypes::new(),
        trawl_core::context::EvalContext::capture(),
    )
    .unwrap_or_else(|e| panic!("{dsl} emits: {e:?}"));
    let params = bound(&emitted);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let sql = format!("SELECT id FROM ({}) AS matched ORDER BY id", emitted.sql);
    let mut statement = conn
        .prepare(&sql)
        .unwrap_or_else(|e| panic!("{dsl} prepares:\n{sql}\n{e}"));
    statement
        .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
        .unwrap_or_else(|e| panic!("{dsl} runs:\n{sql}\n{e}"))
        .map(Result::unwrap)
        .collect()
}

/// The window `earliest=`/`latest=` describes is half-open, and this is
/// the boundary microsecond that says so: an event stamped exactly `T`
/// is INSIDE `earliest="T"` and OUTSIDE `latest="T"` (ADR-0018 ruling
/// 10). That is what lets a scheduler tile consecutive report windows
/// `[a, b)`, `[b, c)` without the event at `b` landing in both runs.
///
/// Executed against real parquet through the emitter's own SQL, because
/// a bound this design leans on should be pinned by what `DuckDB`
/// answers, not by reading `>=` and `<` in the emitter.
#[test]
fn the_absolute_time_window_is_half_open_at_the_microsecond() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("events.parquet");
    let conn = conn();
    // T = 2026-03-14T03:00:00.123456Z, and its two microsecond neighbours.
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
         ('before', TIMESTAMP '2026-03-14 03:00:00.123455'), \
         ('at', TIMESTAMP '2026-03-14 03:00:00.123456'), \
         ('after', TIMESTAMP '2026-03-14 03:00:00.123457')) AS t(id, \"_time\")) \
         TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();

    assert_eq!(
        matching_ids(&conn, &file, r#"earliest="2026-03-14T03:00:00.123456Z""#),
        vec!["after".to_owned(), "at".to_owned()],
        "earliest= is inclusive: the event AT the bound matches"
    );
    assert_eq!(
        matching_ids(&conn, &file, r#"latest="2026-03-14T03:00:00.123456Z""#),
        vec!["before".to_owned()],
        "latest= is exclusive: the event AT the bound does not match"
    );
    assert_eq!(
        matching_ids(
            &conn,
            &file,
            r#"earliest="2026-03-14T03:00:00.123456Z" latest="2026-03-14T03:00:00.123457Z""#
        ),
        vec!["at".to_owned()],
        "a one-microsecond window holds exactly the event at its start"
    );
}
