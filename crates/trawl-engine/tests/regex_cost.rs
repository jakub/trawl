// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What one regex call costs for the worst pattern both of ADR-0063's
//! budgets admit, over the longest value ingest stores, in Rust and in the
//! bundled `DuckDB`'s `RE2`.
//!
//! ADR-0063, as amended on 2026-11-03, claims that the repeat budget (64
//! positions a pattern) together with the ingest cap (`MAX_RAW_CHARS`,
//! 65 536 characters a stored string value) bounds one regex call over
//! one stored value at about 0.4 s. This harness measures that claim and
//! prints the transcript `scripts/evidence/issue-334/cost.sh` commits.
//!
//! Not a correctness test and not run in CI: `#[ignore]`d, because a
//! wall-clock number on a shared machine is evidence for a reader, not a
//! gate. Run it through the script, or directly:
//!
//! ```sh
//! cargo test -p trawl-engine --test regex_cost --release -- --ignored --nocapture --test-threads=1
//! ```
//!
//! # How the worst pattern is searched for
//!
//! Matching is linear in the subject for both engines, so the per-call
//! cost is the subject length times a per-byte constant, and the constant
//! is what a pattern controls. Both engines run a lazily built DFA first.
//! A DFA state is the set of pattern positions still alive, and the
//! pattern `a X{k} c`, with `X` a class that matches all of the subject's
//! characters, needs one state for every subset of the last `k + 1`
//! characters that held an `a`. Over a pseudo-random subject that is up to
//! `2^(k+1)` states, so the DFA's cache fills, is cleared, fills again,
//! and both engines give up on it: Rust falls back to its Pike VM, `RE2` to
//! its NFA. Each then pays for every live position on every byte, up to
//! `k` of them, which is why the repeat budget bounds the constant.
//!
//! The search holds that shape fixed and varies what each position costs:
//!
//! - `X` itself: a three-character class, `.`, `[^\n]`, `\w`, `\pL`, an
//!   alternation of large classes, and a large class under `(?i)`;
//! - nesting, `(?:X{2}){k/2}`, which reaches the same positions through
//!   two counted repeats;
//! - `\B` before each `X`, eight `\B`, two `\b`, or `(?:\b|\B)`, which
//!   always holds, and `\b` around the needle. A Unicode word assertion
//!   makes Rust's lazy DFA quit on the first non-ASCII byte, so over
//!   non-ASCII text Rust runs the Pike VM from the start;
//! - an optional `^` or `$` under `(?m)` before each `X`, which both
//!   engines check on every live step whether it holds or not;
//! - capture groups: an empty one before each `X`, two nested around it,
//!   and 31 empty ones before the needle;
//! - an empty alternation, `(?:|)`, or a repeat of nothing, `(?:)?`,
//!   before each `X`;
//! - the zero-charge family: flags, an empty group and an empty flags
//!   group before each `X`, which `trawl_core`'s walk counts as no
//!   position. It costs what `a [abc]{k} c` costs at the same `k`;
//! - the subject's alphabet: ASCII letters, ASCII letters and a space,
//!   4-byte letters (U+1D41A to U+1D41C), 4-byte letters and a 4-byte
//!   symbol (U+1D11E, not a word character), and Deseret letters, which
//!   have case folds.
//!
//! Each family takes the largest `k` that `trawl_core::parser::parse`
//! admits, so the pattern passes every check a query passes: the
//! 1024-byte body cap, the repeat and translation budgets, and the 1 MiB
//! compiled-size limit. `k` is never above 62, because `a` and `c` take
//! the other two of the 64 positions. Every assertion, capture group,
//! empty branch and repeat of nothing is a position too, so a family that
//! puts one in each repeated unit gets a smaller `k`.
//!
//! The subject defeats literal prefilters by holding the pattern's
//! literals everywhere: every family is timed over a subject that is
//! three tenths `a`, which keeps the set of live positions varied, and
//! one that is eight tenths `a`, which keeps more of them alive at once.
//! The rest is split between `b` and `c`. No `c` falls exactly `k + 1`
//! characters after an `a`, so the pattern never matches and each call
//! scans the whole subject. The harness checks that neither engine finds
//! a match.
//!
//! The slowest family in each engine is then timed at smaller `k`, to
//! show how the cost scales with the repeat count.
//!
//! # What the ceiling covers
//!
//! The ceiling is per call, over one stored value of at most
//! `MAX_RAW_CHARS` characters. Two things fall outside it: values stored
//! before the ingest cap existed keep their length, and a computed
//! subject such as `concat(message, message)` can be longer than any one
//! stored value. A query over either stays correct and pays in proportion
//! to the longer subject.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use duckdb::{Connection, params};
use trawl_core::parser::parse;
use trawl_core::pattern::{PatternKind, Preparer};

/// `trawl_server::ingest::envelope::MAX_RAW_CHARS`, the most characters
/// ingest stores in one string value. trawl-engine cannot depend on
/// trawl-server, so the value is repeated here.
const SUBJECT_CHARS: usize = 65_536;

/// ADR-0063's bound on one regex call over one stored value, as the
/// 2026-11-03 amendment sets it.
const CEILING: Duration = Duration::from_millis(400);

/// Timed calls per case, after one untimed warm-up call.
const RUNS: usize = 7;

/// The share of the subject that is `a`, in tenths. A third keeps the
/// set of live positions varied, which is what defeats a DFA. Eight
/// tenths keeps more positions alive at once, which is what an NFA or
/// Pike VM pays for on every byte.
const A_SHARES: [u64; 2] = [3, 8];

/// The most positions a family's repeat may take: the repeat budget's 64,
/// less the `a` and the `c` around it.
const MAX_K: usize = 62;

/// A subject alphabet: `a` and `c` are the needle's ends, `b` is filler.
#[derive(Clone, Copy)]
struct Alphabet {
    name: &'static str,
    a: char,
    b: char,
    c: char,
}

const ALPHABETS: [Alphabet; 5] = [
    Alphabet {
        name: "ascii",
        a: 'a',
        b: 'b',
        c: 'c',
    },
    Alphabet {
        name: "ascii+space",
        a: 'a',
        b: ' ',
        c: 'c',
    },
    Alphabet {
        name: "4-byte",
        a: '\u{1D41A}',
        b: '\u{1D41B}',
        c: '\u{1D41C}',
    },
    Alphabet {
        name: "4-byte+symbol",
        a: '\u{1D41A}',
        b: '\u{1D11E}',
        c: '\u{1D41C}',
    },
    Alphabet {
        name: "deseret",
        a: '\u{10428}',
        b: '\u{10429}',
        c: '\u{1042A}',
    },
];

/// One shape of the needle. `body` builds the pattern for an alphabet and
/// a repeat count `k`; `gap` is how far after an `a` its `c` sits.
struct Family {
    name: &'static str,
    body: fn(Alphabet, usize) -> String,
    gap: fn(usize) -> usize,
}

fn class3(x: Alphabet) -> String {
    format!("[{}{}{}]", esc(x.a), esc(x.b), esc(x.c))
}

/// A character as a regex literal: escaped when it is a metacharacter
/// (the space needs none outside `(?x)`).
fn esc(c: char) -> String {
    if c.is_ascii_punctuation() {
        format!("\\{c}")
    } else {
        c.to_string()
    }
}

fn plus_one(k: usize) -> usize {
    k + 1
}

const FAMILIES: [Family; 22] = [
    Family {
        name: "a [abc]{k} c",
        body: |x, k| format!("{}{}{{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "(?s) a .{k} c",
        body: |x, k| format!("(?s){}.{{{k}}}{}", x.a, x.c),
        gap: plus_one,
    },
    Family {
        name: "a [^\\n]{k} c",
        body: |x, k| format!("{}[^\\n]{{{k}}}{}", x.a, x.c),
        gap: plus_one,
    },
    Family {
        name: "a \\w{k} c",
        body: |x, k| format!("{}\\w{{{k}}}{}", x.a, x.c),
        gap: plus_one,
    },
    Family {
        name: "a \\pL{k} c",
        body: |x, k| format!("{}\\pL{{{k}}}{}", x.a, x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:\\w|\\pN|[abc]){k} c",
        body: |x, k| format!("{}(?:\\w|\\pN|{}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "(?i) a [abc]{k} c",
        body: |x, k| format!("(?i){}{}{{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "(?i) a [\\pL\\pN]{k} c",
        body: |x, k| format!("(?i){}[\\pL\\pN]{{{k}}}{}", x.a, x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:[abc]{2}){k/2} c",
        body: |x, k| format!("{}(?:{}{{2}}){{{}}}{}", x.a, class3(x), k / 2, x.c),
        gap: |k| 2 * (k / 2) + 1,
    },
    Family {
        name: "a (?:\\B[abc]){k} c",
        body: |x, k| format!("{}(?:\\B{}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "\\b a (?s:.){k} c \\b",
        body: |x, k| format!("\\b{}(?s:.){{{k}}}{}\\b", x.a, x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:\\B\\B\\B\\B\\B\\B\\B\\B[abc]){k} c",
        body: |x, k| format!("{}(?:{}{}){{{k}}}{}", x.a, "\\B".repeat(8), class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:\\b\\b[abc]){k} c",
        body: |x, k| format!("{}(?:\\b\\b{}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:(?:\\b|\\B)[abc]){k} c",
        body: |x, k| format!("{}(?:(?:\\b|\\B){}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "(?m) a (?:(?:^)?[abc]){k} c",
        body: |x, k| format!("(?m){}(?:(?:^)?{}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "(?m) a (?:(?:$)?[abc]){k} c",
        body: |x, k| format!("(?m){}(?:(?:$)?{}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:()[abc]){k} c",
        body: |x, k| format!("{}(?:(){}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:(([abc]))){k} c",
        body: |x, k| format!("{}(?:(({}))){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "\\B ()x31 a [abc]{k} c",
        body: |x, k| format!("\\B{}{}{}{{{k}}}{}", "()".repeat(31), x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:(?:|)[abc]){k} c",
        body: |x, k| format!("{}(?:(?:|){}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    Family {
        name: "a (?:(?:)?[abc]){k} c",
        body: |x, k| format!("{}(?:(?:)?{}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
    // The zero-charge family: nodes that `trawl_core`'s walk counts as no
    // position, before each class. It must cost what `a [abc]{k} c` costs.
    Family {
        name: "a (?:(?:)(?i)(?-i)(?i:)[abc]){k} c",
        body: |x, k| format!("{}(?:(?:)(?i)(?-i)(?i:){}){{{k}}}{}", x.a, class3(x), x.c),
        gap: plus_one,
    },
];

/// A deterministic xorshift64 stream, so every run times the same subject.
struct Rng(u64);

impl Rng {
    fn below_ten(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % 10
    }
}

/// `SUBJECT_CHARS` pseudo-random characters of `x`, `a_tenths` tenths of
/// them `a` and the rest split between `b` and `c`, with no `c` exactly
/// `gap` characters after an `a`.
fn subject(x: Alphabet, gap: usize, a_tenths: u64) -> String {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut chars: Vec<char> = Vec::with_capacity(SUBJECT_CHARS);
    for p in 0..SUBJECT_CHARS {
        let draw = rng.below_ten();
        let mut next = if draw < a_tenths {
            x.a
        } else if draw < a_tenths + (10 - a_tenths) / 2 {
            x.c
        } else {
            x.b
        };
        if next == x.c && p >= gap && chars[p - gap] == x.a {
            next = x.b;
        }
        chars.push(next);
    }
    chars.into_iter().collect()
}

/// The search-stage query that carries `body` as one regex filter.
fn query(body: &str) -> String {
    format!("message=/{body}/")
}

/// The largest `k` at or under [`MAX_K`] whose pattern the parser admits,
/// with the reason the next `k` up was refused.
fn largest_admitted(family: &Family, x: Alphabet) -> Option<(usize, String)> {
    let mut refusal = String::from("none: k is at the position limit");
    for k in (1..=MAX_K).rev() {
        match parse(&query(&(family.body)(x, k))) {
            Ok(_) => return Some((k, refusal)),
            Err(errors) => {
                refusal = errors
                    .first()
                    .map_or_else(String::new, |e| e.message.clone());
            }
        }
    }
    None
}

/// Median and maximum of `samples`, in milliseconds.
fn summary(samples: &mut [Duration]) -> (f64, f64) {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    (
        ms(samples[samples.len() / 2]),
        ms(samples[samples.len() - 1]),
    )
}

/// Time `call` [`RUNS`] times after one warm-up, checking each answer is
/// `false`.
fn time_calls(mut call: impl FnMut() -> bool) -> (f64, f64) {
    assert!(!call(), "the subject must not match");
    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        let matched = call();
        samples.push(start.elapsed());
        assert!(!matched, "the subject must not match");
    }
    summary(&mut samples)
}

struct Row {
    family: &'static str,
    alphabet: &'static str,
    a_tenths: u64,
    k: usize,
    body: String,
    rust: (f64, f64),
    duckdb: (f64, f64),
}

/// Time `body` over `text` in both engines.
fn time_engines(conn: &Connection, body: &str, text: &str) -> ((f64, f64), (f64, f64)) {
    let mut preparer = Preparer::new();
    let pattern = preparer
        .build(PatternKind::Regex, body)
        .expect("the parser admitted it, so it builds");
    let rust = time_calls(|| pattern.is_match(text));

    conn.execute("DELETE FROM subject", [])
        .expect("clear the subject table");
    conn.execute("INSERT INTO subject VALUES (?)", params![text])
        .expect("store the subject");
    let mut stmt = conn
        .prepare("SELECT regexp_matches(message, ?) FROM subject")
        .expect("prepare the regexp_matches call");
    let duckdb = time_calls(|| {
        stmt.query_row(params![body], |row| row.get::<_, bool>(0))
            .expect("regexp_matches returns one row")
    });
    (rust, duckdb)
}

/// The search: every family over every alphabet and `a` share, at the
/// family's largest admitted `k`.
fn search(conn: &Connection, out: &mut String) -> Vec<Row> {
    let mut rows = Vec::new();
    for family in &FAMILIES {
        for x in ALPHABETS {
            let Some((k, refusal)) = largest_admitted(family, x) else {
                let _ = writeln!(
                    out,
                    "{:<36} {:<14} no k in 1..={MAX_K} is admitted",
                    family.name, x.name
                );
                continue;
            };
            let body = (family.body)(x, k);
            for a_tenths in A_SHARES {
                let text = subject(x, (family.gap)(k), a_tenths);
                let (rust, duckdb) = time_engines(conn, &body, &text);
                let row = Row {
                    family: family.name,
                    alphabet: x.name,
                    a_tenths,
                    k,
                    body: body.clone(),
                    rust,
                    duckdb,
                };
                let _ = writeln!(
                    out,
                    "{:<36} {:<14} 0.{} {:>3} {:>5} {:>9.2} {:>9.2} {:>9.2} {:>9.2}  next k: {}",
                    row.family,
                    row.alphabet,
                    row.a_tenths,
                    row.k,
                    row.body.len(),
                    row.rust.0,
                    row.rust.1,
                    row.duckdb.0,
                    row.duckdb.1,
                    refusal
                );
                rows.push(row);
            }
        }
    }
    rows
}

/// Time a whole admission, parse to built patterns, of `q`.
fn time_parse(q: &str) -> (f64, f64) {
    parse(q).expect("the query is admitted");
    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        let parsed = parse(q);
        samples.push(start.elapsed());
        assert!(parsed.is_ok(), "the query is admitted");
    }
    summary(&mut samples)
}

fn admission(out: &mut String) {
    // The calibration's slowest admitted translation (ADR-0063, CP2):
    // (?i) and 14 full-range brackets, 15 844 598 units, under the
    // 2^24-unit budget. One more bracket is refused.
    let brackets = "[\\x00-\\x{10FFFF}]".repeat(14);
    let worst_units = query(&format!("(?i){brackets}"));
    let refused = query(&format!("(?i){brackets}[\\x00-\\x{{10FFFF}}]"));
    let (median, max) = time_parse(&worst_units);
    let _ = writeln!(
        out,
        "most translation units, (?i) + 14 full-range brackets (15 844 598 units): median {median:.2} ms, max {max:.2} ms"
    );
    let _ = writeln!(
        out,
        "  one more bracket (16 976 355 units) is refused: {}",
        parse(&refused).is_err()
    );

    // The most compile work one query may hold: 256 distinct patterns
    // (MAX_QUERY_PATTERNS), each the largest \w{k} under the 1 MiB
    // compiled-size limit. Distinct, because a ledger builds each text
    // once.
    let k = (1..=MAX_K)
        .rev()
        .find(|k| parse(&query(&format!("\\w{{{k}}}"))).is_ok())
        .expect("some \\w{k} is admitted");
    let list: Vec<String> = (0..256).map(|i| format!("/\\w{{{k}}}{i}/")).collect();
    let most_compile = format!("message={}", list.join(","));
    let (median, max) = time_parse(&most_compile);
    let _ = writeln!(
        out,
        "most compile work, 256 distinct \\w{{{k}}}N patterns at the compiled-size limit: median {median:.2} ms, max {max:.2} ms"
    );
}

#[test]
#[ignore = "wall-clock evidence for a reader; run through scripts/evidence/issue-334/cost.sh"]
fn regex_cost_transcript() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    conn.execute("CREATE TABLE subject (message VARCHAR)", [])
        .expect("create the subject table");

    let mut out = String::new();
    let _ = writeln!(
        out,
        "subject: {SUBJECT_CHARS} characters; {RUNS} timed calls per engine after one warm-up; ceiling {} ms per call",
        CEILING.as_millis()
    );
    let _ = writeln!(
        out,
        "\n## the search: each family at its largest admitted k (ms per call)\n"
    );
    let _ = writeln!(
        out,
        "{:<36} {:<14} {:>3} {:>3} {:>5} {:>9} {:>9} {:>9} {:>9}",
        "family", "alphabet", "a", "k", "bytes", "rust med", "rust max", "re2 med", "re2 max"
    );
    let rows = search(&conn, &mut out);
    let ceiling_ms = CEILING.as_secs_f64() * 1000.0;
    worst_cases(&rows, ceiling_ms, &mut out);
    sweep(&conn, &rows, &mut out);

    let _ = writeln!(
        out,
        "\n## admission of the worst admitted queries (parse, price and build)\n"
    );
    admission(&mut out);

    let over_median = rows
        .iter()
        .filter(|r| r.rust.0 > ceiling_ms || r.duckdb.0 > ceiling_ms)
        .count();
    let over_max = rows
        .iter()
        .filter(|r| r.rust.1 > ceiling_ms || r.duckdb.1 > ceiling_ms)
        .count();
    let _ = writeln!(
        out,
        "\nverdict: of {} admitted cases, {over_median} exceed {ceiling_ms:.0} ms per call at the median and {over_max} at the max, in either engine",
        rows.len()
    );
    print!("{out}");
    assert_eq!(over_max, 0, "an admitted case exceeds the ceiling");
}

/// The repeat counts [`sweep`] times the worst families at, each under
/// [`MAX_K`], which the search already timed.
const SWEEP_K: [usize; 5] = [8, 16, 24, 32, 48];

/// How the slowest family in each engine scales with `k`, on the subject
/// it was slowest over, at each [`SWEEP_K`] below the family's largest
/// admitted `k`.
fn sweep(conn: &Connection, rows: &[Row], out: &mut String) {
    let _ = writeln!(
        out,
        "\n## the slowest family in each engine at smaller k (median / max ms per call)\n"
    );
    let slowest = |pick: fn(&Row) -> f64| {
        rows.iter()
            .max_by(|l, r| pick(l).total_cmp(&pick(r)))
            .expect("some family is admitted")
    };
    for (engine, row) in [
        ("rust", slowest(|r| r.rust.0)),
        ("duckdb re2", slowest(|r| r.duckdb.0)),
    ] {
        let family = FAMILIES
            .iter()
            .find(|f| f.name == row.family)
            .expect("a searched family");
        let x = ALPHABETS
            .into_iter()
            .find(|x| x.name == row.alphabet)
            .expect("a searched alphabet");
        let _ = writeln!(
            out,
            "{engine}'s slowest: {} / {} / a 0.{}",
            row.family, row.alphabet, row.a_tenths
        );
        for k in SWEEP_K.into_iter().filter(|&k| k < row.k) {
            let body = (family.body)(x, k);
            assert!(parse(&query(&body)).is_ok(), "a smaller k is admitted");
            let text = subject(x, (family.gap)(k), row.a_tenths);
            let (rust, duckdb) = time_engines(conn, &body, &text);
            let _ = writeln!(
                out,
                "  k = {k:>3}: rust {:>8.2} / {:>8.2}   re2 {:>8.2} / {:>8.2}",
                rust.0, rust.1, duckdb.0, duckdb.1
            );
        }
    }
}

/// The slowest case in each engine, by median, and every case over the
/// ceiling.
fn worst_cases(rows: &[Row], ceiling_ms: f64, out: &mut String) {
    let worst = |pick: fn(&Row) -> f64| {
        rows.iter()
            .max_by(|l, r| pick(l).total_cmp(&pick(r)))
            .expect("some family is admitted")
    };
    let rust = worst(|r| r.rust.0);
    let re2 = worst(|r| r.duckdb.0);
    let _ = writeln!(
        out,
        "\n## the worst admitted case in each engine, by median\n"
    );
    for (engine, row, (median, max)) in [("rust", rust, rust.rust), ("duckdb re2", re2, re2.duckdb)]
    {
        let _ = writeln!(
            out,
            "{engine}: {} / {} / a 0.{}, k = {}: median {median:.2} ms, max {max:.2} ms per call, {} the {ceiling_ms:.0} ms ceiling",
            row.family,
            row.alphabet,
            row.a_tenths,
            row.k,
            if median <= ceiling_ms {
                "under"
            } else {
                "OVER"
            }
        );
        let _ = writeln!(out, "  body ({} bytes): {}", row.body.len(), row.body);
    }
    let _ = writeln!(
        out,
        "\n## every case over the ceiling (median or max, ms per call)\n"
    );
    for r in rows
        .iter()
        .filter(|r| r.rust.1 > ceiling_ms || r.duckdb.1 > ceiling_ms)
    {
        let _ = writeln!(
            out,
            "{} / {} / a 0.{}: rust {:.2} / {:.2}, re2 {:.2} / {:.2}  body: {}",
            r.family, r.alphabet, r.a_tenths, r.rust.0, r.rust.1, r.duckdb.0, r.duckdb.1, r.body
        );
    }
}
