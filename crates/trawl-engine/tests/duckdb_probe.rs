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
/// 2. the GUARD as compaction emits it (`conform_expr`): BIGINT/DOUBLE
///    compare the cast against the value's canonical text re-parsed in
///    DOUBLE space (tolerating representation drift — `4.0 → 4`,
///    `"042" → 42`, and `u64::MAX` → DOUBLE with precision loss, which the
///    AC requires); TIMESTAMP compares in TIMESTAMP space (tolerating
///    format drift — RFC 3339 `T`/`Z` vs `DuckDB`'s space-separated
///    rendering); BOOLEAN compares strict text (`true`/`false` only, so
///    `1` never conforms to a BOOLEAN pin).
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
    let guard_bigint_json = "(CASE WHEN TRY_CAST(json_extract_string(m, '$') AS DOUBLE) = TRY_CAST(m AS BIGINT) \
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
                "SELECT v, (CASE WHEN TRY_CAST(CAST(v AS VARCHAR) AS DOUBLE) = \
                 TRY_CAST(v AS BIGINT) THEN TRY_CAST(v AS BIGINT) END) \
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
                "SELECT count(CASE WHEN TRY_CAST(CAST(u AS VARCHAR) AS DOUBLE) = \
                        TRY_CAST(u AS BIGINT) THEN 1 END)::BIGINT, \
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
                        count(CASE WHEN TRY_CAST(json_extract_string(b,'$') AS DOUBLE) = \
                        TRY_CAST(b AS BIGINT) THEN 1 END)::BIGINT, \
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

/// `DuckDB` identifiers are case-INSENSITIVE, but only over ASCII. Catalog
/// pins are case-SENSITIVE names taken from client JSON keys, so two pins
/// can name one hot column — and a `REPLACE` list carrying both is a hard
/// parse error, not a degraded read. This pins both halves of the fold in
/// `trawl-core`'s `fold_case_variants`: what must collapse, and what must
/// NOT (folding `CAFÉ` onto `café` would silently drop a real pin).
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
/// the execution evidence for the hot buffer merging case-variant keys as it
/// writes the snapshot, rather than trying to pin either spelling.
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

/// Two engine assumptions the hot buffer's case-variant merge rests on
/// (`HotBuffer::build_snapshot`'s `CaseMerge`, ADR-0009 slice 2):
///
/// 1. an UNCONFORMED hot column can throw the WHOLE composite source — the
///    union binds types for every column, so a query whose DSL never names
///    the field fails too; and
/// 2. `UNION ALL BY NAME` matches column names case-INSENSITIVELY, and a
///    `VARCHAR` hot column unions with every cold scalar type.
///
/// Together: merging the collided spellings into one column and pinning it
/// `VARCHAR` is safe under EITHER spelling, while leaving the `_1` twin
/// unconformed is not.
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
