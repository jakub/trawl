// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A search-stage glob matches in the live lane and the kv batch tail as
//! the bundled `DuckDB`'s `GLOB` matches it in batch SQL (ADR-0001's
//! amendment of 2026-11-09).
//!
//! Over an alphabet that holds every glob metacharacter, a two-byte
//! character and the class punctuation, the test compares `DuckDB`'s answer
//! with [`Pattern::is_match`] for every glob of up to three symbols
//! against every input of up to two symbols without `*` and `?`, then for
//! a fixed-seed sample of longer pairs. The named boundary rows live in
//! `duckdb_probe.rs` (`glob_lane_semantics_probe`).

use duckdb::{Connection, params};
use trawl_core::pattern::{Pattern, PatternKind, Preparer};

/// The glob alphabet. Each symbol is a whole character, so `é` is one
/// symbol of two bytes.
const ALPHABET: &[&str] = &["a", "4", "é", "^", "!", "-", "\\", "[", "]", "*", "?"];

/// The input alphabet: [`ALPHABET`] without `*` and `?`.
const INPUTS: &[&str] = &["a", "4", "é", "^", "!", "-", "\\", "[", "]"];

/// The seed of the sampled pairs.
const SEED: u64 = 0x0330_6c0b_9e37_79b9;

/// How many sampled pairs the test compares.
const SAMPLES: usize = 10_000;

/// splitmix64, as `trawl-core/tests/filter_parity.rs` has it.
struct Rng {
    state: u64,
}

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % u64::try_from(n).expect("fits")).expect("below n")
    }

    /// Up to `max` symbols drawn from `alphabet`.
    fn word(&mut self, alphabet: &[&str], max: usize) -> String {
        let len = self.below(max + 1);
        (0..len)
            .map(|_| alphabet[self.below(alphabet.len())])
            .collect()
    }
}

/// Every word of `0..=max` symbols from `alphabet`, shortest first.
fn words(alphabet: &[&str], max: usize) -> Vec<String> {
    let mut all = vec![String::new()];
    let mut last = vec![String::new()];
    for _ in 0..max {
        last = last
            .iter()
            .flat_map(|w| alphabet.iter().map(move |s| format!("{w}{s}")))
            .collect();
        all.extend(last.iter().cloned());
    }
    all
}

/// `glob` built through a ledger of its own, which the pattern outlives,
/// so no ledger's budget holds more than one pattern.
fn live(glob: &str) -> Pattern {
    Preparer::new()
        .build(PatternKind::Glob, glob)
        .unwrap_or_else(|e| panic!("{} builds: {e}", shown(glob)))
}

/// `text` quoted with its escapes, and its bytes in hex.
fn shown(text: &str) -> String {
    let hex: Vec<String> = text.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{text:?} [{}]", hex.join(" "))
}

/// Fill table `name` (`id INTEGER`, then one VARCHAR per column) with
/// `rows`, and check no value arrived as NULL.
fn fill(conn: &Connection, name: &str, columns: &[&str], rows: &[Vec<&str>]) {
    let defs: Vec<String> = columns.iter().map(|c| format!("{c} VARCHAR")).collect();
    conn.execute_batch(&format!(
        "CREATE TABLE {name} (id INTEGER, {})",
        defs.join(", ")
    ))
    .expect("create the table");
    {
        let mut app = conn.appender(name).expect("appender");
        for (id, row) in rows.iter().enumerate() {
            let id = i32::try_from(id).expect("an id fits");
            match row.as_slice() {
                [a] => app.append_row(params![id, a]),
                [a, b] => app.append_row(params![id, a, b]),
                _ => unreachable!("one or two text columns"),
            }
            .expect("append");
        }
    }
    let nulls = columns
        .iter()
        .map(|c| format!("{c} IS NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let (count, null): (i64, i64) = conn
        .query_row(
            &format!("SELECT count(*), count(*) FILTER (WHERE {nulls}) FROM {name}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("count the table");
    assert_eq!(count, i64::try_from(rows.len()).expect("fits"), "{name}");
    assert_eq!(null, 0, "{name}: an empty string must stay text");
}

/// `words` as rows of one text column.
fn one_column(words: &[String]) -> Vec<Vec<&str>> {
    words.iter().map(|w| vec![w.as_str()]).collect()
}

/// The answers of `sql`, whose rows are `(id, matched)` in id order, and
/// whose ids must run `0..n`.
fn answers(conn: &Connection, sql: &str, n: usize) -> Vec<bool> {
    let mut stmt = conn.prepare(sql).expect("prepare");
    let rows: Vec<(i64, bool)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(rows.len(), n, "{sql}");
    for (i, (id, _)) in rows.iter().enumerate() {
        assert_eq!(*id, i64::try_from(i).expect("fits"), "{sql}: ids in order");
    }
    rows.into_iter().map(|(_, m)| m).collect()
}

#[test]
fn glob_matches_duckdb_glob() {
    let conn = Connection::open_in_memory().expect("open DuckDB");
    let mut mismatches = Vec::new();

    // Every glob of up to 3 symbols against every input of up to 2.
    let globs = words(ALPHABET, 3);
    let inputs = words(INPUTS, 2);
    assert_eq!(globs.len(), 1 + 11 + 121 + 1331);
    assert_eq!(inputs.len(), 1 + 9 + 81);
    fill(&conn, "globs", &["g"], &one_column(&globs));
    fill(&conn, "inputs", &["s"], &one_column(&inputs));
    let exhaustive = globs.len() * inputs.len();
    assert_eq!(exhaustive, 133_224);
    let batch = answers(
        &conn,
        &format!(
            "SELECT g.id * {n} + i.id, i.s GLOB g.g FROM globs g CROSS JOIN inputs i ORDER BY 1",
            n = inputs.len()
        ),
        exhaustive,
    );
    let mut compared = 0;
    for (g, glob) in globs.iter().enumerate() {
        let pattern = live(glob);
        for (i, input) in inputs.iter().enumerate() {
            let duckdb = batch[g * inputs.len() + i];
            let rust = pattern.is_match(input).expect("never cancelled");
            if rust != duckdb {
                mismatches.push(format!(
                    "exhaustive glob #{g} {} input #{i} {}: DuckDB {duckdb}, live {rust}",
                    shown(glob),
                    shown(input)
                ));
            }
            compared += 1;
        }
    }
    assert_eq!(compared, exhaustive);
    let exhaustive_hits = batch.iter().filter(|m| **m).count();

    // A fixed-seed sample of globs up to 8 symbols and inputs up to 4.
    let mut rng = Rng { state: SEED };
    let pairs: Vec<(String, String)> = (0..SAMPLES)
        .map(|_| (rng.word(ALPHABET, 8), rng.word(ALPHABET, 4)))
        .collect();
    let rows: Vec<Vec<&str>> = pairs
        .iter()
        .map(|(g, s)| vec![g.as_str(), s.as_str()])
        .collect();
    fill(&conn, "pairs", &["g", "s"], &rows);
    let batch = answers(&conn, "SELECT id, s GLOB g FROM pairs ORDER BY id", SAMPLES);
    let mut compared = 0;
    for (case, ((glob, input), duckdb)) in pairs.iter().zip(&batch).enumerate() {
        let rust = live(glob).is_match(input).expect("never cancelled");
        if rust != *duckdb {
            mismatches.push(format!(
                "seed {SEED:#x} case #{case} glob {} input {}: DuckDB {duckdb}, live {rust}",
                shown(glob),
                shown(input)
            ));
        }
        compared += 1;
    }
    assert_eq!(compared, SAMPLES);
    let sampled_hits = batch.iter().filter(|m| **m).count();

    // Both halves must hold matches and misses, or they prove nothing.
    for (part, hits, total) in [
        ("exhaustive", exhaustive_hits, exhaustive),
        ("sampled", sampled_hits, SAMPLES),
    ] {
        assert!(0 < hits && hits < total, "{part}: {hits} of {total} match");
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} pairs disagree:\n{}",
        mismatches.len(),
        exhaustive + SAMPLES,
        mismatches.join("\n")
    );
    eprintln!(
        "{exhaustive} exhaustive pairs ({exhaustive_hits} match) and {SAMPLES} sampled \
         ({sampled_hits} match) agree"
    );
}
