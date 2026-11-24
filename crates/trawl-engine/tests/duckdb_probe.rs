// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Engine-assumption probe (ADR-0009, ADR-0011): every claim the conform
//! path makes about `DuckDB` is executed here against the bundled engine,
//! never assumed.

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
        &trawl_core::emitter::InputColumns::unprobed(),
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
    writeln!(f, r#"{{"s":"n/a","m":5,"b":410}}"#).unwrap();
    writeln!(f, r#"{{"s":"ok","m":"n/a","b":420}}"#).unwrap();
    f.sync_all().unwrap();

    let conn = duckdb::Connection::open_in_memory().unwrap();
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

#[test]
#[allow(clippy::too_many_lines)]
fn typed_casts_round_so_the_conform_guard_must_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let conn = conn();

    let describe_type = |sql: &str| -> String {
        let mut stmt = conn.prepare(&format!("DESCRIBE {sql}")).unwrap();
        stmt.query_row([], |row| row.get(1)).unwrap()
    };

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
            ("042".into(), Some(42)),
            ("1.5".into(), None),
            ("42".into(), Some(42)),
            ("n/a".into(), None),
        ],
        "VARCHAR class: numeric-space round-trip"
    );

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

#[test]
fn bigint_round_trip_is_exact_in_decimal_space_beyond_2_pow_53() {
    let conn = conn();

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

    let err = replace(r#"to_json("Status") AS "Status", to_json("status") AS "status""#)
        .expect("two ASCII case-variants must be rejected");
    assert!(
        err.contains("Duplicate entry"),
        "expected a duplicate-entry parse error, got: {err}"
    );
    assert_eq!(replace(r#"to_json("STATUS") AS "STATUS""#), None);

    for (name, variant) in [("café", "CAFÉ"), ("Ωx", "ωx")] {
        let err = replace(&format!(r#"to_json("{variant}") AS "{variant}""#))
            .unwrap_or_else(|| panic!("{variant} unexpectedly bound {name}"));
        assert!(
            err.contains("not found"),
            "expected {variant} to be a distinct identifier from {name}, got: {err}"
        );
    }
}

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

#[test]
fn unconformed_hot_column_throws_the_union_and_varchar_conform_saves_it() {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot.ndjson");
    let cold = dir.path().join("cold.parquet");
    let mut f = std::fs::File::create(&hot).unwrap();
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

    let err = count(&union("")).expect_err("an unconformed hot column must throw the union");
    assert!(
        err.to_string().contains("Malformed JSON"),
        "expected the JSON-vs-VARCHAR union conflict, got: {err}"
    );

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

#[test]
fn pin_blind_int_comparison_on_varchar_column_errors() {
    let conn = varchar_status_conn();
    // The class is read from the structured diagnostic, as on every
    // executor connection.
    conn.execute_batch("SET errors_as_json = true").unwrap();
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

    conn.execute_batch("SET errors_as_json = false").unwrap();
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
    assert_eq!(
        count(
            &conn,
            "SELECT count(*)::BIGINT FROM t WHERE (v != '200' OR v IS NULL)"
        )
        .unwrap(),
        4
    );
    assert_eq!(
        count(&conn, "SELECT count(*)::BIGINT FROM t WHERE v = '200.0'").unwrap(),
        0
    );
}

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
    assert_eq!(ordered(">=", "400"), 2,);
    assert_eq!(ordered(">", "1"), 4,);
    assert_eq!(ordered("<", "1000"), 4);
    for literal in ["nan", "inf", "1e40"] {
        assert_eq!(ordered(">", literal), 0, "{literal}");
        assert_eq!(ordered("<", literal), 0, "{literal}");
    }
}

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

const DECIMAL_DOMAIN_INPUTS: &[&str] = &[
    " 200",
    "200 ",
    "\t200\n",
    "\x0b200\x0c",
    "  200  ",
    "\u{a0}200",
    "2 00",
    " ",
    "",
    "200_000",
    "1_000.5",
    "1e1_0",
    "_200",
    "200_",
    "1__0",
    "1_.5",
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

#[test]
fn the_decimal_comparison_space_separates_ids_a_double_equates() {
    let conn = conn();
    conn.execute_batch(
        "CREATE TABLE t AS SELECT unnest([\
         '1737000000123456788', '1737000000123456789', '1737000000123456790', \
         '9007199254740992', '9007199254740993', '200', '200.0', 'accepted']) AS v",
    )
    .unwrap();

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

#[test]
fn re2_reads_class_set_syntax_as_plain_characters() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let matches = |subject: &str, pattern: &str| -> bool {
        conn.query_row("SELECT regexp_matches(?, ?)", [subject, pattern], |row| {
            row.get(0)
        })
        .unwrap()
    };
    for subject in ["a", "b", "&"] {
        assert!(matches(subject, r"^[\pL&&a]$"), "{subject}");
    }
    assert!(matches("-", "^[a-z--b]$"));
    assert!(matches("b", "^[a-z--b]$"));
    assert!(matches("~", r"^[\pL~~\pL]$"));
    assert!(matches("ac]", "^[[ab]c]$"));
    assert!(!matches("c", "^[[ab]c]$"));

    for body in [r"[\pL&&a]", "[a-z--b]", r"[\pL~~\pL]", "[[ab]c]"] {
        let errors = trawl_core::parser::parse(&format!("msg=/{body}/")).expect_err(body);
        assert!(
            errors[0].message.contains("class set operators"),
            "{body}: {errors:?}"
        );
    }
    assert!(matches("&", r"^[a\&&b]$"));
    assert!(!matches("c", r"^[a\&&b]$"));
    assert!(matches("[", r"^[\[]$"));
    assert!(trawl_core::parser::parse(r"msg=/[a\&&b][\[]/").is_ok());
}

#[test]
fn glob_lane_semantics_probe() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let duckdb_glob = |subject: &str, glob: &str| -> bool {
        conn.query_row("SELECT ? GLOB ?", [subject, glob], |row| row.get(0))
            .unwrap_or_else(|e| panic!("{subject:?} GLOB {glob:?}: {e}"))
    };
    let live_glob = |subject: &str, glob: &str| -> bool {
        trawl_core::pattern::Preparer::new()
            .build(trawl_core::pattern::PatternKind::Glob, glob)
            .unwrap_or_else(|e| panic!("{glob:?} builds: {e}"))
            .is_match(subject)
            .unwrap_or_else(|e| panic!("{subject:?} against {glob:?}: {e}"))
    };
    let rows: &[(&str, &str, bool)] = &[
        ("4", "[^4]", true),
        ("é", "?", false),
        ("é", "??", true),
        ("é", "[é]", false),
        ("a", r"\a", true),
        (r"\", r"[\]", true),
        ("1", r"[\d]", false),
        ("&", "[a&&b]", true),
        ("a", "[[:alpha:]]", false),
        ("-", "[a-]", false),
        ("[", "[", false),
        ("x", "[]", false),
        ("b", "[!a]", true),
        ("\n", "?", true),
        ("\n", "[!a]", true),
        ("", "?", false),
        ("", "[!a]", false),
        ("é", "[!a]", false),
        ("é", "[!a][!a]", true),
        ("€", "?", false),
        ("€", "??", false),
        ("€", "???", true),
        ("€", "????", false),
        ("€", "[!a]", false),
        ("€", "[!a][!a][!a]", true),
        ("😀", "???", false),
        ("😀", "????", true),
        ("😀", "?????", false),
        ("😀", "[!a]", false),
        ("😀", "[!a][!a][!a][!a]", true),
        ("😀", "*", true),
        ("😀x", "*x", true),
        // A range compares C `char`s. `[Ã-a]` is the member 0xC3 and the
        // range 0x83..`a`, which as signed bytes holds 0x00-0x61 and
        // 0x83-0xFF.
        ("Z", "[Ã-a]", true),
        ("a", "[é-a]", true),
        ("a", "[a-é]", false),
        ("x", "x\\", false),
        ("x\\", "x\\", false),
        // A member followed by `-` takes the next byte as the range end,
        // even `]`: `[a-]]` is the reversed range a..], an empty set.
        ("-", "[a-]]", false),
        ("]", "[a-]]", false),
        ("a", "[a-]]", false),
        ("]", "[]]", true),
        ("x", "[]]", false),
        ("x", "[!]]", true),
        ("]", "[!]]", false),
        ("x", "[!]", false),
        ("!", "[!", false),
        ("^", "[]-a]", true),
        ("a", "[]-a]", true),
        ("b", "[]-a]", false),
        ("-", "[-a]", true),
        ("a", "[-a]", true),
        ("x", "[z-a]*", false),
        ("", "[z-a]*", false),
        ("x", "[!z-a]", true),
        ("é", "[!z-a]", false),
        ("q", "[z-aq]", true),
        ("z", "[z-aq]", false),
        ("a", "*[", false),
        ("[", "*[", false),
        ("*", "[*]", true),
        ("a", "[*]", false),
        ("*", r"\*", true),
        ("a", r"\*", false),
        ("?", r"\?", true),
        ("a", r"\?", false),
        ("[", r"\[", true),
        ("é", r"\é", true),
        (r"\", r"\\", true),
        ("", "", true),
        ("a", "", false),
        ("\0", "?", true),
        ("a\0b", "a?b", true),
    ];
    for &(subject, glob, stated) in rows {
        let batch = duckdb_glob(subject, glob);
        assert_eq!(batch, stated, "DuckDB: {subject:?} GLOB {glob:?}");
        assert_eq!(
            live_glob(subject, glob),
            batch,
            "live against DuckDB: {subject:?} GLOB {glob:?}"
        );
    }
}

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
        2,
    );
}

const BIGINT_PATTERN_INPUTS: &[&str] = &[
    "404",
    "0404",
    "00200",
    "+5",
    "-0",
    "9223372036854775807",
    "-9223372036854775808",
    " 404 ",
    "\t404\n",
    "404_000",
    "1_0",
    "1_0.5",
    "1e1_0",
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
    "- ",
    "1e ",
    "1e0.",
    "5e-8",
];

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

    let matched: bool = conn
        .query_row(
            &format!("SELECT CAST({guard} AS VARCHAR) GLOB '0*' FROM (SELECT '0404' AS v) t"),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!matched, "the stored BIGINT is 404, so `0*` misses");
}

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
        "0",
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

    let cast_reads: i64 = conn
        .query_row(
            "SELECT count(TRY_CAST(v AS BOOLEAN))::BIGINT \
             FROM (VALUES ('TRUE'), ('t'), ('yes'), ('1'), ('no')) t(v)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cast_reads, 5, "premise: the CAST's vocabulary is wide");

    let numeric: Option<String> = conn
        .query_row(
            &format!("SELECT CAST({guard} AS VARCHAR) FROM (SELECT '200' AS v) t"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(numeric, None, "a numeric text must not conform to BOOLEAN");
}

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

const TIMESTAMP_TEXT_MATRIX: &[&str] = &[
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
    for stored in ["TRUE", "yes", "1"] {
        assert_eq!(trawl_core::compare::conformed_boolean(stored), None);
    }
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

    assert!(
        conn.query_row("SELECT b > ? FROM t", [1.5f64], |row| row.get::<_, bool>(0))
            .unwrap()
    );

    assert!(
        text_param("SELECT ts = ? FROM t", "2026-01-15T09:00:00+05:30").unwrap(),
        "a literal's offset is ignored"
    );
    assert!(text_param("SELECT ts = ? FROM t", "2026-01-15T09:00:00Z").unwrap());
    assert!(text_param("SELECT ts > ? FROM t", "2026-01-15").unwrap());

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

#[test]
fn pinned_rules_hold_over_read_json_columns() {
    let dir = tempfile::tempdir().unwrap();

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

#[test]
#[allow(clippy::too_many_lines)]
fn hot_and_compaction_conform_agree_across_inference_classes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hot.ndjson");
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
        ("2026-01-15T09:00:00+05:30", "2026-01-15 03:30:00"),
        ("2026-01-15T09:00:00-08:00", "2026-01-15 17:00:00"),
        ("2026-01-15T09:00:00+02", "2026-01-15 07:00:00"),
        ("2026-01-15T09:00:00Z", "2026-01-15 09:00:00"),
        ("2026-01-15 09:00:00", "2026-01-15 09:00:00"),
        ("2026/01/15 09:00:00", "2026-01-15 09:00:00"),
        ("2026-01-15", "2026-01-15 00:00:00"),
        ("2026-01-15T09:00:00.1234567", "2026-01-15 09:00:00.123456"),
    ] {
        assert_eq!(read(&conn, text).as_deref(), Some(expected), "{text:?}");
    }
    for text in ["yesterday-ish", "", "1737000000", "2026-01-15T"] {
        assert_eq!(read(&conn, text), None, "{text:?}");
    }

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

#[test]
fn a_cloned_connection_starts_from_the_process_default_not_the_parent() {
    let zone_of = |conn: &duckdb::Connection| -> String {
        conn.query_row("SELECT current_setting('TimeZone')", [], |row| row.get(0))
            .unwrap()
    };
    let default = zone_of(&duckdb::Connection::open_in_memory().unwrap());
    assert!(
        !default.is_empty(),
        "the shared runtime includes ICU, so a session always has a zone"
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

    let clone = parent.try_clone().unwrap();
    clone
        .execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    assert_eq!(zone_of(&clone), "UTC");
}

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
    let cast_text = "TRY_CAST(v AS VARCHAR)";
    assert_ne!(
        read(cast_text),
        read(&untyped_text("v")),
        "premise: the two text renderings of a DOUBLE differ"
    );
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

#[test]
fn resurrection_recovers_shelved_values_per_ladder_target() {
    use trawl_core::conform::{RepinTarget, resurrection_expr};

    let conn = conn();
    conn.execute_batch(
        "CREATE TABLE t (v BIGINT, _raw VARCHAR); \
         INSERT INTO t VALUES \
         (404, '{\"status\":404}'), \
         (NULL, '{\"status\":\"accepted\"}'), \
         (NULL, '{\"status\":\"200\"}'), \
         (NULL, 'not json')",
    )
    .unwrap();

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
    assert_eq!(read("dur", "[1,2]"), None);
    assert_eq!(read("dur", "42"), None);
}

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
        CanonicalType::Severity => unreachable!("physically BIGINT"),
    };
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

#[test]
fn misfit_sample_capture_is_distinct_null_free_and_capped() {
    let conn = conn();
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

    let sql = format!(
        "SELECT to_json(array_slice(list(DISTINCT {text}) \
         FILTER (WHERE v IS NOT NULL AND {cast} IS NULL), 1, 5))::VARCHAR \
         FROM b WHERE v IN ('a', 'b')"
    );
    let rendered: String = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
    let mut samples: Vec<String> = serde_json::from_str(&rendered).unwrap();
    samples.sort();
    assert_eq!(samples, vec!["a".to_owned(), "b".to_owned()]);

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

#[test]
fn left_truncates_by_character_not_by_byte() {
    let conn = conn();
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
    // as 9 in SQL and as nothing in Rust.
    "İNFO",
    "info\u{307}",
    "ı",
    "İ",
    "\u{212a}",
    "ＩＮＦＯ",
    "１７",
    "ERROR",
    "Warning",
    "\u{a0}error\u{a0}",
    "\u{2003}error",
    "\u{3000}error\u{3000}",
    "\u{85}error",
    "\u{202f}\u{2009}error\t",
    "\u{a0}17\u{a0}",
    "er\u{a0}ror",
];

#[test]
fn severity_reading_sql_matches_the_rust_kernel_in_both_dialects() {
    use trawl_core::severity::{self, Dialect};

    let conn = conn();
    let cases: &[&str] = SEVERITY_READING_CASES;
    for dialect in [Dialect::Otel, Dialect::Syslog] {
        for text in cases {
            let escaped = text.replace('\'', "''");
            let kernel = severity::reading_text(text, dialect).map(i64::from);
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
        let sql = format!(
            "SELECT {}",
            trawl_core::conform::severity_reading_sql("CAST(NULL AS VARCHAR)", dialect)
        );
        let engine: Option<i64> = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
        assert_eq!(engine, None, "{dialect:?} read a NULL as a severity");
    }
}

#[test]
fn severity_reading_sql_generates_its_arms_and_types_as_bigint() {
    use trawl_core::severity::{self, Dialect};

    let conn = conn();
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

    for build in [
        trawl_core::conform::severity_reading_sql,
        trawl_core::conform::severity_reading_sql_bind_once,
    ] {
        let sql = format!("DESCRIBE SELECT {} AS s", build("'error'", Dialect::Otel));
        let ty: String = conn.query_row(&sql, [], |row| row.get(1)).unwrap();
        assert_eq!(ty, CanonicalType::Severity.as_duckdb());
    }

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

#[test]
fn severity_conform_rung_reads_the_asserted_dialect() {
    use trawl_core::severity::{self, Dialect};

    let conn = conn();
    for dialect in [Dialect::Otel, Dialect::Syslog] {
        for text in SEVERITY_READING_CASES {
            let escaped = text.replace('\'', "''");
            let subject = format!("'{escaped}'");
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
    assert_eq!(ask("CAST(NULL AS VARCHAR)"), Some(false));
}

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
    assert_eq!(rows, vec![Some(13), Some(17)]);
}

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

/// The wildcard is `DuckDB`'s to expand: a plain `SELECT *, … AS status`
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

/// The SQL half of the overwrite projection's ASCII case fold has to be
/// the EXACT mirror of `schema::catalog_key`. `lower()` is not: it folds
/// non-ASCII too, where `DuckDB`'s own identifier binding does not.
#[test]
fn the_columns_exclusion_fold_mirrors_catalog_key() {
    fn fold_sql(expr: &str) -> String {
        format!("translate({expr}, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz')")
    }
    let fold_c = fold_sql("c");
    let conn = conn();
    conn.execute_batch(r#"CREATE TABLE t AS SELECT 'x' AS "Service", 'y' AS "Ä";"#)
        .unwrap();

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

    assert!(
        conn.prepare(r#"SELECT "SERVICE" FROM t"#).is_ok(),
        "ASCII case is insensitive"
    );
    assert!(
        conn.prepare(r#"SELECT "ä" FROM t"#).is_err(),
        "non-ASCII case is NOT folded by the binder"
    );
}

// The emitter pushes one bound `SqlValue` per DSL literal, so a DSL `1`
// reaches `DuckDB` as a BIGINT PARAMETER. A bare `typeof(1)` typed into SQL
// answers `INTEGER` and is simply a different question.

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

const BOUND_TYPEOF_SPELLINGS: &[&str] = &["BIGINT", "BIGINT", "DOUBLE", "BOOLEAN", "VARCHAR"];

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
    let conn = conn();
    for (value, want) in [(true, "1.0"), (false, "0.0")] {
        let (dtype, text) =
            scalar_type_and_text(&conn, "TRY_CAST(? AS DOUBLE)", &[&value]).unwrap();
        assert_eq!(dtype, "DOUBLE");
        assert_eq!(text.as_deref(), Some(want), "TRY_CAST({value} AS DOUBLE)");
    }
}

/// (function, return type over a BIGINT argument, over a DOUBLE one).
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
/// (`ceil(-0.5)` is `-0.0`, not `0.0`).
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
    let (dtype, text) = scalar_type_and_text(&conn, "round(?, 1)", &[&5_i64]).unwrap();
    assert_eq!((dtype.as_str(), text.as_deref()), ("BIGINT", Some("5")));
}

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
    for (expr, rhs) in [("? + ?", f64::MAX), ("? * ?", 2.0)] {
        assert_eq!(
            scalar_double(&conn, expr, &[&f64::MAX, &rhs]).unwrap(),
            Some(f64::INFINITY),
            "{expr} over DOUBLEs saturates"
        );
    }
}

/// (expression, left, right, `DuckDB`'s answer) over the IEEE specials.
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
    let (_, text) = scalar_type_and_text(&conn, "'-nan'::DOUBLE = 'NaN'::DOUBLE", &[]).unwrap();
    assert_eq!(text.as_deref(), Some("true"));

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

fn conditional_branch(
    conn: &duckdb::Connection,
    sql: &str,
    params: &[&dyn duckdb::ToSql],
) -> Result<Option<i64>, String> {
    conn.query_row(sql, params, |row| row.get(0))
        .map_err(|error| error.to_string())
}

const IF_OVER_BOUND_CONDITION: &str = "SELECT IF(?, 1, 2)";
const CASE_OVER_BOUND_CONDITION: &str = "SELECT CASE WHEN ? THEN 1 ELSE 2 END";

/// (text, the BOOLEAN `DuckDB` casts it to — `None` = Conversion error).
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

/// (bound BIGINT condition, the branch it takes).
const CONDITION_INTEGERS: &[(i64, bool)] = &[
    (0, false),
    (1, true),
    (2, true),
    (-1, true),
    (i64::MAX, true),
    (i64::MIN, true),
];

/// (bound DOUBLE condition, the branch it takes).
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
/// every row is a value the SQL parser cannot constant-fold away.
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

/// The same rows as [`STORED_DOUBLE_ROWS`], as (rendering, value).
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
        matched.sort();
        let mut want: Vec<String> = expected.iter().map(|text| (*text).to_owned()).collect();
        want.sort();
        assert_eq!(matched, want, "metric {op} {literal}");

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
/// Every row builds its NaN sign-EXPLICITLY, never from arithmetic: the sign
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

/// (SQL timestamp expression, its `CAST(… AS VARCHAR)`).
const TIMESTAMP_CAST_TEXTS: &[(&str, &str)] = &[
    ("TIMESTAMP '2026-01-15 09:00:00'", "2026-01-15 09:00:00"),
    (
        "TIMESTAMP '2026-01-15 09:00:00.123456'",
        "2026-01-15 09:00:00.123456",
    ),
    (
        "TIMESTAMP '2026-01-15 09:00:00.100000'",
        "2026-01-15 09:00:00.1",
    ),
    ("TIMESTAMP '0001-01-01 00:00:00'", "0001-01-01 00:00:00"),
    (
        "TIMESTAMP '9999-12-31 23:59:59.999999'",
        "9999-12-31 23:59:59.999999",
    ),
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

        let instant = trawl_core::compare::literal_timestamp(want)
            .unwrap_or_else(|| panic!("the mirror must read {want:?}"));
        assert_eq!(instant.cast_text(), *want, "cast_text disagrees for {expr}");
    }
}

#[test]
fn a_bound_instant_reads_back_through_duckdb() {
    use trawl_core::compare::{bound_instant, literal_timestamp};

    let conn = conn();
    let fmt = trawl_core::compare::TIMESTAMP_PATTERN_SQL_FORMAT;
    let engine = |text: &str| -> Option<String> {
        conn.query_row(
            "SELECT strftime(TRY_CAST(? AS TIMESTAMP), ?)",
            [text, fmt],
            |row| row.get(0),
        )
        .unwrap()
    };
    let named = [
        "2026-03-14",
        "2026-03-14T09:00:00",
        "2026-03-14 09:00:00",
        "2026-03-14T09:00:00.123456",
        "2026-03-14T09:00:00Z",
        "2026-03-14T09:00:00+05:30",
        "-0001-01-01 00:00:00",
        "epoch",
        "infinity",
        "-infinity",
    ];
    for text in named {
        assert!(bound_instant(text).is_some(), "{text:?} must be a bound");
    }
    let mut accepted = 0;
    for text in TIMESTAMP_TEXT_MATRIX.iter().copied().chain(named) {
        let Some(instant) = bound_instant(text) else {
            continue;
        };
        accepted += 1;
        let bound = instant.cast_text();
        assert_eq!(
            engine(&bound),
            Some(instant.pattern_text()),
            "{text:?} binds as {bound:?}, which the engine reads as another instant"
        );
        assert_eq!(
            engine(&bound),
            engine(text),
            "{text:?} and its bound text {bound:?} read differently"
        );
    }
    assert!(accepted > 50, "only {accepted} matrix rows were bounds");

    for text in ["10000-01-01", "262142-12-31 23:59:59"] {
        let instant = literal_timestamp(text).expect("the mirror reads the text");
        assert!(engine(text).is_some(), "the engine reads {text:?}");
        assert!(instant.cast_text().starts_with('+'), "{text:?}");
        assert_eq!(engine(&instant.cast_text()), None, "{text:?}");
        assert_eq!(bound_instant(text), None, "{text:?}");
    }
    assert_eq!(literal_timestamp("300000-01-01"), None);
    assert_eq!(engine("300000-01-01"), None);
    assert_eq!(bound_instant("300000-01-01"), None);
}

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

/// `262143-12-31` is deliberately absent: `DuckDB` reads it and the
/// mirror does not (chrono's calendar stops at +262142).
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

// `DuckDB`'s bare `%f` is a 6-digit microsecond field, chrono's is an
// UNSCALED NANOSECOND count (9 digits out, and a variable-length run
// read as nanoseconds in).

/// (fractional part of the instant, the text `strftime(ts, '%f')` gives).
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

/// (document, path, the TEXT `json_extract` returns).
const JSON_EXTRACT_TEXTS: &[(&str, &str, Option<&str>)] = &[
    (r#"{"a":1}"#, "$.a", Some("1")),
    (r#"{"a":-1}"#, "$.a", Some("-1")),
    (r#"{"a":1.5}"#, "$.a", Some("1.5")),
    (r#"{"a":1.0}"#, "$.a", Some("1.0")),
    (r#"{"a":0.1}"#, "$.a", Some("0.1")),
    (r#"{"a":-0.0}"#, "$.a", Some("-0.0")),
    (r#"{"a":1e2}"#, "$.a", Some("100.0")),
    (r#"{"a":1e300}"#, "$.a", Some("1e300")),
    (r#"{"a":-1e300}"#, "$.a", Some("-1e300")),
    (
        r#"{"a":1.7976931348623157e308}"#,
        "$.a",
        Some("1.7976931348623157e308"),
    ),
    (r#"{"a":1e-7}"#, "$.a", Some("1e-7")),
    (r#"{"a":5e-324}"#, "$.a", Some("5e-324")),
    (r#"{"a":9007199254740993}"#, "$.a", Some("9007199254740993")),
    (
        r#"{"a":18446744073709551615}"#,
        "$.a",
        Some("18446744073709551615"),
    ),
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

    let doc = r#"{"a":1e400}"#;
    let engine: Option<String> = conn
        .query_row("SELECT json_extract(?, '$.a')", [doc], |row| row.get(0))
        .unwrap();
    assert_eq!(engine.as_deref(), Some("1e400"));
    assert_eq!(json_extract_mirror(doc, "$.a"), None);
}

// the json document guard (ADR-0017)

const JSON_GUARD_DOCUMENTS: &[&str] = &[
    "",
    " ",
    "plain text line",
    r#""""#,
    r#"{"k":1}"#,
    r#"{"k":1} x"#,
    "{",
    "[1,2,3]",
    "[1,2,]",
    r#""str""#,
    "42",
    "-0",
    "1e999",
    "null",
    "true",
    r#"{ "k" : [ 1 , 2 ] }"#,
    r#"{"a":NaN}"#,
    r#"{"a":1,"a":2}"#,
    "\u{feff}{}",
];

const JSON_GUARD_FRAGMENTS: &[&str] = &[
    "{", "}", "[", "]", "\"", ":", ",", "1", "a", "null", "true", "-", ".", "e", "\\", " ", "x",
];

fn generated_json_guard_documents() -> Vec<String> {
    let mut state: u64 = 0x0457_0457_0457_0457;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(state >> 33).expect("a 31-bit value fits usize")
    };
    (0..300)
        .map(|_| {
            let len = next() % 7;
            (0..len)
                .map(|_| JSON_GUARD_FRAGMENTS[next() % JSON_GUARD_FRAGMENTS.len()])
                .collect()
        })
        .collect()
}

#[test]
fn try_cast_to_json_nulls_exactly_what_json_valid_rejects() {
    let conn = conn();
    let generated = generated_json_guard_documents();
    let mut valid = 0;
    let mut invalid = 0;
    for doc in JSON_GUARD_DOCUMENTS
        .iter()
        .copied()
        .chain(generated.iter().map(String::as_str))
    {
        let (is_valid, cast): (bool, Option<String>) = conn
            .query_row(
                "SELECT json_valid(d), CAST(TRY_CAST(d AS JSON) AS VARCHAR) \
                 FROM (SELECT CAST(? AS VARCHAR) AS d)",
                [doc],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or_else(|error| panic!("{doc:?}: {error}"));
        if is_valid {
            valid += 1;
            assert_eq!(cast.as_deref(), Some(doc), "a valid document changed");
        } else {
            invalid += 1;
            assert_eq!(cast, None, "an invalid document survived the cast: {doc:?}");
        }
    }
    assert!(
        valid >= 10 && invalid >= 10,
        "valid={valid} invalid={invalid}"
    );
}

#[test]
fn a_parquet_json_column_holding_invalid_text_fails_at_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("json_column.parquet");
    let conn = conn();
    conn.execute_batch(&format!(
        "COPY (SELECT CAST('[\"qqqqqqqqqq\"]' AS JSON) AS j) TO '{}' \
         (FORMAT parquet, COMPRESSION uncompressed)",
        path.display()
    ))
    .unwrap();

    // Swap the valid document for invalid text of the same length,
    // everywhere it is written (the page and the column statistics).
    let valid = br#"["qqqqqqqqqq"]"#;
    let invalid = b"plain text!!!!";
    assert_eq!(valid.len(), invalid.len());
    let mut bytes = std::fs::read(&path).unwrap();
    let mut patched = 0;
    let mut at = 0;
    while let Some(offset) = bytes[at..]
        .windows(valid.len())
        .position(|window| window == valid)
    {
        let start = at + offset;
        bytes[start..start + valid.len()].copy_from_slice(invalid);
        patched += 1;
        at = start + valid.len();
    }
    assert!(patched >= 1, "the fixture value was not found in the file");
    std::fs::write(&path, &bytes).unwrap();

    let column_type: String = conn
        .query_row(
            &format!(
                "SELECT column_type FROM (DESCRIBE SELECT j FROM read_parquet('{}'))",
                path.display()
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(column_type, "JSON");

    for select in [
        "CAST(j AS VARCHAR)",
        "json_extract_string(TRY_CAST(j AS JSON), '$')",
    ] {
        let read: Result<Option<String>, _> = conn.query_row(
            &format!("SELECT {select} FROM read_parquet('{}')", path.display()),
            [],
            |row| row.get(0),
        );
        let error = read.expect_err(select).to_string();
        assert!(error.contains("Malformed JSON"), "{select}: {error}");
    }
}

#[test]
fn a_try_and_case_guard_pair_still_raises_where_the_cast_guard_does_not() {
    let conn = conn();
    let source = "FROM (VALUES ('plain text line'), ('{\"k\":1}')) t(x)";
    let mixed: Result<Option<String>, _> = conn.query_row(
        &format!(
            "SELECT TRY(json_extract_string(x, ?)), \
             CASE WHEN json_valid(x) THEN json_extract_string(x, ?) END {source} LIMIT 1"
        ),
        ["$", "$"],
        |row| row.get(0),
    );
    let error = mixed.expect_err("the mixed TRY and CASE pair").to_string();
    assert!(error.contains("Malformed JSON"), "{error}");

    let mut stmt = conn
        .prepare(&format!(
            "SELECT json_extract_string(TRY_CAST(x AS JSON), ?), \
             CASE WHEN json_valid(x) THEN json_extract_string(TRY_CAST(x AS JSON), ?) END \
             {source}"
        ))
        .unwrap();
    let rows: Vec<(Option<String>, Option<String>)> = stmt
        .query_map(["$", "$"], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (None, None),
            (
                Some(r#"{"k":1}"#.to_string()),
                Some(r#"{"k":1}"#.to_string())
            ),
        ]
    );
}

fn bound_anchor(at: chrono::NaiveDateTime) -> duckdb::types::Value {
    duckdb::types::Value::Timestamp(
        duckdb::types::TimeUnit::Microsecond,
        at.and_utc().timestamp_micros(),
    )
}

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
/// typed literal.
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

/// chrono SIGNS a year past 9999 (`+10000-01-01`), and `DuckDB`'s
/// timestamp parser accepts a leading `-` but not a leading `+`.
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

        let bound = scalar_type_and_text(&conn, "CAST(? AS TIMESTAMP)", &[&bound_anchor(at)])
            .unwrap_or_else(|error| panic!("the bound form of {literal} must run: {error}"));
        assert_eq!(inlined, bound, "{literal} must denote the bound instant");
    }
}

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

fn matching_ids(conn: &duckdb::Connection, file: &std::path::Path, dsl: &str) -> Vec<String> {
    let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl} parses: {e:?}"));
    let emitted = trawl_core::emitter::emit_with_pins(
        &query,
        &file.display().to_string(),
        &trawl_core::schema::FieldTypes::new(),
        &trawl_core::emitter::InputColumns::unprobed(),
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

#[test]
fn the_absolute_time_window_is_half_open_at_the_microsecond() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("events.parquet");
    let conn = conn();
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

/// `json_serialize_sql` takes a constant, not a parameter, so the
/// expression is inlined and its quotes doubled.
fn parse_tree(conn: &duckdb::Connection, expr: &str) -> String {
    let statement = format!("SELECT {expr}").replace('\'', "''");
    conn.prepare(&format!("SELECT json_serialize_sql('{statement}')"))
        .unwrap_or_else(|e| panic!("serializing {expr}: {e}"))
        .query_row([], |row| row.get::<_, String>(0))
        .unwrap_or_else(|e| panic!("serializing {expr}: {e}"))
}

/// The subject marker every case below writes, chosen so it can appear
/// nowhere else in a parse tree: no ladder token, pattern or type name
/// contains it.
const COPY_MARKER: &str = "__c0__";

fn parsed_copies(conn: &duckdb::Connection, expr: &str) -> u64 {
    parse_tree(conn, expr).matches(COPY_MARKER).count() as u64
}

#[test]
fn duckdb_rewrites_a_simple_case_into_one_equality_per_arm() {
    let conn = conn();
    for arms in 1..=5u64 {
        let mut when = String::new();
        for n in 1..=arms {
            use std::fmt::Write as _;
            let _ = write!(when, " WHEN {n} THEN '{n}'");
        }
        assert_eq!(
            parsed_copies(&conn, &format!("(CASE \"{COPY_MARKER}\"{when} END)")),
            arms,
            "a {arms}-arm simple CASE copies its subject once per arm"
        );
    }
    assert_eq!(
        parsed_copies(
            &conn,
            &format!(
                "(CASE WHEN \"{COPY_MARKER}\" = 1 THEN 'a' WHEN \"{COPY_MARKER}\" = 2 THEN 'b' END)"
            )
        ),
        2
    );
}

/// `json_serialize_sql` writes one `"class"` key per expression node, so
/// counting them counts the nodes the binder will walk.
fn parsed_nodes(conn: &duckdb::Connection, expr: &str) -> u64 {
    parse_tree(conn, expr).matches("\"class\":").count() as u64
}

#[test]
fn an_else_less_case_still_binds_an_else_node() {
    use trawl_core::complexity::{FunctionShape, Rendering, profile};

    let conn = conn();
    for argc in 2..=6usize {
        let mut sql = String::from("(CASE");
        for i in 0..argc / 2 {
            use std::fmt::Write as _;
            let _ = write!(sql, " WHEN \"c{}\" THEN \"c{}\"", i * 2, i * 2 + 1);
        }
        if argc % 2 == 1 {
            use std::fmt::Write as _;
            let _ = write!(sql, " ELSE \"c{}\"", argc - 1);
        }
        sql.push_str(" END)");

        let parsed = parsed_nodes(&conn, &sql);
        let declared = {
            let p = profile(&Rendering::Function(FunctionShape::Case(argc)));
            p.fixed + p.copies.iter().sum::<u64>()
        };
        assert_eq!(
            parsed,
            argc as u64 + if argc % 2 == 0 { 2 } else { 1 },
            "an arity-{argc} CASE parses to its operands plus the CASE node, \
             plus the ELSE the parser supplies when none was written: {sql}"
        );
        assert!(
            declared >= parsed,
            "arity {argc}: the parser builds {parsed} nodes, the profile declares {declared}"
        );
    }
}

#[test]
fn declared_copy_counts_cover_the_parsed_tree() {
    use trawl_core::complexity::{ConformShape, FunctionShape, Rendering, profile};
    use trawl_core::conform;
    use trawl_core::severity::Dialect;

    let conn = conn();
    let subject = format!("\"{COPY_MARKER}\"");

    let mut cases: Vec<(String, Rendering, String)> = vec![
        (
            "severity_token_text_sql".to_owned(),
            Rendering::Conform(ConformShape::SeverityTokenText),
            conform::severity_token_text_sql(&subject),
        ),
        (
            "pattern over the severity token text".to_owned(),
            Rendering::Pattern(trawl_core::compare::PatternForm::SeverityText),
            format!(
                "regexp_matches({}, 'e.*')",
                conform::severity_token_text_sql(&subject)
            ),
        ),
    ];
    for dialect in [Dialect::Otel, Dialect::Syslog] {
        cases.push((
            format!("severity_reading_sql({dialect:?})"),
            Rendering::Conform(ConformShape::SeverityReading(dialect)),
            conform::severity_reading_sql(&subject, dialect),
        ));
        cases.push((
            format!("severity_reading_sql_bind_once({dialect:?})"),
            Rendering::Conform(ConformShape::SeverityReadingBindOnce(dialect)),
            conform::severity_reading_sql_bind_once(&subject, dialect),
        ));
        cases.push((
            format!("sev() emission ({dialect:?})"),
            Rendering::Function(FunctionShape::Sev { dialect, argc: 1 }),
            conform::severity_reading_sql_bind_once(&conform::untyped_text(&subject), dialect),
        ));
        for pin in CanonicalType::ALL {
            cases.push((
                format!("guarded_cast_in({pin:?}, {dialect:?})"),
                Rendering::Conform(ConformShape::GuardedCast(pin, dialect)),
                conform::guarded_cast_in(&subject, pin, dialect),
            ));
        }
    }

    for (label, rendering, sql) in cases {
        let declared = profile(&rendering)
            .copies
            .first()
            .copied()
            .unwrap_or_else(|| panic!("{label}: the profile declares no child"));
        let parsed = parsed_copies(&conn, &sql);
        assert!(
            parsed > 0,
            "{label}: the marker vanished from the parse tree, so this case measures nothing"
        );
        assert!(
            declared >= parsed,
            "{label}: the parser builds {parsed} copies of the subject, \
             the profile declares {declared}"
        );
    }
}

#[test]
fn the_severity_kernels_parse_to_their_pinned_copy_counts() {
    use trawl_core::conform;
    use trawl_core::severity::Dialect;

    let conn = conn();
    let subject = format!("\"{COPY_MARKER}\"");

    assert_eq!(
        parsed_copies(&conn, &conform::severity_token_text_sql(&subject)),
        24
    );
    assert_eq!(
        parsed_copies(
            &conn,
            &conform::severity_reading_sql(&subject, Dialect::Otel)
        ),
        79
    );
    assert_eq!(
        parsed_copies(
            &conn,
            &conform::severity_reading_sql(&subject, Dialect::Syslog)
        ),
        85
    );
    for dialect in [Dialect::Otel, Dialect::Syslog] {
        assert_eq!(
            parsed_copies(
                &conn,
                &conform::severity_reading_sql_bind_once(&subject, dialect)
            ),
            1,
            "{dialect:?}"
        );
    }
}

fn engine_version(conn: &duckdb::Connection) -> String {
    conn.query_row("SELECT version()", [], |row| row.get::<_, String>(0))
        .unwrap()
}

#[test]
fn the_bind_time_probes_run_against_the_pinned_engine_line() {
    let version = engine_version(&conn());
    assert!(
        version.starts_with("v1.5."),
        "the bind-time budget is calibrated against DuckDB 1.5.x, this is {version}"
    );
}

fn bind_fixture(conn: &duckdb::Connection, dir: &std::path::Path) -> String {
    let file = dir.join("bind.parquet");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES (TIMESTAMP '2026-01-01 00:00:00', 'nginx', 'h1', 200, 17, \
         'boom')) AS t(_time, service, host, status, _severity, message)) TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();
    file.display().to_string()
}

fn emitted_sql(source: &str, dsl: &str) -> String {
    let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl} parses: {e:?}"));
    trawl_core::emitter::emit(&query, source, trawl_core::context::EvalContext::capture())
        .unwrap_or_else(|e| panic!("{dsl} emits: {e:?}"))
        .sql
}

fn admission(dsl: &str) -> (bool, u64) {
    let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl} parses: {e:?}"));
    let (verdict, stats) =
        trawl_core::complexity::check_pipeline_complexity_with_stats(&query.pipeline);
    (verdict.is_ok(), stats.lateral_delta)
}

fn select_lists(sql: &str) -> Vec<String> {
    let mut lists = Vec::new();
    let mut current: Option<String> = None;
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("SELECT") {
            if let Some(open) = current.take() {
                lists.push(open);
            }
            current = Some(trimmed.to_owned());
        } else if trimmed.starts_with("FROM") {
            if let Some(open) = current.take() {
                lists.push(open);
            }
        } else if let Some(open) = current.as_mut() {
            open.push(' ');
            open.push_str(trimmed);
        }
    }
    if let Some(open) = current {
        lists.push(open);
    }
    lists
}

#[test]
fn a_same_list_alias_reference_copies_its_expression() {
    let conn = conn();
    let version = engine_version(&conn);
    let error = conn
        .prepare("SELECT random() AS r, r AS r2")
        .expect_err("a side-effecting expression cannot be substituted")
        .to_string();
    assert!(
        error.contains("has side effects"),
        "{version}: expected the side-effect refusal, got {error}"
    );
    let doubled: i64 = conn
        .prepare("SELECT 21 AS r, r + r AS r2")
        .unwrap()
        .query_row([], |row| row.get("r2"))
        .unwrap();
    assert_eq!(doubled, 42, "{version}");
}

#[test]
fn a_scalar_stats_or_timechart_output_shares_the_select_list_it_substitutes_into() {
    let dir = tempfile::tempdir().unwrap();
    let conn = conn();
    let source = bind_fixture(&conn, dir.path());
    let version = engine_version(&conn);

    for (dsl, expected) in [
        ("* | stats count() as n, abs(n) as m", 1_i64),
        ("* | stats max(status) as n, abs(n) as m by service", 200),
        ("* | timechart span=1h count() as n, abs(n) as m", 1),
    ] {
        let sql = emitted_sql(&source, dsl);
        let lists = select_lists(&sql);
        let holding: Vec<&String> = lists.iter().filter(|l| l.contains("AS \"m\"")).collect();
        assert_eq!(holding.len(), 1, "{dsl}: one list defines `m`: {sql}");
        assert!(
            holding[0].contains("AS \"n\""),
            "{dsl}: `m` must be defined beside `n`, not downstream of it: {sql}"
        );
        assert!(
            holding[0].contains("ABS(\"n\")"),
            "{dsl}: `m` reads `n` as a same-list alias: {sql}"
        );
        let value: i64 = conn
            .prepare(&sql)
            .unwrap_or_else(|e| panic!("{version}: {dsl} prepares: {e}"))
            .query_row([], |row| row.get("m"))
            .unwrap();
        assert_eq!(value, expected, "{dsl}: {sql}");
    }

    let split = emitted_sql(&source, "* | stats count() as n | let m = abs(n)");
    let lists = select_lists(&split);
    assert!(
        lists.iter().filter(|l| l.contains("AS \"n\"")).count() == 1
            && lists.iter().filter(|l| l.contains("AS \"m\"")).count() == 1
            && !lists
                .iter()
                .any(|l| l.contains("AS \"n\"") && l.contains("AS \"m\"")),
        "the split form defines each output in its own SELECT list: {split}"
    );

    let nested = emitted_sql(&source, "* | stats count() as n, sum(n) as m");
    let error = conn
        .prepare(&nested)
        .expect_err("an aggregate cannot read a same-list alias")
        .to_string();
    assert!(
        error.contains("not found in FROM clause"),
        "{version}: {error}"
    );
}

#[test]
fn eventstats_refuses_a_substituted_alias_inside_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let conn = conn();
    let source = bind_fixture(&conn, dir.path());
    let version = engine_version(&conn);

    let dsl = "* | eventstats count() as n, abs(n) as m by service";
    let (admitted, delta) = admission(dsl);
    assert!(
        admitted && delta > 0,
        "the checker prices this stage itself (delta {delta}), it does not defer to DuckDB"
    );

    let sql = emitted_sql(&source, dsl);
    let lists = select_lists(&sql);
    let holding: Vec<&String> = lists.iter().filter(|l| l.contains("AS \"m\"")).collect();
    assert_eq!(holding.len(), 1, "one SELECT list defines `m`: {sql}");
    assert!(
        holding[0].contains("OVER (PARTITION BY \"service\") AS \"n\"")
            && holding[0].contains("ABS(\"n\") OVER"),
        "both outputs are window-wrapped in that one list: {sql}"
    );

    let error = conn
        .prepare(&sql)
        .expect_err("the substituted window lands inside a window")
        .to_string();
    assert!(
        error.contains("window function calls cannot be nested"),
        "{version}: {error}"
    );
}

#[test]
fn a_severity_chain_multiplies_its_subject_twelve_times_per_link() {
    let dir = tempfile::tempdir().unwrap();
    let conn = conn();
    let source = bind_fixture(&conn, dir.path());
    let points = "1,3,5,7,9,11,13,15,17,19,21,23";

    let sql = emitted_sql(&source, &format!("* | where sev(a0) in ({points})"));
    let link = sql
        .rsplit_once("WHERE ")
        .expect("the search stage emits a WHERE clause")
        .1
        .trim()
        .to_owned();
    assert_eq!(
        link.matches("\"a0\"").count(),
        trawl_core::compare::severity_ranges(&[1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23]).len(),
        "one written subject per disjoint ladder range: {link}"
    );

    let seed = format!("(\"{COPY_MARKER}\" + \"{COPY_MARKER}\")");
    let one_link = link.replace("\"a0\"", &seed);
    let two_links = link.replace("\"a0\"", &format!("({one_link})"));
    assert_eq!(parsed_copies(&conn, &one_link), 24);
    assert_eq!(parsed_copies(&conn, &two_links), 288);
    assert!(
        one_link.len() < 20_000 && two_links.len() < 250_000,
        "SQL text grows twelvefold per link while the DSL grows by one assignment"
    );

    let chain = |links: usize| {
        let mut dsl = String::from("* | let a0 = _severity + _severity");
        for i in 1..=links {
            use std::fmt::Write as _;
            let _ = write!(dsl, ", a{i} = sev(a{}) in ({points})", i - 1);
        }
        dsl
    };
    assert!(admission(&chain(1)).0, "one link is admitted");
    for links in [2, 3, 8] {
        let (admitted, delta) = admission(&chain(links));
        assert!(
            !admitted,
            "{links} links of the full ladder set score {delta} and must be refused"
        );
    }
}

#[test]
fn the_split_let_remedy_admits_a_depth_twenty_four_chain() {
    let dir = tempfile::tempdir().unwrap();
    let conn = conn();
    let source = bind_fixture(&conn, dir.path());
    let version = engine_version(&conn);

    let depth = 24;
    let mut one_stage = String::from("* | let a0 = status + status");
    let mut split = String::from("* | let a0 = status + status");
    for i in 1..depth {
        use std::fmt::Write as _;
        let _ = write!(one_stage, ", a{i} = a{} + a{}", i - 1, i - 1);
        let _ = write!(split, " | let a{i} = a{} + a{}", i - 1, i - 1);
    }

    let (admitted, delta) = admission(&one_stage);
    assert!(
        !admitted,
        "twenty-four doublings in one stage score {delta} and are refused"
    );
    let (admitted, delta) = admission(&split);
    assert!(
        admitted && delta == 0,
        "the split form carries no lateral expansion at all, scored {delta}"
    );

    let sql = emitted_sql(&source, &split);
    let lists = select_lists(&sql);
    assert_eq!(
        lists.len(),
        depth + 1,
        "one SELECT list per stage, plus the source: {sql}"
    );
    for i in 0..depth {
        let defining = lists
            .iter()
            .filter(|l| l.contains(&format!("AS \"a{i}\"")))
            .count();
        assert_eq!(defining, 1, "`a{i}` is defined in exactly one list: {sql}");
    }
    conn.prepare(&sql)
        .unwrap_or_else(|e| panic!("{version}: the split chain prepares: {e}"));
}

// absent-field resolution premises (ADR-0062)
mod absent_field_premises {
    use std::io::Write as _;

    use super::{conn, hot_reader};

    fn describe(
        conn: &duckdb::Connection,
        sql: &str,
    ) -> Result<Vec<(String, String)>, duckdb::Error> {
        let mut stmt = conn.prepare(&format!("DESCRIBE {sql}"))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect()
    }

    fn pairs(cols: &[(&str, &str)]) -> Vec<(String, String)> {
        cols.iter()
            .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
            .collect()
    }

    fn column0(
        conn: &duckdb::Connection,
        sql: &str,
        params: &[&dyn duckdb::ToSql],
    ) -> Result<Vec<Option<String>>, duckdb::Error> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, |r| {
            let v: duckdb::types::Value = r.get(0)?;
            Ok(match v {
                duckdb::types::Value::Null => None,
                duckdb::types::Value::Text(s) => Some(s),
                duckdb::types::Value::Int(i) => Some(i.to_string()),
                duckdb::types::Value::BigInt(i) => Some(i.to_string()),
                duckdb::types::Value::HugeInt(i) => Some(i.to_string()),
                duckdb::types::Value::Boolean(b) => Some(b.to_string()),
                other => Some(format!("{other:?}")),
            })
        })?;
        rows.collect()
    }

    fn write_parquet(conn: &duckdb::Connection, select: &str, path: &std::path::Path) {
        conn.execute_batch(&format!(
            "COPY ({select}) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }

    #[test]
    fn untyped_null_binds_against_parameters() {
        let conn = conn();
        conn.execute_batch("CREATE TABLE t AS SELECT 1 AS a")
            .unwrap();
        let one: &[&dyn duckdb::ToSql] = &[&"x"];
        let two: &[&dyn duckdb::ToSql] = &[&"x", &"y"];
        for (predicate, params) in [
            ("NULL != ?", one),
            ("NULL = ?", one),
            ("NULL IN (?, ?)", two),
            ("NULL GLOB ?", one),
            ("regexp_matches(NULL, ?)", one),
            ("NULL > ?", one),
        ] {
            let value = column0(&conn, &format!("SELECT {predicate} FROM t"), params)
                .unwrap_or_else(|e| panic!("`{predicate}` must prepare and run: {e}"));
            assert_eq!(value, vec![None], "`{predicate}` is unknown");
            let passing =
                column0(&conn, &format!("SELECT a FROM t WHERE {predicate}"), params).unwrap();
            assert!(passing.is_empty(), "`WHERE {predicate}` passes no row");
        }
    }

    #[test]
    fn a_coalesce_hint_types_a_null_and_converts_nothing() {
        let conn = conn();
        conn.execute_batch(
            "CREATE TABLE p AS SELECT CAST(2 AS BIGINT) AS big, CAST(2 AS HUGEINT) AS huge, \
             CAST(1.5 AS DOUBLE) AS dbl, 'web1' AS text, TRUE AS flag, \
             TIMESTAMP '2024-01-15 10:00:03' AS ts",
        )
        .unwrap();
        let hint = |column: &str, ty: &str| format!("COALESCE({column}, CAST(NULL AS {ty}))");

        assert!(
            conn.prepare("SELECT NULL AS a, date_part('day', a) AS y FROM p")
                .is_err(),
            "the premise: an untyped alias leaves date_part no overload"
        );
        for (ty, call) in [
            ("TIMESTAMP", "date_part('day', {})"),
            ("TIMESTAMP", "date_trunc('day', {})"),
            ("TIMESTAMP", "date_diff('day', {}, {})"),
            ("VARCHAR", "contains({}, {})"),
        ] {
            let call = call.replace("{}", &hint("a", ty));
            assert_eq!(
                column0(
                    &conn,
                    &format!("SELECT y FROM (SELECT NULL AS a, {call} AS y FROM p)"),
                    &[]
                )
                .unwrap_or_else(|e| panic!("`{call}` binds over the alias: {e}")),
                vec![None],
                "{call}"
            );
        }

        for (ty, column) in [("TIMESTAMP", "ts"), ("VARCHAR", "text")] {
            let hinted = hint(column, ty);
            assert_eq!(
                column0(
                    &conn,
                    &format!("SELECT ({hinted}) = {column} AND typeof({hinted}) = typeof({column}) FROM p"),
                    &[]
                )
                .unwrap(),
                vec![Some("true".to_owned())],
                "{hinted} keeps the value and the type"
            );
            assert_eq!(
                column0(
                    &conn,
                    &format!("SELECT y FROM (SELECT NULL AS {column}, CAST({hinted} AS VARCHAR) AS y FROM p)"),
                    &[]
                )
                .unwrap(),
                column0(&conn, &format!("SELECT CAST({column} AS VARCHAR) FROM p"), &[]).unwrap(),
                "{hinted} reads the input column, not the alias"
            );
        }

        for (ty, others) in [
            ("TIMESTAMP", ["big", "huge", "dbl", "text", "flag"]),
            ("VARCHAR", ["big", "huge", "dbl", "ts", "flag"]),
        ] {
            for column in others {
                let hinted = hint(column, ty);
                let error = conn
                    .prepare(&format!("SELECT {hinted} FROM p"))
                    .err()
                    .unwrap_or_else(|| panic!("{hinted} must refuse, not convert"));
                assert!(
                    error.to_string().contains("Cannot mix values of type"),
                    "{hinted}: {error}"
                );
            }
        }
    }

    #[test]
    fn where_prefers_the_from_column() {
        let conn = conn();
        conn.execute_batch("CREATE TABLE t AS SELECT 2 AS x, 'row' AS m")
            .unwrap();
        let rows = column0(&conn, "SELECT m, 1 AS x FROM t WHERE x = 2", &[]).unwrap();
        assert_eq!(
            rows,
            vec![Some("row".to_owned())],
            "WHERE x binds t.x = 2, not the alias 1"
        );
    }

    #[test]
    fn projected_untyped_null_is_integer() {
        let conn = conn();
        let untyped = "(SELECT NULL AS f)";
        let varchar = "(SELECT CAST(NULL AS VARCHAR) AS f)";
        assert_eq!(
            describe(&conn, untyped).unwrap(),
            pairs(&[("f", "INTEGER")])
        );
        assert_eq!(
            describe(&conn, varchar).unwrap(),
            pairs(&[("f", "VARCHAR")])
        );

        let sum_int = column0(&conn, &format!("SELECT sum(f) FROM {untyped}"), &[]);
        assert_eq!(
            sum_int.unwrap(),
            vec![None],
            "sum over the INTEGER null is NULL"
        );
        let sum_varchar = column0(&conn, &format!("SELECT sum(f) FROM {varchar}"), &[]);
        let err = sum_varchar.expect_err("sum has no VARCHAR overload");
        assert!(err.to_string().contains("sum"), "{err}");
    }

    #[test]
    fn integer_null_column_against_a_varchar_parameter() {
        let conn = conn();
        conn.execute_batch(
            "CREATE TABLE only_null AS SELECT NULL AS f; \
             CREATE TABLE mixed AS SELECT * FROM (VALUES (NULL), (1)) v(f)",
        )
        .unwrap();
        assert_eq!(
            describe(&conn, "only_null").unwrap(),
            pairs(&[("f", "INTEGER")])
        );
        let x: &[&dyn duckdb::ToSql] = &[&"x"];

        let folded = column0(&conn, "SELECT f FROM (SELECT NULL AS f) WHERE f = ?", x);
        assert_eq!(
            folded.unwrap(),
            Vec::<Option<String>>::new(),
            "the inline constant folds"
        );

        for table in ["only_null", "mixed"] {
            let err = column0(&conn, &format!("SELECT f FROM {table} WHERE f = ?"), x)
                .expect_err("a real INTEGER column casts the parameter");
            assert!(
                err.to_string().contains("Conversion Error"),
                "{table}: {err}"
            );
        }
    }

    #[test]
    fn order_by_absent_key() {
        let conn = conn();
        conn.execute_batch("CREATE TABLE s AS SELECT * FROM (VALUES (3), (1), (2)) v(a)")
            .unwrap();
        let rows = |key: &str| column0(&conn, &format!("SELECT a FROM s ORDER BY {key}, a"), &[]);

        let err = rows("NULL").expect_err("bare NULL is a non-integer literal");
        assert!(
            err.to_string()
                .contains("ORDER BY non-integer literal has no effect"),
            "{err}"
        );
        let ascending = vec![
            Some("1".to_owned()),
            Some("2".to_owned()),
            Some("3".to_owned()),
        ];
        for key in [
            "CAST(NULL AS BIGINT)",
            "CAST(NULL AS VARCHAR)",
            "CAST(NULL AS INTEGER)",
        ] {
            assert_eq!(rows(key).unwrap(), ascending, "`{key}` is a constant key");
            let last = column0(
                &conn,
                &format!("SELECT a FROM s ORDER BY a DESC, {key}"),
                &[],
            );
            assert_eq!(
                last.unwrap(),
                ascending.iter().rev().cloned().collect::<Vec<_>>(),
                "`{key}` as a trailing key changes nothing"
            );
        }
    }

    #[test]
    fn order_by_binds_the_select_alias() {
        let conn = conn();
        conn.execute_batch("CREATE TABLE t AS SELECT * FROM (VALUES (1, 20), (2, 10)) v(x, y)")
            .unwrap();
        let rows = column0(&conn, "SELECT y AS x FROM t ORDER BY x", &[]).unwrap();
        assert_eq!(
            rows,
            vec![Some("10".to_owned()), Some("20".to_owned())],
            "ordered by the alias (y), not t.x"
        );
    }

    #[test]
    fn zero_row_fill_keeps_type_and_folds_case() {
        let conn = conn();
        conn.execute_batch("CREATE TABLE t AS SELECT CAST(7 AS BIGINT) AS a, 'm' AS msg")
            .unwrap();
        let value = "(SELECT max(a) AS \"_v\" FROM t WHERE FALSE HAVING FALSE)";
        let fill = format!(
            "(SELECT * FROM t UNION ALL BY NAME SELECT \"_v\" AS \"A\", \"_v\" AS \"B\" FROM {value})"
        );
        assert_eq!(
            describe(&conn, &fill).unwrap(),
            pairs(&[("a", "BIGINT"), ("msg", "VARCHAR"), ("B", "BIGINT")])
        );
        assert_eq!(
            column0(&conn, &format!("SELECT a FROM {fill}"), &[]).unwrap(),
            vec![Some("7".to_owned())]
        );
        assert_eq!(
            column0(&conn, &format!("SELECT b FROM {fill}"), &[]).unwrap(),
            vec![None]
        );
        assert_eq!(
            column0(&conn, "SELECT max(a) FROM t WHERE FALSE", &[]).unwrap(),
            vec![None],
            "an aggregate without HAVING FALSE answers one row"
        );
    }

    #[test]
    fn pivot_subquery_takes_a_fill_branch() {
        let conn = conn();
        conn.execute_batch(
            "CREATE TABLE t AS SELECT * FROM (VALUES \
               ('h1', 'cpu', 5::BIGINT), ('h1', 'mem', 7::BIGINT), ('h2', 'cpu', 1::BIGINT)) \
             v(host, metric, v)",
        )
        .unwrap();
        let pivot = "PIVOT t\nON \"metric\"\nUSING sum(v)\nGROUP BY \"host\"";
        let fill = format!(
            "(SELECT * FROM ({pivot}) UNION ALL BY NAME \
              SELECT \"_v\" AS \"v\", \"_v\" AS \"CPU\", \"_v\" AS \"zz\" \
              FROM (SELECT sum(v) AS \"_v\" FROM t WHERE FALSE HAVING FALSE))"
        );
        assert_eq!(
            describe(&conn, &fill).unwrap(),
            pairs(&[
                ("host", "VARCHAR"),
                ("cpu", "HUGEINT"),
                ("mem", "HUGEINT"),
                ("v", "HUGEINT"),
                ("zz", "HUGEINT"),
            ])
        );
        assert_eq!(
            column0(&conn, &format!("SELECT v FROM {fill}"), &[]).unwrap(),
            vec![None, None],
            "the consumed column reads NULL on every pivot row"
        );
        assert_eq!(
            column0(&conn, &format!("SELECT cpu FROM {fill} ORDER BY host"), &[]).unwrap(),
            vec![Some("5".to_owned()), Some("1".to_owned())],
            "a present pivot column keeps its values"
        );
    }

    #[test]
    fn exclude_replace_on_read_json() {
        let conn = conn();
        let dir = tempfile::tempdir().unwrap();
        let hot = dir.path().join("hot.ndjson");
        let mut f = std::fs::File::create(&hot).unwrap();
        writeln!(f, r#"{{"m":"a","k":null}}"#).unwrap();
        writeln!(f, r#"{{"m":"b","k":null}}"#).unwrap();
        writeln!(f, r#"{{"m":"c"}}"#).unwrap();
        f.sync_all().unwrap();
        let hot = hot_reader(&hot);

        assert_eq!(
            describe(&conn, &format!("SELECT * FROM {hot}")).unwrap(),
            pairs(&[("m", "VARCHAR"), ("k", "JSON")])
        );
        assert_eq!(
            describe(&conn, &format!("SELECT * EXCLUDE (k) FROM {hot}")).unwrap(),
            pairs(&[("m", "VARCHAR")])
        );
        let replaced = format!("SELECT * REPLACE (CAST(NULL AS BIGINT) AS k) FROM {hot}");
        assert_eq!(
            describe(&conn, &replaced).unwrap(),
            pairs(&[("m", "VARCHAR"), ("k", "BIGINT")])
        );
        assert_eq!(
            column0(&conn, &format!("SELECT k FROM ({replaced})"), &[]).unwrap(),
            vec![None, None, None]
        );

        let cold = dir.path().join("cold.parquet");
        write_parquet(&conn, "SELECT 'z' AS m, CAST(9 AS BIGINT) AS k", &cold);
        let cold = format!("SELECT * FROM read_parquet('{}')", cold.display());
        assert_eq!(
            describe(
                &conn,
                &format!("({cold} UNION ALL BY NAME SELECT * FROM {hot})")
            )
            .unwrap(),
            pairs(&[("m", "VARCHAR"), ("k", "JSON")]),
            "the hot JSON key widens the cold BIGINT column"
        );
        assert_eq!(
            describe(&conn, &format!("({cold} UNION ALL BY NAME {replaced})")).unwrap(),
            pairs(&[("m", "VARCHAR"), ("k", "BIGINT")])
        );
    }

    #[test]
    fn describe_of_a_zero_column_source() {
        const ZERO_COLUMN_SOURCE: &str = "(SELECT 1 WHERE FALSE)";
        let conn = conn();
        let expected = pairs(&[("1", "INTEGER")]);
        assert_eq!(describe(&conn, ZERO_COLUMN_SOURCE).unwrap(), expected);
        assert_eq!(
            describe(&conn, &format!("SELECT * FROM {ZERO_COLUMN_SOURCE}")).unwrap(),
            expected,
            "the probe's own spelling"
        );
    }

    #[test]
    fn describe_select_star_over_union_by_name_parquet() {
        let conn = conn();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        write_parquet(
            &conn,
            "SELECT TIMESTAMP '2024-01-01' AS _time, 'x' AS host",
            &a,
        );
        write_parquet(
            &conn,
            "SELECT TIMESTAMP '2024-01-02' AS _time, CAST(3 AS BIGINT) AS status",
            &b,
        );
        let sql = format!(
            "SELECT * FROM read_parquet(['{}', '{}'], union_by_name=true)",
            a.display(),
            b.display()
        );
        assert_eq!(
            describe(&conn, &sql).unwrap(),
            pairs(&[
                ("_time", "TIMESTAMP"),
                ("host", "VARCHAR"),
                ("status", "BIGINT")
            ])
        );
    }
}
