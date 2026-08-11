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

/// The conform compaction emits for a column it `DESCRIBE`d — the typed
/// text form, then the shared guard. Mirrors
/// `trawl_server::ingest::compaction::canonical_text`, which is the ONLY
/// thing the two lanes do differently.
fn compaction_conform(quoted: &str, dtype: &str, pin: CanonicalType) -> String {
    let text = if dtype == "JSON" {
        format!("json_extract_string({quoted}, '$')")
    } else {
        format!("TRY_CAST({quoted} AS VARCHAR)")
    };
    guarded_cast(&text, pin)
}

/// The conform the emitter's hot-branch `REPLACE` list emits — the untyped
/// text form, then the same guard.
fn hot_conform(quoted: &str, pin: CanonicalType) -> String {
    guarded_cast(&untyped_text(quoted), pin)
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
    let guard_bigint_json = compaction_conform("m", "JSON", CanonicalType::BigInt);
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
        let guard = compaction_conform("v", "VARCHAR", CanonicalType::BigInt);
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
                compaction_conform("u", "HUGEINT", CanonicalType::BigInt),
                compaction_conform("u", "HUGEINT", CanonicalType::Double),
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
                compaction_conform("b", "JSON", CanonicalType::BigInt),
                compaction_conform("b", "JSON", CanonicalType::Boolean),
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
        let guard = compaction_conform("v", "VARCHAR", CanonicalType::Timestamp);
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
        let guard = compaction_conform("v", "VARCHAR", CanonicalType::BigInt);
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
    let inputs = [
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
    ];
    for input in inputs {
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
    let guard = compaction_conform("v", "VARCHAR", CanonicalType::BigInt);
    let inputs = [
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
    ];
    for input in inputs {
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
    let guard = compaction_conform("v", "VARCHAR", CanonicalType::Boolean);
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
    let conform = compaction_conform("v", "VARCHAR", CanonicalType::Timestamp);
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
/// 2. **years outside chrono's calendar** (±262 143) where `DuckDB`'s
///    microsecond range reaches ±~290 000.
///
/// This test also proves the first residual is drawn where the mirror
/// claims: every name in its table really is UTC to `DuckDB`, at two
/// instants a century apart.
#[test]
fn the_timestamp_mirror_residuals_are_one_directional() {
    let conn = conn();
    let fmt = trawl_core::compare::TIMESTAMP_PATTERN_SQL_FORMAT;
    let conform = compaction_conform("v", "VARCHAR", CanonicalType::Timestamp);

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
/// All three must agree, in both lanes. The PREMISE the probe carries with
/// it is why that is not automatic: the bare cast — what the hot branch
/// applied before it went text-first — answers differently in the two
/// classes, so one event's reading depended on what happened to share its
/// buffer.
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
        for (lane, expr) in [
            ("hot/VARCHAR", hot_conform("s", pin)),
            ("hot/JSON", hot_conform("m", pin)),
            (
                "compaction/VARCHAR",
                compaction_conform("s", "VARCHAR", pin),
            ),
            ("compaction/JSON", compaction_conform("m", "JSON", pin)),
        ] {
            assert_eq!(read(&expr), expected, "{lane} conform for {pin:?}");
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
        hot_conform("m", CanonicalType::Boolean),
        hot_conform("n", CanonicalType::Boolean),
        compaction_conform("m", "JSON", CanonicalType::Boolean),
        compaction_conform("n", "BIGINT", CanonicalType::Boolean),
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
    let guard = compaction_conform("v", "VARCHAR", CanonicalType::Timestamp);
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
    let guarded = compaction_conform("v", "VARCHAR", CanonicalType::Timestamp);
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

/// The two lanes derive the column's TEXT differently — compaction knows
/// the observed type and renders it with `CAST(x AS VARCHAR)`, the emitter
/// does not and renders through `to_json` — and for exactly one class
/// those spellings differ: a DOUBLE is `1.7356896001234568e+18` to one and
/// `1735689600123456800.0` to the other.
///
/// They must still conform to the SAME value, or the lanes disagree by the
/// back door. They do, because both renderings are shortest-round-trip and
/// every rung's cast reads them alike — which is what lets compaction keep
/// the cheaper rendering instead of paying for JSON on every value of
/// every ladder candidate of every column.
#[test]
fn the_two_lane_text_forms_conform_a_double_identically() {
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
    for pin in [
        CanonicalType::BigInt,
        CanonicalType::Double,
        CanonicalType::Boolean,
        CanonicalType::Timestamp,
    ] {
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
        if pin == CanonicalType::BigInt {
            assert_ne!(
                read("CAST(v AS VARCHAR)"),
                read(&untyped_text("v")),
                "premise: the two text renderings of a DOUBLE differ"
            );
        }
        assert_eq!(
            read(&compaction_conform("v", "DOUBLE", pin)),
            read(&hot_conform("v", pin)),
            "the lanes must conform a DOUBLE identically under {pin:?}"
        );
    }
}
