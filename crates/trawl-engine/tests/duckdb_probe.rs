// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine-assumption probe (ADR-0009 slice 2, ADR-0011): every claim the
//! conform path makes about `DuckDB` is executed here against the bundled
//! engine, never assumed — the cast domains, the round-trip guard, the
//! `read_json` inference classes a hot snapshot can produce, and the
//! agreement between the two conform lanes and their live mirrors.
//!
//! # The probe matrix is the contract
//!
//! Where a live mirror in [`trawl_core::compare`] claims to reproduce a
//! `DuckDB` reading, the pair runs here side by side over a matrix of
//! inputs, and **a divergence the matrix does not name is a bug in the
//! mirror** — not a tolerance, not an edge case, and never something to
//! be worked around at the call site. The reason is that both engines
//! answer the same user question: the batch query reads the CONFORMED
//! column off disk while the live tail reads the wire JSON, so a mirror
//! that is merely close makes a stream fire on events the equivalent
//! query drops (or drop events it returns) with nothing in the request
//! to explain it.
//!
//! When a new divergence turns up: add the input to the matrix, then fix
//! the mirror against what the engine actually does. Every divergence
//! that survives is a DELIBERATE, one-directional residual with its cost
//! written down and its own test asserting it stays one-directional (see
//! [`the_timestamp_mirror_residuals_are_one_directional`]) — the mirror
//! may under-read what `DuckDB` reads, never the reverse.
//!
//! Establishing ground truth by execution comes FIRST. The four
//! timestamp divergences ADR-0011 ruling #4 fixed (`epoch`, a trailing
//! ` UTC`, hour-24 rollover, and `T09:00+00:00` firing live while batch
//! stored NULL) were all in a mirror whose rules had been reasoned out
//! from a parser's documentation instead.

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

/// The conform BOTH lanes emit: one text form, one guard. Mirrors
/// `trawl_server::ingest::compaction::conform_expr`, which now differs
/// from the emitter's hot-branch `REPLACE` list in nothing at all — the
/// `dtype` it `DESCRIBE`s decides only whether a column is already its
/// pin, never how the column is read.
fn conform(quoted: &str, pin: CanonicalType) -> String {
    guarded_cast(&untyped_text(quoted), pin)
}

/// Issue #91: execute the emitted negated-list predicate against a real
/// VARCHAR parquet column. Every element must use the existing dual
/// text/DECIMAL equality rule, and the search-stage NULL widening must
/// survive their AND composition.
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
    let emitted =
        trawl_core::emitter::emit_with_pins(&query, &file.display().to_string(), &pins).unwrap();
    let params: Vec<Box<dyn duckdb::ToSql>> = emitted
        .params
        .iter()
        .map(|value| -> Box<dyn duckdb::ToSql> {
            match value {
                trawl_core::emitter::SqlValue::String(value) => Box::new(value.clone()),
                trawl_core::emitter::SqlValue::Int(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Float(value) => Box::new(*value),
                trawl_core::emitter::SqlValue::Bool(value) => Box::new(*value),
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
    // Confirm the inference classes are the ones we think we are probing.
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
/// round-trip check (ADR-0009 slice 2 amendment): `TRY_CAST` to a numeric
/// type ROUNDS rather than fails — `1.5 → 2` counts as a "success" — so a
/// bare `count(TRY_CAST(...))` would score a fractional batch ≥90% BIGINT
/// and silently round every value on write, forever, with no
/// `field_conflicts` row. This pins BOTH halves across the inference
/// classes the WAL can produce (VARCHAR, JSON-mixed, HUGEINT numeric):
///
/// 1. the PREMISE: the unguarded cast really does round (a `DuckDB` bump
///    that makes it fail instead would let the guard be simplified), and
/// 2. the GUARD as both lanes emit it
///    ([`trawl_core::conform::guarded_cast`]): BIGINT compares the cast
///    against the value's canonical text re-parsed as `DECIMAL(38,6)` —
///    exact across the whole BIGINT range, where a DOUBLE-space comparison
///    is blind above 2^53 (both sides collapse to one double, so
///    `1735689600123456710.7` conformed BIGINT silently); representation
///    drift is still tolerated (`4.0 → 4`, `"042" → 42`), as is a fraction
///    below DECIMAL(38,6)'s half-microstep (`4.0000001 → 4` — the
///    residual, documented tolerance). DOUBLE stays in DOUBLE space
///    (`u64::MAX` → DOUBLE with precision loss, which the AC requires);
///    BOOLEAN compares strict text (`true`/`false` only, so `1` never
///    conforms to a BOOLEAN pin).
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
    // to DOUBLE — the DOUBLE rung deliberately tolerates the precision loss
    // the AC requires (a u64-range batch pins DOUBLE).
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

    // BOOLEAN: TRY_CAST(true AS BIGINT) = 1 makes the BIGINT rung score
    // booleans under the OLD counting; with the guard they fail BIGINT and
    // pass only the strict-text BOOLEAN rung — the rung is reachable now.
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
/// (`1735689600123456710.7`) "round-tripped" and conformed BIGINT as
/// `...711` — the silent-rounding failure mode the guard exists to refuse,
/// moved up the number line. This pins the boundary classes: 2^53 ± 1 must
/// stay exact accepts, the >2^53 fractional must fail, a >2^53 INTEGER must
/// still pass (DECIMAL is exact where DOUBLE-space merely could not
/// distinguish), `u64::MAX` still NULLs the cast, and the residual
/// tolerance — a fraction below DECIMAL(38,6)'s half-microstep quantizes
/// away (`4.0000001 → 4` accepted, `4.5` refused) — is deliberate and
/// documented, not an accident a bump may silently change.
#[test]
fn bigint_round_trip_is_exact_in_decimal_space_beyond_2_pow_53() {
    let conn = conn();

    // The DOUBLE-space blindness this replaces, kept as the premise.
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
    // that the BIGINT rung now refuses still passes DOUBLE (it pins DOUBLE
    // with DOUBLE's precision, which is what pinning DOUBLE means).
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
/// the execution evidence for folding field names at every producer's door
/// (ADR-0009) rather than trying to pin either spelling once both have
/// reached a snapshot.
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

/// Two engine facts behind the fold-at-every-producer-door policy
/// (ADR-0009 — `envelope::canonicalize`, `syslog::convert`'s SD keys,
/// telemetry's `JsonVisitor`):
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

// ── ADR-0011 slice A: pin-aware comparison rules, execution-evidenced ──
//
// One probe per emission rule, against real VARCHAR and BIGINT columns —
// including the PRE-existing pin-blind behavior being replaced, so the
// change is evidenced (and a DuckDB bump that alters the implicit-cast
// outcome surfaces here, not in production).

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

/// PRE-existing behavior (replaced by the slice-A rules): pin-blind
/// emission binds an INTEGER parameter against the VARCHAR column, and
/// the outcome is an ERROR either way — `=`/IN make `DuckDB` cast the
/// COLUMN to INT64 and the first word value throws a Conversion error;
/// ordered comparisons refuse to bind at all (Binder: "Cannot compare
/// values of type VARCHAR and type BIGINT"). `status=200` against a
/// VARCHAR-pinned column was breakage, not a filter. Pinned here with
/// bound parameters (exactly what the emitter produces) so a `DuckDB` bump
/// that changes the implicit-cast outcome surfaces.
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

/// The reported finding, as a regression: the VARCHAR-pinned equality and
/// `!=` shapes the emitter builds, run over ids that differ by one.
///
/// In DOUBLE space every id above 2^53 collapses onto its neighbours, so
/// `id=1737000000123456789` returned THREE distinct stored ids and
/// `id!=9007199254740993` silently suppressed `9007199254740992`. Both
/// engines collapsed identically, which is why parity testing never saw
/// it — only execution against stored data does. The DOUBLE half is kept
/// as the premise, not as nostalgia: it is what makes the DECIMAL
/// assertions mean something.
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

/// PRE-existing behavior on the pattern rule: GLOB and `regexp_matches`
/// both REFUSE a numeric column outright (Binder: no `~~~(INTEGER,
/// UNKNOWN)` / `regexp_matches(INTEGER, UNKNOWN)` overload) — glob on a
/// numeric pin was an error, not a text match. The explicit CAST is what
/// makes the pattern rules work at all.
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
    // Integral values ABOVE 2^53 written as a fraction or an
    // exponent. The mirror used to read these through `f64` and
    // answered nothing where both batch lanes hold the integer —
    // including the value the pin ladder's own text-first test
    // blesses (`compaction::pin_ladder_beyond_2_pow_53_*`).
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

/// Every text shape whose TIMESTAMP reading the two engines must agree on
/// — THE MATRIX referred to by the module doc's contract.
///
/// Grouped by the rule each row exercises, and deliberately including the
/// shapes nobody would write on purpose: the ones that cost us were
/// `epoch`, a trailing ` UTC`, hour-24 rollover and `T09:00+00:00`, none
/// of which anyone predicted. Add rather than replace.
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
    // A seconds-less time must END the text — the false-POSITIVE
    // direction, where the mirror used to fire and batch NULLed.
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
/// (`crate::filter`), executed: a BOOLEAN column takes the cast's WIDE
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
/// The matrix is the shapes that used to disagree — a boolean spelling the
/// cast reads but the rendering does not round-trip, a fraction the cast
/// rounds, a leading-zero integer, the whitespace and `_` widenings, a
/// numeric that a JSON-inferred column casts to `true` — read three ways
/// over the same texts:
///
/// 1. through a snapshot where the field holds ONLY strings (VARCHAR), and
/// 2. through one where a JSON number shares the field (JSON), and
/// 3. through the live mirror (`compare::conformed_*`).
///
/// All three must agree — in ONE expression, since both lanes now build
/// the same one. The PREMISE the probe carries with it is why that is not
/// automatic: the bare cast — what the hot branch applied before it went
/// text-first — answers differently in the two classes, so one event's
/// reading depended on what happened to share its buffer.
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
        // The premise: the bare cast the hot branch used to apply reads
        // the two inference classes differently.
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
/// Both halves are pinned, because both were wrong before: the plain
/// TIMESTAMP cast IGNORES an offset (storing `09:00` for
/// `09:00:00+05:30`, where `read_json`'s own inference stored `03:30`),
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
/// `note=/^1e/` matched and the same query stopped matching minutes later,
/// when the compactor rewrote the value it had already shown.
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
    // Under the VARCHAR pin that difference IS the stored value — the
    // regression this test exists for.
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
    // the divergence hid for as long as it did.
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
// Slice A′ (issue #66): the pinned comparison shapes in NEW positions —
// SELECT-list values (`| let`) and the LIKE/ILIKE pattern operators.
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

/// PRE-A′ regression, pinned: the pin-blind SELECT-list comparison a
/// `| let x = status > 400` emitted binds an INTEGER against the VARCHAR
/// column and REFUSES to bind — the whole query errors. This is the
/// error the slice-A′ rules replace with the three-valued answer above.
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
// Slice B (issue #53): the repin rewrite's engine assumptions.
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
/// resolves as itself. The case-variant fallback (`_raw` written before the
/// ingest fold, or by a client whose spelling the fold collapsed) recovers a
/// mixed-case original, and the exact spelling wins when both exist.
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

/// The misfit-sample capture shape (ADR-0011 slice C1): what compaction
/// runs, per conflicted column, in the same phase as the conflict tally —
/// the only moment the value the conform is about to null is still in the
/// table.
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
    // `lower('İ')` is `i`, so an ungated token CASE read `İNFO` as 9
    // in SQL and as nothing in Rust. Both dotted/dotless Turkish i,
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
    // token was accepted by ingest before the kernel landed, and a
    // narrowing here would drop those readings silently.
    "\u{a0}error\u{a0}",
    "\u{2003}error",
    "\u{3000}error\u{3000}",
    "\u{85}error",
    "\u{202f}\u{2009}error\t",
    "\u{a0}17\u{a0}",
    "er\u{a0}ror",
];

/// The reading matrix (ADR-0013 slice 2, ruling 9): `severity_reading_sql`
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
/// The wide set is deliberate. Ingest trimmed with `str::trim` before the
/// kernel landed, so a `severity` padded with U+00A0 had a reading; an
/// ASCII-only trim would have dropped it with no repair code and nothing
/// in the event to explain the loss.
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

/// The SEVERITY conform rung under an ASSERTED dialect (issue #79): a
/// repin may declare that a corpus's numerals are syslog PRI, and the rung
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

/// The dialect-ambiguity classifier is ONE predicate in two engines
/// (issue #79): `conform::severity_dialect_ambiguous_sql` in SQL,
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
        let emitted = trawl_core::emitter::emit(&query, &source).unwrap();
        assert!(emitted.params.is_empty(), "{dsl} binds no parameters");
        assert_eq!(describe(&emitted.sql), expected, "{dsl}: {}", emitted.sql);
    }
}

/// A stage after PIVOT forces the inlined PIVOT through CTE finalization.
/// Execute the historical multiline value end to end: reindentation used to
/// insert two spaces after the embedded newline and silently filter this row.
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
    let emitted = trawl_core::emitter::emit(&query, &file.display().to_string()).unwrap();
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
    let emitted = trawl_core::emitter::emit(&query, &source).unwrap();

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
    let emitted = trawl_core::emitter::emit(&query, &source).unwrap();

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
