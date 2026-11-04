// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An `extract` stage binds its regex once (ADR-0063, human ruling
//! 2026-11-04): `DuckDB`'s struct form of `regexp_extract` reads every
//! named group from one RE2.
//!
//! The premise tests pin what the bundled `DuckDB` does with that form,
//! straight in SQL, prepared and then bound as the engine runs a query:
//! how it maps names to groups, what a group that takes no part in the
//! match reads, and when the bound pattern becomes a constant.
//!
//! The struct form needs a constant pattern at bind. The engine prepares
//! a statement before it binds the values, and a bound `?` is no constant
//! then, so `regexp_extract(s, ?, ['g1', ...])` is a binder error at
//! prepare. With the names list bound as a parameter too,
//! `regexp_extract(s, ?, ?::VARCHAR[])`, the binder defers the call until
//! the values are bound, and by then the pattern is a constant.

use duckdb::Connection;

/// `[g1, ..., gn]`: the struct field names the emitter generates, as the
/// text the names parameter binds.
fn names(n: usize) -> String {
    let list: Vec<String> = (1..=n).map(|i| format!("g{i}")).collect();
    format!("[{}]", list.join(", "))
}

/// Each field of the struct `regexp_extract(subject, pattern, names(n))`
/// returns, read in order, with the pattern and the names bound as
/// parameters, as the emitter binds them.
fn fields(subject: Option<&str>, pattern: &str, n: usize) -> Vec<Option<String>> {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let reads: Vec<String> = (1..=n).map(|i| format!("x.g{i}")).collect();
    let sql = format!(
        "SELECT {} FROM (SELECT regexp_extract(?::VARCHAR, ?, ?::VARCHAR[]) AS x)",
        reads.join(", ")
    );
    let mut stmt = conn.prepare(&sql).expect("the struct form prepares");
    stmt.query_row(duckdb::params![subject, pattern, names(n)], |row| {
        (0..n).map(|i| row.get::<_, Option<String>>(i)).collect()
    })
    .expect("the struct form runs")
}

fn some(texts: &[&str]) -> Vec<Option<String>> {
    texts.iter().map(|t| Some((*t).to_owned())).collect()
}

/// With the names written as a literal list, a bound pattern is refused
/// at prepare: the struct form wants a constant pattern, and a `?` is
/// none before its value is bound.
#[test]
fn a_literal_names_list_refuses_a_bound_pattern() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let error = conn
        .prepare("SELECT regexp_extract('ab', ?, ['g1'])")
        .map(|_| ())
        .expect_err("a literal names list needs a constant pattern");
    assert!(
        error
            .to_string()
            .contains("with LIST of group names requires a constant pattern"),
        "{error}"
    );
}

/// With the names bound too, the pattern is a constant by the time the
/// call binds: an invalid pattern fails at bind over zero rows, before
/// any row could compile it. (A pattern that only rows compile would
/// pass over zero rows.)
#[test]
fn bound_names_make_the_pattern_a_constant() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let error = conn
        .prepare(
            "SELECT regexp_extract(s, ?, ?::VARCHAR[]) \
             FROM (SELECT 'a' AS s WHERE false)",
        )
        .and_then(|mut stmt| {
            stmt.query(duckdb::params!["((?P<y>b)", names(1)])
                .map(|_| ())
        })
        .expect_err("an invalid constant pattern fails at bind");
    assert!(
        error.to_string().contains("Pattern failed to parse"),
        "{error}"
    );
}

/// The struct form takes 40 names, past the scalar form's 0 to 9, and
/// field k is the k-th capture group's text.
#[test]
fn struct_form_reads_forty_groups() {
    let letters: Vec<String> = (0..40)
        .map(|i| {
            char::from_u32(0x4E00 + i)
                .expect("a CJK ideograph")
                .to_string()
        })
        .collect();
    let pattern = letters
        .iter()
        .enumerate()
        .fold(String::new(), |body, (i, l)| {
            body + "(?P<name" + &i.to_string() + ">" + l + ")"
        });
    let subject: String = letters.concat();
    let want: Vec<&str> = letters.iter().map(String::as_str).collect();
    assert_eq!(fields(Some(&subject), &pattern, 40), some(&want));
}

/// `DuckDB` maps the k-th name to the k-th capture group by position
/// among every capture group, named or not, never by the group's own
/// name. So the batch body must make its unnamed groups non-capturing, as
/// `trawl_core::pattern::batch_extract` does, for the k-th name to read
/// the k-th named group.
#[test]
fn names_map_to_groups_by_position() {
    // An unnamed group ahead of the named one takes the first name.
    assert_eq!(fields(Some("ab"), "(a)(?P<y>b)", 1), some(&["a"]));
    // The group's own name plays no part.
    assert_eq!(fields(Some("ab"), "(?P<y>a)(?P<x>b)", 2), some(&["a", "b"]));
    // Made non-capturing, the unnamed group drops out of the numbering.
    assert_eq!(fields(Some("ab"), "(?:a)(?P<y>b)", 1), some(&["b"]));
    // Fewer names than groups reads the first groups only.
    assert_eq!(
        fields(Some("abc"), "(?P<x>a)(?P<y>b)(?P<z>c)", 2),
        some(&["a", "b"])
    );
}

/// More names than capture groups is a bind error, never a silent empty
/// field.
#[test]
fn more_names_than_groups_is_refused() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let error = conn
        .prepare("SELECT regexp_extract('ab', ?, ?::VARCHAR[])")
        .and_then(|mut stmt| stmt.query(duckdb::params!["(a)", names(2)]).map(|_| ()))
        .expect_err("two names, one group");
    assert!(
        error.to_string().contains("Not enough capturing groups"),
        "{error}"
    );
}

/// A group that takes no part in the match reads `''`, and so does every
/// group of a row the pattern does not match, so `nullif(.., '')` reads
/// NULL for both, as the live lane does. A NULL subject gives a NULL
/// struct, whose fields read NULL.
#[test]
fn unmatched_groups_read_empty() {
    let pattern = "(?:a)(?P<x>b)|(?:c)(?P<y>d)";
    assert_eq!(
        fields(Some("cd"), pattern, 2),
        vec![Some(String::new()), Some("d".to_owned())]
    );
    assert_eq!(
        fields(Some("zz"), pattern, 2),
        vec![Some(String::new()), Some(String::new())]
    );
    assert_eq!(fields(None, pattern, 2), vec![None, None]);
}
