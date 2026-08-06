// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine-assumption probe (ADR-0009 slice 2): the untyped VARCHAR-pin
//! conform expression `json_extract_string(to_json(x), '$')` must yield
//! UNQUOTED strings over `read_json` columns of every inference class the
//! hot snapshot can produce (VARCHAR, JSON from mixed values, BIGINT).
//! The emitter has no DESCRIBE, so this expression is applied untyped.

use std::io::Write as _;

fn hot_reader(path: &std::path::Path) -> String {
    format!(
        "read_json('{}', format='newline_delimited', records=true, \
         auto_detect=true, field_appearance_threshold=0)",
        path.display()
    )
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
/// 2. the GUARD as compaction emits it (`conform_expr`): BIGINT compares
///    the cast against the value's canonical text re-parsed as
///    `DECIMAL(38,6)` — exact across the whole BIGINT range, where a
///    DOUBLE-space comparison is blind above 2^53 (both sides collapse to
///    one double, so `1735689600123456710.7` conformed BIGINT silently);
///    representation drift is still tolerated (`4.0 → 4`, `"042" → 42`),
///    as is a fraction below DECIMAL(38,6)'s half-microstep
///    (`4.0000001 → 4` — the residual, documented tolerance). DOUBLE
///    compares in DOUBLE space (`u64::MAX` → DOUBLE with precision loss,
///    which the AC requires); TIMESTAMP compares in TIMESTAMP space
///    (tolerating format drift — RFC 3339 `T`/`Z` vs `DuckDB`'s
///    space-separated rendering); BOOLEAN compares strict text
///    (`true`/`false` only, so `1` never conforms to a BOOLEAN pin).
#[test]
#[allow(clippy::too_many_lines)] // one probe per comparison-space decision, kept together
fn typed_casts_round_so_the_conform_guard_must_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();

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

    // --- the guard, exactly as conform_expr emits it ---
    // canon(x) is json_extract_string(x,'$') for a JSON column, else
    // CAST(x AS VARCHAR).
    let guard_bigint_json = "(CASE WHEN TRY_CAST(json_extract_string(m, '$') AS DECIMAL(38,6)) = \
          TRY_CAST(TRY_CAST(m AS BIGINT) AS DECIMAL(38,6)) \
          THEN TRY_CAST(m AS BIGINT) END)";
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
        let mut stmt = conn
            .prepare(
                "SELECT v, (CASE WHEN TRY_CAST(CAST(v AS VARCHAR) AS DECIMAL(38,6)) = \
                 TRY_CAST(TRY_CAST(v AS BIGINT) AS DECIMAL(38,6)) \
                 THEN TRY_CAST(v AS BIGINT) END) \
                 FROM (VALUES ('42'), ('042'), ('1.5'), ('n/a')) t(v) ORDER BY v",
            )
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

    // HUGEINT class: u64::MAX must fail BIGINT but pass the DOUBLE guard —
    // DOUBLE-space comparison deliberately tolerates the precision loss the
    // AC requires (a u64-range batch pins DOUBLE).
    let (bi_ok, db_ok): (i64, i64) = conn
        .query_row(
            &format!(
                "SELECT count(CASE WHEN TRY_CAST(CAST(u AS VARCHAR) AS DECIMAL(38,6)) = \
                        TRY_CAST(TRY_CAST(u AS BIGINT) AS DECIMAL(38,6)) THEN 1 END)::BIGINT, \
                        count(CASE WHEN TRY_CAST(CAST(u AS VARCHAR) AS DOUBLE) = \
                        TRY_CAST(u AS DOUBLE) THEN 1 END)::BIGINT FROM {reader} \
                 WHERE CAST(u AS VARCHAR) = '18446744073709551615'"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (bi_ok, db_ok),
        (0, 1),
        "u64::MAX: BIGINT round-trip fails (cast NULLs), DOUBLE round-trip passes"
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
                        count(CASE WHEN TRY_CAST(json_extract_string(b,'$') AS DECIMAL(38,6)) = \
                        TRY_CAST(TRY_CAST(b AS BIGINT) AS DECIMAL(38,6)) THEN 1 END)::BIGINT, \
                        count(CASE WHEN CAST(TRY_CAST(b AS BOOLEAN) AS VARCHAR) = \
                        json_extract_string(b,'$') THEN 1 END)::BIGINT FROM {breader}"
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

    // TIMESTAMP: TIMESTAMP-space comparison tolerates format drift (RFC
    // 3339 'T'/'Z' vs DuckDB's rendering) — a parse failure still NULLs.
    let ts_cases: Vec<(String, bool)> = {
        let mut stmt = conn
            .prepare(
                "SELECT v, (TRY_CAST(CAST(v AS VARCHAR) AS TIMESTAMP) = \
                 TRY_CAST(v AS TIMESTAMP)) IS NOT DISTINCT FROM true \
                 FROM (VALUES ('2024-01-15T09:00:00Z'), ('2024-01-15'), ('yesterday-ish')) t(v) \
                 ORDER BY v",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        ts_cases,
        vec![
            ("2024-01-15".into(), true),
            ("2024-01-15T09:00:00Z".into(), true),
            ("yesterday-ish".into(), false),
        ],
        "TIMESTAMP round-trip is format-tolerant, parse failures still fail"
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
    let conn = duckdb::Connection::open_in_memory().unwrap();

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
        let mut stmt = conn
            .prepare(
                "SELECT v, (CASE WHEN TRY_CAST(CAST(v AS VARCHAR) AS DECIMAL(38,6)) = \
                 TRY_CAST(TRY_CAST(v AS BIGINT) AS DECIMAL(38,6)) \
                 THEN TRY_CAST(v AS BIGINT) END) \
                 FROM (VALUES ('9007199254740991'), ('9007199254740992'), \
                              ('9007199254740993'), ('1735689600123456710.7'), \
                              ('1735689600123456710'), ('18446744073709551615'), \
                              ('9223372036854775807'), ('-9223372036854775808'), \
                              ('4.0000001'), ('4.5')) t(v)",
            )
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

/// Rule: VARCHAR pin + ordered numeric literal → `TRY_CAST(v AS DOUBLE)`.
/// Numeric-looking strings order numerically, non-numeric values are NULL
/// (excluded), and nothing throws.
#[test]
fn varchar_try_cast_double_orders_numerically_and_nulls_words() {
    let conn = varchar_status_conn();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE TRY_CAST(v AS DOUBLE) >= 400"
        )
        .unwrap(),
        2, // '404', '500'; 'accepted' NULLs out, '200'/'1.5' below
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE TRY_CAST(v AS DOUBLE) > 1"
        )
        .unwrap(),
        4, // everything numeric except nothing — '1.5','200','404','500'
    );
    // TRY_CAST(v) of 'accepted' is NULL: excluded from BOTH sides of the
    // comparison, never an error.
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE TRY_CAST(v AS DOUBLE) < 1000"
        )
        .unwrap(),
        4
    );
}

/// The ordered-numeric rung's DOMAIN, run on both engines side by side:
/// `TRY_CAST(v AS DOUBLE)` in `DuckDB` against `compare::try_cast_double`
/// in the live matcher. `str::parse::<f64>` is NOT that domain — `DuckDB`
/// trims ASCII whitespace and honours `_` digit separators — and every
/// disagreement costs the stream a row the batch query returns.
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

/// `DuckDB`'s DOUBLE ordering is TOTAL — NaN sits above every value
/// (including `inf`) and equals itself, `-0.0` equals `0.0` — where
/// Rust's own operators answer FALSE to every NaN comparison. So a
/// stored `'nan'` matches `dur>1` in batch and must match live too:
/// `compare::double_cmp` is that ordering.
#[test]
fn double_ordering_is_total_with_nan_on_top() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let vals = [
        ("'nan'", f64::NAN),
        ("'inf'", f64::INFINITY),
        ("'-inf'", f64::NEG_INFINITY),
        ("1.0", 1.0),
        ("-1e308", -1e308),
        ("'-0'", -0.0),
        ("0.0", 0.0),
    ];
    for (a_sql, a) in vals {
        for (b_sql, b) in vals {
            let sql: (bool, bool, bool) = conn
                .query_row(
                    &format!(
                        "SELECT CAST({a_sql} AS DOUBLE) > CAST({b_sql} AS DOUBLE), \
                                CAST({a_sql} AS DOUBLE) < CAST({b_sql} AS DOUBLE), \
                                CAST({a_sql} AS DOUBLE) = CAST({b_sql} AS DOUBLE)"
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            let ord = trawl_core::compare::double_cmp(a, b);
            assert_eq!(
                (ord.is_gt(), ord.is_lt(), ord.is_eq()),
                sql,
                "ordering disagrees for {a_sql} vs {b_sql}"
            );
        }
    }
}

/// Why DOUBLE uniformly and never a per-literal BIGINT domain:
/// `TRY_CAST('1.5' AS BIGINT)` ROUNDS to 2, so a BIGINT domain would make
/// `dur>1` and `dur>1.5` disagree about the same stored value. DOUBLE
/// keeps 1.5 as 1.5. (The rounding premise itself is also pinned by the
/// conform-guard probes above.)
#[test]
fn try_cast_bigint_rounds_where_double_preserves() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let (as_bigint, as_double): (i64, f64) = conn
        .query_row(
            "SELECT TRY_CAST('1.5' AS BIGINT), TRY_CAST('1.5' AS DOUBLE)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(as_bigint, 2, "TRY_CAST to BIGINT rounds");
    assert!((as_double - 1.5).abs() < f64::EPSILON, "DOUBLE preserves");
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
/// the live mirror (`compare::try_cast_bigint`) must read the same value
/// out of the same wire text — stringifying the wire text instead answers
/// `status=0*` TRUE for a stored 404 and `status=a*` TRUE for a value the
/// column stores as NULL.
#[test]
fn bigint_pattern_text_is_the_cast_reading_on_both_engines() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
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
                "SELECT CAST(TRY_CAST(? AS BIGINT) AS VARCHAR)",
                [input],
                |r| r.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::try_cast_bigint(input).map(|i| i.to_string());
        assert_eq!(live, sql, "BIGINT pattern text disagrees for {input:?}");
    }

    // Documented residual: the fractional rung reads through f64, so a
    // fractional text within a half-ULP of ±2^63 loses to DuckDB's exact
    // decimal rounding. Integer texts — what a BIGINT-pinned field
    // actually carries — are exact across the whole range (above).
    let sql: Option<String> = conn
        .query_row(
            "SELECT CAST(TRY_CAST('9223372036854775807.4' AS BIGINT) AS VARCHAR)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sql.as_deref(), Some("9223372036854775807"));
    assert_eq!(
        trawl_core::compare::try_cast_bigint("9223372036854775807.4"),
        None
    );

    // The whole point: a stored 404 renders `404`, so the wire text's own
    // leading zero matches on neither side.
    let matched: bool = conn
        .query_row(
            "SELECT CAST(TRY_CAST('0404' AS BIGINT) AS VARCHAR) GLOB '0*'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!matched, "the stored BIGINT is 404, so `0*` misses");
}

/// A JSON number `read_json` typed DOUBLE still conforms to a BIGINT pin —
/// and that cast rounds half to EVEN, where the VARCHAR cast above rounds
/// half AWAY FROM ZERO. Two rungs, two roundings, both mirrored.
#[test]
fn double_to_bigint_rounds_half_to_even_on_both_engines() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let inputs = [
        1.5,
        2.5,
        3.5,
        -1.5,
        -2.5,
        0.4,
        -0.4,
        0.0,
        -0.0,
        200.0,
        1e18,
        1e19,
        1e300,
        9.223_372_036_854_776e18,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for input in inputs {
        let sql: Option<String> = conn
            .query_row(
                "SELECT CAST(TRY_CAST(CAST(? AS DOUBLE) AS BIGINT) AS VARCHAR)",
                [input],
                |r| r.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::double_to_bigint(input).map(|i| i.to_string());
        assert_eq!(live, sql, "DOUBLE→BIGINT disagrees for {input}");
    }
    // The two rungs really do disagree on a tie, so the mirror cannot
    // share one rounding rule.
    assert_eq!(trawl_core::compare::double_to_bigint(2.5), Some(2));
    assert_eq!(trawl_core::compare::try_cast_bigint("2.5"), Some(3));
}

/// The BOOLEAN pin's pattern text is `true`/`false`, lowercase, over a
/// closed vocabulary — a wire `"TRUE"` stores as `true`, so a matcher that
/// stringified the wire value would answer `flag=TRUE*` TRUE where the
/// batch query answers FALSE.
#[test]
fn boolean_pattern_text_is_the_cast_reading_on_both_engines() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
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
                "SELECT CAST(TRY_CAST(? AS BOOLEAN) AS VARCHAR)",
                [input],
                |r| r.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::try_cast_boolean(input).map(|b| b.to_string());
        assert_eq!(live, sql, "BOOLEAN pattern text disagrees for {input:?}");
    }

    // A numeric wire value is its zero test — total, so it never NULLs out.
    for input in [1.0, 0.0, -0.0, 2.0, -1.0, 1.5, f64::NAN, f64::INFINITY] {
        let sql: Option<String> = conn
            .query_row(
                "SELECT CAST(TRY_CAST(CAST(? AS DOUBLE) AS BOOLEAN) AS VARCHAR)",
                [input],
                |r| r.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::double_to_boolean(input).to_string();
        assert_eq!(Some(live), sql, "DOUBLE→BOOLEAN disagrees for {input}");
    }

    // The whole point: a stored `true` renders lowercase.
    let matched: bool = conn
        .query_row(
            "SELECT CAST(TRY_CAST('TRUE' AS BOOLEAN) AS VARCHAR) GLOB 'TRUE*'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !matched,
        "the stored BOOLEAN renders `true`, so `TRUE*` misses"
    );
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

/// The rule: a TIMESTAMP pin globs/regexes against ONE canonical text —
/// `strftime(col, TIMESTAMP_PATTERN_SQL_FORMAT)` in SQL, `compare::
/// canonical_timestamp_text` in the live matcher — so both engines answer
/// the same string for the same wire value. This runs the matrix through
/// `DuckDB` and the Rust mirror side by side; each conform is written the
/// way compaction writes it (`TRY_CAST` to the pin), so a value with no
/// reading is NULL on disk and `None` in memory.
///
/// The shapes cover the separator (`T` vs space), the zone suffix (`Z`,
/// `±HH:MM`, `±HH`, none — `DuckDB` stores the WALL CLOCK and drops the
/// offset), fractional seconds (absent, short, over-long) and the
/// date-only/slash forms.
#[test]
fn timestamp_pattern_text_is_rfc3339_micros_on_both_engines() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let fmt = trawl_core::compare::TIMESTAMP_PATTERN_SQL_FORMAT;
    let inputs = [
        "2026-01-15T09:00:00.000000Z",
        "2026-01-15T09:00:00Z",
        "2026-01-15 09:00:00",
        "2026-01-15T09:00",
        "  2026-01-15 09:00:00  ",
        "2026-01-15T09:00:00+05:30",
        "2026-01-15T09:00:00-08:00",
        "2026-01-15T09:00:00+02",
        "2026-01-15T09:00:00.123Z",
        "2026-01-15T09:00:00.1234567",
        "2026-01-15T09:00:00.0000000000Z",
        "2026-01-15",
        "2026/01/15",
        "2026/01/15 09:00:00",
        // No reading: NULL on the SQL side, None in memory.
        "yesterday-ish",
        "",
        "2026-01-15T",
        "2026-01-15 ",
        // Epoch numerals are not timestamps to DuckDB.
        "1737000000",
        "1737000000123",
    ];
    for input in inputs {
        let sql: Option<String> = conn
            .query_row(
                "SELECT strftime(TRY_CAST(? AS TIMESTAMP), ?)",
                [input, fmt],
                |row| row.get(0),
            )
            .unwrap();
        let live = trawl_core::compare::canonical_timestamp_text(input);
        assert_eq!(live, sql, "canonical pattern text disagrees for {input:?}");
    }

    // The whole point: an anchored pattern now means the same thing on
    // both sides of the same value.
    let matched: bool = conn
        .query_row(
            &format!(
                "SELECT strftime(TRY_CAST('2026-01-15T09:00:00.000000Z' AS TIMESTAMP), '{fmt}') \
                 GLOB '*T09:*'"
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
                "SELECT strftime(TRY_CAST('yesterday-ish' AS TIMESTAMP), '{fmt}') GLOB '2026*'"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unknown, None);
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
