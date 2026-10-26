// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The text fold's lowercase table against the bundled `DuckDB` (ADR-0015).
//!
//! [`trawl_core::text::fold_ci`] folds with a committed copy of `DuckDB`'s
//! `lower()` mapping, so the live filter and `sightings` attribution match
//! `ILIKE` on every character. These tests regenerate the mapping from the
//! bundled `DuckDB` and hold the committed table, `ILIKE` and `fold_ci` to
//! it, so a `DuckDB` bump that changes the mapping fails here first.
//!
//! `cargo test -p trawl-core --test fold_table -- --ignored regenerate`
//! rewrites the committed table from the bundled `DuckDB`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use duckdb::Connection;

use trawl_core::text::fold_ci;

/// The committed table, relative to the crate root.
const TABLE_PATH: &str = "src/text/duckdb_lower.rs";

/// The command that rewrites [`TABLE_PATH`].
const REGENERATE: &str = "cargo test -p trawl-core --test fold_table -- --ignored regenerate";

/// Every Unicode scalar value but NUL, as `cp`: the two ranges either side
/// of the surrogate block, so no plan ever hands `chr` a surrogate. NUL
/// folds to itself and `DuckDB` strings cannot carry it through `chr`.
const SCALARS: &str = "SELECT cp::INTEGER AS cp FROM range(1, 55296) AS t(cp) \
     UNION ALL SELECT cp::INTEGER FROM range(57344, 1114112) AS t(cp)";

/// The code point `lower()` maps one scalar value to, for every scalar
/// value it changes, in code point order.
fn mapping_sql() -> String {
    format!(
        "SELECT cp, unicode(lower(chr(cp))) FROM ({SCALARS}) \
         WHERE lower(chr(cp)) <> chr(cp) ORDER BY cp"
    )
}

fn duckdb() -> Connection {
    Connection::open_in_memory().unwrap()
}

/// The library version string of the bundled `DuckDB`, such as `v1.5.5`.
fn duckdb_version(conn: &Connection) -> String {
    conn.query_row("SELECT library_version FROM pragma_version()", [], |row| {
        row.get(0)
    })
    .unwrap()
}

/// `DuckDB`'s `lower()` mapping: each scalar value it changes, with the one
/// code point it maps to.
///
/// Panics, naming the code points, when `lower()` maps any scalar value to
/// other than exactly one code point: the fold is one code point to one,
/// and a table cannot stand for anything else.
fn duckdb_lower_mapping(conn: &Connection) -> BTreeMap<char, char> {
    let not_one_to_one: Vec<String> = conn
        .prepare(&format!(
            "SELECT printf('U+%04X -> %s', cp, lower(chr(cp))) FROM ({SCALARS}) \
             WHERE length(lower(chr(cp))) <> 1 \
             OR chr(unicode(lower(chr(cp)))) <> lower(chr(cp)) ORDER BY cp"
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        not_one_to_one.is_empty(),
        "DuckDB lower() maps these scalar values to other than one code point: \
         {not_one_to_one:?}"
    );

    let scalar = |cp: u32| char::from_u32(cp).expect("DuckDB returns scalar values");
    conn.prepare(&mapping_sql())
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)))
        .unwrap()
        .map(|pair| {
            let (from, to) = pair.unwrap();
            (scalar(from), scalar(to))
        })
        .collect()
}

/// The committed table's source text for `mapping` under `version`.
fn render(version: &str, mapping: &BTreeMap<char, char>) -> String {
    let mut out = format!(
        "// This Source Code Form is subject to the terms of the Mozilla Public\n\
         // License, v. 2.0. If a copy of the MPL was not distributed with this\n\
         // file, You can obtain one at https://mozilla.org/MPL/2.0/.\n\
         \n\
         //! `DuckDB` {version}'s `lower()` mapping: every Unicode scalar value it\n\
         //! changes, with the one code point it maps to, sorted by code point.\n\
         //!\n\
         //! Generated from the bundled `DuckDB` by\n\
         //! `{REGENERATE}`.\n\
         //! Do not edit.\n\
         \n\
         /// `(from, to)` for every scalar value `lower()` changes, sorted by `from`.\n\
         pub(super) const LOWER: &[(char, char)] = &[\n"
    );
    for (from, to) in mapping {
        writeln!(
            out,
            "    ('\\u{{{:04X}}}', '\\u{{{:04X}}}'),",
            u32::from(*from),
            u32::from(*to)
        )
        .unwrap();
    }
    out.push_str("];\n");
    out
}

fn table_file() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(TABLE_PATH)
}

/// The `(from, to)` pairs of the committed table's source text.
fn committed_pairs(text: &str) -> BTreeMap<char, char> {
    let scalar = |hex: &str| {
        char::from_u32(u32::from_str_radix(hex, 16).expect("hex")).expect("scalar value")
    };
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("('\\u{"))
        .map(|rest| {
            let (from, rest) = rest.split_once("}'").expect("from literal");
            let to = rest
                .strip_prefix(", '\\u{")
                .and_then(|r| r.split_once("}'"))
                .expect("to literal")
                .0;
            (scalar(from), scalar(to))
        })
        .collect()
}

/// Rewrites the committed table from the bundled `DuckDB`.
#[test]
#[ignore = "rewrites src/text/duckdb_lower.rs; run after a DuckDB bump"]
fn regenerate() {
    let conn = duckdb();
    let mapping = duckdb_lower_mapping(&conn);
    std::fs::write(table_file(), render(&duckdb_version(&conn), &mapping)).unwrap();
    eprintln!("wrote {} rows to {TABLE_PATH}", mapping.len());
}

#[test]
fn table_matches_the_bundled_duckdb_lowercase_mapping() {
    let conn = duckdb();
    let mapping = duckdb_lower_mapping(&conn);
    let expected = render(&duckdb_version(&conn), &mapping);
    let committed = std::fs::read_to_string(table_file()).unwrap();
    if committed == expected {
        return;
    }
    let pairs = committed_pairs(&committed);
    let missing: Vec<_> = mapping
        .iter()
        .filter(|(from, to)| pairs.get(from) != Some(to))
        .take(20)
        .collect();
    let extra: Vec<_> = pairs
        .iter()
        .filter(|(from, to)| mapping.get(from) != Some(to))
        .take(20)
        .collect();
    panic!(
        "{TABLE_PATH} is not the bundled DuckDB {}'s lowercase mapping \
         ({} committed rows, {} from DuckDB; first missing {missing:?}, first \
         extra {extra:?}). Rerun `{REGENERATE}` and commit the result.",
        duckdb_version(&conn),
        pairs.len(),
        mapping.len(),
    );
}

#[test]
fn ilike_folds_through_the_same_table() {
    let conn = duckdb();
    let mapping = duckdb_lower_mapping(&conn);
    let values = |rows: &mut dyn Iterator<Item = String>| rows.collect::<Vec<_>>().join(", ");

    // Every pair of the table matches under `ILIKE`, both ways round.
    conn.execute_batch(&format!(
        "CREATE TEMP TABLE pairs (u INTEGER, l INTEGER); INSERT INTO pairs VALUES {}",
        values(
            &mut mapping
                .iter()
                .map(|(u, l)| format!("({}, {})", u32::from(*u), u32::from(*l)))
        )
    ))
    .unwrap();
    let unmatched: Vec<String> = conn
        .prepare(
            "SELECT printf('U+%04X/U+%04X', u, l) FROM pairs \
             WHERE NOT (chr(u) ILIKE chr(l) AND chr(l) ILIKE chr(u)) ORDER BY u",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        unmatched.is_empty(),
        "ILIKE does not match these table pairs: {unmatched:?}"
    );

    // Where Rust's full lowercase mapping disagrees with the table, `ILIKE`
    // sides with the table: the scalar matches its table fold and never its
    // Rust fold.
    let disagreements: Vec<(char, String, char)> = ('\u{1}'..=char::MAX)
        .filter_map(|c| {
            let table = mapping.get(&c).copied().unwrap_or(c);
            let rust: String = c.to_lowercase().collect();
            (rust != table.to_string()).then_some((c, rust, table))
        })
        .collect();
    assert!(
        disagreements.iter().any(|(c, ..)| *c == 'İ'),
        "U+0130 is a known disagreement; the comparison is not running"
    );
    conn.execute_batch(&format!(
        "CREATE TEMP TABLE disagreements (cp INTEGER, rust VARCHAR, tbl INTEGER); \
         INSERT INTO disagreements VALUES {}",
        values(&mut disagreements.iter().map(|(c, rust, table)| {
            format!(
                "({}, '{}', {})",
                u32::from(*c),
                rust.replace('\'', "''"),
                u32::from(*table)
            )
        }))
    ))
    .unwrap();
    let rust_wins: Vec<String> = conn
        .prepare(
            "SELECT printf('U+%04X rust=%s table=U+%04X', cp, rust, tbl) FROM disagreements \
             WHERE NOT chr(cp) ILIKE chr(tbl) OR chr(cp) ILIKE rust ORDER BY cp",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        rust_wins.is_empty(),
        "ILIKE disagrees with the table for {} of {} Rust/table disagreements: {rust_wins:?}",
        rust_wins.len(),
        disagreements.len()
    );
}

#[test]
fn fold_ci_matches_duckdb_lower_on_every_scalar() {
    let conn = duckdb();
    // One string of every scalar value but NUL, folded by both sides at
    // once. `lower()` folds each code point alone, so the whole string
    // folds as its characters do, and no context rule (a final sigma)
    // applies on either side.
    let every: String = ('\u{1}'..=char::MAX).collect();
    let duckdb: String = conn
        .query_row("SELECT lower(?)", [&every], |row| row.get(0))
        .unwrap();
    let ours = fold_ci(&every);
    if ours == duckdb {
        return;
    }
    // `lower()` is one code point to one, so `duckdb` aligns with `every`;
    // each character is folded alone so one wide fold cannot shift the rest.
    let differing: Vec<String> = every
        .chars()
        .zip(duckdb.chars())
        .map(|(c, d)| (c, d, fold_ci(c.encode_utf8(&mut [0; 4]))))
        .filter(|(_, d, o)| *o != d.to_string())
        .take(20)
        .map(|(c, d, o)| format!("U+{:04X}: DuckDB {d:?}, fold_ci {o:?}", u32::from(c)))
        .collect();
    panic!(
        "fold_ci differs from DuckDB lower() ({} vs {} code points); first \
         differences: {differing:?}",
        ours.chars().count(),
        duckdb.chars().count()
    );
}
