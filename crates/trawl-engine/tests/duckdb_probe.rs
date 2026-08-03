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
